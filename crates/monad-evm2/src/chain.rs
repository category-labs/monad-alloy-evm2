//! Block history and reserve policy used by Monad reserve-balance checks.

use crate::reserve_balance::DEFAULT_RESERVE_BALANCE;
use alloc::vec::Vec;
use alloy_primitives::{Address, U256, map::HashSet};

/// Default maximum reserve balance, expressed in wei: 10 MON.
pub const fn default_max_reserve_balance() -> U256 {
    DEFAULT_RESERVE_BALANCE
}

/// Block metadata needed to decide whether a sender may spend its reserve.
///
/// Current-block vectors are indexed by transaction position. They may contain
/// the whole block; entries after [`Self::current_tx_index`] do not affect the
/// decision. Authority lists contain recovered EIP-7702 authorities.
///
/// The default has empty history. Canonical replay requires the caller to supply
/// the parent, grandparent and current-block metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonadChainContext {
    /// Senders and authorities from the parent block.
    pub parent_senders_and_authorities: HashSet<Address>,
    /// Senders and authorities from the grandparent block.
    pub grandparent_senders_and_authorities: HashSet<Address>,
    /// Current-block senders in transaction order.
    pub current_block_senders: Vec<Address>,
    /// Current-block authority lists in transaction order, including empty lists.
    pub current_block_authorities: Vec<Vec<Address>>,
    /// Position of the transaction being executed in the current block.
    pub current_tx_index: usize,
    /// Maximum reserve balance in wei, applied to every account.
    pub max_reserve_balance: U256,
}

impl Default for MonadChainContext {
    fn default() -> Self {
        Self {
            parent_senders_and_authorities: HashSet::default(),
            grandparent_senders_and_authorities: HashSet::default(),
            current_block_senders: Vec::new(),
            current_block_authorities: Vec::new(),
            current_tx_index: 0,
            max_reserve_balance: default_max_reserve_balance(),
        }
    }
}

impl MonadChainContext {
    /// Returns the reserve cap for `address`; the current policy uses a uniform cap.
    pub const fn max_reserve_balance(&self, _address: Address) -> U256 {
        self.max_reserve_balance
    }

    /// Whether the sender is eligible to spend its reserve for this transaction.
    ///
    /// Delegated senders and addresses present in either preceding block are
    /// ineligible. Within the current block, earlier sender entries and authority
    /// entries up to and including the current transaction make it ineligible.
    pub fn sender_can_dip(&self, sender: Address, sender_is_delegated: bool) -> bool {
        if sender_is_delegated
            || self.parent_senders_and_authorities.contains(&sender)
            || self.grandparent_senders_and_authorities.contains(&sender)
        {
            return false;
        }

        let previously_sent =
            self.current_block_senders.iter().take(self.current_tx_index).any(|s| *s == sender);
        let authorized = self
            .current_block_authorities
            .iter()
            .take(self.current_tx_index.saturating_add(1))
            .any(|authorities| authorities.contains(&sender));
        !previously_sent && !authorized
    }
}
