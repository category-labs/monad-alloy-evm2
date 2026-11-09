//! Transaction and inspector lifecycle around the native evm2 host.

use crate::frame::{FrameInspector, FramePrecompiles};
use crate::reserve_balance::{ReserveInspector, ReservePrecompiles, end_reserve_balance_tx};
use crate::{MonadBlockEnv, MonadEvmExt, MonadEvmTypes, MonadHardfork, MonadHost, MonadTxEnvelope};
use alloc::{boxed::Box, sync::Arc};
use alloy_consensus::{TxLegacy, transaction::Recovered};
use core::{
    fmt,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};
use evm2::{
    ExecutionConfig, TxResult,
    evm::{
        AccountChanges, BlockStateAccumulator, ExecutedTx, StateChangeSink, StateChangeSource,
        SystemTx, TxResultWithState, TxStateChanges,
    },
    inspector::{Inspector, NoopInspector},
    precompile::PrecompileProvider,
    registry::{HandlerError, HandlerResult, TxRegistry},
};

/// Monad transaction executor with automatic reserve enforcement.
///
/// Dereferences to [`MonadHost`] for state, environment and instruction APIs.
/// Execute transactions through this wrapper so inspector composition and
/// synthetic-call discard handling remain active.
pub struct MonadEvm<'a> {
    host: MonadHost<'a>,
    inspector: Option<Box<dyn Inspector<MonadEvmTypes> + 'a>>,
    // Kept outside cloneable snapshots so contexts cannot rebind another EVM's provider.
    frame_selection: Arc<AtomicU8>,
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
    pub(crate) const fn new(host: MonadHost<'a>, frame_selection: Arc<AtomicU8>) -> Self {
        Self { host, inspector: None, frame_selection }
    }

    /// Executes a transaction with automatic reserves, composing the installed inspector.
    pub fn transact(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        let inspector =
            self.inspector.as_mut().map(|inner| &mut **inner as &mut dyn Inspector<MonadEvmTypes>);
        transact(&mut self.host, tx, inspector, self.frame_selection.clone())
    }

    /// Executes with a borrowed inspector, retaining Monad's internal reserve observer.
    pub fn transact_with_inspector(
        &mut self,
        tx: &Recovered<MonadTxEnvelope>,
        inspector: &mut dyn Inspector<MonadEvmTypes>,
    ) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        transact(&mut self.host, tx, Some(inspector), self.frame_selection.clone())
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
    ///
    /// Uses the installed inspector. System calls bypass ordinary transaction
    /// validation, fees and reserve enforcement. Monad bookkeeping is isolated
    /// from any preserved synthetic-call context and restored before returning.
    /// The execution registry must retain the Monad legacy transaction handler.
    pub fn system_call(&mut self, tx: SystemTx) -> HandlerResult<MonadExecutedTx<'_, 'a>> {
        let inspector =
            self.inspector.as_mut().map(|inner| &mut **inner as &mut dyn Inspector<MonadEvmTypes>);
        system_call(&mut self.host, tx, inspector, self.frame_selection.clone())
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
        system_call(&mut self.host, tx, Some(inspector), self.frame_selection.clone())
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
        let memory_limit = config.version().memory_limit;
        self.host.set_execution_config(config, spec, registry, ReservePrecompiles(precompiles));
        self.host.ext_mut().frame.memory_limit = Some(memory_limit);
        self.host.ext_mut().frame.reset();
        self.frame_selection.store(spec as u8, Ordering::Relaxed);
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
        let memory_limit = config.version().memory_limit;
        self.host.set_block_and_execution_config(
            block,
            config,
            spec,
            registry,
            ReservePrecompiles(precompiles),
        );
        self.host.ext_mut().frame.memory_limit = Some(memory_limit);
        self.host.ext_mut().frame.reset();
        self.frame_selection.store(spec as u8, Ordering::Relaxed);
    }

    /// Changes block and runtime settings within the configured fork.
    ///
    /// Retains the installed providers and updates the requested frame memory limit.
    pub fn set_block_and_execution_config_within_spec(
        &mut self,
        block: MonadBlockEnv,
        config: ExecutionConfig<MonadEvmTypes>,
    ) {
        let memory_limit = config.version().memory_limit;
        self.host.set_block_and_execution_config_within_spec(block, config);
        self.host.ext_mut().frame.memory_limit = Some(memory_limit);
        self.host.ext_mut().frame.reset();
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
        self.host.set_precompiles(ReservePrecompiles(precompiles));
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
        let precompiles = FramePrecompiles::new(self.frame_selection.clone());
        self.set_precompiles(precompiles);
        let spec = self.host.config_spec_id();
        self.host.ext_mut().frame.reset();
        self.frame_selection.store(spec as u8, Ordering::Relaxed);
    }
}

