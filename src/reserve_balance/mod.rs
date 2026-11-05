//! Account views and transaction inputs for Monad reserve-balance tracking.

use crate::{MonadChainContext, MonadHardfork};
use alloy_primitives::{Address, B256, U256};

/// Default maximum reserve balance in wei: 10 MON.
pub const DEFAULT_RESERVE_BALANCE: U256 = U256::from_limbs([10_000_000_000_000_000_000, 0, 0, 0]);

/// Account fields used by reserve-balance checks, independent of the state backend.
///
/// Original fields describe the account at transaction entry, before fee charging
/// and authorizations. Current fields describe the account at the time of the
/// check. Loading this view must resolve delegation even when bytecode is not
/// already cached by the backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReserveAccount {
    /// Current balance in wei.
    pub balance: U256,
    /// Balance at the start of the transaction, before fees and value transfers.
    pub original_balance: U256,
    /// Current code hash.
    pub code_hash: B256,
    /// Code hash at the start of the transaction.
    pub original_code_hash: B256,
    /// Whether the current code is an EIP-7702 delegation designator.
    pub is_delegated: bool,
    /// Whether the account was both created and self-destructed in this transaction.
    pub selfdestructed_created_locally: bool,
}

impl ReserveAccount {
    /// Reserve at transaction entry, capped by the configured maximum.
    ///
    /// Subsequent credits and debits do not change this baseline.
    pub fn pretx_reserve(&self, max_reserve_balance: U256) -> U256 {
        self.original_balance.min(max_reserve_balance)
    }
}

/// Inputs used to initialize reserve-balance tracking for one transaction.
///
/// The references borrow caller-owned metadata and an optional loaded sender view
/// for the duration of initialization. The sender view reflects fees and EIP-7702
/// authorizations already applied, while retaining its original account fields.
#[derive(Clone, Copy, Debug)]
pub struct ReserveBalanceInit<'a> {
    /// Block history and reserve policy.
    pub chain: &'a MonadChainContext,
    /// Active Monad hardfork.
    pub spec: MonadHardfork,
    /// Transaction sender.
    pub sender: Address,
    /// Effective gas price charged for the transaction, in wei per gas.
    pub effective_gas_price: u128,
    /// Full transaction gas limit used for fee charging.
    pub gas_limit: u64,
    /// Whether the sender is delegated after applying transaction authorizations.
    pub sender_is_delegated: bool,
    /// Optional loaded sender account view.
    pub sender_account: Option<&'a ReserveAccount>,
}

impl ReserveBalanceInit<'_> {
    /// Full-limit transaction fee, widened before multiplication to avoid overflow.
    pub fn gas_fees(&self) -> U256 {
        U256::from(self.effective_gas_price) * U256::from(self.gas_limit)
    }
}
