//! Memory pricing, opcode behavior and pooled limits against monad-revm 0.8.0.

mod common;

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use common::FORKS;
use evm2::{
    AccountInfo, ExecutionError, Inspector, TxResult,
    bytecode::Bytecode,
    evm::InMemoryDB,
    inspector::{InspectorConfig, OpcodeSet},
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageKind, MessageResult, op},
};
use monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MONAD_MEMORY_LIMIT, MonadBlockEnv, MonadEvm, MonadEvmFactory,
    MonadEvmTypes, MonadHardfork, MonadTxEnvelope, monad_memory_cost,
};
use monad_revm::{
    MonadBuilder, MonadCfgEnv, MonadHardfork as ReferenceHardfork, monad_context_with_db,
};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, TxEnv},
    context_interface::result::{ExecutionResult, HaltReason, OutOfGasError},
    database::InMemoryDB as ReferenceDB,
    state::{AccountInfo as ReferenceAccount, Bytecode as ReferenceBytecode},
};

const SENDER: Address = Address::repeat_byte(0x11);
const ROOT: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);
const GRANDCHILD: Address = Address::repeat_byte(0x44);
const GAS: u64 = 1_000_000;
const CALLS: [u8; 4] = [op::CALL, op::CALLCODE, op::DELEGATECALL, op::STATICCALL];

struct ChildStepMask {
    steps: OpcodeSet,
    after: OpcodeSet,
    observed: Vec<u8>,
    switches: usize,
}

impl ChildStepMask {
    const fn child(
        &mut self,
        message: &Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        if message.depth == 2 {
            self.steps = self.after;
            self.switches += 1;
        }
        None
    }
}

impl Inspector<MonadEvmTypes> for ChildStepMask {
    fn inspector_config(&self) -> InspectorConfig {
        InspectorConfig::new().with_steps(self.steps)
    }

    fn call(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.child(message)
    }

    fn create(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.child(message)
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if interp.message().depth == 1 {
            self.observed.push(interp.opcode());
        }
    }
}

#[test]
fn child_messages_reload_the_suspended_callers_inspector_mask() {
    let stores = [op::SSTORE].into_iter().collect::<OpcodeSet>();
    let pops = [op::POP].into_iter().collect::<OpcodeSet>();
    for opcode in CALLS.into_iter().chain([op::CREATE, op::CREATE2]) {
        let mut root = vec![];
        empty_call(&mut root, op::CALL, CHILD);
        root.push(op::STOP);
        let mut child = vec![];
        grow(&mut child, 64);
        if matches!(opcode, op::CREATE | op::CREATE2) {
            instruction(
                &mut child,
                opcode,
                &vec![U256::ZERO; if opcode == op::CREATE { 3 } else { 4 }],
            );
        } else {
            empty_call(&mut child, opcode, GRANDCHILD);
        }
        child.extend([op::POP, op::PUSH1, 1, op::PUSH0, op::SSTORE, op::STOP]);
        let mut fixture = Fixture::new(root);
        fixture.accounts.push((CHILD, child.into()));
        for spec in MonadHardfork::ALL {
            for (before, after) in [
                (OpcodeSet::EMPTY, stores),
                (stores, OpcodeSet::EMPTY),
                (pops, stores),
                (stores, stores),
            ] {
                let mut actual = fixture.run(spec, false);
                let tx = Recovered::new_unchecked(
                    MonadTxEnvelope::Legacy(TxLegacy {
                        nonce: 1,
                        to: ROOT.into(),
                        gas_limit: GAS,
                        ..Default::default()
                    }),
                    SENDER,
                );
                let mut inspector =
                    ChildStepMask { steps: before, after, observed: vec![], switches: 0 };
                let result =
                    actual.evm.transact_with_inspector(&tx, &mut inspector).unwrap().commit();
                assert!(result.is_success(), "{spec} {opcode:#x}");
                assert_eq!(inspector.switches, 1);
                let expected = if after == stores { vec![op::SSTORE] } else { vec![] };
                assert_eq!(inspector.observed, expected, "{spec} {opcode:#x}");
                assert_eq!(actual.evm.ext().memory_pool_offset(), 0);
            }
        }
    }
}

#[derive(Debug)]
struct Step {
    opcode: u8,
    cost: u64,
    memory: usize,
}

