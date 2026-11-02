//! Monad transaction balances, execution consumption and receipt gas.

mod common;

use alloy_consensus::{
    TxEip1559, TxEip2930, TxEip4844, TxEip7702, TxLegacy, transaction::Recovered,
};
use alloy_monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork,
    MonadTxEnvelope, SYSTEM_ADDRESS, SystemSenderIsAuthority,
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use common::FORKS;
use evm2::{
    AccountInfo, EvmFeatures, ExecutionConfig, Inspector, Precompiles,
    bytecode::Bytecode,
    ethereum::LazyTxEip7702,
    evm::{InMemoryDB, SystemTx},
    interpreter::{InstrStop, Interpreter, Message, MessageResult, op},
    registry::HandlerError,
};
use monad_revm::{MonadBuilder, MonadCfgEnv, monad_context_with_db};
use revm::{
    DatabaseCommit, ExecuteEvm,
    context::{BlockEnv, TxEnv},
    context_interface::transaction::{
        AccessListItem, Authorization, RecoveredAuthority, RecoveredAuthorization,
    },
    database::InMemoryDB as ReferenceDB,
    state::{AccountInfo as ReferenceAccount, Bytecode as ReferenceBytecode},
};

const SENDER: Address = Address::repeat_byte(0x11);
const TARGET: Address = Address::repeat_byte(0x22);
const BENEFICIARY: Address = Address::repeat_byte(0x33);
const REVERTER: Address = Address::repeat_byte(0x44);
const INVALID: Address = Address::repeat_byte(0x55);
const AUTHORITY: Address = Address::repeat_byte(0x66);
const REVERT_CODE: &[u8] = &[op::PUSH0, op::PUSH0, op::REVERT];

fn funded_db(balance: U256) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, AccountInfo::default().with_balance(balance));
    db
}

fn insert_code(db: &mut InMemoryDB, target: Address, code: &'static [u8]) {
    db.insert_account_info(
        &target,
        AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::from_static(code))),
    );
}

fn evm(spec: MonadHardfork, basefee: u64, db: InMemoryDB) -> MonadEvm<'static> {
    let block = MonadBlockEnv {
        basefee: U256::from(basefee),
        beneficiary: BENEFICIARY,
        ..Default::default()
    };
    MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID).create_evm(spec, block, db)
}

fn account(evm: &mut MonadEvm<'_>, address: Address) -> AccountInfo {
    evm.state_mut().account_info_untracked(&address).unwrap().unwrap_or_default()
}

const fn recovered(tx: MonadTxEnvelope) -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(tx, SENDER)
}

fn legacy(limit: u64, price: u128) -> TxLegacy {
    TxLegacy {
        chain_id: Some(MONAD_MAINNET_CHAIN_ID),
        gas_limit: limit,
        gas_price: price,
        to: TxKind::Call(TARGET),
        ..Default::default()
    }
}

