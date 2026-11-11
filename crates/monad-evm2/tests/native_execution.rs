//! Native-host integration, independent of the standalone wrapper.

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, U256, map::AddressMap};
use evm2::{
    AccountInfo, Evm, ExecutionConfig, ExecutionError, Inspector, InspectorConfig,
    bytecode::Bytecode,
    evm::{InMemoryDB, SystemTx},
    interpreter::{InstrStop, Interpreter, Message, MessageResult, op},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvmBuilder, MonadEvmExt, MonadEvmFactory, MonadEvmTypes, MonadExecution,
    MonadHaltReason, MonadHardfork, MonadHost, MonadTxEnvelope, ReserveBalanceInit, SYSTEM_ADDRESS,
    frame_hardfork, monad_tx_registry, monad_version,
    reserve_balance::{
        DIPPED_INTO_RESERVE_SELECTOR, RESERVE_BALANCE_ADDRESS, dipped_into_reserve,
        rebase_reserve_balance_with_originals, reserve_account, reserve_inspector,
        sync_reserve_balance,
    },
    staking::{STAKING_ADDRESS, storage::global_slots},
    syscall_snapshot_calldata,
};
use std::panic::{AssertUnwindSafe, catch_unwind};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const BENEFICIARY: Address = Address::repeat_byte(0x33);
const INITIAL: u64 = 1_000_000;
const GAS: u64 = 100_000;

fn database(code: &[u8]) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(INITIAL)));
    db.insert_account_info(
        &TARGET,
        AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::copy_from_slice(code))),
    );
    db
}

fn host(spec: MonadHardfork, code: &[u8]) -> (Evm<'static, MonadEvmTypes>, MonadExecution) {
    // Construct the same native type required by Foundry's factory, using public parts.
    let execution = MonadExecution::new(spec);
    let mut host = Evm::new_with_execution_config_and_ext(
        ExecutionConfig::for_spec_and_version(spec, monad_version(spec, 1)),
        spec,
        MonadBlockEnv { beneficiary: BENEFICIARY, ..Default::default() },
        monad_tx_registry(spec),
        database(code),
        execution.precompiles(),
        MonadEvmExt::default(),
    );
    host.ext_mut().revert_on_reserve_balance_violation = true;
    host.ext_mut().chain.parent_senders_and_authorities.insert(SENDER);
    (host, execution)
}

fn native_host(spec: MonadHardfork, code: &[u8]) -> MonadHost<'static> {
    let factory = MonadEvmFactory;
    let mut host = factory.create_evm(
        spec,
        factory.version(spec, 1),
        MonadBlockEnv { beneficiary: BENEFICIARY, ..Default::default() },
        database(code),
    );
    host.ext_mut().chain.parent_senders_and_authorities.insert(SENDER);
    host
}

fn tx(value: u64, gas_price: u128) -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: TARGET.into(),
            value: U256::from(value),
            gas_limit: GAS,
            gas_price,
            ..Default::default()
        }),
        SENDER,
    )
}

fn preserve(host: &mut MonadHost<'_>) {
    let account = reserve_account(host, SENDER).unwrap();
    let chain = host.ext().chain.clone();
    let spec = host.config_spec_id();
    host.ext_mut().reserve_balance.init(ReserveBalanceInit {
        chain: &chain,
        spec,
        sender: SENDER,
        effective_gas_price: 0,
        gas_limit: 0,
        sender_is_delegated: false,
        sender_account: Some(&account),
    });
    host.ext_mut().preserve_reserve_balance_tracker = true;
}

#[derive(Default)]
struct Trace {
    select: Option<MonadHardfork>,
    steps: usize,
    step_ends: usize,
    calls: usize,
    halt: Option<MonadHaltReason>,
    frame: Option<MonadHardfork>,
    reserve_precompile: bool,
}

impl Inspector<MonadEvmTypes> for Trace {
    fn inspector_config(&self) -> InspectorConfig {
        InspectorConfig::new().with_steps([op::SSTORE].into_iter().collect())
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        assert_eq!(interp.opcode(), op::SSTORE);
        self.steps += 1;
    }

    fn step_end(&mut self, _: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.step_ends += 1;
    }

