//! Transaction and inspector lifecycle around the native evm2 host.

use crate::reserve_balance::{ReserveInspector, ReservePrecompiles, end_reserve_balance_tx};
use crate::{MonadBlockEnv, MonadEvmExt, MonadEvmTypes, MonadHardfork, MonadHost, MonadTxEnvelope};
use alloc::boxed::Box;
use alloy_consensus::transaction::Recovered;
use core::{
    fmt,
    ops::{Deref, DerefMut},
};
use evm2::{
    ExecutionConfig, TxResult,
    evm::{
        AccountChanges, BlockStateAccumulator, ExecutedTx, StateChangeSink, StateChangeSource,
        SystemTx, TxResultWithState, TxStateChanges,
    },
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
    pub(crate) const fn new(host: MonadHost<'a>) -> Self {
        Self { host, inspector: None }
    }

    /// Executes a transaction with automatic reserves, composing the installed inspector.
    pub fn transact(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        let inspector =
            self.inspector.as_mut().map(|inner| &mut **inner as &mut dyn Inspector<MonadEvmTypes>);
        transact(&mut self.host, tx, inspector)
    }

    /// Executes with a borrowed inspector, retaining Monad's internal reserve observer.
    pub fn transact_with_inspector(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
        inspector: &mut dyn Inspector<MonadEvmTypes>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        transact(&mut self.host, tx, Some(inspector))
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
        if self.host.ext().preserve_reserve_balance_tracker {
            let scope = PreservedScope::new(&mut self.host);
            scope.host.validate_tx(tx)
        } else {
            end_reserve_balance_tx(&mut self.host);
            self.host.validate_tx(tx)
        }
    }

    /// Executes a protocol system call with native system gas and nonce rules.
    pub fn system_call(&mut self, tx: SystemTx) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        self.host.system_call(tx).map(|tx| MonadExecutedTx(Pending::Ordinary(tx)))
    }

    /// Changes execution configuration while retaining root precompile observation.
    pub fn set_execution_config(
        &mut self,
        config: ExecutionConfig<MonadEvmTypes>,
        spec: MonadHardfork,
        registry: TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        self.host.set_execution_config(config, spec, registry, ReservePrecompiles(precompiles));
    }

    /// Changes block and execution configuration while retaining reserve observation.
    pub fn set_block_and_execution_config(
        &mut self,
        block: MonadBlockEnv,
        config: ExecutionConfig<MonadEvmTypes>,
        spec: MonadHardfork,
        registry: TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        self.host.set_block_and_execution_config(
            block,
            config,
            spec,
            registry,
            ReservePrecompiles(precompiles),
        );
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
    pub fn set_precompiles(&mut self, precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a) {
        self.host.set_precompiles(ReservePrecompiles(precompiles));
    }

    /// Replaces boxed precompiles while preserving reserve observation.
    pub fn set_boxed_precompiles(
        &mut self,
        precompiles: Box<dyn PrecompileProvider<MonadEvmTypes> + 'a>,
    ) {
        self.set_precompiles(precompiles);
    }
}

fn transact<'e, 'a>(
    host: &'e mut MonadHost<'a>,
    tx: &Recovered<MonadTxEnvelope>,
    inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
) -> HandlerResult<MonadExecutedTx<'e, 'a>> {
    let mut observer = ReserveInspector::new(inspector);
    if host.ext().preserve_reserve_balance_tracker {
        let mut scope = PreservedScope::new(host);
        let executed = scope.host.transact_with_inspector(tx, &mut observer)?.detach();
        let before = scope.before.take().expect("preserved context");
        scope.host.ext_mut().memory_pool_offset = before.memory_pool_offset;
        scope.host.ext_mut().reserve_execution = before.reserve_execution.clone();
        drop(scope);
        Ok(MonadExecutedTx(Pending::Preserved(Box::new(PreservedPending {
            host,
            executed: Some(executed),
            before: Some(before),
        }))))
    } else {
        // Also clears stale state when registry dispatch rejects an unsupported envelope.
        end_reserve_balance_tx(host);
        host.transact_with_inspector(tx, &mut observer)
            .map(|tx| MonadExecutedTx(Pending::Ordinary(tx)))
    }
}

struct PreservedScope<'e, 'a> {
    host: &'e mut MonadHost<'a>,
    before: Option<MonadEvmExt>,
}

impl<'e, 'a> PreservedScope<'e, 'a> {
    fn new(host: &'e mut MonadHost<'a>) -> Self {
        Self { before: Some(host.ext().clone()), host }
    }
}

impl Drop for PreservedScope<'_, '_> {
    fn drop(&mut self) {
        if let Some(before) = self.before.take() {
            *self.host.ext_mut() = before;
        }
    }
}

