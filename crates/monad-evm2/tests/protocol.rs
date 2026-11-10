//! Canonical protocol envelope replay and transaction rollback.

use alloy_consensus::{
    TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy, Typed2718, transaction::Recovered,
};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use evm2::{
    AccountInfo, EvmFeatures, ExecutionConfig, Inspector, TxResult,
    ethereum::LazyTxEip7702,
    evm::{BlockStateAccumulator, InMemoryDB},
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageResult},
    registry::{HandlerError, TxRequest, handler},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmBuilder, MonadEvmFactory, MonadEvmTypes, MonadHardfork,
    MonadHost, MonadTxEnvelope, ProtocolSystemCallError as Error, ReserveBalanceInit,
    SYSTEM_ADDRESS, protocol_system_call,
    reserve_balance::reserve_account,
    staking::{
        STAKING_ADDRESS,
        constants::MON,
        interface::IMonadStaking::*,
        storage::{
            consensus_view_key, global_slots, val_id_secp_key, validator_key, validator_offsets,
        },
    },
    syscall_on_epoch_change_calldata, syscall_reward_calldata, syscall_snapshot_calldata,
};
use std::{cell::Cell, panic::AssertUnwindSafe, rc::Rc};

const CHAIN_ID: u64 = 143;
const AUTHOR: Address = Address::repeat_byte(0x55);
const USER: Address = Address::repeat_byte(0x11);

fn database() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&STAKING_ADDRESS, AccountInfo::default().with_balance(MON));
    db.insert_account_info(&USER, AccountInfo::default().with_balance(MON));
    for (key, value) in [
        (val_id_secp_key(&AUTHOR), U256::ONE << 192),
        (consensus_view_key(1, 0), U256::from(100) * MON),
        (validator_key(1, validator_offsets::STAKE), U256::from(100) * MON),
        (
            validator_key(1, validator_offsets::ADDRESS_FLAGS),
            U256::from_be_slice(AUTHOR.as_slice()) << 96,
        ),
    ] {
        db.insert_account_storage(&STAKING_ADDRESS, &key, &value);
    }
    db
}

fn evm(spec: MonadHardfork) -> MonadEvm<'static> {
    let mut evm =
        MonadEvmBuilder::new(CHAIN_ID).create_evm(spec, MonadBlockEnv::default(), database());
    evm.ext_mut().protocol_replay = true;
    evm
}

fn native_evm(spec: MonadHardfork) -> MonadHost<'static> {
    let factory = MonadEvmFactory;
    let mut host = factory.create_evm(
        spec,
        factory.version(spec, CHAIN_ID),
        MonadBlockEnv::default(),
        database(),
    );
    host.ext_mut().protocol_replay = true;
    host
}

fn legacy(nonce: u64, value: U256, input: Bytes) -> TxLegacy {
    TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        to: STAKING_ADDRESS.into(),
        value,
        input,
        ..Default::default()
    }
}

fn reward(nonce: u64) -> TxLegacy {
    legacy(nonce, MON, syscallRewardCall { blockAuthor: AUTHOR }.abi_encode().into())
}

const fn recovered(tx: TxLegacy) -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), SYSTEM_ADDRESS)
}

fn error(error: HandlerError) -> Error {
    error.external_ref::<Error>().unwrap_or_else(|| panic!("unexpected error: {error}")).clone()
}

fn balances(evm: &mut MonadHost<'_>) -> (u64, U256, U256) {
    let sender =
        evm.state_mut().account_info_untracked(&SYSTEM_ADDRESS).unwrap().unwrap_or_default();
    let staking =
        evm.state_mut().account_info_untracked(&STAKING_ADDRESS).unwrap().unwrap().balance;
    (sender.nonce, sender.balance, staking)
}

fn reward_slot(evm: &mut MonadHost<'_>) -> U256 {
    evm.state_mut()
        .storage_slot_untracked(
            &STAKING_ADDRESS,
            &validator_key(1, validator_offsets::UNCLAIMED_REWARDS),
        )
        .unwrap()
}

