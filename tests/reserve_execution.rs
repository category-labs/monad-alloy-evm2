//! Reserve tracking across transaction boundaries, native rollbacks and query calls.

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork, MonadTxEnvelope,
    ReserveAccount, ReserveBalanceInit,
    reserve_balance::{
        DIPPED_INTO_RESERVE_SELECTOR, RESERVE_BALANCE_ADDRESS, dipped_into_reserve,
        run_reserve_balance_precompile,
    },
};
use alloy_primitives::{Address, Bytes, U256};
use evm2::{
    AccountInfo,
    bytecode::Bytecode,
    evm::InMemoryDB,
    interpreter::{GasTracker, InstrStop, Message, MessageKind, op},
    precompiles::{PrecompileError, PrecompileHalt},
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
fn opt_in_enforcement_reverts_value_but_keeps_fees_and_nonce() {
    for spec in MonadHardfork::ALL {
        for restricted in [false, true] {
            for enforce in [false, true] {
                let mut evm = evm(spec, restricted);
                evm.ext_mut().revert_on_reserve_balance_violation = enforce;
                let result = evm.transact(&tx(TARGET, 1, 0)).unwrap().commit();
                assert_eq!(result.stop.is_success(), !(restricted && enforce), "{spec}");
                assert_eq!(result.ext.reverted_for_reserve_balance, restricted && enforce);
                assert_eq!(result.ext.dipped_into_reserve, restricted && !enforce);
                assert_eq!(result.tx_gas_used(), 100_000);
                assert_eq!(balance(&mut evm, BENEFICIARY), U256::from(100_000));
                let value = u64::from(!(restricted && enforce));
                assert_eq!(balance(&mut evm, SENDER), U256::from(INITIAL - 100_000 - value));
                assert_eq!(balance(&mut evm, TARGET), U256::from(value));
                assert_eq!(evm.state_mut().account(&SENDER).unwrap().nonce(), 1);
                assert!(!evm.ext().reserve_balance.tracking_enabled());
                // A new transaction uses its own accepted-state baseline.
                let next = evm.transact(&tx(TARGET, 0, 1)).unwrap().commit();
                assert!(next.is_success());
                assert!(!next.ext.dipped_into_reserve);
            }
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
    assert_ne!(result.stop, InstrStop::Revert);
    assert!(evm.transact(&tx(TARGET, 0, 0)).is_err());
    assert!(!evm.ext().reserve_balance.tracking_enabled());
    assert!(!evm.ext().reserve_balance.has_violation());
}