#[derive(Debug, Default)]
struct Trace {
    opcode: u8,
    gas_before: u64,
    steps: Vec<Step>,
    children: Vec<(MessageKind, InstrStop)>,
}

impl Inspector<MonadEvmTypes> for Trace {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if interp.message().depth == 0 {
            self.opcode = interp.opcode();
            self.gas_before = interp.gas().remaining();
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if interp.message().depth == 0 {
            self.steps.push(Step {
                opcode: self.opcode,
                cost: self.gas_before.saturating_sub(interp.gas().remaining()),
                memory: interp.memory().len(),
            });
        }
    }

    fn call_end(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        if message.depth != 0 {
            self.children.push((message.kind, result.stop));
        }
    }

    fn create_end(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.children.push((message.kind, result.stop));
    }
}

struct Run {
    evm: MonadEvm<'static>,
    result: TxResult<MonadEvmTypes>,
    trace: Trace,
}

struct Fixture {
    code: Bytes,
    accounts: Vec<(Address, Bytes)>,
    input: Bytes,
    gas: u64,
    memory_limit: Option<u64>,
}

impl Fixture {
    fn new(code: Vec<u8>) -> Self {
        Self {
            code: code.into(),
            accounts: vec![],
            input: Bytes::new(),
            gas: GAS,
            memory_limit: None,
        }
    }

    fn run(&self, spec: MonadHardfork, inspect: bool) -> Run {
        let mut db = InMemoryDB::default();
        db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(GAS)));
        for (address, code) in [(ROOT, self.code.clone())].iter().chain(&self.accounts) {
            db.insert_account_info(
                address,
                AccountInfo::default()
                    .with_balance(U256::from(GAS))
                    .with_code(Bytecode::new_raw(code.clone())),
            );
        }
        let mut factory = MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID);
        factory.limits.memory_limit = self.memory_limit;
        factory.limits.tx_gas_limit_cap = Some(self.gas);
        let block = MonadBlockEnv { gas_limit: U256::from(self.gas), ..Default::default() };
        let mut evm = factory.create_evm(spec, block, db);
        let tx = Recovered::new_unchecked(
            MonadTxEnvelope::Legacy(TxLegacy {
                to: TxKind::Call(ROOT),
                gas_limit: self.gas,
                input: self.input.clone(),
                ..Default::default()
            }),
            SENDER,
        );
        let mut trace = Trace::default();
        let result = if inspect {
            evm.transact_with_inspector(&tx, &mut trace).unwrap().commit()
        } else {
            evm.transact(&tx).unwrap().commit()
        };
        assert_eq!(evm.ext().memory_pool_offset(), 0);
        assert_eq!(result.tx_gas_used(), self.gas);
        Run { evm, result, trace }
    }

    fn reference(&self, spec: ReferenceHardfork) -> ExecutionResult {
        let mut db = ReferenceDB::default();
        db.insert_account_info(SENDER, ReferenceAccount::default().with_balance(U256::from(GAS)));
        for (address, code) in [(ROOT, self.code.clone())].iter().chain(&self.accounts) {
            db.insert_account_info(
                *address,
                ReferenceAccount::default()
                    .with_balance(U256::from(GAS))
                    .with_code(ReferenceBytecode::new_raw(code.clone())),
            );
        }
        let mut cfg = MonadCfgEnv::new_with_spec(spec).with_chain_id(MONAD_MAINNET_CHAIN_ID);
        cfg.0.tx_gas_limit_cap = Some(self.gas);
        #[cfg(feature = "memory_limit")]
        if let Some(limit) = self.memory_limit {
            cfg.0.memory_limit = limit;
        }
        let mut evm = monad_context_with_db(db)
            .with_cfg(cfg)
            .with_block(BlockEnv { gas_limit: self.gas, basefee: 0, ..Default::default() })
            .build_monad();
        let tx = TxEnv::builder()
            .caller(SENDER)
            .kind(TxKind::Call(ROOT))
            .gas_limit(self.gas)
            .gas_price(0)
            .chain_id(None)
            .data(self.input.clone())
            .build()
            .unwrap();
        evm.transact(tx).unwrap().result
    }

    fn compare(&self, forks: (MonadHardfork, ReferenceHardfork)) -> Run {
        let actual = self.run(forks.0, true);
        let expected = self.reference(forks.1);
        assert_eq!(actual.result.is_success(), expected.is_success(), "{forks:?}: {expected:?}");
        assert_eq!(actual.result.is_revert(), matches!(expected, ExecutionResult::Revert { .. }));
        assert_eq!(actual.result.output.as_ref(), expected.output().map_or(&[][..], Bytes::as_ref));
        assert_eq!(actual.result.logs, expected.logs());
        assert_eq!(actual.result.total_gas_spent, expected.gas().total_gas_spent(), "{forks:?}");
        if let ExecutionResult::Halt { reason: HaltReason::OutOfGas(_), .. } = expected {
            assert!(actual.result.stop.is_out_of_gas(), "{:?}", actual.result.stop);
        }
        let plain = self.run(forks.0, false);
        assert_eq!(actual.result, plain.result);
        actual
    }
}

