//! Inspector callbacks and lifecycle isolation for protocol system calls.

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, Log, U256};
use evm2::{
    AccountInfo, ExecutionConfig, ExecutionError, Inspector, TxResult,
    bytecode::Bytecode,
    evm::{BlockStateAccumulator, InMemoryDB, SystemTx},
    inspector::InspectorConfig,
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageResult, op},
    registry::{TxRequest, handler},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork, MonadHost,
    MonadTxEnvelope, ReserveBalanceInit, SYSTEM_ADDRESS, reserve_balance::reserve_account,
};
use std::{
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};

const ROOT: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);
const BENEFICIARY: Address = Address::repeat_byte(0x44);
const GAS: u64 = 1_000_000;

fn evm(spec: MonadHardfork, stop: u8) -> MonadEvm<'static> {
    let mut code = vec![op::PUSH1, 1, op::PUSH0, op::SSTORE, op::PUSH0, op::PUSH0, op::LOG0];
    for (opcode, offset) in
        [(op::CALLER, 0), (op::ORIGIN, 32), (op::GASPRICE, 64), (op::CALLVALUE, 96)]
    {
        code.extend([opcode, op::PUSH1, offset, op::MSTORE]);
    }
    // Exercise both CREATE callbacks, then a nested CALL with a value transfer.
    code.extend([op::PUSH0, op::PUSH0, op::PUSH0, op::CREATE, op::POP]);
    code.extend([op::PUSH0, op::PUSH0, op::PUSH0, op::PUSH0, op::PUSH1, 1, op::PUSH20]);
    code.extend(CHILD.as_slice());
    code.extend([op::GAS, op::CALL, op::POP, op::PUSH1, 128, op::PUSH0, stop]);
    let child = Bytes::from_static(&[
        op::PUSH1,
        2,
        op::PUSH0,
        op::SSTORE,
        op::PUSH0,
        op::PUSH0,
        op::LOG0,
        op::STOP,
    ]);
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SYSTEM_ADDRESS,
        AccountInfo::default()
            .with_nonce(7)
            .with_balance(U256::ONE)
            .with_code(Bytecode::new_legacy(Bytes::from_static(&[op::INVALID]))),
    );
    db.insert_account_info(
        &ROOT,
        AccountInfo::default()
            .with_balance(U256::from(10))
            .with_code(Bytecode::new_legacy(code.into())),
    );
    db.insert_account_info(&CHILD, AccountInfo::default().with_code(Bytecode::new_legacy(child)));
    let mut factory = MonadEvmFactory::new(1);
    // Ordinary transactions cannot pass these limits or pay this base fee.
    factory.limits.tx_gas_limit_cap = Some(1);
    factory.create_evm(
        spec,
        MonadBlockEnv {
            gas_limit: U256::ONE,
            basefee: U256::from(1_000),
            beneficiary: BENEFICIARY,
            ..Default::default()
        },
        db,
    )
}

fn system_tx() -> SystemTx {
    SystemTx::new(ROOT, Bytes::from_static(&[1, 2, 3]))
        .with_caller(SYSTEM_ADDRESS)
        .with_gas_limit(GAS)
}

fn slot(evm: &mut MonadEvm<'_>, address: Address) -> U256 {
    evm.state_mut().storage_slot_untracked(&address, &U256::ZERO).unwrap()
}

#[derive(Default, Debug)]
struct Trace {
    calls: Vec<(u16, Address)>,
    ends: Vec<(u16, InstrStop)>,
    creates: usize,
    create_ends: usize,
    initialized: usize,
    steps: usize,
    step_ends: usize,
    logs: Vec<Address>,
    selfdestructs: Vec<(Address, Address, U256)>,
}

struct Observer(Rc<RefCell<Trace>>);

impl Inspector<MonadEvmTypes> for Observer {
    fn inspector_config(&self) -> InspectorConfig {
        InspectorConfig::new().with_steps([op::SSTORE].into_iter().collect())
    }

    fn initialize_interp(&mut self, _: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.0.borrow_mut().initialized += 1;
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        assert_eq!(interp.opcode(), op::SSTORE);
        self.0.borrow_mut().steps += 1;
    }

