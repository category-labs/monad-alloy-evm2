//! Reserve tracking across transaction boundaries, native rollbacks and query calls.

use alloy_consensus::{TxEip7702, TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use evm2::{
    AccountInfo, ExecutionConfig, ExecutionError, Inspector,
    bytecode::Bytecode,
    ethereum::LazyTxEip7702,
    evm::InMemoryDB,
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageKind, MessageResult, op},
    precompiles::{PrecompileError, PrecompileHalt},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHaltReason, MonadHardfork,
    MonadTxEnvelope, ReserveAccount, ReserveBalanceInit,
    reserve_balance::{
        DIPPED_INTO_RESERVE_SELECTOR, MonadCheckpoint, MonadStateSnapshot, RESERVE_BALANCE_ADDRESS,
        dipped_into_reserve, rebase_reserve_balance, reserve_account,
        run_reserve_balance_precompile,
    },
};
use revm::context_interface::transaction::{
    Authorization, RecoveredAuthority, RecoveredAuthorization,
};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const BENEFICIARY: Address = Address::repeat_byte(0x33);
const INITIAL: u64 = 1_000_000;

fn evm(spec: MonadHardfork, restricted: bool) -> MonadEvm<'static> {
    evm_with_code(spec, restricted, Bytes::new())
}

fn evm_with_code(spec: MonadHardfork, restricted: bool, code: Bytes) -> MonadEvm<'static> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(INITIAL)));
    db.insert_account_info(&TARGET, AccountInfo::default().with_code(Bytecode::new_legacy(code)));
    let mut evm = MonadEvmFactory::new(1).create_evm(
        spec,
        MonadBlockEnv { beneficiary: BENEFICIARY, ..Default::default() },
        db,
    );
    if restricted {
        evm.ext_mut().chain.parent_senders_and_authorities.insert(SENDER);
    }
    evm
}

fn tx(to: Address, value: u64, nonce: u64) -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: to.into(),
            value: U256::from(value),
            nonce,
            gas_limit: 100_000,
            gas_price: 1,
            ..Default::default()
        }),
        SENDER,
    )
}

fn balance(evm: &mut MonadEvm<'_>, address: Address) -> U256 {
    evm.state_mut().account(&address).unwrap().balance()
}

#[test]
fn automatic_enforcement_reverts_value_but_keeps_fees_and_nonce() {
    for spec in MonadHardfork::ALL {
        for restricted in [false, true] {
            let mut evm = evm(spec, restricted);
            let result = evm.transact(&tx(TARGET, 1, 0)).unwrap().commit();
            assert_eq!(result.is_success(), !restricted, "{spec}");
            assert_eq!(result.ext.reverted_for_reserve_balance, restricted);
            assert!(!result.ext.dipped_into_reserve);
            assert_eq!(result.tx_gas_used(), 100_000);
            assert_eq!(balance(&mut evm, BENEFICIARY), U256::from(100_000));
            let value = u64::from(!restricted);
            assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL - 100_000 - value));
            assert_eq!(balance(&mut evm, TARGET), U256::from(value));
            assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 1);
            assert!(!evm.ext().reserve_balance.tracking_enabled());
            let next = evm.transact(&tx(TARGET, 0, 1)).unwrap().commit();
            assert!(next.is_success());
            assert!(!next.ext.dipped_into_reserve);
        }
    }
}

#[test]
fn query_observes_value_transfer_before_transaction_settlement() {
    // Store the selector, call 0x1001, and return its boolean output.
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
        // Repay the temporary dip before the outer transaction ends.
        op::PUSH0,
        op::PUSH0,
        op::PUSH0,
        op::PUSH0,
        op::CALLVALUE,
        op::CALLER,
        op::GAS,
        op::CALL,
        op::POP,
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ]);
    for restricted in [false, true] {
        let mut evm =
            evm_with_code(MonadHardfork::MonadNine, restricted, Bytes::from(code.clone()));
        let result = evm.transact(&tx(TARGET, 1, 0)).unwrap().commit();
        assert!(result.is_success());
        assert_eq!(U256::from_be_slice(&result.output), U256::from(u8::from(restricted)));
    }
}

