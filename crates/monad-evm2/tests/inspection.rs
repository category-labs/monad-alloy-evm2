//! Inspector subscriptions composed with automatic reserve observation.

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, U256};
use evm2::{
    AccountInfo, Inspector, InspectorConfig, OpcodeSet,
    bytecode::Bytecode,
    evm::InMemoryDB,
    interpreter::{InstrStop, Interpreter, Message, MessageResult, op},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmBuilder, MonadEvmTypes, MonadHaltReason, MonadHardfork,
    MonadTxEnvelope,
};

const SENDER: Address = Address::repeat_byte(0x11);
const ROOT: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);

fn evm(spec: MonadHardfork, code: Vec<u8>) -> MonadEvm<'static> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(1_000_000)));
    for (address, code) in [(ROOT, code), (CHILD, vec![op::PUSH1, 2, op::POP, op::STOP])] {
        db.insert_account_info(
            &address,
            AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::from(code))),
        );
    }
    MonadEvmBuilder::new(1).create_evm(spec, MonadBlockEnv::default(), db)
}

fn tx(value: u64) -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: ROOT.into(),
            value: U256::from(value),
            gas_limit: 1_000_000,
            ..Default::default()
        }),
        SENDER,
    )
}

#[derive(Default)]
struct Trace {
    steps: OpcodeSet,
    after_child: Option<OpcodeSet>,
    started: Vec<(u16, u8)>,
    ended: Vec<(u16, u8)>,
    pending: Vec<(u16, u8)>,
    messages: usize,
}

impl Inspector<MonadEvmTypes> for Trace {
    fn inspector_config(&self) -> InspectorConfig {
        InspectorConfig::new().with_steps(self.steps)
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let step = (interp.message().depth, interp.opcode());
        self.started.push(step);
        self.pending.push(step);
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let step = self.pending.pop().expect("step_end must have a matching step");
        assert_eq!(step.0, interp.message().depth);
        self.ended.push(step);
    }

    fn call(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.messages += 1;
        if message.depth > 0
            && let Some(steps) = self.after_child.take()
        {
            self.steps = steps;
        }
        None
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.call(interp, message)
    }
}

fn message_code(opcode: u8) -> Vec<u8> {
    let mut code = Vec::new();
    if matches!(opcode, op::CREATE | op::CREATE2) {
        if opcode == op::CREATE2 {
            code.push(op::PUSH0);
        }
        code.extend([op::PUSH1, 1, op::PUSH0, op::PUSH0]);
    } else {
        let args = if matches!(opcode, op::CALL | op::CALLCODE) { 5 } else { 4 };
        code.extend(core::iter::repeat_n(op::PUSH0, args));
        code.push(op::PUSH20);
        code.extend_from_slice(CHILD.as_slice());
        code.extend([op::PUSH2, 0xff, 0xff]);
    }
    code.extend([opcode, op::POP, op::PUSH1, 1, op::PUSH0, op::SSTORE, op::STOP]);
    code
}

#[test]
fn root_step_subscriptions_do_not_inherit_the_reserve_observers_mask() {
    for spec in MonadHardfork::ALL {
        for steps in [OpcodeSet::EMPTY, [op::SSTORE].into_iter().collect(), OpcodeSet::ALL] {
            let mut evm = evm(spec, vec![op::PUSH1, 1, op::PUSH0, op::SSTORE, op::STOP]);
            let mut trace = Trace { steps, ..Default::default() };
            let result = evm.transact_with_inspector(&tx(0), &mut trace).unwrap().commit();
            assert!(result.status);
            let expected = [op::PUSH1, op::PUSH0, op::SSTORE, op::STOP]
                .into_iter()
                .filter(|opcode| steps.contains(*opcode))
                .map(|opcode| (0, opcode))
                .collect::<Vec<_>>();
            assert_eq!(trace.started, expected, "{spec}");
            assert_eq!(trace.ended, expected, "{spec}");
            assert!(trace.pending.is_empty());
            assert_eq!(trace.messages, 1);
        }
    }
}

#[test]
fn child_hooks_can_enable_replace_or_disable_the_parent_subscription() {
    let stores = [op::SSTORE].into_iter().collect();
    for spec in MonadHardfork::ALL {
        for opcode in
            [op::CALL, op::CALLCODE, op::DELEGATECALL, op::STATICCALL, op::CREATE, op::CREATE2]
        {
            let messages = [opcode].into_iter().collect();
            for (before, after, expected) in [
                (OpcodeSet::EMPTY, stores, vec![(0, op::SSTORE)]),
                (stores, OpcodeSet::EMPTY, vec![]),
                (messages, stores, vec![(0, opcode), (0, op::SSTORE)]),
            ] {
                let mut evm = evm(spec, message_code(opcode));
                let mut trace =
                    Trace { steps: before, after_child: Some(after), ..Default::default() };
                let result = evm.transact_with_inspector(&tx(0), &mut trace).unwrap().commit();
                assert!(result.status, "{spec} {opcode:#x}");
                assert_eq!(trace.started, expected, "{spec} {opcode:#x}");
                assert_eq!(trace.ended, expected, "{spec} {opcode:#x}");
                assert!(trace.pending.is_empty());
                assert_eq!(trace.messages, 2);
            }
        }
    }
}

#[test]
fn nested_steps_keep_the_suspended_parent_step_end_subscription() {
    let mut evm = evm(MonadHardfork::MonadTen, message_code(op::CALL));
    let mut trace = Trace {
        steps: [op::CALL].into_iter().collect(),
        after_child: Some([op::POP].into_iter().collect()),
        ..Default::default()
    };
    let result = evm.transact_with_inspector(&tx(0), &mut trace).unwrap().commit();
    assert!(result.status);
    assert_eq!(trace.started, [(0, op::CALL), (1, op::POP), (0, op::POP)]);
    assert_eq!(trace.ended, [(1, op::POP), (0, op::CALL), (0, op::POP)]);
    assert!(trace.pending.is_empty());
}

#[test]
fn unobserved_root_halts_still_enforce_reserves() {
    for code in [
        vec![op::STOP],
        vec![op::INVALID],
        vec![op::SSTORE],
        vec![op::PUSH0, op::PUSH0, op::REVERT],
    ] {
        let mut evm = evm(MonadHardfork::MonadTen, code);
        evm.ext_mut().chain.parent_senders_and_authorities.insert(SENDER);
        let mut trace = Trace { steps: OpcodeSet::EMPTY, ..Default::default() };
        let result = evm.transact_with_inspector(&tx(1), &mut trace).unwrap().commit();
        assert_eq!(result.stop, InstrStop::Revert);
        assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
        assert!(trace.started.is_empty());
        assert!(trace.ended.is_empty());
        assert_eq!(evm.read_account_info(&SENDER).unwrap().unwrap().balance, U256::from(1_000_000));
        assert_eq!(evm.read_account_info(&SENDER).unwrap().unwrap().nonce, 1);
    }
}