    fn step_end(&mut self, _: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.0.borrow_mut().step_ends += 1;
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        assert!(!interp.host().ext().reserve_balance.tracking_enabled());
        assert!(!interp.host().ext().preserve_reserve_balance_tracker);
        if message.depth == 0 {
            assert_eq!(message.caller, SYSTEM_ADDRESS);
            assert_eq!(message.value, U256::ZERO);
            assert_eq!(message.gas_limit, GAS);
            assert_eq!(message.input, system_tx().data);
        }
        self.0.borrow_mut().calls.push((message.depth, message.destination));
        None
    }

    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.0.borrow_mut().ends.push((message.depth, result.stop));
    }

    fn create(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.0.borrow_mut().creates += 1;
        None
    }

    fn create_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        _: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.0.borrow_mut().create_ends += 1;
    }

    fn log(&mut self, log: &Log, _: &mut MonadHost<'_>) {
        self.0.borrow_mut().logs.push(log.address);
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        _: &mut MonadHost<'_>,
    ) {
        self.0.borrow_mut().selfdestructs.push((*contract, *target, *value));
    }
}

#[test]
fn inspection_preserves_native_system_results_and_state_across_forks() {
    for spec in MonadHardfork::ALL {
        for stop in [op::RETURN, op::REVERT, op::INVALID] {
            for mode in 0..3 {
                let mut actual = evm(spec, stop);
                let mut expected = evm(spec, stop);
                let trace = Rc::new(RefCell::new(Trace::default()));
                let result = match mode {
                    0 => actual.system_call(system_tx()).unwrap().commit(),
                    1 => {
                        actual.set_inspector(Observer(trace.clone()));
                        actual.system_call(system_tx()).unwrap().commit()
                    }
                    _ => actual
                        .system_call_with_inspector(system_tx(), &mut Observer(trace.clone()))
                        .unwrap()
                        .commit(),
                };
                let native = MonadHost::system_call(&mut expected, system_tx()).unwrap().commit();
                assert_eq!(result, native, "{spec}, stop={stop:#x}, mode={mode}");
                assert_eq!(result.is_success(), stop == op::RETURN);
                assert_eq!(result.floor_gas, 0);
                if stop != op::INVALID {
                    assert!(result.tx_gas_used() < GAS);
                    let words =
                        result.output.chunks_exact(32).map(U256::from_be_slice).collect::<Vec<_>>();
                    let sender = U256::from_be_slice(SYSTEM_ADDRESS.as_slice());
                    assert_eq!(words, [sender, sender, U256::ZERO, U256::ZERO]);
                }
                for address in [SYSTEM_ADDRESS, ROOT, CHILD, BENEFICIARY, ROOT.create(0)] {
                    assert_eq!(
                        actual.state_mut().account_info_untracked(&address).unwrap(),
                        expected.state_mut().account_info_untracked(&address).unwrap(),
                    );
                    assert_eq!(slot(&mut actual, address), slot(&mut expected, address));
                }
                let sender =
                    actual.state_mut().account_info_untracked(&SYSTEM_ADDRESS).unwrap().unwrap();
                assert_eq!((sender.balance, sender.nonce), (U256::ONE, 7));
                assert_eq!(actual.ext().memory_pool_offset(), 0);
                if mode != 0 {
                    let trace = trace.borrow();
                    assert_eq!(trace.calls, [(0, ROOT), (1, CHILD)]);
                    assert_eq!(trace.ends.len(), 2);
                    assert_eq!(trace.ends[0], (1, InstrStop::Stop));
                    assert_eq!(trace.ends[1], (0, result.stop));
                    assert_eq!((trace.creates, trace.create_ends), (1, 1));
                    assert!(trace.initialized >= 2);
                    assert_eq!((trace.steps, trace.step_ends), (2, 2));
                    assert_eq!(trace.logs, [ROOT, CHILD]);
                }
            }
        }
    }
}

#[test]
fn borrowed_inspector_takes_precedence_and_is_released_before_state_resolution() {
    let mut evm = evm(MonadHardfork::MonadTen, op::RETURN);
    let installed = Rc::new(RefCell::new(Trace::default()));
    evm.set_inspector(Observer(installed.clone()));
    let mut borrowed = Observer(Rc::new(RefCell::new(Trace::default())));
    let pending = evm.system_call_with_inspector(system_tx(), &mut borrowed).unwrap();
    assert_eq!(borrowed.0.borrow().calls.len(), 2);
    assert!(installed.borrow().calls.is_empty());
    assert!(pending.preserved_context().is_none());
    assert!(pending.account_changes().any(|change| change.address == ROOT));
    assert!(pending.discard().is_success());
    assert_eq!(slot(&mut evm, ROOT), U256::ZERO);
    assert!(evm.system_call(system_tx()).unwrap().commit().is_success());
    assert_eq!(installed.borrow().calls.len(), 2);
    assert_eq!(slot(&mut evm, ROOT), U256::ONE);
    assert!(evm.inspector().is_some());
}