fn assert_zero_gas(result: &TxResult<MonadEvmTypes>) {
    assert!(result.is_success());
    assert_eq!(
        (result.total_gas_spent, result.state_gas_spent, result.refunded, result.floor_gas),
        (0, 0, 0, 0)
    );
    assert_eq!((result.tx_gas_used(), result.execution_gas_spent()), (0, 0));
    assert_eq!(result.ext, Default::default());
}

#[test]
fn classifier_decodes_canonical_calls_without_promoting_other_senders() {
    for tx in [
        reward(7),
        legacy(7, U256::ZERO, syscall_snapshot_calldata()),
        legacy(7, U256::ZERO, syscall_on_epoch_change_calldata(u64::MAX)),
    ] {
        assert!(
            protocol_system_call(&Recovered::new_unchecked(
                MonadTxEnvelope::Legacy(tx.clone()),
                USER
            ))
            .unwrap()
            .is_none()
        );
        let call = protocol_system_call(&recovered(tx.clone())).unwrap().unwrap();
        assert_eq!(
            (call.caller, call.contract, call.nonce, call.chain_id),
            (SYSTEM_ADDRESS, STAKING_ADDRESS, 7, Some(CHAIN_ID))
        );
        if tx.value.is_zero() {
            assert_eq!(call.data, tx.input);
            assert_eq!(call.balance_increment, None);
        } else {
            assert_eq!(call.data, syscall_reward_calldata(AUTHOR, MON));
            assert_eq!(call.balance_increment, Some((STAKING_ADDRESS, MON)));
        }
        assert_eq!(call.system_tx().data, call.data);
        assert_eq!(call.system_tx().caller, SYSTEM_ADDRESS);
        assert!(call.validate_chain_id(CHAIN_ID).is_ok());
    }
}

#[test]
fn malformed_envelopes_fail_classification_and_admission() {
    let base = reward(0);
    let selector = syscallRewardCall::SELECTOR;
    let mut bad_address = base.input.to_vec();
    bad_address[4] = 1;
    let mut bad_epoch = syscall_on_epoch_change_calldata(1).to_vec();
    bad_epoch[4] = 1;
    let cases = [
        (TxLegacy { to: TxKind::Create, ..base.clone() }, Error::InvalidTarget(TxKind::Create)),
        (TxLegacy { to: USER.into(), ..base.clone() }, Error::InvalidTarget(USER.into())),
        (TxLegacy { gas_limit: 1, ..base.clone() }, Error::NonZeroGasLimit(1)),
        (TxLegacy { gas_price: 1, ..base.clone() }, Error::NonZeroGasPrice(1)),
        (TxLegacy { input: Bytes::new(), ..base.clone() }, Error::MissingSelector),
        (
            TxLegacy { input: Bytes::from_static(&[0; 4]), ..base.clone() },
            Error::UnknownSelector([0; 4]),
        ),
        (
            TxLegacy { input: Bytes::copy_from_slice(&base.input[..35]), ..base.clone() },
            Error::InvalidCalldataLength { selector, expected: 36, got: 35 },
        ),
        (
            TxLegacy { input: syscall_reward_calldata(AUTHOR, MON), ..base.clone() },
            Error::InvalidCalldataLength { selector, expected: 36, got: 68 },
        ),
        (
            TxLegacy { input: bad_address.into(), ..base.clone() },
            Error::NonCanonicalCalldata { selector },
        ),
        (
            legacy(0, U256::ZERO, bad_epoch.into()),
            Error::NonCanonicalCalldata { selector: syscallOnEpochChangeCall::SELECTOR },
        ),
        (
            legacy(0, MON, syscall_snapshot_calldata()),
            Error::NonZeroValue { selector: syscallSnapshotCall::SELECTOR },
        ),
        (
            legacy(0, MON, syscall_on_epoch_change_calldata(1)),
            Error::NonZeroValue { selector: syscallOnEpochChangeCall::SELECTOR },
        ),
        (
            legacy(
                0,
                U256::ZERO,
                Bytes::from([syscallSnapshotCall::SELECTOR.as_slice(), &[0]].concat()),
            ),
            Error::InvalidCalldataLength {
                selector: syscallSnapshotCall::SELECTOR,
                expected: 4,
                got: 5,
            },
        ),
    ];
    let mut evm = evm(MonadHardfork::MonadTen);
    for (tx, expected) in cases {
        let tx = recovered(tx);
        assert_eq!(protocol_system_call(&tx), Err(expected.clone()));
        assert_eq!(error(evm.validate_tx(&tx).unwrap_err()), expected);
        assert_eq!(error(evm.transact(&tx).unwrap_err()), expected);
        assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
    }
}