#[test]
fn receipts_and_balances_across_success_revert_and_halt() {
    let initial = U256::from(10_000_000);
    let value = U256::from(13);
    for (spec, reference_spec) in FORKS {
        let mut db = funded_db(initial);
        let mut reference_db = ReferenceDB::default();
        reference_db.insert_account_info(SENDER, ReferenceAccount::default().with_balance(initial));
        for (address, code) in [(REVERTER, REVERT_CODE), (INVALID, &[op::INVALID][..])] {
            insert_code(&mut db, address, code);
            reference_db.insert_account_info(
                address,
                ReferenceAccount::default()
                    .with_code(ReferenceBytecode::new_raw(Bytes::from_static(code))),
            );
        }
        let mut evm = evm(spec, 3, db);
        let mut reference = monad_context_with_db(reference_db)
            .with_cfg(
                MonadCfgEnv::new_with_spec(reference_spec).with_chain_id(MONAD_MAINNET_CHAIN_ID),
            )
            .with_block(BlockEnv { basefee: 3, beneficiary: BENEFICIARY, ..Default::default() })
            .build_monad();
        let mut receipt_gas = 0;
        let mut block_gas = 0;
        for (nonce, (to, limit, stop, consumed, cumulative)) in [
            (TARGET, 100_000, InstrStop::Stop, 21_000, 100_000),
            (REVERTER, 120_000, InstrStop::Revert, 21_004, 220_000),
            (INVALID, 140_000, InstrStop::InvalidFEOpcode, 140_000, 360_000),
        ]
        .into_iter()
        .enumerate()
        {
            let tx =
                TxLegacy { nonce: nonce as u64, to: TxKind::Call(to), value, ..legacy(limit, 5) };
            let reference_tx = TxEnv::builder()
                .caller(SENDER)
                .kind(tx.to)
                .value(value)
                .chain_id(tx.chain_id)
                .nonce(tx.nonce)
                .gas_price(tx.gas_price)
                .gas_limit(limit)
                .build()
                .unwrap();
            let expected = reference.transact(reference_tx).unwrap();
            let result = evm.transact(&recovered(MonadTxEnvelope::Legacy(tx))).unwrap().commit();
            assert_eq!(result.stop, stop, "{spec}");
            assert_eq!(result.is_success(), expected.result.is_success());
            assert_eq!(result.total_gas_spent, consumed);
            assert_eq!(result.total_gas_spent, expected.result.gas().total_gas_spent());
            // The C++ receipt charges the gas limit, including on an early return or revert.
            assert_eq!(result.floor_gas, limit);
            assert_eq!(result.tx_gas_used(), limit);
            assert_eq!(
                (result.refunded, result.final_refunded(), result.state_gas_spent()),
                (0, 0, 0)
            );
            receipt_gas += result.tx_gas_used();
            block_gas += result.execution_gas_spent();
            assert_eq!((receipt_gas, block_gas), (cumulative, cumulative));

            let sender = account(&mut evm, SENDER);
            let reward = account(&mut evm, BENEFICIARY).balance;
            assert_eq!(sender.nonce, nonce as u64 + 1);
            assert_eq!(sender.balance, initial - U256::from(5 * cumulative) - value);
            assert_eq!(reward, U256::from(2 * cumulative));
            assert_eq!(account(&mut evm, to).balance, if nonce == 0 { value } else { U256::ZERO });
            assert_eq!(sender.balance, expected.state[&SENDER].info.balance);
            assert_eq!(reward, expected.state[&BENEFICIARY].info.balance);
            reference.0.ctx.journaled_state.database.commit(expected.state);
        }
    }
}

#[test]
fn beneficiary_reward_can_exceed_u128() {
    for basefee in [0, 100] {
        let mut evm = evm(MonadHardfork::MonadTen, basefee, funded_db(U256::MAX));
        let limit = 21_000;
        let tx = recovered(MonadTxEnvelope::Legacy(legacy(limit, u128::MAX)));
        assert!(evm.transact(&tx).unwrap().commit().is_success());
        let fee = U256::from(u128::MAX) * U256::from(limit);
        let reward = fee - U256::from(basefee) * U256::from(limit);
        assert!(reward > U256::from(u128::MAX));
        assert_eq!(account(&mut evm, SENDER).balance, U256::MAX - fee);
        assert_eq!(account(&mut evm, BENEFICIARY).balance, reward);
    }
}

#[test]
fn dynamic_fee_admission_checks_the_effective_fee_and_execution_checks_value() {
    let limit = 100_000;
    let basefee = 10;
    let value = U256::from(5);
    for (max_fee, price) in [(1_000, 13), (12, 12)] {
        let fee = U256::from(limit) * U256::from(price);
        let tx = recovered(MonadTxEnvelope::Eip1559(TxEip1559 {
            chain_id: MONAD_MAINNET_CHAIN_ID,
            gas_limit: limit,
            to: TxKind::Call(TARGET),
            value,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: 3,
            ..Default::default()
        }));
        let mut poor = evm(MonadHardfork::MonadTen, basefee, funded_db(fee - U256::ONE));
        let error = HandlerError::InsufficientFunds { fee, balance: fee - U256::ONE };
        assert_eq!(poor.validate_tx(&tx), Err(error.clone()));
        assert_eq!(poor.call_tx(&tx).unwrap_err(), error);
        let sender = account(&mut poor, SENDER);
        assert_eq!((sender.nonce, sender.balance), (0, fee - U256::ONE));
        assert_eq!(account(&mut poor, BENEFICIARY).balance, U256::ZERO);

        for (balance, stop) in [(fee, InstrStop::OutOfFunds), (fee + value, InstrStop::Stop)] {
            let mut evm = evm(MonadHardfork::MonadTen, basefee, funded_db(balance));
            evm.validate_tx(&tx).unwrap();
            let result = evm.transact(&tx).unwrap().commit();
            assert_eq!(result.stop, stop);
            assert_eq!(result.tx_gas_used(), limit);
            assert_eq!(evm.tx_env().gas_price, U256::from(price));
            let sender = account(&mut evm, SENDER);
            assert_eq!((sender.nonce, sender.balance), (1, U256::ZERO));
            assert_eq!(account(&mut evm, TARGET).balance, balance - fee);
            assert_eq!(account(&mut evm, BENEFICIARY).balance, fee - U256::from(basefee * limit));
        }
    }
}

