//! Optional execution against the published monad-revm 0.8.0 reference.

use crate::{
    engine::{BlockContext, Request},
    fixture::{Account, Allocation},
    parity::Observation,
    state,
};
use alloy_consensus::Transaction;
use alloy_primitives::U256;
use evm2::evm::InMemoryDB;
use eyre::Result;
use monad_evm2::MonadHardfork;
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

/// Rebuild the reference from canonical state before each operation so a known
/// reference difference cannot corrupt the inputs of subsequent comparisons.
pub(crate) fn database(db: &InMemoryDB) -> Result<ReferenceDB> {
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

pub(crate) fn execute(
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
            state_root: state::root(&snapshot(&db)),
        },
        corrected,
    ))
}

pub(crate) fn snapshot(db: &ReferenceDB) -> Allocation {
    db.cache
        .accounts
        .iter()
        .filter_map(|(address, account)| {
            let info = account.info()?;
            let code = db
                .cache
                .contracts
                .get(&info.code_hash)
                .map(ReferenceBytecode::original_bytes)
                .unwrap_or_default();
            let storage = account
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(k, v)| (*k, *v))
                .collect();
            Some((*address, Account { nonce: info.nonce, balance: info.balance, code, storage }))
        })
        .collect()
}