fn push(code: &mut Vec<u8>, word: U256) {
    if word.is_zero() {
        code.push(op::PUSH0);
    } else {
        let bytes = word.to_be_bytes::<32>();
        let first = bytes.iter().position(|byte| *byte != 0).unwrap();
        code.push(op::PUSH1 + (31 - first) as u8);
        code.extend_from_slice(&bytes[first..]);
    }
}

fn instruction(code: &mut Vec<u8>, opcode: u8, operands: &[U256]) {
    for value in operands.iter().rev() {
        push(code, *value);
    }
    code.push(opcode);
}

fn grow(code: &mut Vec<u8>, size: usize) {
    if size != 0 {
        instruction(code, op::MSTORE8, &[U256::from(size - 1), U256::from(0xab)]);
    }
}

fn return_word(code: &mut Vec<u8>) {
    push(code, U256::ZERO);
    code.push(op::MSTORE);
    instruction(code, op::RETURN, &[U256::ZERO, U256::from(32)]);
}

fn call(
    code: &mut Vec<u8>,
    opcode: u8,
    target: Address,
    input: (U256, U256),
    output: (U256, U256),
) {
    let mut operands = vec![U256::MAX, target.into_word().into()];
    if matches!(opcode, op::CALL | op::CALLCODE) {
        operands.push(U256::ZERO);
    }
    operands.extend([input.0, input.1, output.0, output.1]);
    instruction(code, opcode, &operands);
}

fn empty_call(code: &mut Vec<u8>, opcode: u8, target: Address) {
    call(code, opcode, target, (U256::ZERO, U256::ZERO), (U256::ZERO, U256::ZERO));
}

fn memory_cost(spec: MonadHardfork, words: usize) -> u64 {
    let words = words as u64;
    if spec >= MonadHardfork::MonadNine { words / 2 } else { 3 * words + words * words / 512 }
}

#[test]
fn word_rounding_and_incremental_expansion_match_release() {
    let sizes = [1, 32, 33, 64, 65, 32, 96, 97, 161];
    let mut code = vec![];
    for size in sizes {
        grow(&mut code, size);
    }
    code.push(op::MSIZE);
    return_word(&mut code);
    let fixture = Fixture::new(code);
    for forks in FORKS {
        let actual = fixture.compare(forks);
        assert_eq!(actual.result.output.as_ref(), U256::from(192).to_be_bytes::<32>());
        let mut previous_words = 0;
        for (step, size) in actual.trace.steps.iter().filter(|s| s.opcode == op::MSTORE8).zip(sizes)
        {
            let words = size.div_ceil(32).max(previous_words);
            assert_eq!(step.memory, words * 32);
            assert_eq!(
                step.cost,
                3 + memory_cost(forks.0, words) - memory_cost(forks.0, previous_words)
            );
            previous_words = words;
        }
    }
    assert_eq!(monad_memory_cost(1), 0);
    assert_eq!(monad_memory_cost(2), 1);
    assert_eq!(monad_memory_cost((MONAD_MEMORY_LIMIT / 32) as usize), 131_072);
}