#[test]
fn rollback_and_identical_replacement_journal_refresh_tracked_accounts() {
    let mut evm = evm(MonadHardfork::MonadNine, true);
    let view = ReserveAccount {
        balance: U256::from(INITIAL),
        original_balance: U256::from(INITIAL),
        ..Default::default()
    };
    let chain = evm.ext().chain.clone();
    evm.ext_mut().reserve_balance.init(ReserveBalanceInit {
        chain: &chain,
        spec: MonadHardfork::MonadNine,
        sender: SENDER,
        effective_gas_price: 0,
        gas_limit: 0,
        sender_is_delegated: false,
        sender_account: Some(&view),
    });
    evm.state_mut().account(&SENDER).unwrap();
    let checkpoint = evm.state().checkpoint();
    let features = evm.version().features;
    for _ in 0..2 {
        evm.state_mut().account(&SENDER).unwrap().set_balance(U256::from(INITIAL - 1));
        assert!(dipped_into_reserve(&mut evm).unwrap());
        evm.state_mut().rollback(checkpoint.clone(), features);
        assert!(!dipped_into_reserve(&mut evm).unwrap());
    }
}

#[test]
fn reserve_query_gas_input_and_call_context_validation() {
    let mut evm = evm(MonadHardfork::MonadNine, false);
    let message = Message::<MonadEvmTypes> {
        destination: RESERVE_BALANCE_ADDRESS,
        code_address: RESERVE_BALANCE_ADDRESS,
        call_target: RESERVE_BALANCE_ADDRESS,
        input: Bytes::copy_from_slice(&DIPPED_INTO_RESERVE_SELECTOR),
        gas_limit: 100,
        ..Default::default()
    };
    let mut gas = GasTracker::new(100);
    let result = run_reserve_balance_precompile(&mut evm, &message, &mut gas).unwrap();
    assert_eq!(result.bytes(), &[0; 32]);
    assert_eq!(gas.spent(), 100);
    let mut invalid = message.clone();
    invalid.gas_limit = 99;
    assert!(matches!(
        run_reserve_balance_precompile(&mut evm, &invalid, &mut GasTracker::new(99)),
        Err(PrecompileError::Halt(PrecompileHalt::OutOfGas))
    ));
    for kind in [MessageKind::StaticCall, MessageKind::DelegateCall, MessageKind::CallCode] {
        let invalid = Message::<MonadEvmTypes> { kind, ..message.clone() };
        let mut gas = GasTracker::new(100);
        assert!(matches!(
            run_reserve_balance_precompile(&mut evm, &invalid, &mut gas),
            Err(PrecompileError::Revert(_))
        ));
        assert_eq!(gas.remaining(), 0);
    }
    for (input, value, expected) in [
        (Bytes::new(), U256::ZERO, "method not supported"),
        (message.input.clone(), U256::ONE, "value is nonzero"),
        (
            Bytes::from([DIPPED_INTO_RESERVE_SELECTOR.as_slice(), &[0]].concat()),
            U256::ZERO,
            "input is invalid",
        ),
    ] {
        let invalid = Message::<MonadEvmTypes> { input, value, ..message.clone() };
        let mut gas = GasTracker::new(100);
        let Err(PrecompileError::Revert(bytes)) =
            run_reserve_balance_precompile(&mut evm, &invalid, &mut gas)
        else {
            panic!("expected revert")
        };
        assert_eq!(bytes.as_ref(), expected.as_bytes());
        assert_eq!(gas.remaining(), 0);
    }
}

#[test]
fn invalid_transactions_clear_tracker_state() {
    let mut evm = evm(MonadHardfork::MonadNine, true);
    let result = evm.transact(&tx(TARGET, 1, 0)).unwrap().commit();
    assert_eq!(result.stop, InstrStop::Revert);
    assert!(evm.transact(&tx(TARGET, 0, 0)).is_err());
    assert!(!evm.ext().reserve_balance.tracking_enabled());
    assert!(!evm.ext().reserve_balance.has_violation());
}

