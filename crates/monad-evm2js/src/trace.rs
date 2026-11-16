use alloy_primitives::{U256, address, hex};
use evm2::{
    Inspector,
    interpreter::{
        InstrStop, Interpreter, Message, MessageKind, MessageResult,
        opcode::{
            OpCode,
            op::{CLZ, KECCAK256},
        },
    },
};
use monad_evm2::{MonadEvmTypes, MonadHardfork, extension};
use serde::Serialize;
use std::{collections::BTreeMap, ops::Range};

const MAX_STEPS: usize = 100_000;
const MAX_TRACE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
struct PendingStep {
    index: usize,
    hash_input: Option<Range<usize>>,
    gas_before_child: Option<u64>,
}

fn hash_input_range(stack: &[U256]) -> Option<Range<usize>> {
    let [.., len, offset] = stack else { return None };
    let len = usize::try_from(*len).ok()?;
    // A zero-length hash ignores the offset, even if it does not fit in usize.
    let start = if len == 0 { 0 } else { usize::try_from(*offset).ok()? };
    Some(start..start.checked_add(len)?)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Step {
    pc: usize,
    op: String,
    gas: u64,
    gas_cost: u64,
    depth: usize,
    stack: Vec<String>,
    memory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug)]
pub(super) struct Trace {
    pub(super) steps: Vec<Step>,
    pub(super) preimages: BTreeMap<String, String>,
    pub(super) code_by_address: BTreeMap<String, String>,
    pub(super) console_calls: Vec<String>,
    pub(super) truncated: bool,
    spec: MonadHardfork,
    bytes: usize,
    pending: Vec<Option<PendingStep>>,
}

impl Trace {
    pub(super) const fn new(spec: MonadHardfork) -> Self {
        Self {
            steps: Vec::new(),
            preimages: BTreeMap::new(),
            code_by_address: BTreeMap::new(),
            console_calls: Vec::new(),
            truncated: false,
            spec,
            bytes: 0,
            pending: Vec::new(),
        }
    }

    fn record_preimage(
        &mut self,
        interp: &Interpreter<'_, '_, MonadEvmTypes>,
        input: Range<usize>,
    ) {
        let Some(hash) = interp.stack().as_slice().last().copied() else { return };
        let hash = format!("{hash:064x}");
        if self.preimages.contains_key(&hash) {
            return;
        }
        let size = 128 + input.len() * 2;
        if self.bytes + size > MAX_TRACE_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += size;
        self.preimages.insert(hash, hex::encode(&interp.memory().as_slice()[input]));
    }
}

impl Inspector<MonadEvmTypes> for Trace {
    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        // Keep the requested callee as the key, even when EIP-7702 resolves other code.
        if interp.message().kind.is_create() {
            return;
        }
        let address = interp.message().call_target;
        let address = format!("{address:#x}");
        if self.truncated || self.code_by_address.contains_key(&address) {
            return;
        }
        let code = interp.original_bytecode_slice();
        let size = 96 + code.len() * 2;
        if self.bytes + size > MAX_TRACE_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += size;
        self.code_by_address.insert(address, hex::encode_prefixed(code));
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.before_child(interp, message);
        if message.kind == MessageKind::StaticCall
            && message.call_target == address!("000000000000000000636f6e736f6c652e6c6f67")
        {
            self.console_calls.push(hex::encode_prefixed(message.input.as_bytes(interp.memory())));
        }
        None
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.before_child(interp, message);
        None
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let depth = usize::from(interp.message().depth);
        self.pending.resize_with(depth + 1, || None);
        self.pending[depth] = None;
        let size =
            128 + interp.stack().as_slice().len() * 66 + interp.memory().as_slice().len() * 2;
        if self.truncated || self.steps.len() >= MAX_STEPS || self.bytes + size > MAX_TRACE_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes += size;
        let pc = interp.pc();
        let opcode = interp.opcode();
        let op = if opcode == extension::EXTENSION && self.spec == MonadHardfork::MonadNext {
            match interp.original_bytecode_slice().get(pc + 1) {
                Some(&extension::CALLSTACKDEPTH_SELECTOR) => "CALLSTACKDEPTH",
                Some(&extension::CALLERN_SELECTOR) => "CALLERN",
                _ => "INVALID",
            }
        } else if opcode == CLZ && self.spec == MonadHardfork::MonadEight {
            "INVALID"
        } else {
            OpCode::new(opcode).map_or("INVALID", |op| op.as_str())
        };
        let memory = interp.memory().as_slice();
        self.pending[depth] = Some(PendingStep {
            gas_before_child: None,
            index: self.steps.len(),
            hash_input: if opcode == KECCAK256 {
                hash_input_range(interp.stack().as_slice())
            } else {
                None
            },
        });
        self.steps.push(Step {
            pc,
            op: op.to_owned(),
            gas: interp.gas().remaining(),
            gas_cost: 0,
            depth: depth + 1,
            stack: interp.stack().as_slice().iter().map(|word| format!("{word:064x}")).collect(),
            memory: hex::encode(memory),
            error: None,
        });
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let depth = usize::from(interp.message().depth);
        if let Some(pending) = self.pending.get_mut(depth).and_then(Option::take) {
            let step = &mut self.steps[pending.index];
            // revm reports CALL/CREATE before the child runs. Keep that gas convention
            // while evm2's step_end runs after the child has returned.
            step.gas_cost = step
                .gas
                .saturating_sub(pending.gas_before_child.unwrap_or(interp.gas().remaining()));
            if let Err(stop) = interp.result() {
                if !stop.is_success() {
                    step.error = Some(format!("{stop:?}"));
                }
            } else if let Some(input) = pending.hash_input {
                self.record_preimage(interp, input);
            }
        }
    }
}

impl Trace {
    fn before_child(
        &mut self,
        interp: &Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
    ) {
        if message.depth == 0 {
            return;
        }
        if let Some(pending) =
            self.pending.get_mut(usize::from(message.depth - 1)).and_then(Option::as_mut)
        {
            pending.gas_before_child = Some(interp.gas().remaining());
        }
    }
}

pub(super) fn halt_error(stop: InstrStop) -> String {
    let reason = match stop {
        InstrStop::OutOfGas => "OutOfGas(Basic)".into(),
        InstrStop::MemoryOOG => "OutOfGas(Memory)".into(),
        InstrStop::MemoryLimitOOG => "OutOfGas(MemoryLimit)".into(),
        InstrStop::PrecompileOOG => "OutOfGas(Precompile)".into(),
        InstrStop::InvalidOperandOOG => "OutOfGas(InvalidOperand)".into(),
        InstrStop::ReentrancySentryOOG => "OutOfGas(ReentrancySentry)".into(),
        other => format!("{other:?}"),
    };
    format!("Ethereum({reason})")
}
