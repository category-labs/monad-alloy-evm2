//! Adapts evm2's reversible account journal to reserve-balance tracking.

use super::{ReserveAccount, ReserveBalanceInit};
use crate::MonadEvm;
use alloc::vec::Vec;
use alloy_primitives::{
    Address, B256, KECCAK256_EMPTY, U256,
    map::{HashMap, HashSet},
};
use evm2::{
    DatabaseError,
    evm::{AccountFlags, DynDatabase, JournalEntry},
};

/// Transaction journal position after fee charging and authorization processing.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReserveBalanceSync {
    base: usize,
}

/// Reads the current account view and the accepted transaction-start baseline.
pub fn reserve_account(
    evm: &mut MonadEvm<'_>,
    address: Address,
) -> Result<ReserveAccount, DatabaseError> {
    let original = evm.overlay_db_mut().get_account(&address)?;
    let mut account = evm.state_mut().account(&address)?;
    let present = account.get().cloned();
    let is_delegated = account.load_code()?.is_eip7702();
    Ok(ReserveAccount {
        balance: present.as_ref().map_or(U256::ZERO, |info| info.balance),
        original_balance: original.as_ref().map_or(U256::ZERO, |info| info.balance),
        code_hash: present.as_ref().map_or(KECCAK256_EMPTY, |info| info.code_hash),
        original_code_hash: original.as_ref().map_or(KECCAK256_EMPTY, |info| info.code_hash),
        is_delegated,
        selfdestructed_created_locally: account.is_destructed() && account.is_created(),
    })
}

pub(crate) fn begin_reserve_balance_tx(
    evm: &mut MonadEvm<'_>,
    sender: Address,
    gas_price: U256,
    gas_limit: u64,
) -> Result<(), DatabaseError> {
    let sender_account = reserve_account(evm, sender)?;
    let spec = evm.config_spec_id();
    let base = evm.state().journal().len();
    let ext = evm.ext_mut();
    ext.reserve_balance.init(ReserveBalanceInit {
        chain: &ext.chain,
        spec,
        sender,
        // Transaction envelope fee fields are u128; the effective price cannot exceed them.
        effective_gas_price: gas_price.to::<u128>(),
        gas_limit,
        sender_is_delegated: sender_account.is_delegated,
        sender_account: Some(&sender_account),
    });
    ext.reserve_balance_sync.base = base;
    Ok(())
}

pub(crate) fn end_reserve_balance_tx(evm: &mut MonadEvm<'_>) {
    evm.ext_mut().reserve_balance.clear();
    evm.ext_mut().reserve_balance_sync.base = 0;
}

/// Account fields at a native journal boundary.
#[derive(Clone, Copy)]
struct Session {
    balance: U256,
    code_hash: B256,
    destroyed: bool,
}

/// Refreshes reserve violations from account changes and the current state.
///
/// Only account changes affect reserves; storage access and transient bookkeeping
/// do not. Previously tracked accounts are always refreshed, including after a
/// rollback whose replacement journal entries happen to equal the old entries.
pub fn sync_reserve_balance(evm: &mut MonadEvm<'_>) -> Result<(), DatabaseError> {
    if !evm.ext().reserve_balance.tracking_enabled() {
        return Ok(());
    }
    let base = evm.ext().reserve_balance_sync.base.min(evm.state().journal().len());
    let addresses: HashSet<_> = evm.state().journal()[base..]
        .iter()
        .filter_map(|entry| {
            if let JournalEntry::AccountChange { address, .. } = entry {
                Some(*address)
            } else {
                None
            }
        })
        .collect();
    let mut sessions = HashMap::<Address, Session>::default();
    for address in addresses {
        let account = evm.state_mut().account(&address)?;
        sessions.insert(
            address,
            Session {
                balance: account.balance(),
                code_hash: account.get().map_or(KECCAK256_EMPTY, |info| info.code_hash),
                destroyed: account.is_destructed(),
            },
        );
    }
    let mut affected: HashSet<_> = evm.ext().reserve_balance.tracked_addresses().copied().collect();
    for entry in evm.state().journal()[base..].iter().rev() {
        let JournalEntry::AccountChange { address, previous, previous_flags } = entry else {
            continue;
        };
        let before = Session {
            balance: previous.as_ref().map_or(U256::ZERO, |info| info.balance),
            code_hash: previous.as_ref().map_or(KECCAK256_EMPTY, |info| info.code_hash),
            destroyed: previous_flags.contains(AccountFlags::DESTROYED),
        };
        let after = sessions.insert(*address, before).expect("journal account has a session");
        if after.balance < before.balance
            || after.code_hash != before.code_hash
            || (!before.destroyed && after.destroyed)
        {
            affected.insert(*address);
        }
    }
    let mut views = Vec::with_capacity(affected.len());
    for address in affected {
        views.push((address, reserve_account(evm, address)?));
    }
    // Recompute thresholds as well as balances: reverted code changes can restore an EOA.
    evm.ext_mut()
        .reserve_balance
        .on_checkpoint_revert(views.iter().map(|(address, view)| (*address, Some(view))));
    Ok(())
}

/// Returns the current reserve violation status after synchronizing account changes.
pub fn dipped_into_reserve(evm: &mut MonadEvm<'_>) -> Result<bool, DatabaseError> {
    sync_reserve_balance(evm)?;
    Ok(evm.ext().reserve_balance.has_violation())
}
