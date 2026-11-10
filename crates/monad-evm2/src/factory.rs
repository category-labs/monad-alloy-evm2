//! Construction of Monad EVM instances.

use crate::{
    MonadBlockEnv, MonadEvm, MonadEvmExt, MonadEvmTypes, MonadExecution, MonadHardfork, MonadHost,
    MonadLimits, MonadPrecompiles, monad_memory_limit, monad_tx_registry, monad_version,
};
use evm2::{ExecutionConfig, TxResult, Version, evm::DynDatabase, registry::TxRegistry};

/// Stateless factory for native Monad hosts.
///
/// The caller supplies the runtime version and owns inspectors and state lifecycle.
/// Ordinary transactions require [`crate::reserve_balance::reserve_inspector`];
/// trusted protocol replay works directly through the native transaction registry.
/// Use [`MonadEvmBuilder`] for a standalone executor with automatic observer composition.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonadEvmFactory;

impl MonadEvmFactory {
    /// Returns the runtime version for a Monad hardfork and chain ID.
    pub const fn version(&self, spec: MonadHardfork, chain_id: u64) -> Version {
        monad_version(spec, chain_id)
    }

    /// Configures Monad instructions and enforces the hardfork's memory cap.
    pub fn execution_config(
        &self,
        spec: MonadHardfork,
        mut version: Version,
    ) -> ExecutionConfig<MonadEvmTypes> {
        version.memory_limit = monad_memory_limit(spec, version.memory_limit);
        ExecutionConfig::for_spec_and_version(spec, version)
    }

    /// Returns the native transaction handlers for a Monad hardfork.
    pub fn tx_registry(
        &self,
        spec: MonadHardfork,
    ) -> TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>> {
        monad_tx_registry(spec)
    }

    /// Returns the precompiles for a Monad hardfork, including reserve observation.
    pub fn precompiles(&self, spec: MonadHardfork) -> MonadPrecompiles {
        MonadPrecompiles::new(spec)
    }

    /// Creates a native host without a standalone frame selector or lifecycle wrapper.
    ///
    /// Compose the application's inspector with [`crate::reserve_balance::reserve_inspector`]
    /// for each ordinary transaction. A native host follows its configured hardfork;
    /// `message.ext.hardfork` overrides belong to the standalone executor.
    pub fn create_evm<'a>(
        &self,
        spec: MonadHardfork,
        version: Version,
        block: MonadBlockEnv,
        db: impl DynDatabase + 'a,
    ) -> MonadHost<'a> {
        MonadHost::new_with_execution_config_and_ext(
            self.execution_config(spec, version),
            spec,
            block,
            self.tx_registry(spec),
            db,
            self.precompiles(spec),
            MonadEvmExt::default(),
        )
    }
}

/// Builds standalone executors with a chain ID and requested runtime limits.
#[derive(Clone, Copy, Debug)]
pub struct MonadEvmBuilder {
    /// Chain ID used by transaction validation and the `CHAINID` instruction.
    pub chain_id: u64,
    /// Requested runtime limits, applied for each EVM's active hardfork.
    pub limits: MonadLimits,
}

impl MonadEvmBuilder {
    /// Creates a builder with the default Monad limits.
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
    /// uses MIP-3 pricing from MonadNine; memory limits are shared by active
    /// call frames. Other instructions are inherited from the base fork.
    ///
    /// ```
    /// use monad_evm2::{MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvmBuilder, MonadHardfork};
    /// use evm2::evm::InMemoryDB;
    ///
    /// let builder = MonadEvmBuilder::new(MONAD_MAINNET_CHAIN_ID);
    /// let evm = builder.create_evm(
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
        let (host, execution) = self.create_host(spec, block, db);
        MonadEvm::new(host, execution)
    }

    /// Creates a native evm2 host and its execution hooks for an embedding application.
    ///
    /// Keep the pair together. Execute through [`MonadExecution::transact`] with
    /// the application's inspector, or use its documented low-level inspector hook.
    /// Each nested host needs a fresh pair, even when inheriting extension state.
    pub fn create_host<'a>(
        &self,
        spec: MonadHardfork,
        block: MonadBlockEnv,
        db: impl DynDatabase + 'a,
    ) -> (MonadHost<'a>, MonadExecution) {
        let config =
            ExecutionConfig::for_spec_and_version(spec, self.limits.version(spec, self.chain_id));
        let mut ext = MonadEvmExt::default();
        ext.frame.memory_limit = Some(
            self.limits
                .memory_limit
                .unwrap_or(evm2::Version::new(spec.into_eth_spec()).memory_limit),
        );
        let execution = MonadExecution::new(spec);
        let host = MonadHost::new_with_execution_config_and_ext(
            config,
            spec,
            block,
            monad_tx_registry(spec),
            db,
            execution.precompiles(),
            ext,
        );
        (host, execution)
    }
}