#[derive(Default)]
struct RootObserver {
    seen: usize,
    create: bool,
    gas_remaining: u64,
}

impl RootObserver {
    fn observe(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &MessageResult<MonadEvmTypes>,
    ) {
        if message.depth != 0 {
            return;
        }
        self.seen += 1;
        assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
        assert_eq!(result.stop, InstrStop::Revert);
        assert!(result.output.is_empty());
        assert!(result.created_address.is_none());
        assert_eq!(
            interp.host().state_mut().account(&SENDER).unwrap().balance(),
            U256::from(INITIAL - 100_000)
        );
        assert_eq!(
            interp.host().state_mut().account(&message.destination).unwrap().balance(),
            U256::ZERO
        );
        assert_eq!(
            interp.host().state_mut().storage_slot_untracked(&TARGET, &U256::ZERO).unwrap(),
            U256::ZERO
        );
        assert!(interp.host().logs().is_empty());
        self.gas_remaining = result.gas.remaining();
    }
}

impl Inspector<MonadEvmTypes> for RootObserver {
    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.observe(interp, message, result);
    }
    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.create = true;
        self.observe(interp, message, result);
    }
}

#[test]
fn root_callbacks_observe_reserve_rejection_before_native_rollback_loses_the_violation() {
    for spec in MonadHardfork::ALL {
        for ending in [vec![op::STOP], vec![op::PUSH0, op::PUSH0, op::REVERT], vec![op::INVALID]] {
            // Write storage and emit a log before the root finishes or fails.
            let mut code =
                vec![op::PUSH1, 1, op::PUSH0, op::SSTORE, op::PUSH0, op::PUSH0, op::LOG0];
            code.extend(&ending);
            let mut evm = evm_with_code(spec, true, code.into());
            let mut observer = RootObserver::default();
            let result =
                evm.transact_with_inspector(&tx(TARGET, 1, 0), &mut observer).unwrap().commit();
            assert_eq!(observer.seen, 1);
            assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
            assert_eq!(result.tx_gas_used(), 100_000);
            assert!(result.logs.is_empty());
            assert_eq!(observer.gas_remaining == 0, ending == [op::INVALID]);
        }
    }
}

#[test]
fn create_rejection_consumes_execution_gas_and_keeps_sender_nonce() {
    for code in [
        Bytes::new(),
        Bytes::from_static(&[op::STOP]),
        Bytes::from_static(&[op::PUSH0, op::PUSH0, op::REVERT]),
        Bytes::from_static(&[op::INVALID]),
    ] {
        let mut evm = evm(MonadHardfork::MonadTen, true);
        let tx = Recovered::new_unchecked(
            MonadTxEnvelope::Legacy(TxLegacy {
                to: TxKind::Create,
                input: code,
                value: U256::ONE,
                gas_price: 1,
                gas_limit: 100_000,
                ..Default::default()
            }),
            SENDER,
        );
        let mut observer = RootObserver::default();
        let result = evm.transact_with_inspector(&tx, &mut observer).unwrap().commit();
        assert_eq!(observer.seen, 1);
        assert!(observer.create);
        assert_eq!(observer.gas_remaining, 0);
        assert_eq!(result.total_gas_spent, 100_000);
        assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
        assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 1);
        assert!(
            evm.state_mut().account(&SENDER.create(0)).unwrap().load_code().unwrap().is_empty()
        );
    }
}

#[test]
fn precompile_failures_are_adjudicated_before_the_transfer_is_rolled_back() {
    for target in [Address::with_last_byte(4), Address::with_last_byte(9), RESERVE_BALANCE_ADDRESS]
    {
        let mut evm = evm(MonadHardfork::MonadNine, true);
        let mut observer = RootObserver::default();
        let result =
            evm.transact_with_inspector(&tx(target, 1, 0), &mut observer).unwrap().commit();
        assert_eq!(observer.seen, 1);
        assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    }
}