#[test]
fn typed_system_envelopes_are_rejected_and_blobs_remain_unsupported() {
    let envelopes = [
        MonadTxEnvelope::Eip2930(TxEip2930::default()),
        MonadTxEnvelope::Eip1559(TxEip1559::default()),
        MonadTxEnvelope::Eip4844(TxEip4844::default().into()),
        LazyTxEip7702::from_signed_authorizations(TxEip7702::default()).into(),
    ];
    let mut evm = evm(MonadHardfork::MonadTen);
    for envelope in envelopes {
        let expected = Error::NotLegacy { tx_type: envelope.ty() };
        let tx = Recovered::new_unchecked(envelope, SYSTEM_ADDRESS);
        assert_eq!(protocol_system_call(&tx), Err(expected.clone()));
        assert_eq!(error(evm.validate_tx(&tx).unwrap_err()), expected);
        assert_eq!(error(evm.transact(&tx).unwrap_err()), expected);
    }
    let blob =
        Recovered::new_unchecked(MonadTxEnvelope::Eip4844(TxEip4844::default().into()), USER);
    assert!(matches!(evm.transact(&blob), Err(HandlerError::UnsupportedTransactionType(3))));
}

#[derive(Default)]
struct Recorder {
    calls: Vec<Bytes>,
    ends: usize,
}

impl Inspector<MonadEvmTypes> for Recorder {
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        assert_eq!(
            (message.caller, message.destination, message.value),
            (SYSTEM_ADDRESS, STAKING_ADDRESS, U256::ZERO)
        );
        assert!(!interp.host().ext().preserve_reserve_balance_tracker);
        assert!(!interp.host().ext().reserve_balance.tracking_enabled());
        self.calls.push(message.input.to_bytes());
        None
    }
    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        _: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.ends += 1;
    }
}

#[test]
fn replays_rewards_and_epoch_calls_with_logs_nonce_and_zero_gas() {
    for spec in MonadHardfork::ALL {
        let mut evm = evm(spec);
        let mut recorder = Recorder::default();
        let reward =
            evm.transact_with_inspector(&recovered(reward(0)), &mut recorder).unwrap().commit();
        assert_zero_gas(&reward);
        assert!(!reward.logs.is_empty());
        assert_eq!(balances(&mut evm), (1, U256::ZERO, MON * U256::from(2)));
        assert_eq!(reward_slot(&mut evm), MON);
        for (nonce, data) in
            [(1, syscall_snapshot_calldata()), (2, syscall_on_epoch_change_calldata(1))]
        {
            let result = evm
                .transact_with_inspector(&recovered(legacy(nonce, U256::ZERO, data)), &mut recorder)
                .unwrap()
                .commit();
            assert_zero_gas(&result);
        }
        assert_eq!(
            recorder.calls,
            [
                syscall_reward_calldata(AUTHOR, MON),
                syscall_snapshot_calldata(),
                syscall_on_epoch_change_calldata(1)
            ]
        );
        assert_eq!(recorder.ends, 3);
        assert_eq!(balances(&mut evm).0, 3);
        assert_eq!(
            evm.state_mut().storage_slot_untracked(&STAKING_ADDRESS, &global_slots::EPOCH).unwrap(),
            U256::ONE << 192
        );
    }
}

