//! Valid-block execution commitments and optional reference observations.

use crate::{
    engine::{self, BlockContext, Request},
    fixture::Fixture,
    parity::ParityReport,
    state,
};
use alloy_consensus::{
    EMPTY_OMMER_ROOT_HASH, Receipt, ReceiptEnvelope, Transaction, TxType,
    proofs::{calculate_receipt_root, calculate_transaction_root, calculate_withdrawals_root},
    transaction::SignerRecoverable,
};
use alloy_primitives::{Address, B256, Bloom, Bytes, U256, address, b256, map::HashSet};
use eyre::{Result, WrapErr, ensure, eyre};
use monad_evm2::MonadChainContext;

const BEACON_ROOTS: Address = address!("000f3df6d732807ef1319fb7b8bb8522d0beac02");
const HISTORY_STORAGE: Address = address!("0000f90827f1c53a10cb7a02335b175320002935");
const EMPTY_REQUESTS_HASH: B256 =
    b256!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");

pub(crate) struct Stats {
    pub(crate) blocks: usize,
    pub(crate) transactions: usize,
}

pub(crate) fn execute(fixture: &Fixture, _parity: &mut ParityReport) -> Result<Stats> {
    let mut db = engine::database(&fixture.pre)?;
    ensure!(
        state::root(&fixture.pre) == fixture.genesis.state_root,
        "genesis state root differs from pre allocation"
    );
    let mut parent = &fixture.genesis;
    let mut parent_participants = HashSet::default();
    let mut grandparent_participants = HashSet::default();
    db.insert_block_hash(&U256::from(parent.number), &parent.hash_slow());
    let mut stats = Stats { blocks: 0, transactions: 0 };

    for (index, block) in fixture.blocks.iter().enumerate() {
        let header = &block.header;
        ensure!(
            parent.number.checked_add(1) == Some(header.number),
            "block {index}: nonconsecutive block number"
        );
        ensure!(header.parent_hash == parent.hash_slow(), "block {index}: parent hash mismatch");
        ensure!(header.timestamp >= parent.timestamp, "block {index}: timestamp precedes parent");
        ensure!(
            header.difficulty.is_zero() && header.nonce.is_zero(),
            "block {index}: nonzero proof-of-work fields"
        );
        ensure!(header.ommers_hash == EMPTY_OMMER_ROOT_HASH, "block {index}: nonempty ommers hash");
        ensure!(header.extra_data.len() <= 32, "block {index}: extraData exceeds 32 bytes");
        ensure!(
            header.blob_gas_used.unwrap_or_default() == 0,
            "block {index}: nonzero blob gas used"
        );
        if let Some(hash) = header.requests_hash {
            ensure!(hash == EMPTY_REQUESTS_HASH, "block {index}: expected an empty requests hash");
        }
        let txs = &block.body.transactions;
        ensure!(
            calculate_transaction_root(txs) == header.transactions_root,
            "block {index}: transactions root mismatch"
        );
        let withdrawals = block.body.withdrawals.as_ref().map_or(&[][..], |w| w.as_slice());
        if let Some(expected) = header.withdrawals_root {
            ensure!(
                calculate_withdrawals_root(withdrawals) == expected,
                "block {index}: withdrawals root mismatch"
            );
        } else {
            ensure!(withdrawals.is_empty(), "block {index}: withdrawals without a header root");
        }

        let senders = txs
            .iter()
            .map(|tx| tx.recover_signer().wrap_err("invalid transaction signature"))
            .collect::<Result<Vec<_>>>()?;
        let authorities = txs
            .iter()
            .map(|tx| {
                tx.authorization_list()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|auth| auth.recover_authority().ok())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let participants =
            senders.iter().copied().chain(authorities.iter().flatten().copied()).collect();
        let spec = fixture.network.at(header.timestamp);

        ensure!(header.base_fee_per_gas.is_some(), "block {index}: missing base fee");
        let mut context = BlockContext {
            spec,
            chain_id: fixture.chain_id,
            header,
            chain: MonadChainContext {
                parent_senders_and_authorities: parent_participants,
                grandparent_senders_and_authorities: grandparent_participants,
                current_block_senders: senders.clone(),
                current_block_authorities: authorities,
                ..Default::default()
            },
        };
        for (target, data) in [
            (BEACON_ROOTS, header.parent_beacon_block_root),
            (HISTORY_STORAGE, Some(header.parent_hash)),
        ] {
            let has_code =
                db.account_info(&target).is_some_and(|account| !account.is_empty_code_hash());
            if let Some(data) = data.filter(|_| has_code) {
                let at = format!("block {index}, system call {target}");
                let request =
                    Request::System { target, data: Bytes::copy_from_slice(data.as_slice()) };
                #[cfg(feature = "compare-monad-revm")]
                let reference_db = crate::reference::database(&db)?;
                let _actual =
                    engine::actual(&mut db, &context, &request).wrap_err_with(|| at.clone())?;
                #[cfg(feature = "compare-monad-revm")]
                _parity.record(
                    at,
                    _actual,
                    crate::reference::execute(reference_db, &context, &request),
                );
            }
        }
        let mut cumulative_gas_used = 0u64;
        let mut receipts = Vec::with_capacity(txs.len());
        let mut outcomes = Vec::with_capacity(txs.len());
        let mut bloom = Bloom::ZERO;
        for (tx_index, (tx, sender)) in txs.iter().zip(senders).enumerate() {
            ensure!(
                tx.tx_type() != TxType::Eip4844,
                "block {index}, transaction {tx_index}: blob transactions are invalid on Monad"
            );
            ensure!(
                tx.gas_limit() <= header.gas_limit - cumulative_gas_used,
                "block {index}: transactions exceed block gas limit"
            );
            context.chain.current_tx_index = tx_index;
            let at = format!("block {index}, transaction {tx_index}");
            let request = Request::Transaction { tx, sender };
            #[cfg(feature = "compare-monad-revm")]
            let reference_db = crate::reference::database(&db)?;
            let actual = engine::actual(&mut db, &context, &request)
                .wrap_err_with(|| format!("{at}: unexpected transaction rejection"))?;
            ensure!(
                actual.receipt_gas == Some(tx.gas_limit()),
                "{at}: receipt must charge the full gas limit"
            );
            cumulative_gas_used = cumulative_gas_used
                .checked_add(tx.gas_limit())
                .ok_or_else(|| eyre!("block {index}: cumulative gas overflow"))?;
            ensure!(
                cumulative_gas_used <= header.gas_limit,
                "block {index}: transactions exceed block gas limit"
            );
            outcomes.push(format!("{tx_index}: {}", actual.status));
            let receipt = Receipt {
                status: (actual.status == "success").into(),
                cumulative_gas_used,
                logs: actual.logs.clone(),
            }
            .with_bloom();
            bloom |= receipt.logs_bloom;
            receipts.push(ReceiptEnvelope::from_typed(tx.tx_type(), receipt));
            #[cfg(feature = "compare-monad-revm")]
            _parity.record(at, actual, crate::reference::execute(reference_db, &context, &request));
            stats.transactions += 1;
        }
        grandparent_participants = context.chain.parent_senders_and_authorities;
        parent_participants = participants;

        for withdrawal in withdrawals {
            if withdrawal.amount == 0 {
                continue;
            }
            let mut info = db.account_info(&withdrawal.address).cloned().unwrap_or_default();
            let amount = U256::from(withdrawal.amount) * U256::from(1_000_000_000u64);
            info.balance = info
                .balance
                .checked_add(amount)
                .ok_or_else(|| eyre!("withdrawal balance overflow"))?;
            db.insert_account_info(&withdrawal.address, info);
        }
        ensure!(
            cumulative_gas_used == header.gas_used,
            "block {index}: gas used mismatch: expected {}, got {cumulative_gas_used}",
            header.gas_used
        );
        ensure!(
            bloom == header.logs_bloom,
            "block {index}: logs bloom mismatch; transactions [{}]",
            outcomes.join(", ")
        );
        let receipt_root = calculate_receipt_root(&receipts);
        ensure!(
            receipt_root == header.receipts_root,
            "block {index}: receipts root mismatch: expected {}, got {receipt_root}; transactions [{}]",
            header.receipts_root,
            outcomes.join(", ")
        );
        let actual_root = state::root(&state::snapshot(&db));
        ensure!(
            actual_root == header.state_root,
            "block {index}: state root mismatch: expected {}, got {actual_root}",
            header.state_root
        );
        db.insert_block_hash(&U256::from(header.number), &header.hash_slow());
        parent = header;
        stats.blocks += 1;
    }
    ensure!(parent.hash_slow() == fixture.last_block_hash, "lastblockhash mismatch");
    let actual = state::snapshot(&db);
    if let Some(expected) = &fixture.post {
        state::compare(&actual, expected)?;
    }
    if let Some(expected) = fixture.post_hash {
        ensure!(state::root(&actual) == expected, "postStateHash mismatch");
    }
    Ok(stats)
}