#[test]
fn memory_opcodes_match_outputs_logs_and_execution_gas() {
    let opcodes = [
        op::MLOAD,
        op::MSTORE,
        op::MSTORE8,
        op::MCOPY,
        op::KECCAK256,
        op::CALLDATACOPY,
        op::CODECOPY,
        op::RETURNDATACOPY,
        op::EXTCODECOPY,
        op::LOG0,
        op::LOG1,
        op::LOG2,
        op::LOG3,
        op::LOG4,
        op::RETURN,
        op::REVERT,
    ];
    let mut child = vec![];
    instruction(&mut child, op::MSTORE, &[U256::ZERO, U256::from(0xfeed)]);
    instruction(&mut child, op::RETURN, &[U256::ZERO, U256::from(96)]);
    for opcode in opcodes {
        for offset in [0usize, 8192] {
            let mut code = vec![];
            instruction(&mut code, op::MSTORE, &[U256::ZERO, U256::from(0x123456)]);
            if opcode == op::RETURNDATACOPY {
                empty_call(&mut code, op::CALL, CHILD);
                code.push(op::POP);
            }
            let offset = U256::from(offset);
            let len = U256::from(33);
            match opcode {
                op::MLOAD => instruction(&mut code, opcode, &[offset]),
                op::MSTORE | op::MSTORE8 => {
                    instruction(&mut code, opcode, &[offset, U256::from(0xfedc)])
                }
                op::MCOPY | op::CALLDATACOPY | op::CODECOPY | op::RETURNDATACOPY => {
                    instruction(&mut code, opcode, &[offset, U256::ZERO, len]);
                }
                op::EXTCODECOPY => instruction(
                    &mut code,
                    opcode,
                    &[CHILD.into_word().into(), offset, U256::ZERO, len],
                ),
                op::LOG0..=op::LOG4 => {
                    let mut args = vec![offset, len];
                    args.extend((0..opcode - op::LOG0).map(|i| U256::from(i + 1)));
                    instruction(&mut code, opcode, &args);
                }
                _ => instruction(&mut code, opcode, &[offset, len]),
            }
            if matches!(opcode, op::MLOAD | op::KECCAK256) {
                push(&mut code, U256::ZERO);
                code.push(op::MSTORE);
            }
            if !matches!(opcode, op::RETURN | op::REVERT) {
                instruction(&mut code, op::RETURN, &[U256::ZERO, offset + U256::from(64)]);
            }
            let mut fixture = Fixture::new(code);
            fixture.accounts.push((CHILD, child.clone().into()));
            fixture.input = Bytes::from_static(b"memory-copy-input");
            for forks in FORKS {
                let actual = fixture.compare(forks);
                assert!(
                    actual.result.is_success() || opcode == op::REVERT,
                    "{forks:?}/{opcode:#x}"
                );
            }
        }
    }
}

#[test]
fn mcopy_handles_overlap_in_both_directions_and_zero_fills_expansion() {
    for (dst, src, len) in [(1, 0, 63), (0, 1, 63), (8192, 0, 65), (0, 8192, 65)] {
        let mut code = vec![];
        instruction(&mut code, op::MSTORE, &[U256::ZERO, U256::MAX]);
        instruction(&mut code, op::MCOPY, &[U256::from(dst), U256::from(src), U256::from(len)]);
        instruction(&mut code, op::RETURN, &[U256::ZERO, U256::from(dst.max(src) + len)]);
        let fixture = Fixture::new(code);
        for forks in FORKS {
            assert!(fixture.compare(forks).result.is_success());
        }
    }
}

#[test]
fn calls_expand_both_ranges_before_the_child_gas_split() {
    let mut child = vec![op::GAS];
    return_word(&mut child);
    for opcode in CALLS {
        for (input_len, output_len) in [(0, 0), (32, 0), (0, 33), (32, 33)] {
            let mut code = vec![];
            call(
                &mut code,
                opcode,
                CHILD,
                (U256::from(8192), U256::from(input_len)),
                (U256::from(8256), U256::from(output_len)),
            );
            code.push(op::POP);
            instruction(&mut code, op::RETURN, &[U256::from(8256), U256::from(output_len)]);
            let mut fixture = Fixture::new(code);
            fixture.accounts.push((CHILD, child.clone().into()));
            for forks in FORKS {
                assert!(fixture.compare(forks).result.is_success());
            }
        }
    }
}

