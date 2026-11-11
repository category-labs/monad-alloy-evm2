//! Reserve adjudication at the outer message boundary.

use super::sync_reserve_balance;
use crate::{MonadEvmTypes, MonadHaltReason, MonadHost};
use alloc::vec::Vec;
use alloy_primitives::{Address, Bytes, Log, U256};
use core::cell::Cell;
use evm2::{
    DatabaseError, PrecompileError,
    evm::{PrewarmSet, StateCheckpoint},
    inspector::{Inspector, InspectorConfig, OpcodeSet},
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageResult},
    precompile::{PrecompileOutput, PrecompileProvider},
    precompiles::{MovePrecompileError, PrecompileId},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct ReserveExecution {
    pub(crate) checkpoint: Option<StateCheckpoint>,
    pub(crate) depth: u16,
    violation_before_rollback: bool,
    root_active: bool,
    pub(crate) rejected: bool,
    pub(crate) observed: bool,
}

/// Save the root's reserve status before evm2 can roll back its failed execution.
fn capture(host: &mut MonadHost<'_>, depth: u16) -> Result<(), DatabaseError> {
    let execution = &host.ext().reserve_execution;
    if execution.checkpoint.is_none() || !execution.root_active || execution.depth != depth {
        return Ok(());
    }
    sync_reserve_balance(host)?;
    host.ext_mut().reserve_execution.violation_before_rollback =
        host.ext().reserve_balance.has_violation();
    Ok(())
}

pub(crate) struct ReserveInspector<'a> {
    inner: Option<&'a mut dyn Inspector<MonadEvmTypes>>,
    frames: usize,
    root_override: bool,
    enforce_root: bool,
    user_steps: Cell<OpcodeSet>,
    pending_steps: Vec<bool>,
}

/// Composes reserve enforcement around an application's native inspector.
///
/// Required for ordinary transactions when
/// [`crate::MonadEvmExt::revert_on_reserve_balance_violation`] is enabled.
/// Create a fresh adapter for each transaction and pass it to native
/// `transact_with_inspector`. Installing it alone does not enable rejection.
/// Use [`crate::MonadPrecompiles`] or wrap custom
/// providers with [`reserve_precompiles`] so failed root precompiles are observed
/// before rollback. This adapter uses the host's configured hardfork; it does not
/// require a frame selector or manage synthetic-call commit and discard.
pub fn reserve_inspector(
    inspector: Option<&mut dyn Inspector<MonadEvmTypes>>,
) -> impl Inspector<MonadEvmTypes> + '_ {
    ReserveInspector::new(inspector)
}

/// Observes root reserve violations and protects native addresses in a custom provider.
///
/// [`crate::MonadPrecompiles`] already includes these safeguards.
pub fn reserve_precompiles<P: PrecompileProvider<MonadEvmTypes>>(
    provider: P,
) -> impl PrecompileProvider<MonadEvmTypes> {
    ReservePrecompiles(provider)
}

pub(crate) fn observe_precompile(
    host: &mut MonadHost<'_>,
    message: &Message<MonadEvmTypes>,
    result: Result<PrecompileOutput, PrecompileError>,
) -> Result<PrecompileOutput, PrecompileError> {
    if !matches!(result, Err(PrecompileError::Database(_) | PrecompileError::Fatal(_))) {
        capture(host, message.depth)?;
    }
    result
}

impl<'a> ReserveInspector<'a> {
    pub(crate) const fn new(inner: Option<&'a mut dyn Inspector<MonadEvmTypes>>) -> Self {
        Self {
            inner,
            frames: 0,
            root_override: false,
            enforce_root: false,
            user_steps: Cell::new(OpcodeSet::EMPTY),
            pending_steps: Vec::new(),
        }
    }

    fn capture_stop(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if self.frames == 1
            && let Err(stop) = interp.result()
            && !stop.is_fatal()
        {
            let depth = interp.message().depth;
            if let Err(error) = capture(interp.host(), depth) {
                interp.fail(error);
            }
        }
    }

    fn begin(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
        create: bool,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.frames += 1;
        let execution = &mut interp.host().ext_mut().reserve_execution;
        execution.root_active = self.frames == 1;
        if self.frames == 1 {
            execution.observed = true;
            self.enforce_root = execution.checkpoint.is_some();
            execution.depth = message.depth;
        }
        let result = self.inner.as_mut().and_then(|inner| {
            if create { inner.create(interp, message) } else { inner.call(interp, message) }
        });
        if self.frames == 1 {
            self.root_override = result.is_some();
            interp.host().ext_mut().reserve_execution.depth = message.depth;
        }
        result
    }

