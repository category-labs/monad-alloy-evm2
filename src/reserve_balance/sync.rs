//! Adapts evm2's reversible account journal to reserve-balance tracking.

use super::{ReserveAccount, ReserveBalanceInit};
use crate::MonadHost;
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
    originals: HashMap<Address, (U256, B256)>,
}

/// Reads the current account view and the accepted transaction-start baseline.
pub fn reserve_account(
    evm: &mut MonadHost<'_>,
    address: Address,
) -> Result<ReserveAccount, DatabaseError> {
    let (original_balance, original_code_hash) =
        if let Some(original) = evm.ext().reserve_balance_sync.originals.get(&address) {
            *original
        } else {
            let original = evm.overlay_db_mut().get_account(&address)?;
            let fields = (
                original.as_ref().map_or(U256::ZERO, |info| info.balance),
                original.as_ref().map_or(KECCAK256_EMPTY, |info| info.code_hash),
            );
            evm.ext_mut().reserve_balance_sync.originals.insert(address, fields);
            fields
        };
    let mut account = evm.state_mut().account(&address)?;
    let present = account.get().cloned();
    let is_delegated = account.load_code()?.is_eip7702();
    Ok(ReserveAccount {
        balance: present.as_ref().map_or(U256::ZERO, |info| info.balance),
        original_balance,
        code_hash: present.as_ref().map_or(KECCAK256_EMPTY, |info| info.code_hash),
        original_code_hash,
        is_delegated,
        selfdestructed_created_locally: account.is_destructed() && account.is_created(),
    })
}

pub(crate) fn begin_reserve_balance_tx(
    evm: &mut MonadHost<'_>,
    sender: Address,
    gas_price: U256,
    gas_limit: u64,
) -> Result<(), DatabaseError> {
    if evm.ext().preserve_reserve_balance_tracker {
        // Synthetic transactions keep the enclosing sender, fees and original balances.
        return sync_reserve_balance(evm);
    }
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

pub(crate) fn end_reserve_balance_tx(evm: &mut MonadHost<'_>) {
    let ext = evm.ext_mut();
    ext.reserve_execution = Default::default();
    if !ext.preserve_reserve_balance_tracker {
        ext.reserve_balance.clear();
        ext.reserve_balance_sync = Default::default();
        ext.memory_pool_offset = 0;
    } else {
        ext.reserve_balance_sync.base = 0;
    }
}

/// Rebuilds reserves against replacement state and its enclosing chain context.
///
/// Synchronize the old state before replacing it. This function intentionally reads
/// only the replacement state: its journal may be unrelated to the old journal.
/// The enclosing sender and gas allowance remain unchanged.
/// The replacement state's accepted balances become the new baseline; do not rebase
/// between synthetic calls that belong to the same unchanged state.
pub fn rebase_reserve_balance(evm: &mut MonadHost<'_>) -> Result<(), DatabaseError> {
    let saved = evm.ext().clone();
    evm.ext_mut().reserve_balance_sync = Default::default();
    let tracked: Vec<_> = saved.reserve_balance.tracked_addresses().copied().collect();
    let mut views = HashMap::<Address, ReserveAccount>::default();
    for address in tracked {
        let view = evm.state_mut().account_info_untracked(&address).and_then(|info| {
            if info.is_some() { reserve_account(evm, address).map(Some) } else { Ok(None) }
        });
        match view {
            Ok(Some(view)) => {
                views.insert(address, view);
            }
            Ok(None) => {}
            Err(error) => {
                *evm.ext_mut() = saved;
                return Err(error);
            }
        }
    }
    let base = evm.state().journal().len();
    let ext = evm.ext_mut();
    ext.reserve_balance_sync.base = base;
    ext.reserve_balance.rebase(&ext.chain, |address| views.get(&address).copied());
    Ok(())
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
pub fn sync_reserve_balance(evm: &mut MonadHost<'_>) -> Result<(), DatabaseError> {
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
        // Credits may be debited by a later synthetic call after this state is accepted.
        // Keep their enclosing-transaction baseline without making them tracked debits.
        reserve_account(evm, address)?;
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
pub fn dipped_into_reserve(evm: &mut MonadHost<'_>) -> Result<bool, DatabaseError> {
    sync_reserve_balance(evm)?;
    Ok(evm.ext().reserve_balance.has_violation())
}