#[test]
fn reserve_rejection_retains_eip7702_authorizations() {
    let authority = Address::repeat_byte(0x44);
    let mut evm = evm(MonadHardfork::MonadTen, true);
    let authorization = RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::ZERO, address: TARGET, nonce: 0 },
        RecoveredAuthority::Valid(authority),
    );
    let envelope = LazyTxEip7702::from_cached_recovered_authorizations(
        TxEip7702 {
            chain_id: 1,
            gas_limit: 100_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: authority,
            value: U256::ONE,
            ..Default::default()
        },
        vec![authorization],
    );
    let result = evm.transact(&Recovered::new_unchecked(envelope.into(), SENDER)).unwrap().commit();
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    let mut account = evm.state_mut().account(&authority).unwrap();
    assert_eq!(account.nonce(), 1);
    assert_eq!(account.load_code().unwrap().eip7702_address(), Some(TARGET));
    assert_eq!(account.balance(), U256::ZERO);
}

struct FatalObserver;
impl Inspector<MonadEvmTypes> for FatalObserver {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        interp.fail(ExecutionError::Fatal("reserve test host failure".into()));
    }
    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        _: &mut MessageResult<MonadEvmTypes>,
    ) {
        panic!("fatal host errors must not become reserve outcomes");
    }
}

#[test]
fn fatal_host_errors_take_precedence_over_reserve_rejection() {
    let mut evm = evm_with_code(MonadHardfork::MonadTen, true, Bytes::from_static(&[op::STOP]));
    let error = evm.transact_with_inspector(&tx(TARGET, 1, 0), &mut FatalObserver).unwrap_err();
    assert!(error.to_string().contains("reserve test host failure"));
    assert!(!evm.ext().reserve_balance.tracking_enabled());
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL));
    assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 0);
}

fn preserve(evm: &mut MonadEvm<'_>) {
    let account = reserve_account(evm, SENDER).unwrap();
    let chain = evm.ext().chain.clone();
    let spec = evm.config_spec_id();
    evm.ext_mut().reserve_balance.init(ReserveBalanceInit {
        chain: &chain,
        spec,
        sender: SENDER,
        effective_gas_price: 0,
        gas_limit: 0,
        sender_is_delegated: false,
        sender_account: Some(&account),
    });
    evm.ext_mut().preserve_reserve_balance_tracker = true;
}

fn synthetic(sender: Address, to: Address, value: u64, nonce: u64) -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: to.into(),
            value: U256::from(value),
            nonce,
            gas_limit: 100_000,
            gas_price: 0,
            ..Default::default()
        }),
        sender,
    )
}

#[test]
fn preserved_calls_defer_enforcement_and_keep_enclosing_sender_and_balance_baselines() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    preserve(&mut evm);
    let result = evm.transact(&synthetic(SENDER, TARGET, 1, 0)).unwrap().commit();
    assert!(result.is_success());
    assert!(result.ext.dipped_into_reserve);
    assert!(result.ext.halt_reason.is_none());
    assert!(dipped_into_reserve(&mut evm).unwrap());
    // TARGET received its balance during the enclosing transaction: it has no reserve.
    // SENDER is still the enclosing sender even though TARGET sends this synthetic call.
    let result = evm.transact(&synthetic(TARGET, SENDER, 1, 0)).unwrap().commit();
    assert!(result.is_success());
    assert!(!result.ext.dipped_into_reserve);
    assert!(!dipped_into_reserve(&mut evm).unwrap());
    // Ordinary execution always starts a new context and enforces again.
    evm.ext_mut().preserve_reserve_balance_tracker = false;
    let result = evm.transact(&tx(TARGET, 1, 1)).unwrap().commit();
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
}