#[test]
fn creation_expansion_keeps_initcode_charges_and_inspector_step_costs() {
    for create2 in [false, true] {
        for len in [0usize, 1, 32, 33] {
            let opcode = if create2 { op::CREATE2 } else { op::CREATE };
            let mut code = vec![];
            let mut args = vec![U256::ZERO, U256::from(8192), U256::from(len)];
            if create2 {
                args.push(U256::ONE);
            }
            instruction(&mut code, opcode, &args);
            return_word(&mut code);
            let mut fixture = Fixture::new(code);
            for forks in FORKS {
                let actual = fixture.compare(forks);
                assert!(actual.result.is_success());
                let step = actual.trace.steps.iter().find(|s| s.opcode == opcode).unwrap();
                let words = if len == 0 { 0 } else { (8192 + len).div_ceil(32) };
                assert_eq!(
                    step.cost,
                    32_000
                        + memory_cost(forks.0, words)
                        + len.div_ceil(32) as u64 * if create2 { 8 } else { 2 }
                );
                let required = actual.result.total_gas_spent;
                fixture.gas = required;
                assert!(fixture.compare(forks).result.is_success());
                fixture.gas = required - 1;
                assert!(!fixture.compare(forks).result.is_success());
                fixture.gas = GAS;
            }
        }
    }
}

#[test]
fn zero_length_ignores_memory_offsets_without_skipping_other_checks() {
    let cases = [
        (op::MCOPY, vec![U256::MAX, U256::MAX, U256::ZERO]),
        (op::KECCAK256, vec![U256::MAX, U256::ZERO]),
        (op::CALLDATACOPY, vec![U256::MAX, U256::MAX, U256::ZERO]),
        (op::CODECOPY, vec![U256::MAX, U256::MAX, U256::ZERO]),
        (op::RETURNDATACOPY, vec![U256::MAX, U256::ZERO, U256::ZERO]),
        (op::EXTCODECOPY, vec![CHILD.into_word().into(), U256::MAX, U256::MAX, U256::ZERO]),
        (op::LOG0, vec![U256::MAX, U256::ZERO]),
        (op::RETURN, vec![U256::MAX, U256::ZERO]),
        (op::REVERT, vec![U256::MAX, U256::ZERO]),
        (op::CREATE, vec![U256::ZERO, U256::MAX, U256::ZERO]),
        (op::CREATE2, vec![U256::ZERO, U256::MAX, U256::ZERO, U256::ZERO]),
    ];
    for (opcode, operands) in cases {
        let mut code = vec![];
        instruction(&mut code, opcode, &operands);
        let fixture = Fixture::new(code);
        for forks in FORKS {
            let actual = fixture.compare(forks);
            assert!(actual.result.is_success() || opcode == op::REVERT);
            assert_eq!(actual.trace.steps.iter().find(|s| s.opcode == opcode).unwrap().memory, 0);
        }
    }
    for opcode in CALLS {
        let mut code = vec![];
        call(&mut code, opcode, CHILD, (U256::MAX, U256::ZERO), (U256::MAX, U256::ZERO));
        for forks in FORKS {
            let actual = Fixture::new(code.clone()).compare(forks);
            assert!(actual.result.is_success());
            assert_eq!(actual.trace.steps.iter().find(|s| s.opcode == opcode).unwrap().memory, 0);
        }
    }
    let mut code = vec![];
    instruction(&mut code, op::RETURNDATACOPY, &[U256::MAX, U256::ONE, U256::ZERO]);
    for forks in FORKS {
        assert_eq!(Fixture::new(code.clone()).compare(forks).result.stop, InstrStop::OutOfOffset);
    }
}

#[test]
fn expansion_gas_and_reserved_copy_cost_have_exact_failure_boundaries() {
    for opcode in [op::MLOAD, op::CODECOPY, op::CALLDATACOPY] {
        let mut code = vec![];
        let mut operands = vec![U256::from(8192)];
        if opcode != op::MLOAD {
            operands.extend([U256::ZERO, U256::from(33)]);
        }
        instruction(&mut code, opcode, &operands);
        let mut fixture = Fixture::new(code);
        for forks in FORKS {
            let used = fixture.compare(forks).result.total_gas_spent;
            fixture.gas = used;
            assert!(fixture.compare(forks).result.is_success());
            fixture.gas = used - 1;
            let actual = fixture.compare(forks);
            assert_eq!(actual.result.stop, InstrStop::MemoryOOG);
            assert_eq!(actual.trace.steps.iter().find(|s| s.opcode == opcode).unwrap().memory, 0);
            assert!(matches!(
                fixture.reference(forks.1),
                ExecutionResult::Halt { reason: HaltReason::OutOfGas(OutOfGasError::Memory), .. }
            ));
            fixture.gas = GAS;
        }
    }
}

