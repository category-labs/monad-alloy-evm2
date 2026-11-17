//! Contract execution with independent protocol expectations.

mod common;

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256, uint};
use common::FORKS;
use evm2::{
    AccountInfo, TxResult,
    bytecode::Bytecode,
    evm::InMemoryDB,
    interpreter::{InstrStop, op},
    registry::HandlerError,
};
use monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID, MonadBlockEnv, MonadEvmBuilder, MonadEvmTypes,
    MonadHardfork, MonadTxEnvelope,
};

const CALLER: Address = Address::new([0x11; 20]);
const CONTRACT: Address = Address::new([0x22; 20]);
const CALLER_BALANCE: U256 = uint!(100_000_000_000_000_000_000_U256);
const CALLER_NONCE: u64 = 7;
const BLOCK_NUMBER: u64 = 256;

fn legacy_call(gas_limit: u64) -> TxLegacy {
    // Omit the optional signature chain ID so CHAINID must come from the EVM.
    TxLegacy {
        chain_id: None,
        nonce: CALLER_NONCE,
        gas_price: 1,
        gas_limit,
        to: TxKind::Call(CONTRACT),
        ..Default::default()
    }
}

fn evm2_db(code: Bytes) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &CALLER,
        AccountInfo::default().with_balance(CALLER_BALANCE).with_nonce(CALLER_NONCE),
    );
    db.insert_account_info(&CONTRACT, AccountInfo::default().with_code(Bytecode::new_legacy(code)));
    db
}

fn run_contract(
    spec: MonadHardfork,
    chain_id: u64,
    code: &'static [u8],
) -> TxResult<MonadEvmTypes> {
    let code = Bytes::from_static(code);
    let tx = legacy_call(100_000);
    let number = U256::from(BLOCK_NUMBER);

    let block = MonadBlockEnv { number, ..Default::default() };
    let mut evm = MonadEvmBuilder::new(chain_id).create_evm(spec, block, evm2_db(code));
    assert_eq!(evm.config_spec_id(), spec);
    assert_eq!(evm.spec_id(), spec.into_eth_spec());
    let envelope = Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), CALLER);
    evm.call_tx(&envelope).unwrap()
}

fn assert_returns(actual: TxResult<MonadEvmTypes>, word: U256) {
    assert!(actual.status);
    assert_eq!(actual.stop, InstrStop::Return);
    assert_eq!(actual.output.as_ref(), word.to_be_bytes::<32>());
}

#[test]
fn chainid_uses_mainnet_and_testnet_configuration() {
    const CODE: &[u8] = &[op::CHAINID, op::PUSH0, op::MSTORE, op::PUSH1, 32, op::PUSH0, op::RETURN];
    for forks in FORKS {
        for chain_id in [MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID] {
            assert_returns(run_contract(forks, chain_id, CODE), U256::from(chain_id));
        }
    }
}

#[test]
fn clz_activates_at_monad_nine() {
    const CODE: &[u8] =
        &[op::NUMBER, op::CLZ, op::PUSH0, op::MSTORE, op::PUSH1, 32, op::PUSH0, op::RETURN];
    for forks in FORKS {
        let actual = run_contract(forks, MONAD_MAINNET_CHAIN_ID, CODE);
        if forks == MonadHardfork::MonadEight {
            assert!(!actual.status);
            assert_eq!(actual.stop, InstrStop::NotActivated);
            assert_eq!(actual.total_gas_spent, 100_000);
        } else {
            // Block number 256 has 247 leading zero bits in an EVM word.
            assert_returns(actual, U256::from(247));
        }
    }
}

#[test]
fn factory_enforces_the_requested_transaction_gas_cap() {
    let cap = 50_000;
    let mut factory = MonadEvmBuilder::new(MONAD_MAINNET_CHAIN_ID);
    factory.limits.tx_gas_limit_cap = Some(cap);
    let mut db = evm2_db(Bytes::new());
    let mut evm = factory.create_evm(MonadHardfork::MonadTen, MonadBlockEnv::default(), &mut db);
    let tx = |gas_limit| {
        Recovered::new_unchecked(MonadTxEnvelope::Legacy(legacy_call(gas_limit)), CALLER)
    };
    assert_eq!(evm.validate_tx(&tx(cap)), Ok(()));
    assert_eq!(
        evm.validate_tx(&tx(cap + 1)),
        Err(HandlerError::TxGasLimitGreaterThanCap { gas_limit: cap + 1, cap })
    );
}
