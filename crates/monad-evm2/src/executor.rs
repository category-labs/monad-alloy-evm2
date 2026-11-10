//! Transaction and inspector lifecycle around the native evm2 host.

use crate::{
    MonadBlockEnv, MonadEvmTypes, MonadExecutedTx, MonadExecution, MonadHardfork, MonadHost,
    MonadTxEnvelope,
};
use alloc::boxed::Box;
use alloy_consensus::transaction::Recovered;
use core::{
    fmt,
    ops::{Deref, DerefMut},
};
use evm2::{
    ExecutionConfig, TxResult,
    evm::SystemTx,
    inspector::Inspector,
    precompile::PrecompileProvider,
    registry::{HandlerResult, TxRegistry},
};

/// Monad transaction executor with automatic reserve enforcement.
///
/// Dereferences to [`MonadHost`] for state, environment and instruction APIs.
/// Execute transactions through this wrapper so inspector composition and
/// synthetic-call discard handling remain active.
pub struct MonadEvm<'a> {
    host: MonadHost<'a>,
    inspector: Option<Box<dyn Inspector<MonadEvmTypes> + 'a>>,
    execution: MonadExecution,
}

impl fmt::Debug for MonadEvm<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonadEvm").field("host", &self.host).finish_non_exhaustive()
    }
}

impl<'a> Deref for MonadEvm<'a> {
    type Target = MonadHost<'a>;
    fn deref(&self) -> &Self::Target {
        &self.host
    }
}

impl DerefMut for MonadEvm<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.host
    }
}

impl<'a> MonadEvm<'a> {
    pub(crate) const fn new(host: MonadHost<'a>, execution: MonadExecution) -> Self {
        Self { host, inspector: None, execution }
    }

    /// Executes a transaction with automatic reserves, composing the installed inspector.
    pub fn transact(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        let inspector =
            self.inspector.as_mut().map(|inner| &mut **inner as &mut dyn Inspector<MonadEvmTypes>);
        self.execution.transact(&mut self.host, tx, inspector)
    }

    /// Executes with a borrowed inspector, retaining Monad's internal reserve observer.
    pub fn transact_with_inspector(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
        inspector: &mut dyn Inspector<MonadEvmTypes>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        self.execution.transact(&mut self.host, tx, Some(inspector))
    }

