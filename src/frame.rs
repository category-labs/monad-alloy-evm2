//! Frame-local Monad rules over evm2's immutable execution configuration.

use crate::{MonadEvmTypes, MonadHardfork, MonadHost, monad_precompiles};
use alloc::{sync::Arc, vec::Vec};
use alloy_primitives::{Address, Log, U256};
use core::sync::atomic::{AtomicU8, Ordering};
use evm2::{
    Inspector, PrecompileError, Precompiles,
    evm::PrewarmSet,
    inspector::InspectorConfig,
    interpreter::{GasTracker, Interpreter, Message, MessageResult},
    precompile::{PrecompileOutput, PrecompileProvider},
    precompiles::{MovePrecompileError, PrecompileId},
};

/// Returns the executing frame's Monad hardfork, or the configured fork outside execution.
///
/// Use this in inspectors and custom precompiles. evm2's `config_spec_id()` and
/// interpreter `spec()` describe the fixed engine configuration, which cannot
/// be changed while its interpreter holds references to it.
pub fn frame_hardfork(host: &MonadHost<'_>) -> MonadHardfork {
    host.ext().frame.active.unwrap_or_else(|| host.config_spec_id())
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FrameContext {
    active: Option<MonadHardfork>,
    /// The factory's requested limit, before the MonadNine cap.
    pub(crate) memory_limit: Option<u64>,
    /// Transaction warmth independent of the currently selected precompile set.
    pub(crate) base_warm: Option<PrewarmSet>,
}

impl FrameContext {
    pub(crate) fn reset(&mut self) {
        self.active = None;
        self.base_warm = None;
    }
}

#[cfg(feature = "memory_limit")]
pub(crate) fn memory_limit(host: &MonadHost<'_>) -> u64 {
    let requested = host.ext().frame.memory_limit.unwrap_or(host.version().memory_limit);
    crate::monad_memory_limit(frame_hardfork(host), requested)
}

/// Selects immutable precompile tables without replacing evm2's active provider.
pub(crate) struct FramePrecompiles {
    selection: Arc<AtomicU8>,
    providers: [Precompiles<MonadEvmTypes>; 4],
}

impl FramePrecompiles {
    pub(crate) fn new(selection: Arc<AtomicU8>) -> Self {
        Self { selection, providers: MonadHardfork::ALL.map(monad_precompiles) }
    }

    fn selected(&self) -> &Precompiles<MonadEvmTypes> {
        &self.providers
            [(self.selection.load(Ordering::Relaxed) - MonadHardfork::MonadEight as u8) as usize]
    }
}

impl PrecompileProvider<MonadEvmTypes> for FramePrecompiles {
    fn addresses(&self) -> Vec<Address> {
        self.selected().addresses()
    }
    fn warm_addresses(&self, warm: &mut PrewarmSet) {
        self.selected().warm_addresses(warm);
    }
    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        self.selected().precompile_ids()
    }
    fn contains(&self, address: &Address) -> bool {
        self.selected().contains(address)
    }
    fn move_precompiles(
        &mut self,
        moves: &[(Address, Address)],
    ) -> Result<(), MovePrecompileError> {
        // Validate the active table first. An address absent in an earlier fork
        // remains absent there, but gains the same relocation when activated.
        let mut providers = self.providers.clone();
        let active =
            (self.selection.load(Ordering::Relaxed) - MonadHardfork::MonadEight as u8) as usize;
        providers[active].move_precompiles(moves)?;
        for (index, provider) in providers.iter_mut().enumerate() {
            if index != active {
                let available = moves
                    .iter()
                    .copied()
                    .filter(|(source, _)| provider.contains(source))
                    .collect::<Vec<_>>();
                provider.move_precompiles(&available)?;
            }
        }
        self.providers = providers;
        Ok(())
    }
    fn execute(
        &mut self,
        host: &mut MonadHost<'_>,
        message: &Message<MonadEvmTypes>,
        gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        let index = (frame_hardfork(host) as u8 - MonadHardfork::MonadEight as u8) as usize;
        self.providers[index].execute(host, message, gas)
    }
}