#[derive(Default)]
struct RemainingGas(Option<u64>);

impl Inspector<MonadEvmTypes> for RemainingGas {
    fn call_end(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.0 = Some(result.gas.remaining());
    }
}

#[test]
fn calldata_floor_is_validated_and_execution_consumption_is_preserved() {
    let mut evm = evm(MonadHardfork::MonadTen, 0, funded_db(U256::ZERO));
    let mut tx = TxLegacy { input: Bytes::from_static(b"hello"), ..legacy(21_080, 0) };
    assert!(matches!(
        evm.validate_tx(&recovered(MonadTxEnvelope::Legacy(tx.clone()))),
        Err(HandlerError::IntrinsicGasTooLow { required: 21_200, got: 21_080 })
    ));
    tx.gas_limit = 100_000;
    let mut remaining = RemainingGas::default();
    let result = evm
        .transact_with_inspector(&recovered(MonadTxEnvelope::Legacy(tx)), &mut remaining)
        .unwrap()
        .commit();
    assert!(result.is_success());
    assert_eq!(result.total_gas_spent, 21_080);
    assert_eq!(result.tx_gas_used(), 100_000);
    assert_eq!(remaining.0, Some(100_000 - 21_080));
}

#[test]
fn clearing_storage_and_unused_gas_do_not_refund_an_access_list_transaction() {
    let mut db = funded_db(U256::from(100_000));
    insert_code(&mut db, TARGET, &[op::PUSH0, op::PUSH0, op::SSTORE, op::STOP]);
    db.insert_account_storage(&TARGET, &U256::ZERO, &U256::ONE);
    let mut evm = evm(MonadHardfork::MonadEight, 0, db);
    let tx = recovered(MonadTxEnvelope::Eip2930(TxEip2930 {
        chain_id: MONAD_MAINNET_CHAIN_ID,
        gas_limit: 100_000,
        gas_price: 1,
        to: TxKind::Call(TARGET),
        access_list: vec![AccessListItem { address: TARGET, storage_keys: vec![B256::ZERO] }]
            .into(),
        ..Default::default()
    }));
    let result = evm.transact(&tx).unwrap().commit();
    assert!(result.is_success());
    assert_eq!(evm.state_mut().storage_slot_untracked(&TARGET, &U256::ZERO).unwrap(), U256::ZERO);
    assert_eq!((result.refunded, result.final_refunded(), result.tx_gas_used()), (0, 0, 100_000));
    assert!(result.total_gas_spent < result.tx_gas_used());
    assert_eq!(account(&mut evm, SENDER).balance, U256::ZERO);
    assert_eq!(account(&mut evm, BENEFICIARY).balance, U256::from(100_000));
}

fn set_code(authorities: &[Address]) -> Recovered<MonadTxEnvelope> {
    let tx = TxEip7702 {
        chain_id: MONAD_MAINNET_CHAIN_ID,
        gas_limit: 100_000,
        max_fee_per_gas: 50,
        max_priority_fee_per_gas: 1,
        to: AUTHORITY,
        ..Default::default()
    };
    let authorizations = authorities
        .iter()
        .map(|&address| {
            RecoveredAuthorization::new_unchecked(
                Authorization { chain_id: U256::ZERO, address: REVERTER, nonce: 0 },
                RecoveredAuthority::Valid(address),
            )
        })
        .collect();
    recovered(LazyTxEip7702::from_cached_recovered_authorizations(tx, authorizations).into())
}

