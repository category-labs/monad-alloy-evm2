//! Independent engine execution from the same canonical pre-state.

use crate::{fixture::Allocation, parity::Observation, state};
use alloy_consensus::{Header, Transaction, TxEnvelope, transaction::Recovered};
use alloy_primitives::{Address, Bytes, U256};
use evm2::{
    AccountInfo,
    bytecode::Bytecode,
    evm::{InMemoryDB, SystemTx},
};
use eyre::{Result, WrapErr};
use monad_evm2::{
    MonadBlockEnv, MonadChainContext, MonadEvmFactory, MonadHardfork, MonadTxEnvelope,
};
use monad_revm::{
    MonadBuilder, MonadCfgEnv, MonadChainContext as ReferenceChain,
    MonadHardfork as ReferenceHardfork, MonadJournalTr, monad_context_with_db,
};
use revm::{
    DatabaseCommit, ExecuteEvm,
    context::{BlockEnv, TxEnv},
    context_interface::{JournalTr, either::Either, result::ExecutionResult},
    database::InMemoryDB as ReferenceDB,
    handler::system_call::SystemCallEvm,
    state::{AccountInfo as ReferenceAccount, Bytecode as ReferenceBytecode},
};

pub(crate) struct BlockContext<'a> {
    pub(crate) spec: MonadHardfork,
    pub(crate) chain_id: u64,
    pub(crate) header: &'a Header,
    pub(crate) chain: MonadChainContext,
}

/// Rebuild the reference from canonical state before each operation so a known
/// reference difference cannot corrupt the inputs of subsequent comparisons.
pub(crate) fn reference_db(db: &InMemoryDB) -> Result<ReferenceDB> {
    let mut reference = ReferenceDB::default();
    for (address, account) in state::snapshot(db) {
        reference.insert_account_info(
            address,
            ReferenceAccount::default()
                .with_balance(account.balance)
                .with_nonce(account.nonce)
                .with_code(ReferenceBytecode::new_raw_checked(account.code)?),
        );
        for (slot, value) in account.storage {
            reference.insert_account_storage(address, slot, value)?;
        }
    }
    reference.cache.block_hashes.extend(db.cache.block_hashes.iter().map(|(k, v)| (*k, *v)));
    Ok(reference)
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
    let mut evm = MonadEvmFactory::new(context.chain_id).create_evm(context.spec, block, &mut *db);
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

pub(crate) fn reference(
    mut db: ReferenceDB,
    context: &BlockContext<'_>,
    request: &Request<'_>,
) -> Result<(Observation, bool)> {
    let spec = match context.spec {
        MonadHardfork::MonadEight => ReferenceHardfork::MonadEight,
        MonadHardfork::MonadNine => ReferenceHardfork::MonadNine,
        MonadHardfork::MonadTen => ReferenceHardfork::MonadTen,
        MonadHardfork::MonadNext => ReferenceHardfork::MonadNext,
    };
    let header = context.header;
    let mut ctx = monad_context_with_db(&mut db);
    ctx.cfg = MonadCfgEnv::new_with_spec(spec).with_chain_id(context.chain_id);
    ctx.journaled_state.set_monad_spec(spec);
    ctx.journaled_state.set_spec_id(spec.into());
    ctx.block = BlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        gas_limit: header.gas_limit,
        basefee: header.base_fee_per_gas.unwrap_or_default(),
        difficulty: header.difficulty,
        prevrandao: Some(header.mix_hash),
        ..Default::default()
    };
    ctx.chain = ReferenceChain {
        parent_senders_and_authorities: context.chain.parent_senders_and_authorities.clone(),
        grandparent_senders_and_authorities: context
            .chain
            .grandparent_senders_and_authorities
            .clone(),
        current_block_senders: context.chain.current_block_senders.clone(),
        current_block_authorities: context.chain.current_block_authorities.clone(),
        current_tx_index: context.chain.current_tx_index,
        ..Default::default()
    };
    let mut evm = ctx.build_monad();
    let (mut executed, receipt_gas) = match request {
        Request::Transaction { tx, sender } => {
            let tx = *tx;
            let env = TxEnv {
                tx_type: tx.tx_type() as u8,
                caller: *sender,
                gas_limit: tx.gas_limit(),
                gas_price: tx.max_fee_per_gas(),
                kind: tx.kind(),
                value: tx.value(),
                data: tx.input().clone(),
                nonce: tx.nonce(),
                chain_id: tx.chain_id(),
                access_list: tx.access_list().cloned().unwrap_or_default(),
                gas_priority_fee: tx.max_priority_fee_per_gas(),
                authorization_list: tx
                    .authorization_list()
                    .unwrap_or_default()
                    .iter()
                    .cloned()
                    .map(Either::Left)
                    .collect(),
                ..Default::default()
            };
            (evm.transact(env)?, Some(tx.gas_limit()))
        }
        Request::System { target, data } => (
            evm.system_call_with_caller(
                revm::handler::system_call::SYSTEM_ADDRESS,
                *target,
                data.clone(),
            )?,
            None,
        ),
    };
    drop(evm);
    // The reference cache does not itself delete touched empty accounts under EIP-161.
    for account in executed.state.values_mut() {
        if account.is_touched() && account.info.is_empty() {
            account.mark_selfdestruct();
        }
    }
    db.commit(executed.state);
    let result = executed.result;
    let corrected = receipt_gas.is_some_and(|gas| result.tx_gas_used() != gas);
    let status = match result {
        ExecutionResult::Success { .. } => "success",
        ExecutionResult::Revert { .. } => "revert",
        ExecutionResult::Halt { .. } => "halt",
    };
    Ok((
        Observation {
            status,
            gas_spent: result.gas().total_gas_spent(),
            refunded: result.gas().inner_refunded(),
            receipt_gas,
            output: result.output().cloned().unwrap_or_default(),
            logs: result.logs().to_vec(),
            created_address: result.created_address(),
            state_root: state::root(&state::reference_snapshot(&db)),
        },
        corrected,
    ))
}
