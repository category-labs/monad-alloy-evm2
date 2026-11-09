//! Construction of Monad EVM instances.

use crate::{
    MonadBlockEnv, MonadEvm, MonadEvmExt, MonadHardfork, MonadHost, MonadLimits,
    frame::FramePrecompiles, reserve_balance::ReservePrecompiles, transaction::tx_registry,
};
use alloc::sync::Arc;
use core::sync::atomic::AtomicU8;
use evm2::{ExecutionConfig, evm::DynDatabase};

/// Builds EVMs for one chain using the supplied hardfork, block, and database.
#[derive(Clone, Copy, Debug)]
pub struct MonadEvmFactory {
    /// Chain ID used by transaction validation and the `CHAINID` instruction.
    pub chain_id: u64,
    /// Requested runtime limits, applied for each EVM's active hardfork.
    pub limits: MonadLimits,
}

impl MonadEvmFactory {
    /// Creates a factory with the default Monad limits.
    pub const fn new(chain_id: u64) -> Self {
        Self { chain_id, limits: MonadLimits::new() }
    }

    /// Creates an EVM with the Monad gas table and limits for `spec`.
    ///
    /// Legacy, access-list, dynamic-fee and set-code transactions pay for their full
    /// gas limit at the effective price. The beneficiary receives the priority fee.
    /// [`tx_gas_used()`](evm2::TxResultExt::tx_gas_used) reports the charged limit,
    /// and [`total_gas_spent`](evm2::TxResultExt::total_gas_spent) records execution
    /// consumption. Cryptographic precompiles use Monad pricing and activation.
    /// `CREATE` and `CREATE2` reject delegated execution contexts. Memory expansion
    /// uses MIP-3 pricing from MonadNine; enabled memory limits are shared by active
    /// call frames. Other instructions are inherited from the base fork.
    ///
    /// ```
    /// use alloy_monad_evm2::{MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvmFactory, MonadHardfork};
    /// use evm2::evm::InMemoryDB;
    ///
    /// let factory = MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID);
    /// let evm = factory.create_evm(
    ///     MonadHardfork::MonadTen,
    ///     MonadBlockEnv::default(),
    ///     InMemoryDB::default(),
    /// );
    /// assert_eq!(evm.config_spec_id(), MonadHardfork::MonadTen);
    /// assert_eq!(evm.version().chain_id, MONAD_MAINNET_CHAIN_ID);
    /// ```
    pub fn create_evm<'a>(
        &self,
        spec: MonadHardfork,
        block: MonadBlockEnv,
        db: impl DynDatabase + 'a,
    ) -> MonadEvm<'a> {
        let config =
            ExecutionConfig::for_spec_and_version(spec, self.limits.version(spec, self.chain_id));
        let mut ext = MonadEvmExt::default();
        ext.frame.memory_limit = Some(
            self.limits
                .memory_limit
                .unwrap_or(evm2::Version::new(spec.into_eth_spec()).memory_limit),
        );
        let selection = Arc::new(AtomicU8::new(spec as u8));
        let precompiles = FramePrecompiles::new(selection.clone());
        let host = MonadHost::new_with_execution_config_and_ext(
            config,
            spec,
            block,
            tx_registry(),
            db,
            ReservePrecompiles(precompiles),
            ext,
        );
        MonadEvm::new(host, selection)
    }
}