    /// Executes and commits each transaction with Monad lifecycle handling.
    pub fn transact_iter<'txs, I>(
        &'txs mut self,
        txs: I,
    ) -> impl Iterator<Item = HandlerResult<TxResult<MonadEvmTypes>>> + 'txs
    where
        I: IntoIterator<Item = &'txs Recovered<MonadTxEnvelope>>,
        I::IntoIter: 'txs,
        Self: 'txs,
    {
        txs.into_iter().map(move |tx| self.transact(tx).map(MonadExecutedTx::commit))
    }

    /// Simulates a transaction and restores its state and preserved reserve context.
    pub fn call_tx(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
    ) -> HandlerResult<TxResult<MonadEvmTypes>> {
        self.transact(tx).map(MonadExecutedTx::discard)
    }

    /// Validates admission without retaining state or changing a preserved tracker.
    pub fn validate_tx(&mut self, tx: &Recovered<MonadTxEnvelope>) -> HandlerResult<()> {
        self.execution.validate_tx(&mut self.host, tx)
    }

    /// Executes a protocol system call with native system gas and nonce rules.
    ///
    /// Uses the installed inspector. System calls bypass ordinary transaction
    /// validation, fees and reserve enforcement. Monad bookkeeping is isolated
    /// from any preserved synthetic-call context and restored before returning.
    /// The execution registry must retain the Monad legacy transaction handler.
    pub fn system_call(&mut self, tx: SystemTx) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        let inspector =
            self.inspector.as_mut().map(|inner| &mut **inner as &mut dyn Inspector<MonadEvmTypes>);
        self.execution.system_call(&mut self.host, tx, inspector)
    }

    /// Executes a protocol system call with an inspector borrowed for this call.
    ///
    /// The borrowed inspector takes precedence over the installed inspector,
    /// which is retained for subsequent calls, including after errors or unwinding.
    /// The inspector borrow ends before the returned state handle is resolved.
    pub fn system_call_with_inspector(
        &mut self,
        tx: SystemTx,
        inspector: &mut dyn Inspector<MonadEvmTypes>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        self.execution.system_call(&mut self.host, tx, Some(inspector))
    }

    /// Changes execution configuration while retaining root precompile observation.
    ///
    /// The version's memory limit becomes the requested limit for subsequent
    /// frames. The supplied precompile provider applies to every frame; call
    /// [`Self::reset_precompiles`] to install the default fork-aware provider.
    pub fn set_execution_config(
        &mut self,
        config: ExecutionConfig<MonadEvmTypes>,
        spec: MonadHardfork,
        registry: TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        self.execution.set_execution_config(&mut self.host, config, spec, registry, precompiles);
    }

    /// Changes block and execution configuration while retaining reserve observation.
    ///
    /// Memory limits and precompiles follow [`Self::set_execution_config`].
    pub fn set_block_and_execution_config(
        &mut self,
        block: MonadBlockEnv,
        config: ExecutionConfig<MonadEvmTypes>,
        spec: MonadHardfork,
        registry: TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        self.execution.set_block_and_execution_config(
            &mut self.host,
            block,
            config,
            spec,
            registry,
            precompiles,
        );
    }

    /// Changes block and runtime settings within the configured fork.
    ///
    /// Retains the installed providers and updates the requested frame memory limit.
    pub fn set_block_and_execution_config_within_spec(
        &mut self,
        block: MonadBlockEnv,
        config: ExecutionConfig<MonadEvmTypes>,
    ) {
        self.execution.set_block_and_execution_config_within_spec(&mut self.host, block, config);
    }

    /// Installs a user inspector alongside automatic reserve enforcement.
    pub fn set_inspector(&mut self, inspector: impl Inspector<MonadEvmTypes> + 'a) {
        self.set_boxed_inspector(Box::new(inspector));
    }

    /// Installs a boxed user inspector alongside automatic reserve enforcement.
    pub fn set_boxed_inspector(&mut self, inspector: Box<dyn Inspector<MonadEvmTypes> + 'a>) {
        self.inspector = Some(inspector);
    }

    /// Returns the installed user inspector.
    pub fn inspector(&self) -> Option<&(dyn Inspector<MonadEvmTypes> + 'a)> {
        self.inspector.as_deref()
    }

    /// Returns the installed user inspector mutably.
    pub fn inspector_mut(&mut self) -> Option<&mut (dyn Inspector<MonadEvmTypes> + 'a)> {
        self.inspector.as_deref_mut()
    }

    /// Removes the user inspector while retaining reserve enforcement.
    pub fn clear_inspector(&mut self) -> Option<Box<dyn Inspector<MonadEvmTypes> + 'a>> {
        self.inspector.take()
    }

    /// Replaces precompiles while preserving observation of root precompile failures.
    ///
    /// The supplied provider is used for every frame. Custom providers are
    /// responsible for their own hardfork selection.
    pub fn set_precompiles(&mut self, precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a) {
        self.execution.set_precompiles(&mut self.host, precompiles);
    }

    /// Replaces boxed precompiles while preserving reserve observation.
    pub fn set_boxed_precompiles(
        &mut self,
        precompiles: Box<dyn PrecompileProvider<MonadEvmTypes> + 'a>,
    ) {
        self.set_precompiles(precompiles);
    }

    /// Installs Monad's default precompiles with automatic per-frame hardfork selection.
    pub fn reset_precompiles(&mut self) {
        self.execution.reset_precompiles(&mut self.host);
    }
}
