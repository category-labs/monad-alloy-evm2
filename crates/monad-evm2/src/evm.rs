//! Monad types used by the evm2 interpreter.

use crate::reserve_balance::{ReserveBalanceSync, ReserveBalanceTracker};
use crate::{MonadChainContext, MonadHardfork, instructions, monad_version};
use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;
use evm2::{
    Evm, EvmConfig, EvmConfigSelector, EvmTypesHost, ExecutionConfig, OpcodeConfig, SpecId,
    env::BlockEnv, ethereum::TxEnvelope,
};

/// The native host used by Monad instructions, precompiles and inspectors.
///
/// Use [`crate::reserve_balance::reserve_inspector`] for ordinary native transactions.
/// [`crate::MonadEvm`] adds standalone inspector and synthetic-context handling.
pub type MonadHost<'a> = Evm<'a, MonadEvmTypes>;

/// Block inputs visible to a Monad EVM.
pub type MonadBlockEnv = BlockEnv<MonadEvmTypes>;

/// The transaction envelope accepted by the Monad transaction registry.
pub type MonadTxEnvelope = TxEnvelope;

/// Selects the hardfork, transaction, and host types used by a Monad EVM.
#[derive(Clone, Copy, Debug)]
pub struct MonadEvmTypes;

impl EvmTypesHost for MonadEvmTypes {
    type ConfigSelector = Self;
    type SpecId = MonadHardfork;
    type Tx = MonadTxEnvelope;
    type EvmExt = MonadEvmExt;
    type MessageExt = MonadMessageExt;
    type MessageResultExt = MonadMessageResultExt;
    type TxEnvExt = ();
    type TxResultExt = MonadTxResultExt;
    type BlockEnvExt = ();
    type Host<'a> = MonadHost<'a>;
}

/// Monad execution state shared by active call frames.
#[derive(Clone, Debug, Default)]
pub struct MonadEvmExt {
    /// Block history and reserve policy supplied by the caller.
    pub chain: MonadChainContext,
    /// Reserve-balance state for the executing transaction.
    pub reserve_balance: ReserveBalanceTracker,
    /// Preserve an enclosing transaction's reserve tracker across synthetic calls.
    ///
    /// Only internal-call simulations may set this. An initialized tracker is required.
    /// Enforcement is deferred to the enclosing transaction, which must check its
    /// reserves before accepting execution. Restore `false` for ordinary transactions.
    pub preserve_reserve_balance_tracker: bool,
    /// Replay canonical protocol envelopes from the trusted system sender.
    ///
    /// Disabled by default. Enable only for authenticated historical block replay,
    /// never transaction-pool admission. See [`crate::protocol`] for validation rules.
    pub protocol_replay: bool,
    pub(crate) protocol_dispatch: Option<Arc<AtomicBool>>,
    /// Cursor and original account fields used by reserve synchronization.
    pub reserve_balance_sync: ReserveBalanceSync,
    pub(crate) reserve_execution: crate::reserve_balance::ReserveExecution,
    /// Memory held by suspended ancestor frames.
    pub memory_pool_offset: u64,
    pub(crate) system_call: Option<crate::system::SystemCall>,
    pub(crate) frame: crate::frame::FrameContext,
}

/// Optional rules for a call or creation, selected before its frame starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonadMessageExt {
    /// Override this frame's hardfork. Descendants inherit it unless overridden.
    ///
    /// Inspectors may set this in `call` or `create`. Returning restores the
    /// parent's rules; transaction admission and fees retain the configured fork.
    pub hardfork: Option<MonadHardfork>,
}

/// Reserve-balance outcome attached to a transaction result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonadTxResultExt {
    /// Whether the final transaction state violates reserve-balance rules.
    pub dipped_into_reserve: bool,
    /// Whether reserve-balance enforcement rejected execution.
    pub reverted_for_reserve_balance: bool,
    /// Monad-specific failure, separate from an EVM `REVERT`.
    pub halt_reason: Option<MonadHaltReason>,
}

/// Monad protocol failures that do not have a native evm2 instruction stop code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonadHaltReason {
    /// Execution left an account below its required reserve balance.
    ReserveBalanceViolation,
}

