//! Monad types used by the evm2 interpreter.

use crate::{MonadHardfork, instructions::Create, monad_version};
use evm2::{
    Evm, EvmConfig, EvmConfigSelector, EvmTypesHost, ExecutionConfig, OpcodeConfig, SpecId,
    env::BlockEnv, ethereum::TxEnvelope, interpreter::op,
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
            config.set_instruction::<Create<false>>(op::CREATE, 0);
            config.set_instruction::<Create<true>>(op::CREATE2, 0);
        }
        config
    };
}