#[test]
fn configured_limits_apply_to_rounded_allocations() {
    for (limit, size) in [(0, 1), (31, 1), (32, 32), (33, 33), (63, 33), (64, 64), (64, 65)] {
        let mut code = vec![];
        grow(&mut code, size);
        let mut fixture = Fixture::new(code);
        fixture.memory_limit = Some(limit);
        for forks in FORKS {
            let actual = fixture.compare(forks);
            let fits = !cfg!(feature = "memory_limit") || size.div_ceil(32) as u64 * 32 <= limit;
            assert_eq!(actual.result.is_success(), fits);
            if !fits {
                assert_eq!(actual.result.stop, InstrStop::MemoryLimitOOG);
            }
        }
    }
}

#[test]
fn eight_mebibytes_is_a_pooled_protocol_cap_from_nine() {
    for size in [MONAD_MEMORY_LIMIT - 1, MONAD_MEMORY_LIMIT, MONAD_MEMORY_LIMIT + 1] {
        let mut code = vec![];
        grow(&mut code, size as usize);
        let mut fixture = Fixture::new(code);
        fixture.memory_limit = Some(MONAD_MEMORY_LIMIT * 2);
        for forks in FORKS.into_iter().skip(1) {
            let actual = fixture.compare(forks);
            let fits = !cfg!(feature = "memory_limit") || size <= MONAD_MEMORY_LIMIT;
            assert_eq!(actual.result.is_success(), fits);
            if !fits {
                assert_eq!(actual.result.stop, InstrStop::MemoryLimitOOG);
            }
        }
    }
}

#[test]
fn parent_and_child_memory_share_the_configured_limit_for_every_call_kind() {
    for opcode in CALLS {
        for child_size in [64, 65] {
            let mut parent = vec![];
            grow(&mut parent, 64);
            empty_call(&mut parent, opcode, CHILD);
            return_word(&mut parent);
            let mut child = vec![];
            grow(&mut child, child_size);
            let mut fixture = Fixture::new(parent);
            fixture.accounts.push((CHILD, child.into()));
            fixture.memory_limit = Some(128);
            for forks in FORKS {
                let actual = fixture.compare(forks);
                let fits = !cfg!(feature = "memory_limit") || child_size <= 64;
                assert_eq!(
                    actual.result.output.as_ref(),
                    U256::from(u8::from(fits)).to_be_bytes::<32>()
                );
                assert_eq!(actual.trace.children[0].1.is_success(), fits);
            }
        }
    }
}

#[test]
fn memory_is_released_after_success_revert_and_exceptional_halt() {
    for exit in [op::STOP, op::REVERT, op::INVALID] {
        let mut parent = vec![];
        grow(&mut parent, 64);
        empty_call(&mut parent, op::CALL, CHILD);
        parent.push(op::POP);
        empty_call(&mut parent, op::CALL, GRANDCHILD);
        return_word(&mut parent);
        let mut child = vec![];
        grow(&mut child, 64);
        if exit == op::REVERT {
            instruction(&mut child, exit, &[U256::ZERO, U256::ZERO]);
        } else {
            child.push(exit);
        }
        let mut grandchild = vec![];
        grow(&mut grandchild, 64);
        let mut fixture = Fixture::new(parent);
        fixture.accounts = vec![(CHILD, child.into()), (GRANDCHILD, grandchild.into())];
        fixture.memory_limit = Some(128);
        for forks in FORKS {
            let mut actual = fixture.compare(forks);
            assert_eq!(actual.result.output.as_ref(), U256::ONE.to_be_bytes::<32>());
            assert!(actual.trace.children[1].1.is_success());
            let next = Recovered::new_unchecked(
                MonadTxEnvelope::Legacy(TxLegacy {
                    nonce: 1,
                    to: TxKind::Call(GRANDCHILD),
                    gas_limit: GAS,
                    ..Default::default()
                }),
                SENDER,
            );
            assert!(actual.evm.transact(&next).unwrap().commit().is_success());
            assert_eq!(actual.evm.ext().memory_pool_offset(), 0);
        }
    }
}

