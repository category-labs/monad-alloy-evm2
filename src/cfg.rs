//! Monad gas parameters and runtime limits.

use crate::MonadHardfork;
use evm2::{
    Version,
    version::{GasId, GasParams},
};

/// Maximum transaction gas limit, applied to every Monad hardfork.
pub const MONAD_TX_GAS_LIMIT_CAP: u64 = 30_000_000;

/// MIP-3 memory limit in bytes (8 MiB), pooled across the call stack.
///
/// Selected for MonadNine and later when the `memory_limit` feature is enabled.
pub const MONAD_MEMORY_LIMIT: u64 = 8 * 1024 * 1024;

/// Maximum deployed contract code size in bytes (128 KiB).
pub const MONAD_MAX_CODE_SIZE: usize = 0x20000;

/// Maximum contract creation initcode size in bytes (256 KiB).
pub const MONAD_MAX_INITCODE_SIZE: usize = MONAD_MAX_CODE_SIZE * 2;

/// Cold storage access cost for SLOAD and SSTORE.
pub const COLD_SLOAD_COST: u64 = 8100;

/// Cold account access cost.
pub const COLD_ACCOUNT_ACCESS_COST: u64 = 10100;

/// Warm account and storage read cost.
pub const WARM_STORAGE_READ_COST: u64 = 100;

/// Returns the dynamic gas table for a Monad hardfork.
///
/// Starts with the inherited Ethereum table and replaces the cold access costs.
/// MIP-3 memory pricing and MIP-8 storage pricing require custom instruction handlers;
/// their formulas are not represented by this table.
pub const fn monad_gas_params(spec: MonadHardfork) -> GasParams {
    let mut params = Version::base(spec.into_eth_spec()).gas_params;
    // SSTORE charges the full cold storage cost.
    params.set(GasId::ColdStorageCost, COLD_SLOAD_COST as u32);
    // SLOAD and account accesses already charge the warm cost separately.
    params.set(GasId::ColdStorageAdditionalCost, (COLD_SLOAD_COST - WARM_STORAGE_READ_COST) as u32);
    params.set(
        GasId::ColdAccountAdditionalCost,
        (COLD_ACCOUNT_ACCESS_COST - WARM_STORAGE_READ_COST) as u32,
    );
    params
}

/// Returns the effective memory limit for a hardfork and requested limit.
///
/// With `memory_limit`, MonadNine and later clamp the requested limit to 8 MiB,
/// preserving stricter limits. MonadEight uses the requested limit unchanged.
/// Without the feature, memory is uncapped (`u64::MAX`) for every hardfork.
pub const fn monad_memory_limit(spec: MonadHardfork, configured: u64) -> u64 {
    if !cfg!(feature = "memory_limit") {
        u64::MAX
    } else if MonadHardfork::MonadNine.is_enabled_in(spec) && configured > MONAD_MEMORY_LIMIT {
        MONAD_MEMORY_LIMIT
    } else {
        configured
    }
}

/// Requested runtime limits, before applying hardfork-specific defaults and caps.
///
/// Keeping these values separate from the effective [`Version`] allows a hardfork
/// change to restore a previously clamped limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MonadLimits {
    /// Transaction gas limit override. `None` selects 30 million gas.
    pub tx_gas_limit_cap: Option<u64>,
    /// Deployed code size override in bytes. `None` selects 128 KiB.
    pub max_code_size: Option<usize>,
    /// Initcode size override in bytes.
    ///
    /// `None` selects twice the effective code size, saturating at `usize::MAX`.
    pub max_initcode_size: Option<usize>,
    /// Memory limit override in bytes, before applying the MIP-3 cap.
    ///
    /// `None` selects the engine default (2^32 - 1 bytes). This field is ignored
    /// when the `memory_limit` feature is disabled.
    pub memory_limit: Option<u64>,
}

impl MonadLimits {
    /// Creates limits using Monad defaults.
    pub const fn new() -> Self {
        Self {
            tx_gas_limit_cap: None,
            max_code_size: None,
            max_initcode_size: None,
            memory_limit: None,
        }
    }

    /// Builds runtime settings for a hardfork and chain ID, applying these overrides.
    ///
    /// Retains the inherited engine feature flags, including EIP-7623 and EIP-7702.
    /// These flags are not a complete list of supported EIPs: blob transaction
    /// rejection, precompile selection, and full gas-limit charging are handled
    /// separately. Pair this [`Version`] with Monad's instruction and transaction
    /// handlers for the protocol rules described in [`crate::spec`].
    ///
    /// ```
    /// use alloy_monad_evm2::{MonadHardfork, MonadLimits, MONAD_MAINNET_CHAIN_ID};
    ///
    /// let limits = MonadLimits { max_code_size: Some(100_000), ..Default::default() };
    /// let version = limits.version(MonadHardfork::MonadTen, MONAD_MAINNET_CHAIN_ID);
    /// assert_eq!(version.max_code_size, 100_000);
    /// assert_eq!(version.max_initcode_size, 200_000);
    /// ```
    pub const fn version(self, spec: MonadHardfork, chain_id: u64) -> Version {
        let mut version = Version::new(spec.into_eth_spec());
        version.gas_params = monad_gas_params(spec);
        version.chain_id = chain_id;
        version.tx_gas_limit_cap = match self.tx_gas_limit_cap {
            Some(limit) => limit,
            None => MONAD_TX_GAS_LIMIT_CAP,
        };
        version.max_code_size = match self.max_code_size {
            Some(limit) => limit,
            None => MONAD_MAX_CODE_SIZE,
        };
        version.max_initcode_size = match self.max_initcode_size {
            Some(limit) => limit,
            None => version.max_code_size.saturating_mul(2),
        };
        let configured_memory = match self.memory_limit {
            Some(limit) => limit,
            None => version.memory_limit,
        };
        version.memory_limit = monad_memory_limit(spec, configured_memory);
        version
    }
}

/// Builds the default runtime settings for a Monad hardfork and chain ID.
///
/// Use [`MonadLimits::version`] to override limits. These settings supply gas prices
/// and limits; Monad execution also requires its instruction and transaction handlers.
pub const fn monad_version(spec: MonadHardfork, chain_id: u64) -> Version {
    MonadLimits::new().version(spec, chain_id)
}
