//! Construction of Monad EVM instances.

use crate::{MonadBlockEnv, MonadEvm, MonadEvmTypes, MonadHardfork, MonadLimits, MonadTxEnvelope};
use evm2::{
    ExecutionConfig, Precompiles,
    ethereum::legacy,
    evm::DynDatabase,
    handler::DefaultTxHandlerHooks,
    registry::{TxRegistry, handler},
};

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
    /// The factory accepts legacy transactions using evm2's Ethereum validation and
    /// fee settlement. Instructions and precompiles are inherited from the base fork.
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
        let registry = TxRegistry::new().with_handler(
            0,
            MonadTxEnvelope::as_legacy,
            handler(
                legacy::prepare_with_hooks::<MonadEvmTypes, DefaultTxHandlerHooks>,
                legacy::execute_prepared::<MonadEvmTypes, DefaultTxHandlerHooks>,
            ),
        );
        MonadEvm::new_with_execution_config(
            config,
            spec,
            block,
            registry,
            db,
            Precompiles::base(spec.into_eth_spec()),
        )
    }
}