    fn call(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.calls += 1;
        message.ext.hardfork = self.select;
        None
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.halt = result.ext.halt_reason;
        self.frame = Some(frame_hardfork(interp.host()));
        self.reserve_precompile = interp.host().precompiles().contains(&RESERVE_BALANCE_ADDRESS);
    }
}

#[test]
fn native_opt_in_enforces_reserves_without_a_frame_selector() {
    for spec in MonadHardfork::ALL {
        for terminal in [op::STOP, op::REVERT, op::INVALID] {
            for value in [0, 1] {
                let code = [op::PUSH1, 1, op::PUSH0, op::SSTORE, op::PUSH0, op::PUSH0, terminal];
                let mut host = native_host(spec, &code);
                host.ext_mut().revert_on_reserve_balance_violation = true;
                let mut trace = Trace::default();
                let result = host
                    .transact_with_inspector(
                        &tx(value, 1),
                        &mut reserve_inspector(Some(&mut trace)),
                    )
                    .unwrap()
                    .commit();
                let halt = (value == 1).then_some(MonadHaltReason::ReserveBalanceViolation);
                assert_eq!(result.ext.halt_reason, halt);
                assert_eq!(trace.halt, halt);
                assert_eq!(trace.frame, Some(spec));
                assert_eq!((trace.steps, trace.step_ends, trace.calls), (1, 1, 1));
                assert_eq!(result.is_success(), value == 0 && terminal == op::STOP);
                assert_eq!(result.tx_gas_used(), GAS);
                assert_eq!(
                    host.state_mut().account(&SENDER).unwrap().balance(),
                    U256::from(INITIAL - GAS)
                );
                assert_eq!(host.state_mut().account(&SENDER).unwrap().nonce(), 1);
                assert_eq!(
                    host.state_mut().account(&BENEFICIARY).unwrap().balance(),
                    U256::from(GAS)
                );
                assert_eq!(host.state_mut().account(&TARGET).unwrap().balance(), U256::ZERO);
                assert_eq!(
                    host.state_mut().storage_slot_untracked(&TARGET, &U256::ZERO).unwrap(),
                    U256::from(u8::from(result.is_success())),
                );
            }
        }
    }
}

#[test]
fn native_factory_honors_memory_limits_with_default_features_disabled() {
    let factory = MonadEvmFactory;
    for spec in MonadHardfork::ALL {
        let mut version = factory.version(spec, 1);
        version.memory_limit = 128 * 1024 * 1024;
        let config = factory.execution_config(spec, version);
        assert_eq!(
            config.version().memory_limit,
            if spec >= MonadHardfork::MonadNine {
                monad_evm2::MONAD_MEMORY_LIMIT
            } else {
                version.memory_limit
            }
        );
        version.memory_limit = 64;
        let mut host = factory.create_evm(
            spec,
            version,
            MonadBlockEnv::default(),
            database(&[op::PUSH0, op::PUSH1, 64, op::MSTORE, op::STOP]),
        );
        let result =
            host.transact_with_inspector(&tx(0, 0), &mut reserve_inspector(None)).unwrap().commit();
        assert!(!result.is_success());
        assert_eq!(result.stop, InstrStop::MemoryLimitOOG);
    }
}

#[test]
fn native_observer_enforces_reserves_before_success_and_failure_rollback() {
    for spec in MonadHardfork::ALL {
        for terminal in [op::STOP, op::REVERT, op::INVALID] {
            let code = [op::PUSH1, 1, op::PUSH0, op::SSTORE, op::PUSH0, op::PUSH0, terminal];
            let (mut host, execution) = host(spec, &code);
            let mut trace = Trace::default();
            let result = {
                let mut observer = execution.inspector(spec, Some(&mut trace));
                host.transact_with_inspector(&tx(1, 1), &mut observer).unwrap().commit()
            };
            assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
            assert_eq!(trace.halt, result.ext.halt_reason);
            assert_eq!((trace.steps, trace.step_ends, trace.calls), (1, 1, 1));
            assert_eq!(result.tx_gas_used(), GAS);
            assert_eq!(
                host.state_mut().account(&SENDER).unwrap().balance(),
                U256::from(INITIAL - GAS)
            );
            assert_eq!(host.state_mut().account(&SENDER).unwrap().nonce(), 1);
            assert_eq!(host.state_mut().account(&BENEFICIARY).unwrap().balance(), U256::from(GAS));
            assert_eq!(host.state_mut().account(&TARGET).unwrap().balance(), U256::ZERO);
            assert_eq!(
                host.state_mut().storage_slot_untracked(&TARGET, &U256::ZERO).unwrap(),
                U256::ZERO
            );
            assert!(!host.ext().reserve_balance.tracking_enabled());
        }
    }
}