#[test]
fn nested_calls_count_all_ancestors() {
    let mut parent = vec![];
    grow(&mut parent, 64);
    call(&mut parent, op::CALL, CHILD, (U256::ZERO, U256::ZERO), (U256::ZERO, U256::from(32)));
    parent.push(op::POP);
    instruction(&mut parent, op::RETURN, &[U256::ZERO, U256::from(32)]);
    let mut child = vec![];
    grow(&mut child, 64);
    empty_call(&mut child, op::CALL, GRANDCHILD);
    return_word(&mut child);
    let mut grandchild = vec![];
    grow(&mut grandchild, 1);
    let mut fixture = Fixture::new(parent);
    fixture.accounts = vec![(CHILD, child.into()), (GRANDCHILD, grandchild.into())];
    fixture.memory_limit = Some(128);
    for forks in FORKS {
        let actual = fixture.compare(forks);
        let fits = !cfg!(feature = "memory_limit");
        assert_eq!(actual.result.output.as_ref(), U256::from(u8::from(fits)).to_be_bytes::<32>());
        assert_eq!(actual.trace.children[0].1.is_success(), fits);
        assert!(actual.trace.children[1].1.is_success());
    }
}

#[test]
fn initcode_uses_the_same_pool_and_releases_it_after_creation() {
    for opcode in [op::CREATE, op::CREATE2] {
        for size in [64, 65] {
            let mut init = vec![];
            grow(&mut init, size);
            init.push(op::STOP);
            let mut word = [0u8; 32];
            word[..init.len()].copy_from_slice(&init);
            let mut parent = vec![];
            grow(&mut parent, 64);
            instruction(&mut parent, op::MSTORE, &[U256::ZERO, U256::from_be_bytes(word)]);
            let mut args = vec![U256::ZERO, U256::ZERO, U256::from(init.len())];
            if opcode == op::CREATE2 {
                args.push(U256::ONE);
            }
            instruction(&mut parent, opcode, &args);
            return_word(&mut parent);
            let mut fixture = Fixture::new(parent);
            fixture.memory_limit = Some(128);
            for forks in FORKS {
                let actual = fixture.compare(forks);
                let fits = !cfg!(feature = "memory_limit") || size <= 64;
                assert_eq!(U256::from_be_slice(&actual.result.output).is_zero(), !fits);
                assert_eq!(actual.trace.children[0].1.is_success(), fits);
            }
        }
    }
}

#[test]
fn enormous_offsets_and_lengths_halt_without_allocating() {
    let cases = [
        (op::MLOAD, vec![U256::MAX]),
        (op::MSTORE, vec![U256::from(usize::MAX), U256::ZERO]),
        (op::MCOPY, vec![U256::MAX, U256::ZERO, U256::ONE]),
        (op::CALLDATACOPY, vec![U256::ZERO, U256::ZERO, U256::MAX]),
        (op::CODECOPY, vec![U256::MAX, U256::ZERO, U256::ONE]),
        (op::RETURN, vec![U256::from(usize::MAX), U256::ONE]),
        (op::CREATE, vec![U256::ZERO, U256::ZERO, U256::MAX]),
    ];
    for (opcode, args) in cases {
        let mut code = vec![];
        instruction(&mut code, opcode, &args);
        let fixture = Fixture::new(code);
        for forks in FORKS {
            let actual = fixture.compare(forks);
            assert!(actual.result.stop.is_out_of_gas());
            assert_eq!(actual.trace.steps.iter().find(|s| s.opcode == opcode).unwrap().memory, 0);
        }
    }
}

#[test]
fn initcode_gas_is_reserved_before_allocating_creation_memory() {
    for opcode in [op::CREATE, op::CREATE2] {
        let mut code = vec![];
        let mut args = vec![U256::ZERO, U256::ZERO, U256::from(33)];
        if opcode == op::CREATE2 {
            args.push(U256::ONE);
        }
        instruction(&mut code, opcode, &args);
        let mut fixture = Fixture::new(code);
        for forks in FORKS {
            let pushes = if opcode == op::CREATE2 { 10 } else { 7 };
            let expansion_boundary = 21_000 + pushes + 4 + memory_cost(forks.0, 2);
            for enough_memory_gas in [false, true] {
                fixture.gas = expansion_boundary - u64::from(!enough_memory_gas);
                let actual = fixture.compare(forks);
                let step = actual.trace.steps.iter().find(|s| s.opcode == opcode).unwrap();
                assert_eq!(step.memory, if enough_memory_gas { 64 } else { 0 });
                assert_eq!(
                    actual.result.stop,
                    if enough_memory_gas { InstrStop::OutOfGas } else { InstrStop::MemoryOOG }
                );
                assert!(actual.trace.children.is_empty());
            }
        }
    }
}