#[test]
fn synthetic_discard_drop_detach_validation_and_fatal_errors_restore_context() {
    let mut evm = evm_with_code(MonadHardfork::MonadTen, true, Bytes::from_static(&[op::STOP]));
    preserve(&mut evm);
    let initial = evm.ext().reserve_balance.clone();
    let tx = synthetic(SENDER, TARGET, 1, 0);
    assert!(evm.call_tx(&tx).unwrap().ext.dipped_into_reserve);
    assert_eq!(evm.ext().reserve_balance, initial);
    drop(evm.transact(&tx).unwrap());
    assert_eq!(evm.ext().reserve_balance, initial);
    let detached = evm.transact(&tx).unwrap().detach();
    assert!(detached.result.ext.dipped_into_reserve);
    assert_eq!(evm.ext().reserve_balance, initial);
    evm.validate_tx(&tx).unwrap();
    assert_eq!(evm.ext().reserve_balance, initial);
    assert!(evm.transact(&synthetic(SENDER, TARGET, 0, 2)).is_err());
    assert_eq!(evm.ext().reserve_balance, initial);
    assert!(evm.transact_with_inspector(&tx, &mut FatalObserver).is_err());
    assert_eq!(evm.ext().reserve_balance, initial);
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL));
}

#[test]
fn preserved_failed_frames_do_not_finalize_the_enclosing_transaction() {
    let mut evm = evm_with_code(
        MonadHardfork::MonadTen,
        true,
        Bytes::from_static(&[op::PUSH0, op::PUSH0, op::REVERT]),
    );
    preserve(&mut evm);
    let result = evm.transact(&synthetic(SENDER, TARGET, 1, 0)).unwrap().commit();
    assert!(result.is_revert());
    assert!(result.ext.halt_reason.is_none());
    assert!(!dipped_into_reserve(&mut evm).unwrap());
    assert!(evm.ext().reserve_balance.tracking_enabled());
}

#[test]
fn deferral_requires_an_initialized_enclosing_context() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    evm.ext_mut().preserve_reserve_balance_tracker = true;
    let error = evm.transact(&tx(TARGET, 1, 0)).unwrap_err();
    assert!(error.to_string().contains("initialized reserve tracker"));
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL));
}

#[test]
fn paired_checkpoints_restore_balances_tracking_chain_and_transient_state() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    preserve(&mut evm);
    let checkpoint = MonadCheckpoint::capture(&mut evm).unwrap();
    let initial = evm.ext().reserve_balance.clone();
    evm.state_mut().account(&SENDER).unwrap().set_balance(U256::ONE);
    evm.state_mut().tstore(&Address::with_last_byte(1), &U256::ONE, &U256::ONE);
    assert!(dipped_into_reserve(&mut evm).unwrap());
    evm.ext_mut().chain.max_reserve_balance = U256::ONE;
    checkpoint.revert(&mut evm);
    assert_eq!(evm.ext().reserve_balance, initial);
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL));
    assert_eq!(evm.state_mut().tload(&Address::with_last_byte(1), &U256::ONE), U256::ZERO);
    assert_ne!(evm.ext().chain.max_reserve_balance, U256::ONE);
}

#[test]
fn fork_rebase_and_snapshot_restore_use_matching_account_baselines() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    preserve(&mut evm);
    evm.state_mut().account(&SENDER).unwrap().set_balance(U256::from(INITIAL - 1));
    let snapshot = MonadStateSnapshot::capture(&mut evm).unwrap();
    assert!(evm.ext().reserve_balance.has_violation());
    let mut replacement = InMemoryDB::default();
    replacement.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(100)));
    evm.set_database(replacement);
    rebase_reserve_balance(&mut evm).unwrap();
    assert!(!dipped_into_reserve(&mut evm).unwrap());
    evm.state_mut().account(&SENDER).unwrap().set_balance(U256::from(99));
    assert!(dipped_into_reserve(&mut evm).unwrap());
    evm.ext_mut().chain.parent_senders_and_authorities.clear();
    rebase_reserve_balance(&mut evm).unwrap();
    assert!(!dipped_into_reserve(&mut evm).unwrap());
    snapshot.restore(&mut evm, InMemoryDB::default());
    assert!(dipped_into_reserve(&mut evm).unwrap());
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL - 1));
}