#[test]
fn protocol_checks_ignore_ordinary_validation_relaxations() {
    let mut evm = evm(MonadHardfork::MonadTen);
    let mut version = *evm.version();
    version.features.remove(EvmFeatures::NONCE_CHECK | EvmFeatures::TX_CHAIN_ID_CHECK);
    let spec = evm.config_spec_id();
    evm.set_execution_config(
        ExecutionConfig::for_spec_and_version(spec, version),
        spec,
        evm.registry().clone(),
        monad_evm2::monad_precompiles(spec),
    );
    for (tx, expected) in [
        (
            TxLegacy { chain_id: Some(CHAIN_ID + 1), ..reward(0) },
            Error::ChainIdMismatch { expected: CHAIN_ID, got: CHAIN_ID + 1 },
        ),
        (reward(1), Error::NonceMismatch { expected: 0, got: 1 }),
        (reward(u64::MAX), Error::NonceOverflow),
        (TxLegacy { value: U256::MAX, ..reward(0) }, Error::BalanceOverflow),
    ] {
        let tx = recovered(tx);
        assert_eq!(error(evm.validate_tx(&tx).unwrap_err()), expected);
        assert_eq!(error(evm.transact(&tx).unwrap_err()), expected);
        assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
    }
    let mut tx = reward(0);
    tx.chain_id = None;
    assert_zero_gas(&evm.transact(&recovered(tx)).unwrap().commit());
}

#[test]
fn failed_reward_restores_nonce_mint_storage_and_allows_retry() {
    for spec in MonadHardfork::ALL {
        let mut evm = evm(spec);
        let bad = legacy(0, MON, syscallRewardCall { blockAuthor: USER }.abi_encode().into());
        let failure = error(evm.transact(&recovered(bad)).unwrap_err());
        assert!(matches!(failure, Error::ExecutionFailed { stop: InstrStop::Revert, .. }));
        assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
        assert_eq!(reward_slot(&mut evm), U256::ZERO);
        let result = evm.transact(&recovered(reward(0))).unwrap().commit();
        assert_zero_gas(&result);
        assert_eq!(reward_slot(&mut evm), MON);
    }
}

fn preserve(evm: &mut MonadHost<'_>) {
    evm.ext_mut().chain.parent_senders_and_authorities.insert(USER);
    let chain = evm.ext().chain.clone();
    let sender = reserve_account(evm, USER).unwrap();
    let spec = evm.config_spec_id();
    evm.ext_mut().reserve_balance.init(ReserveBalanceInit {
        chain: &chain,
        spec,
        sender: USER,
        effective_gas_price: 0,
        gas_limit: 0,
        sender_is_delegated: false,
        sender_account: Some(&sender),
    });
    evm.ext_mut().preserve_reserve_balance_tracker = true;
}

#[test]
fn native_protocol_replay_preserves_context_for_every_state_resolution() {
    for spec in MonadHardfork::ALL {
        for mode in 0..5 {
            let mut host = native_evm(spec);
            preserve(&mut host);
            host.ext_mut().memory_pool_offset = 64;
            let before = host.ext().clone();
            let calls = Rc::new(Cell::new(0));
            host.set_inspector(Counter(calls.clone()));
            let tx = recovered(reward(0));
            host.validate_tx(&tx).unwrap();
            assert_eq!(balances(&mut host), (0, U256::ZERO, MON));
            let pending = host.transact(&tx).unwrap();
            assert_zero_gas(pending.result());
            match mode {
                0 => drop(pending),
                1 => {
                    assert_zero_gas(&pending.discard());
                }
                2 => {
                    let detached = pending.detach();
                    assert_eq!(balances(&mut host), (0, U256::ZERO, MON));
                    host.commit_source(&detached.pending_state);
                }
                3 => {
                    assert_zero_gas(&pending.commit_to(&mut BlockStateAccumulator::default()));
                }
                _ => {
                    assert_zero_gas(&pending.commit());
                }
            }
            assert_eq!(calls.get(), 1);
            assert_eq!(host.ext().reserve_balance, before.reserve_balance);
            assert_eq!(host.ext().chain, before.chain);
            assert_eq!(host.ext().memory_pool_offset, 64);
            assert!(host.ext().preserve_reserve_balance_tracker && host.ext().protocol_replay);
            let committed = mode >= 2;
            assert_eq!(
                balances(&mut host),
                (u64::from(committed), U256::ZERO, MON * U256::from(1 + u8::from(committed)))
            );
        }
    }
}

