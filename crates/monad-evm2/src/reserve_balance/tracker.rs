//! Reserve-balance state transitions and transaction tracking.

use super::{ReserveAccount, ReserveBalanceInit};
use crate::{MonadChainContext, MonadHardfork, staking::STAKING_ADDRESS};
use alloy_primitives::{
    Address, KECCAK256_EMPTY, U256,
    map::{HashMap, HashSet},
};

/// Cached reserve-balance state for the current transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReserveBalanceTracker {
    tracking_enabled: bool,
    chain: MonadChainContext,
    use_recent_code_hash: bool,
    sender: Address,
    sender_gas_fees: U256,
    sender_is_delegated: bool,
    sender_can_dip: bool,
    allow_init_selfdestruct_exemption: bool,
    violation_thresholds: HashMap<Address, Option<U256>>,
    failed: HashSet<Address>,
}

impl ReserveBalanceTracker {
    /// Returns true if tracking is enabled.
    pub const fn tracking_enabled(&self) -> bool {
        self.tracking_enabled
    }

    /// Returns true if the transaction is currently violating reserve balance.
    pub fn has_violation(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Returns the addresses currently violating reserve balance.
    pub fn violations(&self) -> impl Iterator<Item = &Address> {
        self.failed.iter()
    }

    /// Returns the addresses tracked by this transaction, whether violating or not.
    pub fn tracked_addresses(&self) -> impl Iterator<Item = &Address> {
        self.violation_thresholds.keys()
    }

    /// Clears all cached state.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Initializes the tracker for a new transaction.
    pub fn init(&mut self, init: ReserveBalanceInit<'_>) {
        self.clear();
        self.tracking_enabled = true;
        self.chain = init.chain.clone();
        self.use_recent_code_hash = MonadHardfork::MonadEight.is_enabled_in(init.spec);
        self.sender = init.sender;
        self.sender_is_delegated = init.sender_is_delegated;
        self.allow_init_selfdestruct_exemption = MonadHardfork::MonadNine.is_enabled_in(init.spec);
        self.sender_gas_fees = init.gas_fees();
        self.sender_can_dip = init.chain.sender_can_dip(self.sender, init.sender_is_delegated);
        self.update_loaded_account(init.sender_account, self.sender);
    }

    /// Rebases tracked accounts onto replacement state and chain metadata.
    ///
    /// This preserves the enclosing transaction's sender and gas invariants while discarding
    /// cached thresholds derived from the previous state. Accounts that were merely loaded but
    /// never affected by reserve tracking remain untracked. `account` returns the replacement
    /// view of a tracked address, or `None` when it is absent from the replacement state.
    pub fn rebase(
        &mut self,
        chain: &MonadChainContext,
        mut account: impl FnMut(Address) -> Option<ReserveAccount>,
    ) {
        if !self.tracking_enabled {
            return;
        }

        let tracked = core::mem::take(&mut self.violation_thresholds);
        self.chain = chain.clone();
        self.sender_can_dip = chain.sender_can_dip(self.sender, self.sender_is_delegated);
        self.failed.clear();

        for address in tracked.into_keys() {
            let view = account(address);
            self.update_loaded_account(view.as_ref(), address);
        }
    }

    /// Recomputes the violation status of an address after a debit.
    pub fn on_debit(&mut self, account: Option<&ReserveAccount>, address: Address) {
        self.update_loaded_account(account, address);
    }

    /// Recomputes the violation status of an address after a credit if it was failing.
    pub fn on_credit(&mut self, account: Option<&ReserveAccount>, address: Address) {
        if self.failed.contains(&address) {
            self.update_loaded_account(account, address);
        }
    }

    /// Recomputes the violation status of an address after code changes.
    ///
    /// `is_smart_contract_code` is true for non-empty code that is not an EIP-7702 delegation.
    pub fn on_set_code(
        &mut self,
        account: Option<&ReserveAccount>,
        address: Address,
        is_smart_contract_code: bool,
    ) {
        if !self.tracking_enabled || !self.use_recent_code_hash {
            return;
        }

        if is_smart_contract_code {
            self.violation_thresholds.insert(address, Some(U256::ZERO));
            self.failed.remove(&address);
            return;
        }

        self.violation_thresholds.remove(&address);
        self.update_loaded_account(account, address);
    }

    /// Recomputes violation status for reverted addresses.
    pub fn on_checkpoint_revert<'a, I>(&mut self, reverted: I)
    where
        I: IntoIterator<Item = (Address, Option<&'a ReserveAccount>)>,
    {
        if !self.tracking_enabled {
            return;
        }

        for (address, account) in reverted {
            self.violation_thresholds.remove(&address);
            self.update_loaded_account(account, address);
        }
    }

    fn update_loaded_account(&mut self, account: Option<&ReserveAccount>, address: Address) {
        if !self.tracking_enabled {
            return;
        }

        let Some(account) = account else {
            self.failed.remove(&address);
            self.violation_thresholds.remove(&address);
            return;
        };

        if self.allow_init_selfdestruct_exemption && account.selfdestructed_created_locally {
            self.failed.remove(&address);
            self.violation_thresholds.insert(address, Some(U256::ZERO));
            return;
        }

        let threshold = match self.violation_thresholds.get(&address).copied() {
            Some(threshold) => threshold,
            None => {
                let threshold = self.compute_violation_threshold(account, address);
                self.violation_thresholds.insert(address, threshold);
                threshold
            }
        };

        let Some(threshold) = threshold else {
            self.failed.insert(address);
            return;
        };

        if threshold.is_zero() || account.balance >= threshold {
            self.failed.remove(&address);
        } else {
            self.failed.insert(address);
        }
    }

    fn pretx_reserve(&self, address: Address, account: &ReserveAccount) -> U256 {
        account.pretx_reserve(self.chain.max_reserve_balance(address))
    }

    fn compute_violation_threshold(
        &self,
        account: &ReserveAccount,
        address: Address,
    ) -> Option<U256> {
        if !self.is_subject_account(account, address) {
            return Some(U256::ZERO);
        }

        let mut reserve = self.pretx_reserve(address, account);
        if address == self.sender {
            if self.sender_can_dip {
                return Some(U256::ZERO);
            }
            reserve = reserve.checked_sub(self.sender_gas_fees)?;
        }
        Some(reserve)
    }

    fn is_subject_account(&self, account: &ReserveAccount, address: Address) -> bool {
        if address == STAKING_ADDRESS {
            return false;
        }

        let effective_code_hash =
            if self.use_recent_code_hash { account.code_hash } else { account.original_code_hash };
        if effective_code_hash.is_zero() || effective_code_hash == KECCAK256_EMPTY {
            return true;
        }

        account.is_delegated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn account(original: u64, balance: u64) -> ReserveAccount {
        ReserveAccount {
            original_balance: U256::from(original),
            balance: U256::from(balance),
            ..Default::default()
        }
    }

    #[test]
    fn rebase_updates_sender_eligibility_in_both_directions() {
        let sender = Address::with_last_byte(1);
        let account = account(12, 9);
        let fresh_chain = MonadChainContext::default();
        let restricted_chain = MonadChainContext {
            parent_senders_and_authorities: [sender].into_iter().collect(),
            ..Default::default()
        };
        let mut tracker = ReserveBalanceTracker::default();
        tracker.init(ReserveBalanceInit {
            chain: &fresh_chain,
            spec: MonadHardfork::MonadNine,
            sender,
            effective_gas_price: 0,
            gas_limit: 0,
            sender_is_delegated: false,
            sender_account: Some(&account),
        });
        assert!(!tracker.has_violation());
        tracker.rebase(&restricted_chain, |_| Some(account));
        assert!(tracker.has_violation());
        tracker.rebase(&fresh_chain, |_| Some(account));
        assert!(!tracker.has_violation());
    }

    #[test]
    fn rebase_drops_accounts_absent_from_replacement_state() {
        let sender = Address::with_last_byte(1);
        let tracked = Address::with_last_byte(2);
        let sender_account = account(12, 12);
        let chain = MonadChainContext {
            parent_senders_and_authorities: [sender].into_iter().collect(),
            ..Default::default()
        };
        let mut tracker = ReserveBalanceTracker::default();
        tracker.init(ReserveBalanceInit {
            chain: &chain,
            spec: MonadHardfork::MonadNine,
            sender,
            effective_gas_price: 0,
            gas_limit: 0,
            sender_is_delegated: false,
            sender_account: Some(&sender_account),
        });
        tracker.on_debit(Some(&account(12, 9)), tracked);
        assert!(tracker.has_violation());
        tracker.rebase(&chain, |address| (address == sender).then_some(sender_account));
        assert!(!tracker.has_violation());
    }

    #[test]
    fn credits_and_reverts_restore_reserve_thresholds() {
        let sender = Address::with_last_byte(1);
        let other = Address::with_last_byte(2);
        let chain = MonadChainContext { max_reserve_balance: U256::from(10), ..Default::default() };
        for spec in MonadHardfork::ALL {
            let mut tracker = ReserveBalanceTracker::default();
            tracker.init(ReserveBalanceInit {
                chain: &chain,
                spec,
                sender,
                effective_gas_price: 0,
                gas_limit: 0,
                sender_is_delegated: false,
                sender_account: Some(&account(100, 100)),
            });
            assert!(!tracker.has_violation());
            tracker.on_debit(Some(&account(12, 5)), other);
            assert!(tracker.has_violation());
            tracker.on_credit(Some(&account(12, 9)), other);
            assert!(tracker.has_violation());
            tracker.on_credit(Some(&account(12, 10)), other);
            assert!(!tracker.has_violation());
            tracker.on_debit(Some(&account(12, 1)), other);
            assert!(tracker.has_violation());
            tracker.on_checkpoint_revert([(other, Some(&account(12, 12)))]);
            assert!(!tracker.has_violation());
            tracker.on_debit(Some(&account(12, 0)), STAKING_ADDRESS);
            assert!(!tracker.has_violation());
            let contract = ReserveAccount {
                code_hash: alloy_primitives::B256::repeat_byte(1),
                ..account(12, 0)
            };
            tracker.on_set_code(Some(&contract), other, true);
            assert!(!tracker.has_violation());
            let delegated = ReserveAccount { is_delegated: true, ..contract };
            tracker.on_set_code(Some(&delegated), other, false);
            assert!(tracker.has_violation());
        }
    }
}