#[test]
fn native_opt_in_requires_an_observer_without_committing_fees() {
    let (mut host, _) = host(MonadHardfork::MonadTen, &[]);
    let error = host.transact(&tx(1, 1)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "reserve-balance enforcement requires the Monad execution inspector"
    );
    assert_eq!(host.state_mut().account(&SENDER).unwrap().balance(), U256::from(INITIAL));
    assert_eq!(host.state_mut().account(&SENDER).unwrap().nonce(), 0);
}

#[test]
fn native_precompile_observation_retains_reserve_rejection_on_failure() {
    let mut host = native_host(MonadHardfork::MonadTen, &[]);
    host.ext_mut().revert_on_reserve_balance_violation = true;
    let precompile = Address::with_last_byte(9);
    let tx = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: precompile.into(),
            value: U256::ONE,
            gas_limit: GAS,
            gas_price: 1,
            ..Default::default()
        }),
        SENDER,
    );
    let result = {
        let mut observer = reserve_inspector(None);
        host.transact_with_inspector(&tx, &mut observer).unwrap().commit()
    };
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    assert_eq!(host.state_mut().account(&precompile).unwrap().balance(), U256::ZERO);
    assert_eq!(host.state_mut().account(&SENDER).unwrap().balance(), U256::from(INITIAL - GAS));
}

#[test]
fn native_hooks_resolve_preserved_context_with_the_pending_state() {
    let (mut host, execution) = host(MonadHardfork::MonadTen, &[]);
    preserve(&mut host);
    let before = host.ext().reserve_balance.clone();
    let tx = tx(1, 0);

    execution.validate_tx(&mut host, &tx).unwrap();
    assert_eq!(host.ext().reserve_balance, before);
    let result = execution.transact(&mut host, &tx, None).unwrap().discard();
    assert!(result.is_success() && result.ext.dipped_into_reserve);
    assert_eq!(host.ext().reserve_balance, before);
    drop(execution.transact(&mut host, &tx, None).unwrap());
    assert_eq!(host.ext().reserve_balance, before);

    let pending = execution.transact(&mut host, &tx, None).unwrap();
    let accepted_context = pending.preserved_context().unwrap().clone();
    let detached = pending.detach();
    assert_eq!(host.ext().reserve_balance, before);
    assert_eq!(host.read_account_info(&SENDER).unwrap().unwrap().balance, U256::from(INITIAL));
    host.commit_source(&detached.pending_state);
    host.ext_mut().replace_while_executing(accepted_context);
    assert!(dipped_into_reserve(&mut host).unwrap());
    assert_eq!(host.state_mut().account(&SENDER).unwrap().balance(), U256::from(INITIAL - 1));
    assert!(execution.transact(&mut host, &tx, None).is_err());
    assert!(dipped_into_reserve(&mut host).unwrap());
}

struct Abort(bool);

impl Inspector<MonadEvmTypes> for Abort {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if self.0 {
            panic!("inspector aborted");
        }
        interp.fail(ExecutionError::Fatal("inspector failed".into()));
    }
}

#[test]
fn native_hooks_restore_preserved_context_on_fatal_errors_and_unwinding() {
    let (mut host, execution) = host(MonadHardfork::MonadTen, &[op::STOP]);
    preserve(&mut host);
    let before = host.ext().reserve_balance.clone();
    assert!(execution.transact(&mut host, &tx(1, 0), Some(&mut Abort(false))).is_err());
    assert_eq!(host.ext().reserve_balance, before);
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _ = execution.transact(&mut host, &tx(1, 0), Some(&mut Abort(true)));
        }))
        .is_err()
    );
    assert_eq!(host.ext().reserve_balance, before);
    assert_eq!(host.state_mut().account(&SENDER).unwrap().balance(), U256::from(INITIAL));
    assert!(execution.transact(&mut host, &tx(0, 0), None).unwrap().commit().is_success());
}

