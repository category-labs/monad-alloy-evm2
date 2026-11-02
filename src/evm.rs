//! Monad types used by the evm2 interpreter.

use crate::{MonadHardfork, monad_version};
use evm2::{
    BaseEvmConfig, BaseEvmConfigSelector, Evm, EvmConfigSelector, EvmTypesHost, ExecutionConfig,
    env::BlockEnv, ethereum::TxEnvelope,
};

/// An evm2 instance that retains its active Monad hardfork.
pub type MonadEvm<'a> = Evm<'a, MonadEvmTypes>;

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
    type EvmExt = ();
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = ();
    type BlockEnvExt = ();
    type Host<'a> = MonadEvm<'a>;
}

impl EvmConfigSelector<Self> for MonadEvmTypes {
    type Config<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32> = BaseEvmConfig<BASE_SPEC_ID>;

    fn execution_config(spec: MonadHardfork) -> ExecutionConfig<Self> {
        let base = <BaseEvmConfigSelector as EvmConfigSelector<Self>>::execution_config(spec);
        let chain_id = base.version().chain_id;
        base.with_version(monad_version(spec, chain_id))
    }
}