#[test]
fn system_calls_report_selfdestruct_transfers_to_the_inspector() {
    for spec in MonadHardfork::ALL {
        let mut evm = evm(spec, op::RETURN);
        let mut code = vec![op::PUSH20];
        code.extend(CHILD.as_slice());
        code.push(op::SELFDESTRUCT);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &ROOT,
            AccountInfo::default()
                .with_balance(U256::from(10))
                .with_code(Bytecode::new_legacy(code.into())),
        );
        evm.set_database(db);
        let trace = Rc::new(RefCell::new(Trace::default()));
        let result = evm
            .system_call_with_inspector(system_tx(), &mut Observer(trace.clone()))
            .unwrap()
            .commit();
        assert!(result.is_success());
        assert_eq!(trace.borrow().selfdestructs, [(ROOT, CHILD, U256::from(10))]);
        assert_eq!(evm.state_mut().account(&ROOT).unwrap().balance(), U256::ZERO);
        assert_eq!(evm.state_mut().account(&CHILD).unwrap().balance(), U256::from(10));
    }
}

fn preserve(evm: &mut MonadEvm<'_>) {
    evm.ext_mut().chain.parent_senders_and_authorities.extend([SYSTEM_ADDRESS, ROOT]);
    let chain = evm.ext().chain.clone();
    let sender = reserve_account(evm, SYSTEM_ADDRESS).unwrap();
    let spec = evm.config_spec_id();
    evm.ext_mut().reserve_balance.init(ReserveBalanceInit {
        chain: &chain,
        spec,
        sender: SYSTEM_ADDRESS,
        effective_gas_price: 0,
        gas_limit: 0,
        sender_is_delegated: false,
        sender_account: Some(&sender),
    });
    evm.ext_mut().preserve_reserve_balance_tracker = true;
}

#[test]
fn pending_state_resolution_keeps_the_enclosing_reserve_context() {
    for mode in 0..4 {
        let mut evm = evm(MonadHardfork::MonadTen, op::RETURN);
        preserve(&mut evm);
        let before = evm.ext().clone();
        let pending =
            evm.system_call_with_inspector(system_tx(), &mut Observer(Rc::default())).unwrap();
        assert!(pending.result().is_success());
        assert!(!pending.result().ext.reverted_for_reserve_balance);
        assert!(pending.preserved_context().is_none());
        match mode {
            0 => drop(pending),
            1 => {
                assert!(pending.discard().is_success());
            }
            2 => {
                let detached = pending.detach();
                assert_eq!(slot(&mut evm, ROOT), U256::ZERO);
                evm.commit_source(&detached.pending_state);
            }
            _ => {
                assert!(pending.commit_to(&mut BlockStateAccumulator::default()).is_success());
            }
        }
        assert_eq!(evm.ext().reserve_balance, before.reserve_balance);
        assert_eq!(evm.ext().chain, before.chain);
        assert!(evm.ext().preserve_reserve_balance_tracker);
        assert_eq!(evm.ext().memory_pool_offset(), before.memory_pool_offset());
        assert_eq!(slot(&mut evm, ROOT), U256::from(u8::from(mode >= 2)));
        assert_eq!(slot(&mut evm, CHILD), U256::from(if mode >= 2 { 2 } else { 0 }));
    }
}

struct InterruptChild {
    panic: bool,
}

impl Inspector<MonadEvmTypes> for InterruptChild {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if interp.message().depth == 1 && interp.opcode() == op::STOP {
            assert_eq!(
                interp.host().state_mut().storage_slot_untracked(&CHILD, &U256::ZERO).unwrap(),
                U256::from(2)
            );
            assert!(!self.panic, "injected system inspector panic");
            interp.fail(ExecutionError::Fatal("injected system inspector failure".into()));
        }
    }

    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        _: &mut MessageResult<MonadEvmTypes>,
    ) {
        panic!("fatal errors must not become call outcomes");
    }
}