    fn end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
        create: bool,
    ) {
        if self.frames == 1 {
            let host = interp.host();
            let execution = &host.ext().reserve_execution;
            let early_failure = !self.root_override
                && matches!(
                    result.stop,
                    InstrStop::OutOfFunds
                        | InstrStop::CallTooDeep
                        | InstrStop::NonceOverflow
                        | InstrStop::CreateCollision
                );
            if execution.checkpoint.is_some() && !early_failure && !result.stop.is_fatal() {
                if let Err(error) = sync_reserve_balance(host) {
                    interp.fail(error);
                    self.frames -= 1;
                    return;
                }
                let host = interp.host();
                let execution = &host.ext().reserve_execution;
                if execution.violation_before_rollback || host.ext().reserve_balance.has_violation()
                {
                    let checkpoint = execution.checkpoint.clone().expect("active execution");
                    let features = host.version().features;
                    host.state_mut().rollback(checkpoint, features);
                    // Fees, nonce and authorizations were recorded before this checkpoint.
                    if create || result.stop.is_halt() {
                        result.gas.spend_all();
                    }
                    result.gas.set_refunded(0);
                    result.stop = InstrStop::Revert;
                    result.output = Bytes::new();
                    result.created_address = None;
                    result.ext.halt_reason = Some(MonadHaltReason::ReserveBalanceViolation);
                    host.ext_mut().reserve_execution.rejected = true;
                    if let Err(error) = sync_reserve_balance(host) {
                        interp.fail(error);
                        self.frames -= 1;
                        return;
                    }
                }
            }
        }
        if let Some(inner) = &mut self.inner {
            if create {
                inner.create_end(interp, message, result);
            } else {
                inner.call_end(interp, message, result);
            }
        }
        self.frames -= 1;
        interp.host().ext_mut().reserve_execution.root_active = self.frames == 1;
    }
}

impl Inspector<MonadEvmTypes> for ReserveInspector<'_> {
    fn inspector_config(&self) -> InspectorConfig {
        let config = self.inner.as_ref().map_or_else(
            || InspectorConfig::new().with_steps(OpcodeSet::EMPTY),
            |inner| inner.inspector_config(),
        );
        // Refresh at the same boundaries where the engine reads its configuration.
        self.user_steps.set(config.steps());
        if self.enforce_root && self.frames == 1 {
            // Any root instruction can halt, including static-gas and stack failures.
            InspectorConfig::new()
        } else {
            config
        }
    }

    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if let Some(inner) = &mut self.inner {
            inner.initialize_interp(interp);
        }
        self.capture_stop(interp);
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if let Some(inner) = &mut self.inner {
            let selected = self.user_steps.get().contains(interp.opcode());
            // Child callbacks can change the filter before this instruction ends.
            self.pending_steps.push(selected);
            if selected {
                inner.step(interp);
            }
        }
        self.capture_stop(interp);
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if let Some(inner) = &mut self.inner
            && self.pending_steps.pop().unwrap_or(false)
        {
            inner.step_end(interp);
        }
        self.capture_stop(interp);
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.begin(interp, message, false)
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.begin(interp, message, true)
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.end(interp, message, result, false);
    }

    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.end(interp, message, result, true);
    }

    fn log(&mut self, log: &Log, host: &mut MonadHost<'_>) {
        if let Some(inner) = &mut self.inner {
            inner.log(log, host);
        }
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        host: &mut MonadHost<'_>,
    ) {
        if let Some(inner) = &mut self.inner {
            inner.selfdestruct(contract, target, value, host);
        }
    }
}

/// Observe precompile failures before the native executor rolls back their transfers.
pub(crate) struct ReservePrecompiles<P>(pub(crate) P);

impl<P: PrecompileProvider<MonadEvmTypes>> PrecompileProvider<MonadEvmTypes>
    for ReservePrecompiles<P>
{
    fn addresses(&self) -> Vec<Address> {
        self.0.addresses()
    }
    fn warm_addresses(&self, warm: &mut PrewarmSet) {
        self.0.warm_addresses(warm);
    }
    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        self.0.precompile_ids()
    }
    fn contains(&self, address: &Address) -> bool {
        self.0.contains(address)
    }
    fn move_precompiles(
        &mut self,
        moves: &[(Address, Address)],
    ) -> Result<(), MovePrecompileError> {
        crate::precompiles::validate_native_relocations(moves)?;
        self.0.move_precompiles(moves)
    }

    fn execute(
        &mut self,
        host: &mut MonadHost<'_>,
        message: &Message<MonadEvmTypes>,
        gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        let result = self.0.execute(host, message, gas)?;
        Some(observe_precompile(host, message, result))
    }
}
