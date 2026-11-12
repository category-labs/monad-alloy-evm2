//! Standalone inspector composition and transaction-context lifecycle.
//!
//! [`MonadExecution`] owns the per-host frame selector and supplies the same
//! transaction handling used by [`crate::MonadEvm`]. The embedder retains the
//! native [`MonadHost`], its database and its inspector stack:
//!
//! ```
//! use alloy_consensus::{TxLegacy, transaction::Recovered};
//! use alloy_primitives::Address;
//! use evm2::{evm::InMemoryDB, inspector::NoopInspector};
//! use monad_evm2::{MonadBlockEnv, MonadEvmBuilder, MonadHardfork, MonadTxEnvelope};
//!
//! let (mut host, execution) = MonadEvmBuilder::new(143).create_host(
//!     MonadHardfork::MonadTen,
//!     MonadBlockEnv::default(),
//!     InMemoryDB::default(),
//! );
//! let tx = Recovered::new_unchecked(
//!     MonadTxEnvelope::Legacy(TxLegacy {
//!         to: Address::repeat_byte(0x22).into(),
//!         gas_limit: 100_000,
//!         ..Default::default()
//!     }),
//!     Address::repeat_byte(0x11),
//! );
//! let mut inspector = NoopInspector::default();
//! let result = execution.transact(&mut host, &tx, Some(&mut inspector))?.commit();
//! assert!(result.is_success());
//! assert_eq!(result.ext.charged_gas, 100_000);
//! # Ok::<(), evm2::registry::HandlerError>(())
//! ```
//!
//! Native embedders can instead use [`crate::MonadEvmFactory`], opting into
//! reserve rejection with [`MonadEvmExt::revert_on_reserve_balance_violation`]
//! and [`crate::reserve_balance::reserve_inspector`].
//! That path follows the native host's configured hardfork and leaves snapshots,
//! nested state acceptance and synthetic-context resolution to the embedder.
//! Protocol replay isolates context inside the transaction handler on both paths.
//!
//! [`MonadExecution`] supplies the standalone frame selector and synthetic-call
//! lifecycle. Keep one instance per host, outside cloneable extension snapshots.
//! Each nested host needs its own instance and provider. The low-level
//! [`MonadExecution::inspector`] method composes frame selection with reserves but
//! does not resolve synthetic context; use [`MonadExecution::transact`] for that.
//! Before detaching synthetic state, capture [`MonadExecutedTx::preserved_context`]
//! and accept it together with the detached state.
//!
//! Synchronize reserves before out-of-band account changes and rebase after state
//! replacement. [`crate::reserve_balance::rebase_reserve_balance_with_originals`]
//! retains transaction-start fields for persistent accounts carried into another
//! fork, including accounts first debited later. Propagate database errors.
//! [`MonadEvmExt::replace_while_executing`] restores matching context while
//! retaining memory, frame selection and enforcement owned by active execution.

