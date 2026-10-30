//! Monad execution on evm2.
//!
//! [`MonadHardfork`] selects the network rules, and [`monad_version`] builds their
//! gas parameters and runtime limits. [`MonadLimits`] supplies explicit limit overrides.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod cfg;
pub use cfg::{
    MONAD_MAX_CODE_SIZE, MONAD_MAX_INITCODE_SIZE, MONAD_MEMORY_LIMIT, MONAD_TX_GAS_LIMIT_CAP,
    MonadLimits, monad_gas_params, monad_memory_limit, monad_version,
};

pub mod spec;
pub use spec::{
    MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID, MonadHardfork, UnknownMonadHardfork,
};