pub(crate) struct FrameInspector<I> {
    inner: I,
    parents: Vec<FrameEntry>,
    selection: Arc<AtomicU8>,
    root: MonadHardfork,
    precompile_addresses: Option<Vec<Address>>,
}

enum FrameEntry {
    // An override skips frame creation but still receives an end callback.
    Override,
    Executed { parent: Option<MonadHardfork> },
}

impl<I: Inspector<MonadEvmTypes>> FrameInspector<I> {
    pub(crate) fn new(inner: I, selection: Arc<AtomicU8>, root: MonadHardfork) -> Self {
        selection.store(root as u8, Ordering::Relaxed);
        Self { inner, parents: Vec::new(), selection, root, precompile_addresses: None }
    }

    fn select_precompiles(&mut self, host: &mut MonadHost<'_>, spec: MonadHardfork) {
        if self.precompile_addresses.is_some()
            && self.selection.load(Ordering::Relaxed) == spec as u8
        {
            return;
        }
        // evm2 combines base and precompile warmth in one set. Remove only the
        // previous provider's contribution, retaining access lists and coinbase.
        let previous =
            self.precompile_addresses.take().unwrap_or_else(|| host.precompiles().addresses());
        if host.ext().frame.base_warm.is_none() {
            // Native system calls have only warmed their target at this point.
            host.ext_mut().frame.base_warm = Some(host.state().prewarm_set().clone());
        }
        for address in previous {
            if !host.ext().frame.base_warm.as_ref().expect("base warmth").is_warm(&address) {
                host.state_mut().prewarm_set_mut().remove(&address);
            }
        }
        self.selection.store(spec as u8, Ordering::Relaxed);
        self.precompile_addresses = Some(host.precompiles().addresses());
        host.warm_precompiles();
    }

    fn enter(
        &mut self,
        host: &mut MonadHost<'_>,
        message: &Message<MonadEvmTypes>,
        overridden: bool,
    ) {
        if overridden {
            self.parents.push(FrameEntry::Override);
            return;
        }
        let spec = message.ext.hardfork.unwrap_or_else(|| frame_hardfork(host));
        self.parents.push(FrameEntry::Executed { parent: host.ext().frame.active });
        host.ext_mut().frame.active = Some(spec);
        self.select_precompiles(host, spec);
    }

    fn leave(&mut self, host: &mut MonadHost<'_>) {
        if let FrameEntry::Executed { parent } =
            self.parents.pop().expect("balanced frame callbacks")
        {
            host.ext_mut().frame.active = parent;
            self.select_precompiles(host, parent.unwrap_or(self.root));
        }
    }
}

impl<I> Drop for FrameInspector<I> {
    fn drop(&mut self) {
        // This belongs to the executor, not its cloneable extension context.
        // Restore the provider even when a fatal error or panic skips end hooks.
        self.selection.store(self.root as u8, Ordering::Relaxed);
    }
}

impl<I: Inspector<MonadEvmTypes>> Inspector<MonadEvmTypes> for FrameInspector<I> {
    fn inspector_config(&self) -> InspectorConfig {
        self.inner.inspector_config()
    }
    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.inner.initialize_interp(interp);
    }
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.inner.step(interp);
    }
    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.inner.step_end(interp);
    }
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        let result = self.inner.call(interp, message);
        self.enter(interp.host(), message, result.is_some());
        result
    }
    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        let result = self.inner.create(interp, message);
        self.enter(interp.host(), message, result.is_some());
        result
    }
    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.inner.call_end(interp, message, result);
        self.leave(interp.host());
    }
    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.inner.create_end(interp, message, result);
        self.leave(interp.host());
    }
    fn log(&mut self, log: &Log, host: &mut MonadHost<'_>) {
        self.inner.log(log, host);
    }
    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        host: &mut MonadHost<'_>,
    ) {
        self.inner.selfdestruct(contract, target, value, host);
    }
}