#[test]
fn set_code_charges_the_limit_and_preserves_delegation_on_revert() {
    let mut db = funded_db(U256::from(200_000));
    db.insert_account_info(&AUTHORITY, AccountInfo::default().with_balance(U256::ONE));
    insert_code(&mut db, REVERTER, REVERT_CODE);
    let mut evm = evm(MonadHardfork::MonadTen, 1, db);
    assert_eq!(evm.validate_tx(&set_code(&[])), Err(HandlerError::EmptyAuthorizationList));
    let invalid = set_code(&[AUTHORITY, SYSTEM_ADDRESS]);
    assert!(evm.call_tx(&invalid).unwrap_err().external_ref::<SystemSenderIsAuthority>().is_some());
    let blob = recovered(MonadTxEnvelope::Eip4844(TxEip4844::default().into()));
    assert_eq!(evm.validate_tx(&blob), Err(HandlerError::UnsupportedTransactionType(3)));
    let legacy_handler = evm.registry().try_get_by_type(0).unwrap();
    assert_eq!(
        legacy_handler.validate(blob.as_recovered_ref(), &mut evm),
        Err(HandlerError::WrongTransactionType { expected: 0 })
    );

    let result = evm.transact(&set_code(&[AUTHORITY])).unwrap().commit();
    assert!(result.is_revert());
    assert_eq!(result.total_gas_spent, 46_004);
    assert_eq!((result.refunded, result.tx_gas_used()), (0, 100_000));
    assert_eq!(account(&mut evm, AUTHORITY).nonce, 1);
    let code = evm.state_mut().account(&AUTHORITY).unwrap().load_code().unwrap();
    assert_eq!(code.eip7702_address(), Some(REVERTER));
    assert_eq!(account(&mut evm, SENDER).balance, U256::ZERO);
    assert_eq!(account(&mut evm, BENEFICIARY).balance, U256::from(100_000));
}

#[test]
fn system_calls_only_meter_execution() {
    let mut db = funded_db(U256::ONE);
    insert_code(&mut db, REVERTER, REVERT_CODE);
    let mut evm = evm(MonadHardfork::MonadTen, 10, db);
    let call = SystemTx::new(REVERTER, Bytes::new()).with_caller(SENDER).with_gas_limit(100_000);
    let result = evm.system_call(call).unwrap().commit();
    assert!(result.is_revert());
    assert_eq!((result.floor_gas, result.tx_gas_used(), result.total_gas_spent), (0, 4, 4));
    let sender = account(&mut evm, SENDER);
    assert_eq!((sender.nonce, sender.balance), (0, U256::ONE));
    assert_eq!(account(&mut evm, BENEFICIARY).balance, U256::ZERO);
}

#[test]
fn simulation_switches_control_balance_top_up_and_fee_transfers() {
    let spec = MonadHardfork::MonadTen;
    for charge_fees in [true, false] {
        let mut evm = evm(spec, 3, funded_db(U256::ZERO));
        let mut version = *evm.version();
        version.features.remove(EvmFeatures::BALANCE_CHECK | EvmFeatures::BASE_FEE_CHECK);
        version.features.insert(EvmFeatures::BALANCE_TOP_UP);
        version.features.set(EvmFeatures::FEE_CHARGE, charge_fees);
        evm.set_execution_config(
            ExecutionConfig::for_spec_and_version(spec, version),
            spec,
            evm.registry().clone(),
            Precompiles::base(spec.into_eth_spec()),
        );
        let tx =
            recovered(MonadTxEnvelope::Legacy(TxLegacy { value: U256::ONE, ..legacy(100_000, 5) }));
        let result = evm.transact(&tx).unwrap().commit();
        assert!(result.is_success());
        assert_eq!(result.tx_gas_used(), 100_000);
        assert_eq!(account(&mut evm, TARGET).balance, U256::ONE);
        assert_eq!(account(&mut evm, SENDER).balance, U256::ZERO);
        let reward = if charge_fees { 200_000 } else { 0 };
        assert_eq!(account(&mut evm, BENEFICIARY).balance, U256::from(reward));
    }
}