/// Protocol outcome visible to root call/create inspectors.
///
/// evm2 uses `InstrStop::Revert` internally to retain unused CALL gas. Consumers
/// must consult this extension to distinguish reserve rejection from EVM `REVERT`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonadMessageResultExt {
    /// Monad-specific failure, if the root execution was rejected.
    pub halt_reason: Option<MonadHaltReason>,
}

impl MonadEvmExt {
    /// Sets preservation for synthetic calls within an initialized transaction context.
    pub const fn set_preserve_reserve_balance_tracker(&mut self, preserve: bool) {
        self.preserve_reserve_balance_tracker = preserve;
    }

    /// Memory in bytes held by suspended ancestor frames.
    pub const fn memory_pool_offset(&self) -> u64 {
        self.memory_pool_offset
    }

    /// Restores context while retaining bookkeeping owned by currently executing frames.
    ///
    /// Use when an embedder restores a snapshot or accepts a preserved synthetic
    /// call into a running host. Chain context, reserves and their journal cursor
    /// come from `context`; suspended memory, frame selection, reserve adjudication
    /// and system dispatch remain attached to the running execution.
    ///
    /// Restore matching native state first. After merging changes from a different
    /// journal, call [`crate::reserve_balance::rebase_reserve_balance`] (or its
    /// transaction-originals variant). A context alone does not restore state or
    /// the host's block and execution configuration.
    pub fn replace_while_executing(&mut self, mut context: Self) {
        core::mem::swap(&mut context.memory_pool_offset, &mut self.memory_pool_offset);
        core::mem::swap(&mut context.frame, &mut self.frame);
        core::mem::swap(&mut context.reserve_execution, &mut self.reserve_execution);
        core::mem::swap(&mut context.system_call, &mut self.system_call);
        core::mem::swap(&mut context.protocol_dispatch, &mut self.protocol_dispatch);
        context.preserve_reserve_balance_tracker = self.preserve_reserve_balance_tracker;
        *self = context;
    }
}

impl EvmConfigSelector<Self> for MonadEvmTypes {
    type Config<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32> =
        MonadEvmConfig<BASE_SPEC_ID, CUSTOM_SPEC_ID>;

    fn execution_config(spec: MonadHardfork) -> ExecutionConfig<Self> {
        let config = match spec {
            MonadHardfork::MonadEight => ExecutionConfig::for_config::<
                MonadEvmConfig<{ SpecId::PRAGUE as u32 }, { MonadHardfork::MonadEight as u32 }>,
            >(),
            MonadHardfork::MonadNine => ExecutionConfig::for_config::<
                MonadEvmConfig<{ SpecId::OSAKA as u32 }, { MonadHardfork::MonadNine as u32 }>,
            >(),
            MonadHardfork::MonadTen => ExecutionConfig::for_config::<
                MonadEvmConfig<{ SpecId::OSAKA as u32 }, { MonadHardfork::MonadTen as u32 }>,
            >(),
            MonadHardfork::MonadNext => ExecutionConfig::for_config::<
                MonadEvmConfig<{ SpecId::OSAKA as u32 }, { MonadHardfork::MonadNext as u32 }>,
            >(),
        };
        let chain_id = config.version().chain_id;
        config.with_version(monad_version(spec, chain_id))
    }
}

/// Monad instruction configuration over an inherited Ethereum specification.
///
/// `MONAD_SPEC_ID` is a [`MonadHardfork`] discriminant. The `u32::MAX` sentinel
/// selects the unchanged Ethereum instruction table.
#[derive(Clone, Copy, Debug)]
pub struct MonadEvmConfig<const BASE_SPEC_ID: u32, const MONAD_SPEC_ID: u32>(());

impl<const BASE_SPEC_ID: u32, const MONAD_SPEC_ID: u32> EvmConfig<MonadEvmTypes>
    for MonadEvmConfig<BASE_SPEC_ID, MONAD_SPEC_ID>
{
    const BASE_SPEC_ID: SpecId = SpecId::try_from_u32(BASE_SPEC_ID).expect("invalid base spec ID");
    const OPCODE_CONFIG: &'static OpcodeConfig<MonadEvmTypes> = &{
        let mut config = OpcodeConfig::base::<Self>();
        if MONAD_SPEC_ID <= u8::MAX as u32
            && MonadHardfork::try_from_u8(MONAD_SPEC_ID as u8).is_some()
        {
            instructions::configure(&mut config);
        }
        config
    };
}