#[test]
fn native_frame_selection_changes_precompiles_and_resets_for_the_next_execution() {
    let (mut host, execution) = host(MonadHardfork::MonadEight, &[]);
    let query = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: RESERVE_BALANCE_ADDRESS.into(),
            input: Bytes::copy_from_slice(&DIPPED_INTO_RESERVE_SELECTOR),
            gas_limit: GAS,
            ..Default::default()
        }),
        SENDER,
    );
    let mut trace = Trace { select: Some(MonadHardfork::MonadNine), ..Default::default() };
    let result = execution.transact(&mut host, &query, Some(&mut trace)).unwrap().discard();
    assert!(result.is_success());
    assert_eq!(result.output.as_ref(), &[0; 32]);
    assert_eq!(trace.frame, Some(MonadHardfork::MonadNine));
    assert!(trace.reserve_precompile);
    assert_eq!(frame_hardfork(&host), MonadHardfork::MonadEight);
    assert!(!host.precompiles().contains(&RESERVE_BALANCE_ADDRESS));
    assert!(execution.transact(&mut host, &query, None).unwrap().discard().output.is_empty());
}

#[test]
fn nested_native_hosts_inherit_reserves_without_sharing_frame_selection() {
    struct Nested;
    impl Inspector<MonadEvmTypes> for Nested {
        fn call(
            &mut self,
            _: &mut Interpreter<'_, '_, MonadEvmTypes>,
            message: &mut Message<MonadEvmTypes>,
        ) -> Option<MessageResult<MonadEvmTypes>> {
            message.ext.hardfork = Some(MonadHardfork::MonadNine);
            None
        }

        fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
            let parent = interp.host();
            assert_eq!(frame_hardfork(parent), MonadHardfork::MonadNine);
            assert!(parent.precompiles().contains(&RESERVE_BALANCE_ADDRESS));
            let (mut child, execution) = host(MonadHardfork::MonadEight, &[]);
            *child.ext_mut() = parent.ext().clone();
            child.ext_mut().preserve_reserve_balance_tracker = true;
            let mut trace = Trace::default();
            let result =
                execution.transact(&mut child, &tx(1, 0), Some(&mut trace)).unwrap().discard();
            assert!(result.is_success() && result.ext.dipped_into_reserve);
            assert_eq!(trace.frame, Some(MonadHardfork::MonadEight));
            assert!(!trace.reserve_precompile);
            assert_eq!(frame_hardfork(parent), MonadHardfork::MonadNine);
            assert!(parent.precompiles().contains(&RESERVE_BALANCE_ADDRESS));
        }
    }
    let (mut host, execution) = host(MonadHardfork::MonadEight, &[op::STOP]);
    assert!(
        execution.transact(&mut host, &tx(0, 0), Some(&mut Nested)).unwrap().commit().is_success()
    );
    assert!(!host.precompiles().contains(&RESERVE_BALANCE_ADDRESS));
}