use crate::frame::{FrameInspector, FramePrecompiles};
use crate::reserve_balance::{ReserveInspector, ReservePrecompiles, end_reserve_balance_tx};
use crate::{
    MonadBlockEnv, MonadEvmExt, MonadEvmTypes, MonadHardfork, MonadHost, MonadTxEnvelope,
    SYSTEM_ADDRESS,
};
use alloc::{boxed::Box, sync::Arc};
use alloy_consensus::{TxLegacy, transaction::Recovered};
use core::{
    fmt,
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

/// Execution hooks for an embedder-owned [`MonadHost`].
///
/// Keep one instance alongside each host. Its frame selector is shared with that
/// host's precompiles, outside cloneable extension snapshots. Create a fresh
/// instance and provider for each nested host; copy only [`MonadEvmExt`].
/// [`crate::MonadEvmBuilder::create_host`] constructs this pair, while
/// [`crate::MonadEvm`] owns the same pair for standalone use.
///
/// Transaction and system-call methods borrow the caller's inspector for execution
/// only. Their result handle keeps native state and preserved Monad context
/// together through commit, discard, detach, errors, and unwinding. An inspector
/// installed directly on the host is not used: pass it explicitly to these methods.
#[derive(Debug)]
pub struct MonadExecution {
    frame_selection: Arc<AtomicU8>,
}

impl MonadExecution {
    /// Creates hooks for one host, initially configured for `spec`.
    pub fn new(spec: MonadHardfork) -> Self {
        Self { frame_selection: Arc::new(AtomicU8::new(spec as u8)) }
    }

    /// Creates the host's default precompiles with frame selection and reserve observation.
    ///
    /// The returned provider owns its selector reference and does not borrow `self`.
    /// Do not share it with another host or replace it through the native host's
    /// setters; use [`Self::set_precompiles`] or [`Self::set_execution_config`].
    pub fn precompiles(&self) -> impl PrecompileProvider<MonadEvmTypes> + use<> {
        FramePrecompiles::new(self.frame_selection.clone())
    }

    /// Composes Monad's frame and reserve observers around a user inspector.
    ///
    /// Construct a fresh observer for each ordinary transaction and drop it before
    /// reconfiguring the host. `spec` must be the host's configured hardfork. Pass
    /// this observer to the native `transact_with_inspector` entrypoint, retaining
    /// the provider installed by these hooks. Callback filters apply to the user
    /// inspector even when reserve enforcement needs additional internal steps.
    ///
    /// This low-level hook does not restore preserved synthetic or system-call
    /// context. Use [`Self::transact`] and [`Self::system_call`] for those lifecycles.
    /// Pass the original user inspector to those methods, without this adapter.
    pub fn inspector<'i>(
        &self,
        spec: MonadHardfork,
        inspector: Option<&'i mut dyn Inspector<MonadEvmTypes>>,
    ) -> impl Inspector<MonadEvmTypes> + use<'i> {
        FrameInspector::new(ReserveInspector::new(inspector), self.frame_selection.clone(), spec)
    }

    /// Executes on a native host with reserves and preserved-context lifecycle handling.
    ///
    /// Ordinary transactions follow [`MonadEvmExt::revert_on_reserve_balance_violation`].
    /// Preserved synthetic calls defer enforcement to their enclosing transaction. Trusted protocol
    /// replay isolates the enclosing context and retains zero-gas system receipts.
    pub fn transact<'e, 'a>(
        &self,
        host: &'e mut MonadHost<'a>,
        tx: &Recovered<MonadTxEnvelope>,
        inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
    ) -> HandlerResult<MonadExecutedTx<'e, 'a>> {
        transact(host, tx, inspector, self.frame_selection.clone())
    }

    /// Validates admission without retaining state or changing a preserved tracker.
    pub fn validate_tx(
        &self,
        host: &mut MonadHost<'_>,
        tx: &Recovered<MonadTxEnvelope>,
    ) -> HandlerResult<()> {
        if host.ext().preserve_reserve_balance_tracker
            || (host.ext().protocol_replay && tx.signer() == SYSTEM_ADDRESS)
        {
            let scope = PreservedScope::new(host);
            scope.host.validate_tx(tx)
        } else {
            end_reserve_balance_tx(host);
            host.validate_tx(tx)
        }
    }

    /// Executes an inspected system call while isolating enclosing Monad bookkeeping.
    ///
    /// Uses native system gas and nonce rules, bypassing ordinary fees and reserve
    /// enforcement. The registry must retain the Monad legacy transaction handler.
    pub fn system_call<'e, 'a>(
        &self,
        host: &'e mut MonadHost<'a>,
        tx: SystemTx,
        inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
    ) -> HandlerResult<MonadExecutedTx<'e, 'a>> {
        system_call(host, tx, inspector, self.frame_selection.clone())
    }

    /// Changes execution configuration and keeps root precompile observation active.
    ///
    /// Call only between executions. The version's memory limit becomes the
    /// requested limit for subsequent frames. The supplied provider applies to
    /// every frame; [`Self::reset_precompiles`] restores the default fork tables.
    pub fn set_execution_config<'a>(
        &self,
        host: &mut MonadHost<'a>,
        config: ExecutionConfig<MonadEvmTypes>,
        spec: MonadHardfork,
        registry: TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        let memory_limit = config.version().memory_limit;
        host.set_execution_config(config, spec, registry, ReservePrecompiles(precompiles));
        host.ext_mut().frame.memory_limit = Some(memory_limit);
        host.ext_mut().frame.reset();
        self.frame_selection.store(spec as u8, Ordering::Relaxed);
    }

    /// Changes block and execution configuration between executions.
    pub fn set_block_and_execution_config<'a>(
        &self,
        host: &mut MonadHost<'a>,
        block: MonadBlockEnv,
        config: ExecutionConfig<MonadEvmTypes>,
        spec: MonadHardfork,
        registry: TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        self.set_execution_config(host, config, spec, registry, precompiles);
        host.set_block(block);
    }

    /// Updates block and runtime settings without changing fork or providers.
    pub fn set_block_and_execution_config_within_spec(
        &self,
        host: &mut MonadHost<'_>,
        block: MonadBlockEnv,
        config: ExecutionConfig<MonadEvmTypes>,
    ) {
        let memory_limit = config.version().memory_limit;
        host.set_block_and_execution_config_within_spec(block, config);
        host.ext_mut().frame.memory_limit = Some(memory_limit);
        host.ext_mut().frame.reset();
    }

    /// Installs a custom provider with reserve observation and native-address protection.
    ///
    /// The provider applies to every frame and manages its own hardfork selection.
    pub fn set_precompiles<'a>(
        &self,
        host: &mut MonadHost<'a>,
        precompiles: impl PrecompileProvider<MonadEvmTypes> + 'a,
    ) {
        host.set_precompiles(ReservePrecompiles(precompiles));
    }

    /// Restores Monad's default fork-aware provider between executions.
    pub fn reset_precompiles(&self, host: &mut MonadHost<'_>) {
        host.set_precompiles(self.precompiles());
        host.ext_mut().frame.reset();
        self.frame_selection.store(host.config_spec_id() as u8, Ordering::Relaxed);
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
        let scope = PreservedScope::isolated(host);
        scope.host.ext_mut().system_call =
            Some(crate::system::SystemCall::new(tx, dispatched.clone()));
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
    if host.ext().protocol_replay && tx.signer() == SYSTEM_ADDRESS {
        return replay_protocol(host, tx, inspector, selection);
    }
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

fn replay_protocol<'e, 'a>(
    host: &'e mut MonadHost<'a>,
    tx: &Recovered<MonadTxEnvelope>,
    inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
    selection: Arc<AtomicU8>,
) -> HandlerResult<MonadExecutedTx<'e, 'a>> {
    let dispatched = Arc::new(AtomicBool::new(false));
    let executed = {
        let scope = PreservedScope::new(host);
        scope.host.ext_mut().protocol_dispatch = Some(dispatched.clone());
        let mut noop = NoopInspector::default();
        let mut observer = FrameInspector::new(
            inspector.unwrap_or(&mut noop),
            selection,
            scope.host.config_spec_id(),
        );
        let pending = scope.host.transact_with_inspector(tx, &mut observer)?;
        if !dispatched.load(Ordering::Relaxed) {
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

struct PreservedScope<'e, 'a> {
    host: &'e mut MonadHost<'a>,
    before: Option<MonadEvmExt>,
}

impl<'e, 'a> PreservedScope<'e, 'a> {
    fn new(host: &'e mut MonadHost<'a>) -> Self {
        Self { before: Some(host.ext().clone()), host }
    }

    fn isolated(host: &'e mut MonadHost<'a>) -> Self {
        let scope = Self::new(host);
        let chain = scope.host.ext().chain.clone();
        let mut frame = scope.host.ext().frame.clone();
        frame.reset();
        *scope.host.ext_mut() = MonadEvmExt { chain, frame, ..Default::default() };
        scope
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