#[test]
fn native_failed_and_interrupted_replay_restore_context_and_prestate() {
    for mode in 0..3 {
        let mut host = native_evm(MonadHardfork::MonadTen);
        preserve(&mut host);
        let before = host.ext().clone();
        if mode == 0 {
            let tx = recovered(legacy(
                0,
                MON,
                syscallRewardCall { blockAuthor: USER }.abi_encode().into(),
            ));
            assert!(matches!(
                error(host.transact(&tx).unwrap_err()),
                Error::ExecutionFailed { .. }
            ));
        } else {
            let interrupted = std::panic::catch_unwind(AssertUnwindSafe(|| {
                host.transact_with_inspector(
                    &recovered(reward(0)),
                    &mut Interrupt { panic: mode == 2 },
                )
                .map(|tx| tx.commit())
            }));
            if mode == 2 {
                assert!(interrupted.is_err());
            } else {
                assert!(matches!(
                    error(interrupted.unwrap().unwrap_err()),
                    Error::ExecutionFailed { .. }
                ));
            }
        }
        assert_eq!(balances(&mut host), (0, U256::ZERO, MON));
        assert_eq!(reward_slot(&mut host), U256::ZERO);
        assert_eq!(host.ext().reserve_balance, before.reserve_balance);
        assert_eq!(host.ext().chain, before.chain);
        assert!(host.ext().preserve_reserve_balance_tracker && host.ext().protocol_replay);
        assert_zero_gas(&host.transact(&recovered(reward(0))).unwrap().commit());
    }
}

#[test]
fn validation_and_state_handles_preserve_the_enclosing_context() {
    for mode in 0..5 {
        let mut evm = evm(MonadHardfork::MonadTen);
        preserve(&mut evm);
        let before = evm.ext().clone();
        let tx = recovered(reward(0));
        evm.validate_tx(&tx).unwrap();
        assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
        let pending = evm.transact(&tx).unwrap();
        assert_zero_gas(pending.result());
        assert!(pending.preserved_context().is_none());
        match mode {
            0 => drop(pending),
            1 => {
                assert_zero_gas(&pending.discard());
            }
            2 => {
                let detached = pending.detach();
                assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
                evm.commit_source(&detached.pending_state);
            }
            3 => {
                assert_zero_gas(&pending.commit_to(&mut BlockStateAccumulator::default()));
            }
            _ => {
                assert_zero_gas(&pending.commit());
            }
        }
        assert_eq!(evm.ext().reserve_balance, before.reserve_balance);
        assert_eq!(evm.ext().chain, before.chain);
        assert!(evm.ext().preserve_reserve_balance_tracker && evm.ext().protocol_replay);
        assert_eq!(evm.ext().memory_pool_offset(), before.memory_pool_offset());
        let committed = mode >= 2;
        assert_eq!(
            balances(&mut evm),
            (u64::from(committed), U256::ZERO, MON * U256::from(1 + u8::from(committed)))
        );
    }
}

struct Interrupt {
    panic: bool,
}
impl Inspector<MonadEvmTypes> for Interrupt {
    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        if self.panic {
            panic!("interrupt protocol replay");
        }
        result.stop = InstrStop::InvalidFEOpcode;
        result.gas = GasTracker::new(0);
    }
}

#[test]
fn late_halts_and_panics_restore_writes_and_context() {
    for panic in [false, true] {
        let mut evm = evm(MonadHardfork::MonadTen);
        preserve(&mut evm);
        let before = evm.ext().clone();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            evm.transact_with_inspector(&recovered(reward(0)), &mut Interrupt { panic })
                .map(|tx| tx.commit())
        }));
        if panic {
            assert!(result.is_err());
        } else {
            assert!(matches!(
                error(result.unwrap().unwrap_err()),
                Error::ExecutionFailed { stop: InstrStop::InvalidFEOpcode, .. }
            ));
        }
        assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
        assert_eq!(reward_slot(&mut evm), U256::ZERO);
        assert_eq!(evm.ext().reserve_balance, before.reserve_balance);
        assert_eq!(evm.ext().chain, before.chain);
        assert!(evm.ext().protocol_replay && evm.ext().preserve_reserve_balance_tracker);
        assert_zero_gas(&evm.transact(&recovered(reward(0))).unwrap().commit());
    }
}