#[test]
fn native_system_calls_and_protocol_replay_keep_the_enclosing_tracker() {
    let (mut host, execution) = MonadEvmBuilder::new(1).create_host(
        MonadHardfork::MonadTen,
        MonadBlockEnv::default(),
        database(&[op::PUSH1, 7, op::PUSH0, op::SSTORE, op::STOP]),
    );
    preserve(&mut host);
    let before = host.ext().reserve_balance.clone();
    let mut trace = Trace::default();
    let result = execution
        .system_call(
            &mut host,
            SystemTx::new(TARGET, Bytes::new()).with_caller(SYSTEM_ADDRESS),
            Some(&mut trace),
        )
        .unwrap()
        .commit();
    assert!(result.is_success());
    assert_eq!((trace.calls, trace.steps, trace.step_ends), (1, 1, 1));
    assert_eq!(host.ext().reserve_balance, before);
    assert!(host.ext().preserve_reserve_balance_tracker);
    assert_eq!(
        host.state_mut().storage_slot_untracked(&TARGET, &U256::ZERO).unwrap(),
        U256::from(7)
    );

    host.ext_mut().protocol_replay = true;
    let protocol = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: STAKING_ADDRESS.into(),
            input: syscall_snapshot_calldata(),
            ..Default::default()
        }),
        SYSTEM_ADDRESS,
    );
    assert!(host.transact(&protocol).unwrap().discard().is_success());
    let result = execution.transact(&mut host, &protocol, Some(&mut trace)).unwrap().commit();
    assert!(result.is_success());
    assert_eq!(result.tx_gas_used(), 0);
    assert_eq!(trace.calls, 2);
    assert_eq!(host.state_mut().account(&SYSTEM_ADDRESS).unwrap().nonce(), 1);
    assert_ne!(
        host.state_mut()
            .storage_slot_untracked(&STAKING_ADDRESS, &global_slots::IN_BOUNDARY)
            .unwrap(),
        U256::ZERO
    );
    assert_eq!(host.ext().reserve_balance, before);
    assert!(host.ext().preserve_reserve_balance_tracker);
}

#[test]
fn fork_rebase_keeps_persistent_originals_including_accounts_debited_later() {
    let (mut host, _) = host(MonadHardfork::MonadTen, &[]);
    preserve(&mut host);
    // The replacement fork accepted these balances, but the enclosing transaction
    // began with a larger sender balance and no TARGET account.
    let mut replacement = InMemoryDB::default();
    replacement
        .insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(INITIAL - 1)));
    replacement.insert_account_info(&TARGET, AccountInfo::default().with_balance(U256::from(10)));
    sync_reserve_balance(&mut host).unwrap();
    host.set_database(replacement);
    let originals = AddressMap::from_iter([
        (SENDER, Some(AccountInfo::default().with_balance(U256::from(INITIAL)))),
        (TARGET, None),
    ]);
    rebase_reserve_balance_with_originals(&mut host, &originals).unwrap();
    assert!(dipped_into_reserve(&mut host).unwrap());
    assert_eq!(reserve_account(&mut host, SENDER).unwrap().original_balance, U256::from(INITIAL));
    assert_eq!(reserve_account(&mut host, TARGET).unwrap().original_balance, U256::ZERO);
    host.state_mut().account(&TARGET).unwrap().set_balance(U256::from(9));
    host.state_mut().account(&SENDER).unwrap().set_balance(U256::from(INITIAL));
    assert!(!dipped_into_reserve(&mut host).unwrap());
}

#[test]
fn context_replacement_during_execution_retains_reserve_adjudication_and_frame_selection() {
    struct Restore;
    impl Inspector<MonadEvmTypes> for Restore {
        fn call(
            &mut self,
            _: &mut Interpreter<'_, '_, MonadEvmTypes>,
            message: &mut Message<MonadEvmTypes>,
        ) -> Option<MessageResult<MonadEvmTypes>> {
            message.ext.hardfork = Some(MonadHardfork::MonadNine);
            None
        }

        fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
            let host = interp.host();
            assert_eq!(frame_hardfork(host), MonadHardfork::MonadNine);
            let mut context = MonadEvmExt::default();
            context.chain = host.ext().chain.clone();
            context.reserve_balance = host.ext().reserve_balance.clone();
            host.ext_mut().replace_while_executing(context);
            assert_eq!(frame_hardfork(host), MonadHardfork::MonadNine);
        }
    }
    let (mut host, execution) = host(MonadHardfork::MonadTen, &[op::INVALID]);
    let result = execution.transact(&mut host, &tx(1, 1), Some(&mut Restore)).unwrap().commit();
    assert_eq!(result.stop, InstrStop::Revert);
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    assert_eq!(host.state_mut().account(&TARGET).unwrap().balance(), U256::ZERO);
    assert_eq!(frame_hardfork(&host), MonadHardfork::MonadTen);
}