#[test]
fn static_guards_precede_memory_expansion_and_invalid_offsets() {
    for opcode in [op::LOG0, op::CREATE, op::CREATE2, op::CALL] {
        let mut parent = vec![];
        empty_call(&mut parent, op::STATICCALL, CHILD);
        return_word(&mut parent);
        let mut child = vec![];
        match opcode {
            op::LOG0 => instruction(&mut child, opcode, &[U256::MAX, U256::ONE]),
            op::CREATE => instruction(&mut child, opcode, &[U256::ZERO, U256::MAX, U256::ONE]),
            op::CREATE2 => {
                instruction(&mut child, opcode, &[U256::ZERO, U256::MAX, U256::ONE, U256::ZERO])
            }
            _ => instruction(
                &mut child,
                opcode,
                &[
                    U256::MAX,
                    GRANDCHILD.into_word().into(),
                    U256::ONE,
                    U256::MAX,
                    U256::ONE,
                    U256::MAX,
                    U256::ONE,
                ],
            ),
        }
        let mut fixture = Fixture::new(parent);
        fixture.accounts.push((CHILD, child.into()));
        for forks in FORKS {
            let actual = fixture.compare(forks);
            assert_eq!(actual.result.output.as_ref(), U256::ZERO.to_be_bytes::<32>());
            assert_eq!(
                actual.trace.children[0].1,
                if opcode == op::CALL {
                    InstrStop::CallNotAllowedInsideStatic
                } else {
                    InstrStop::StateChangeDuringStaticCall
                }
            );
        }
    }
}

#[derive(Debug)]
struct Intercept {
    fatal: bool,
    count: usize,
}

impl Intercept {
    fn child(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        if message.depth == 0 {
            return None;
        }
        self.count += 1;
        assert_eq!(interp.host().ext().memory_pool_offset(), 64);
        if self.fatal {
            interp.fail(ExecutionError::Fatal("injected execution failure".into()));
            None
        } else {
            Some(MessageResult::<MonadEvmTypes> {
                stop: InstrStop::Stop,
                gas: GasTracker::new(message.gas_limit),
                created_address: matches!(message.kind, MessageKind::Create | MessageKind::Create2)
                    .then_some(message.destination),
                ..Default::default()
            })
        }
    }
}

impl Inspector<MonadEvmTypes> for Intercept {
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.child(interp, message)
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.child(interp, message)
    }
}

#[test]
fn inspector_overrides_and_fatal_errors_restore_the_memory_pool() {
    for opcode in CALLS.into_iter().chain([op::CREATE, op::CREATE2]) {
        let mut parent = vec![];
        grow(&mut parent, 64);
        if matches!(opcode, op::CREATE | op::CREATE2) {
            let args = vec![U256::ZERO; if opcode == op::CREATE { 3 } else { 4 }];
            instruction(&mut parent, opcode, &args);
        } else {
            empty_call(&mut parent, opcode, CHILD);
        }
        return_word(&mut parent);
        let mut fixture = Fixture::new(parent);
        fixture.memory_limit = Some(128);
        for (spec, _) in FORKS {
            for fatal in [false, true] {
                let mut actual = fixture.run(spec, false);
                let tx = |nonce| {
                    Recovered::new_unchecked(
                        MonadTxEnvelope::Legacy(TxLegacy {
                            nonce,
                            to: TxKind::Call(ROOT),
                            gas_limit: GAS,
                            ..Default::default()
                        }),
                        SENDER,
                    )
                };
                let mut inspector = Intercept { fatal, count: 0 };
                let result = actual
                    .evm
                    .transact_with_inspector(&tx(1), &mut inspector)
                    .map(|tx| tx.commit());
                assert_eq!(result.is_err(), fatal);
                assert_eq!(inspector.count, 1);
                assert_eq!(actual.evm.ext().memory_pool_offset(), 0);
                let nonce =
                    actual.evm.state_mut().account_info_untracked(&SENDER).unwrap().unwrap().nonce;
                assert!(actual.evm.transact(&tx(nonce)).unwrap().commit().is_success());
                assert_eq!(actual.evm.ext().memory_pool_offset(), 0);
            }
        }
    }
}