fn system_call<'e, 'a>(
    host: &'e mut MonadHost<'a>,
    tx: SystemTx,
    inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
    selection: Arc<AtomicU8>,
) -> HandlerResult<MonadExecutedTx<'e, 'a>> {
    // The pinned engine scopes borrowed inspectors through its transaction
    // dispatcher. This envelope only selects the handler; the private request
    // carries the system call and bypasses ordinary transaction preparation.
    let envelope = Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy {
            to: tx.system_contract_address.into(),
            input: tx.data.clone(),
            gas_limit: tx.gas_limit,
            ..Default::default()
        }),
        tx.caller,
    );
    let dispatched = Arc::new(AtomicBool::new(false));
    let executed = {
        let scope = PreservedScope::new(host);
        let chain = scope.host.ext().chain.clone();
        let mut frame = scope.host.ext().frame.clone();
        frame.reset();
        *scope.host.ext_mut() = MonadEvmExt {
            chain,
            frame,
            system_call: Some(crate::system::SystemCall::new(tx, dispatched.clone())),
            ..Default::default()
        };
        let mut noop = NoopInspector::default();
        let mut observer = FrameInspector::new(
            inspector.unwrap_or(&mut noop),
            selection,
            scope.host.config_spec_id(),
        );
        let pending = scope.host.transact_with_inspector(&envelope, &mut observer)?;
        if !dispatched.load(Ordering::Relaxed) {
            // A replacement registry must not silently treat this as an ordinary tx.
            return Err(HandlerError::external(MissingSystemCallHandler));
        }
        pending.detach()
    };
    Ok(MonadExecutedTx(Pending::Detached(Box::new(DetachedPending {
        host,
        executed: Some(executed),
        before: None,
    }))))
}

#[derive(Debug)]
struct MissingSystemCallHandler;

impl fmt::Display for MissingSystemCallHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("system calls require the Monad transaction handler")
    }
}

impl core::error::Error for MissingSystemCallHandler {}

fn transact<'e, 'a>(
    host: &'e mut MonadHost<'a>,
    tx: &Recovered<MonadTxEnvelope>,
    inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
    selection: Arc<AtomicU8>,
) -> HandlerResult<MonadExecutedTx<'e, 'a>> {
    let mut observer =
        FrameInspector::new(ReserveInspector::new(inspector), selection, host.config_spec_id());
    if host.ext().preserve_reserve_balance_tracker {
        let mut scope = PreservedScope::new(host);
        let executed = scope.host.transact_with_inspector(tx, &mut observer)?.detach();
        let before = scope.before.take().expect("preserved context");
        scope.host.ext_mut().memory_pool_offset = before.memory_pool_offset;
        scope.host.ext_mut().reserve_execution = before.reserve_execution.clone();
        drop(scope);
        Ok(MonadExecutedTx(Pending::Detached(Box::new(DetachedPending {
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
    Detached(Box<DetachedPending<'e, 'a>>),
}

struct DetachedPending<'e, 'a> {
    host: &'e mut MonadHost<'a>,
    executed: Option<TxResultWithState<MonadEvmTypes>>,
    before: Option<MonadEvmExt>,
}

impl Drop for DetachedPending<'_, '_> {
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
/// System calls detach state after restoring their isolated Monad bookkeeping.
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
            Pending::Detached(pending) => {
                &pending.executed.as_ref().expect("pending execution").result
            }
        }
    }

    /// Returns a borrowed view of account and storage changes.
    pub fn changes(&self) -> TxStateChanges<'_> {
        match &self.0 {
            Pending::Ordinary(tx) => tx.changes(),
            Pending::Detached(pending) => {
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
            Pending::Detached(mut pending) => {
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
            Pending::Detached(mut pending) => {
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
            Pending::Detached(pending) => {
                if pending.before.is_some() {
                    Some(pending.host.ext())
                } else {
                    None
                }
            }
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
            Pending::Detached(mut pending) => pending.executed.take().expect("pending execution"),
        }
    }
}

impl StateChangeSource for MonadExecutedTx<'_, '_> {
    fn visit<S: StateChangeSink>(&self, sink: &mut S) -> Result<(), S::Error> {
        self.changes().visit(sink)
    }
}