#[test]
fn native_defaults_report_reserves_with_or_without_an_inspector() {
    for spec in MonadHardfork::ALL {
        for terminal in [op::STOP, op::REVERT, op::INVALID] {
            for mode in 0..4 {
                let code = [op::PUSH1, 1, op::PUSH0, op::SSTORE, op::PUSH0, op::PUSH0, terminal];
                let mut host = native_host(spec, &code);
                assert!(!host.ext().revert_on_reserve_balance_violation);
                let mut trace = Trace::default();
                let tx = tx(1, 1);
                let pending = match mode {
                    0 => host.transact(&tx),
                    1 => host.transact_with_inspector(&tx, &mut trace),
                    2 => {
                        host.transact_with_inspector(&tx, &mut reserve_inspector(Some(&mut trace)))
                    }
                    _ => {
                        host.set_inspector(Trace::default());
                        host.transact(&tx)
                    }
                }
                .unwrap();
                let result = pending.commit();
                let success = terminal == op::STOP;
                assert_eq!(result.is_success(), success);
                assert_eq!(result.ext.dipped_into_reserve, success);
                assert!(!result.ext.reverted_for_reserve_balance);
                assert_eq!(result.ext.halt_reason, None);
                assert_eq!(result.tx_gas_used(), GAS);
                assert_eq!(
                    host.state_mut().account(&SENDER).unwrap().balance(),
                    U256::from(INITIAL - GAS - u64::from(success))
                );
                assert_eq!(host.state_mut().account(&SENDER).unwrap().nonce(), 1);
                assert_eq!(
                    host.state_mut().account(&BENEFICIARY).unwrap().balance(),
                    U256::from(GAS)
                );
                assert_eq!(
                    host.state_mut().account(&TARGET).unwrap().balance(),
                    U256::from(u8::from(success))
                );
                if mode == 1 || mode == 2 {
                    assert_eq!((trace.steps, trace.step_ends, trace.calls), (1, 1, 1));
                    assert_eq!(trace.halt, None);
                }
                assert!(!host.ext().reserve_balance.tracking_enabled());
            }
        }
    }
}

#[test]
fn native_preserved_calls_defer_rejection_without_a_reserve_observer() {
    for enforce in [false, true] {
        let mut host = native_host(MonadHardfork::MonadTen, &[]);
        host.ext_mut().revert_on_reserve_balance_violation = enforce;
        preserve(&mut host);
        let result = host.transact(&tx(1, 0)).unwrap().commit();
        assert!(result.is_success() && result.ext.dipped_into_reserve);
        assert!(!result.ext.reverted_for_reserve_balance);
        assert!(host.ext().reserve_balance.tracking_enabled());
        assert!(dipped_into_reserve(&mut host).unwrap());
    }
}

#[test]
fn native_report_only_queries_reserves_and_resolves_state_normally() {
    let mut code = vec![op::PUSH4];
    code.extend(DIPPED_INTO_RESERVE_SELECTOR);
    code.extend([
        op::PUSH0,
        op::MSTORE,
        op::PUSH1,
        32,
        op::PUSH0,
        op::PUSH1,
        4,
        op::PUSH1,
        28,
        op::PUSH0,
        op::PUSH2,
        0x10,
        1,
        op::PUSH2,
        0x03,
        0xe8,
        op::CALL,
        op::POP,
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ]);
    for spec in MonadHardfork::ALL.into_iter().skip(1) {
        let mut host = native_host(spec, &code);
        let tx = tx(1, 1);
        host.validate_tx(&tx).unwrap();
        assert!(!host.ext().reserve_balance.tracking_enabled());
        for mode in 0..4 {
            let pending = host.transact(&tx).unwrap();
            assert_eq!(U256::from_be_slice(&pending.result().output), U256::ONE);
            assert!(pending.result().ext.dipped_into_reserve);
            match mode {
                0 => drop(pending),
                1 => assert!(pending.discard().is_success()),
                2 => assert!(pending.detach().result.is_success()),
                _ => assert!(pending.commit().is_success()),
            }
            let committed = mode == 3;
            assert_eq!(
                host.state_mut().account(&SENDER).unwrap().balance(),
                U256::from(INITIAL - if committed { GAS + 1 } else { 0 })
            );
            assert_eq!(host.state_mut().account(&SENDER).unwrap().nonce(), u64::from(committed));
            assert!(!host.ext().reserve_balance.tracking_enabled());
        }
    }
}