#[test]
fn early_out_of_funds_keeps_its_original_reason() {
    for kind in [TxKind::Call(TARGET), TxKind::Create] {
        let mut evm = evm(MonadHardfork::MonadTen, true);
        // The fee allowance underflows this reserve, but frame initialization fails first.
        evm.ext_mut().chain.max_reserve_balance = U256::ONE;
        let transaction = Recovered::new_unchecked(
            MonadTxEnvelope::Legacy(TxLegacy {
                to: kind,
                value: U256::from(INITIAL),
                gas_price: 1,
                gas_limit: 100_000,
                ..Default::default()
            }),
            SENDER,
        );
        let result = evm.transact(&transaction).unwrap().commit();
        assert_eq!(result.stop, InstrStop::OutOfFunds);
        assert!(result.ext.halt_reason.is_none());
        assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 1);
    }
}

struct OverrideRoot(RootObserver);

impl Inspector<MonadEvmTypes> for OverrideRoot {
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        assert!(interp.host().state_mut().transfer(&SENDER, &TARGET, &U256::ONE).unwrap());
        Some(MessageResult::<MonadEvmTypes> {
            // An inspector supplied this outcome after making changes, so it is not an
            // early native failure that can bypass the reserve check.
            stop: InstrStop::OutOfFunds,
            gas: GasTracker::new(message.gas_limit),
            ..Default::default()
        })
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.0.observe(interp, message, result);
        result.stop = InstrStop::Return;
    }
}

#[test]
fn inspector_overrides_are_checked_and_cannot_report_a_rejected_transaction_as_successful() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    let mut inspector = OverrideRoot(RootObserver::default());
    let result = evm.transact_with_inspector(&tx(TARGET, 0, 0), &mut inspector).unwrap().commit();
    assert_eq!(inspector.0.seen, 1);
    assert!(!result.is_success());
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
}

#[test]
fn installed_inspectors_and_nonzero_initial_depth_keep_automatic_enforcement() {
    let mut evm = evm_with_code(
        MonadHardfork::MonadTen,
        true,
        Bytes::from_static(&[op::PUSH0, op::PUSH0, op::REVERT]),
    );
    evm.set_inspector(RootObserver::default());
    let first = evm.transact(&tx(TARGET, 1, 0)).unwrap().commit();
    assert_eq!(first.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    assert!(evm.clear_inspector().is_some());
    let spec = evm.config_spec_id();
    let mut version = *evm.version();
    version.initial_call_depth = 7;
    evm.set_execution_config(
        ExecutionConfig::for_spec_and_version(spec, version),
        spec,
        evm.registry().clone(),
        monad_evm2::monad_precompiles(spec),
    );
    let result = evm.transact(&tx(TARGET, 1, 1)).unwrap().commit();
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    let result = evm.transact(&tx(Address::with_last_byte(9), 1, 2)).unwrap().commit();
    assert_eq!(result.ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
}

#[test]
fn detached_synthetic_state_can_be_accepted_with_its_context() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    preserve(&mut evm);
    let pending = evm.transact(&synthetic(SENDER, TARGET, 1, 0)).unwrap();
    let context = pending.preserved_context().unwrap().clone();
    let detached = pending.detach();
    assert!(!evm.ext().reserve_balance.has_violation());
    evm.commit_source(&detached.pending_state);
    *evm.ext_mut() = context;
    assert!(dipped_into_reserve(&mut evm).unwrap());
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL - 1));
}

#[test]
fn rebase_drops_accounts_missing_from_the_replacement_fork() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    preserve(&mut evm);
    evm.set_database(InMemoryDB::default());
    rebase_reserve_balance(&mut evm).unwrap();
    assert_eq!(evm.ext().reserve_balance.tracked_addresses().count(), 0);
    assert!(!dipped_into_reserve(&mut evm).unwrap());
}

#[test]
fn batch_transactions_enforce_reserves_and_native_entrypoints_cannot_bypass_the_wrapper() {
    let mut evm = evm(MonadHardfork::MonadTen, true);
    let error = monad_evm2::MonadHost::transact(&mut evm, &tx(TARGET, 1, 0)).unwrap_err();
    assert!(error.to_string().contains("through MonadEvm"));
    assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL));
    let transactions = [tx(TARGET, 1, 0), tx(TARGET, 0, 1)];
    let results = evm.transact_iter(&transactions).collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(results[0].ext.halt_reason, Some(MonadHaltReason::ReserveBalanceViolation));
    assert!(results[1].is_success());
}