struct Counter(Rc<Cell<usize>>);
impl Inspector<MonadEvmTypes> for Counter {
    fn call(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.0.set(self.0.get() + 1);
        None
    }
}

#[test]
fn borrowed_inspector_overrides_installed_and_replay_requires_opt_in() {
    let mut evm = evm(MonadHardfork::MonadTen);
    let calls = Rc::new(Cell::new(0));
    evm.set_inspector(Counter(calls.clone()));
    evm.ext_mut().protocol_replay = false;
    assert!(evm.transact(&recovered(reward(0))).is_err());
    assert_eq!(calls.get(), 0);
    evm.ext_mut().protocol_replay = true;
    assert_zero_gas(
        &evm.transact_with_inspector(&recovered(reward(0)), &mut Recorder::default())
            .unwrap()
            .discard(),
    );
    assert_eq!(calls.get(), 0);
    assert_zero_gas(&evm.transact(&recovered(reward(0))).unwrap().commit());
    assert_eq!(calls.get(), 1);
    assert!(evm.inspector().is_some());
    let user = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(legacy(0, U256::ZERO, syscall_snapshot_calldata())),
        USER,
    );
    assert!(matches!(evm.transact(&user), Err(HandlerError::IntrinsicGasTooLow { .. })));
    let ordinary = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: USER.into(),
            gas_limit: 21_000,
            ..Default::default()
        }),
        USER,
    );
    let result = evm.transact(&ordinary).unwrap().commit();
    assert_eq!(result.tx_gas_used(), 21_000);
    assert_eq!(balances(&mut evm).0, 1);
}

#[test]
fn replay_mode_retains_ordinary_reserve_enforcement() {
    let mut evm = evm(MonadHardfork::MonadTen);
    evm.ext_mut().chain.parent_senders_and_authorities.insert(USER);
    let fee = U256::from(100_000);
    let tx = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: AUTHOR.into(),
            gas_limit: 100_000,
            gas_price: 1,
            value: MON - fee,
            ..Default::default()
        }),
        USER,
    );
    let result = evm.transact(&tx).unwrap().commit();
    assert!(result.ext.reverted_for_reserve_balance);
    assert_eq!(result.tx_gas_used(), 100_000);
    let sender = evm.state_mut().account_info_untracked(&USER).unwrap().unwrap();
    assert_eq!((sender.nonce, sender.balance), (1, MON - fee));
    assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
}

#[test]
fn native_replay_works_and_standalone_checks_its_transaction_handler() {
    let mut evm = evm(MonadHardfork::MonadTen);
    let tx = recovered(reward(0));
    assert_zero_gas(&MonadHost::transact(&mut evm, &tx).unwrap().discard());
    assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
    let original = evm.registry().clone();
    let mut registry = original.clone();
    registry.register(
        0,
        |tx| tx.as_legacy(),
        handler(
            |_: &mut TxRequest<'_, '_, MonadEvmTypes, TxLegacy>| Ok(()),
            |request: TxRequest<'_, '_, MonadEvmTypes, TxLegacy>, ()| {
                request.host.state_mut().account(&STAKING_ADDRESS)?.set_balance(U256::ZERO);
                Ok(TxResult::<MonadEvmTypes>::default())
            },
        ),
    );
    let spec = evm.config_spec_id();
    let config = ExecutionConfig::for_spec_and_version(spec, *evm.version());
    evm.set_execution_config(config.clone(), spec, registry, monad_evm2::monad_precompiles(spec));
    let failure = evm.transact(&tx).unwrap_err();
    assert!(failure.to_string().contains("system calls require the Monad transaction handler"));
    assert_eq!(balances(&mut evm), (0, U256::ZERO, MON));
    evm.set_execution_config(config, spec, original, monad_evm2::monad_precompiles(spec));
    assert_zero_gas(&evm.transact(&tx).unwrap().commit());
}