enum Pending<'e, 'a> {
    Ordinary(ExecutedTx<'e, 'a, MonadEvmTypes>),
    Preserved(Box<PreservedPending<'e, 'a>>),
}

struct PreservedPending<'e, 'a> {
    host: &'e mut MonadHost<'a>,
    executed: Option<TxResultWithState<MonadEvmTypes>>,
    before: Option<MonadEvmExt>,
}

impl Drop for PreservedPending<'_, '_> {
    fn drop(&mut self) {
        if let Some(before) = self.before.take() {
            *self.host.ext_mut() = before;
        }
    }
}

/// Transaction state awaiting commit, discard or detach.
///
/// Ordinary transactions retain evm2's borrowed change view. Preserved synthetic
/// calls detach their state so discarding can also restore the enclosing context.
#[must_use = "transaction state must be committed, discarded or detached"]
pub struct MonadExecutedTx<'e, 'a>(Pending<'e, 'a>);

impl fmt::Debug for MonadExecutedTx<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonadExecutedTx").field("result", self.result()).finish_non_exhaustive()
    }
}

impl MonadExecutedTx<'_, '_> {
    /// Returns the result before resolving transaction state.
    pub fn result(&self) -> &TxResult<MonadEvmTypes> {
        match &self.0 {
            Pending::Ordinary(tx) => tx.result(),
            Pending::Preserved(pending) => {
                &pending.executed.as_ref().expect("pending execution").result
            }
        }
    }

    /// Returns a borrowed view of account and storage changes.
    pub fn changes(&self) -> TxStateChanges<'_> {
        match &self.0 {
            Pending::Ordinary(tx) => tx.changes(),
            Pending::Preserved(pending) => {
                pending.executed.as_ref().expect("pending execution").pending_state.changes()
            }
        }
    }

    /// Returns each account loaded by the transaction with its changes.
    pub fn account_changes(&self) -> impl Iterator<Item = AccountChanges<'_>> {
        self.changes().account_changes()
    }

    /// Accepts execution state and, for synthetic calls, its updated reserve context.
    pub fn commit(self) -> TxResult<MonadEvmTypes> {
        match self.0 {
            Pending::Ordinary(tx) => tx.commit(),
            Pending::Preserved(mut pending) => {
                let executed = pending.executed.take().expect("pending execution");
                pending.host.commit_source(&executed.pending_state);
                pending.before = None;
                executed.result
            }
        }
    }

    /// Accepts execution state and records changes in a block accumulator.
    pub fn commit_to(self, block: &mut BlockStateAccumulator) -> TxResult<MonadEvmTypes> {
        let Ok(result) = self.commit_with(block);
        result
    }

    /// Streams changes to a sink before accepting execution state.
    pub fn commit_with<S: StateChangeSink>(
        self,
        sink: &mut S,
    ) -> Result<TxResult<MonadEvmTypes>, S::Error> {
        self.changes().visit(sink)?;
        Ok(self.commit())
    }

    /// Discards state and restores the enclosing reserve context for synthetic calls.
    pub fn discard(self) -> TxResult<MonadEvmTypes> {
        match self.0 {
            Pending::Ordinary(tx) => tx.discard(),
            Pending::Preserved(mut pending) => {
                pending.executed.take().expect("pending execution").result
            }
        }
    }

    /// Streams changes to a sink and then discards execution.
    pub fn discard_with<S: StateChangeSink>(
        self,
        sink: &mut S,
    ) -> Result<TxResult<MonadEvmTypes>, S::Error> {
        self.changes().visit(sink)?;
        Ok(self.discard())
    }

    /// Returns the post-execution context of a preserved synthetic call.
    ///
    /// Clone this before detaching if the detached state will be accepted later.
    pub const fn preserved_context(&self) -> Option<&MonadEvmExt> {
        match &self.0 {
            Pending::Ordinary(_) => None,
            Pending::Preserved(pending) => Some(pending.host.ext()),
        }
    }

    /// Detaches state and restores the enclosing reserve context.
    ///
    /// To accept synthetic changes later, first clone [`Self::preserved_context`],
    /// then commit the detached state and install that context together. A detached
    /// state diff alone cannot reconstruct the enclosing transaction's baselines.
    pub fn detach(self) -> TxResultWithState<MonadEvmTypes> {
        match self.0 {
            Pending::Ordinary(tx) => tx.detach(),
            Pending::Preserved(mut pending) => pending.executed.take().expect("pending execution"),
        }
    }
}

impl StateChangeSource for MonadExecutedTx<'_, '_> {
    fn visit<S: StateChangeSink>(&self, sink: &mut S) -> Result<(), S::Error> {
        self.changes().visit(sink)
    }
}