#[test]
fn errors_and_unwinding_restore_state_context_and_the_installed_inspector() {
    for panic in [false, true] {
        let mut evm = evm(MonadHardfork::MonadTen, op::RETURN);
        preserve(&mut evm);
        let before = evm.ext().clone();
        let installed = Rc::new(RefCell::new(Trace::default()));
        evm.set_inspector(Observer(installed.clone()));
        if panic {
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    let _ = evm
                        .system_call_with_inspector(system_tx(), &mut InterruptChild { panic })
                        .unwrap()
                        .commit();
                }))
                .is_err()
            );
        } else {
            let error = evm
                .system_call_with_inspector(system_tx(), &mut InterruptChild { panic })
                .unwrap_err();
            assert!(error.to_string().contains("injected system inspector failure"));
        }
        assert_eq!(slot(&mut evm, ROOT), U256::ZERO);
        assert_eq!(slot(&mut evm, CHILD), U256::ZERO);
        assert_eq!(evm.state_mut().account(&ROOT).unwrap().balance(), U256::from(10));
        assert_eq!(evm.state_mut().account(&ROOT).unwrap().nonce(), 0);
        assert_eq!(evm.state_mut().account(&CHILD).unwrap().balance(), U256::ZERO);
        assert!(evm.state_mut().account_info_untracked(&ROOT.create(0)).unwrap().is_none());
        assert_eq!(evm.ext().reserve_balance, before.reserve_balance);
        assert_eq!(evm.ext().chain, before.chain);
        assert!(evm.ext().preserve_reserve_balance_tracker);
        assert_eq!(evm.ext().memory_pool_offset(), 0);
        assert!(installed.borrow().calls.is_empty());
        assert!(evm.system_call(system_tx()).unwrap().commit().is_success());
        assert_eq!(installed.borrow().calls.len(), 2);
        // The private system request must not leak into a later ordinary transaction.
        evm.ext_mut().preserve_reserve_balance_tracker = false;
        let ordinary = Recovered::new_unchecked(
            MonadTxEnvelope::Legacy(TxLegacy {
                to: ROOT.into(),
                gas_limit: GAS,
                ..Default::default()
            }),
            SYSTEM_ADDRESS,
        );
        assert!(evm.transact(&ordinary).is_err());
    }
}

struct OverrideRoot {
    ended: bool,
}

impl Inspector<MonadEvmTypes> for OverrideRoot {
    fn call(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        assert_eq!(message.depth, 0);
        let mut gas = GasTracker::new(message.gas_limit);
        gas.spend(17).unwrap();
        Some(MessageResult::<MonadEvmTypes> {
            gas,
            stop: InstrStop::Return,
            output: Bytes::from_static(b"override"),
            ..Default::default()
        })
    }

    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        assert_eq!(result.output, Bytes::from_static(b"override"));
        result.output = Bytes::from_static(b"after call_end");
        self.ended = true;
    }
}

#[test]
fn root_override_skips_execution_and_call_end_can_change_the_output() {
    let mut evm = evm(MonadHardfork::MonadTen, op::RETURN);
    let mut inspector = OverrideRoot { ended: false };
    let result = evm.system_call_with_inspector(system_tx(), &mut inspector).unwrap().commit();
    assert!(inspector.ended && result.is_success());
    assert_eq!(result.output, Bytes::from_static(b"after call_end"));
    assert_eq!(result.tx_gas_used(), 17);
    assert_eq!(slot(&mut evm, ROOT), U256::ZERO);
}

#[test]
fn a_replacement_registry_cannot_silently_execute_system_calls_as_transactions() {
    let mut evm = evm(MonadHardfork::MonadTen, op::RETURN);
    let original = evm.registry().clone();
    let mut replacement = original.clone();
    replacement.register(
        0,
        |tx| tx.as_legacy(),
        handler(
            |_: &mut TxRequest<'_, '_, MonadEvmTypes, TxLegacy>| Ok(()),
            |request: TxRequest<'_, '_, MonadEvmTypes, TxLegacy>, ()| {
                request.host.state_mut().account(&ROOT)?.set_balance(U256::ZERO);
                Ok(TxResult::<MonadEvmTypes>::default())
            },
        ),
    );
    let spec = evm.config_spec_id();
    let config = ExecutionConfig::for_spec_and_version(spec, *evm.version());
    evm.set_execution_config(config, spec, replacement, monad_evm2::monad_precompiles(spec));
    let error = evm.system_call(system_tx()).unwrap_err();
    assert!(error.to_string().contains("system calls require the Monad transaction handler"));
    assert_eq!(evm.state_mut().account(&ROOT).unwrap().balance(), U256::from(10));
    let config = ExecutionConfig::for_spec_and_version(spec, *evm.version());
    evm.set_execution_config(config, spec, original, monad_evm2::monad_precompiles(spec));
    assert!(evm.system_call(system_tx()).unwrap().commit().is_success());
}
