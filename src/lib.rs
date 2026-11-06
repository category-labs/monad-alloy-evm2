//! Monad execution on evm2.
//!
//! [`MonadEvmFactory`] builds an EVM for a [`MonadHardfork`], block, and database.
//! [`monad_version`] supplies the fork's gas parameters and runtime limits, and
//! [`MonadLimits`] allows callers to override those limits.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod cfg;
pub use cfg::{
    MONAD_MAX_CODE_SIZE, MONAD_MAX_INITCODE_SIZE, MONAD_MEMORY_LIMIT, MONAD_TX_GAS_LIMIT_CAP,
    MonadLimits, monad_gas_params, monad_memory_limit, monad_version,
};

pub mod chain;
pub use chain::{MonadChainContext, default_max_reserve_balance};

mod evm;
pub use evm::{
    MonadBlockEnv, MonadEvm, MonadEvmConfig, MonadEvmExt, MonadEvmTypes, MonadTxEnvelope,
    MonadTxResultExt,
};

mod factory;
pub use factory::MonadEvmFactory;

mod instructions;

mod memory;
pub use memory::monad_memory_cost;

pub mod page;

pub mod precompiles;
pub use precompiles::monad_precompiles;

pub mod reserve_balance;
pub use reserve_balance::{ReserveAccount, ReserveBalanceInit};

pub mod staking;

pub mod spec;
pub use spec::{
    MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID, MonadHardfork, UnknownMonadHardfork,
};

mod transaction;
pub use transaction::{SYSTEM_ADDRESS, SystemSenderIsAuthority};
