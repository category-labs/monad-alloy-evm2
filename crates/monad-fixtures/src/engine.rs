//! Independent engine execution from the same canonical pre-state.

use crate::{fixture::Allocation, parity::Observation, state};
use alloy_consensus::{Header, TxEnvelope, transaction::Recovered};
use alloy_primitives::{Address, Bytes, U256};
use evm2::{
    AccountInfo,
    bytecode::Bytecode,
    evm::{InMemoryDB, SystemTx},
};
use eyre::{Result, WrapErr};
use monad_evm2::{
    MonadBlockEnv, MonadChainContext, MonadEvmBuilder, MonadHardfork, MonadTxEnvelope,
};

pub(crate) struct BlockContext<'a> {
    pub(crate) spec: MonadHardfork,
    pub(crate) chain_id: u64,
    pub(crate) header: &'a Header,
    pub(crate) chain: MonadChainContext,
}

pub(crate) fn database(pre: &Allocation) -> Result<InMemoryDB> {
    let mut db = InMemoryDB::default();
    for (address, account) in pre {
        let code = Bytecode::new_raw_checked(account.code.clone())
            .wrap_err_with(|| format!("account {address}: invalid pre-state bytecode"))?;
        db.insert_account_info(
            address,
            AccountInfo::default()
                .with_balance(account.balance)
                .with_nonce(account.nonce)
                .with_code(code),
        );
        for (slot, value) in &account.storage {
            db.insert_account_storage(address, slot, value);
        }
    }
    Ok(db)
}

pub(crate) enum Request<'a> {
    Transaction { tx: &'a TxEnvelope, sender: Address },
    System { target: Address, data: Bytes },
}

pub(crate) fn actual(
    db: &mut InMemoryDB,
    context: &BlockContext<'_>,
    request: &Request<'_>,
) -> Result<Observation> {
    let header = context.header;
    let block = MonadBlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        gas_limit: U256::from(header.gas_limit),
        basefee: U256::from(header.base_fee_per_gas.unwrap_or_default()),
        difficulty: header.difficulty,
        prevrandao: header.mix_hash.into(),
        blob_basefee: U256::ONE,
        ..Default::default()
    };
    let mut evm = MonadEvmBuilder::new(context.chain_id).create_evm(context.spec, block, &mut *db);
    evm.ext_mut().chain = context.chain.clone();
    let executed = match request {
        Request::Transaction { tx, sender } => {
            let envelope = match (*tx).clone() {
                TxEnvelope::Legacy(tx) => MonadTxEnvelope::Legacy(tx.strip_signature()),
                TxEnvelope::Eip2930(tx) => MonadTxEnvelope::Eip2930(tx.strip_signature()),
                TxEnvelope::Eip1559(tx) => MonadTxEnvelope::Eip1559(tx.strip_signature()),
                TxEnvelope::Eip4844(tx) => MonadTxEnvelope::Eip4844(tx.strip_signature()),
                TxEnvelope::Eip7702(tx) => MonadTxEnvelope::from(tx.strip_signature()),
            };
            let envelope = Recovered::new_unchecked(envelope, *sender);
            evm.transact(&envelope)?.detach()
        }
        Request::System { target, data } => {
            evm.system_call(SystemTx::new(*target, data.clone()))?.detach()
        }
    };
    drop(evm);
    db.commit_pending(&executed.pending_state);
    let result = executed.result;
    let status = if result.ext.reverted_for_reserve_balance {
        "reserve_balance_violation"
    } else if result.is_success() {
        "success"
    } else if result.is_revert() {
        "revert"
    } else {
        "halt"
    };
    let receipt_gas = matches!(request, Request::Transaction { .. }).then(|| result.tx_gas_used());
    Ok(Observation {
        status,
        gas_spent: result.total_gas_spent,
        refunded: result.refunded,
        receipt_gas,
        output: result.output,
        logs: result.logs,
        created_address: result.created_address,
        state_root: state::root(&state::snapshot(db)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_primitives::{Signature, hex};

    #[test]
    fn consumption_and_opcode_boundaries_are_independent_of_receipt_charges() {
        let sender = Address::repeat_byte(0x11);
        let target = Address::repeat_byte(0x22);
        let header =
            Header { gas_limit: 1_000_000, base_fee_per_gas: Some(0), ..Default::default() };
        for spec in MonadHardfork::ALL {
            let context = BlockContext {
                spec,
                chain_id: 143,
                header: &header,
                chain: MonadChainContext::default(),
            };
            // PUSH0 + POP costs four gas. REVERT has no additional opcode cost.
            for (code, limit, status, consumed) in [
                ("00", 80_000, "success", 21_000),
                ("5f5000", 21_003, "halt", 21_003),
                ("5f5000", 21_004, "success", 21_004),
                ("5f5000", 80_000, "success", 21_004),
                ("5f5ffd", 80_000, "revert", 21_004),
                ("fe", 80_000, "halt", 80_000),
            ] {
                let mut db = InMemoryDB::default();
                db.insert_account_info(&sender, AccountInfo::default().with_balance(U256::MAX));
                db.insert_account_info(
                    &target,
                    AccountInfo::default()
                        .with_code(Bytecode::new_raw(hex::decode(code).unwrap().into())),
                );
                let tx = TxLegacy { gas_limit: limit, to: target.into(), ..Default::default() };
                let tx =
                    TxEnvelope::from(tx.into_signed(Signature::new(U256::ONE, U256::ONE, false)));
                let result =
                    actual(&mut db, &context, &Request::Transaction { tx: &tx, sender }).unwrap();
                assert_eq!(result.status, status, "{spec:?} {code} {limit}");
                assert_eq!(result.gas_spent, consumed, "{spec:?} {code} {limit}");
                assert_eq!(result.receipt_gas, Some(limit));
                assert_eq!(result.refunded, 0);
                assert!(result.output.is_empty());
                assert!(result.logs.is_empty());
                assert_eq!(db.account_info(&sender).unwrap().nonce, 1);
            }
        }
    }
}
