//! Cryptographic precompiles with Monad gas pricing and fork activation.
//!
//! Monad Eight uses Berlin ModExp pricing. Monad Nine and later use Osaka's
//! EIP-7823 input limits and EIP-7883 pricing. P-256 verification uses EIP-7951
//! pricing on every supported Monad hardfork.

use crate::{MonadEvm, MonadEvmTypes, MonadHardfork};
use alloy_primitives::Address;
use evm2::{
    Precompiles,
    interpreter::{GasTracker, Message},
    precompiles::{
        P256VERIFY_OSAKA, Precompile, PrecompileHalt, PrecompileId, PrecompileResult, blake2,
        bn254, kzg_point_evaluation, secp256k1,
    },
};

/// Gas charged for ecRecover at address `0x01`.
pub const MONAD_ECRECOVER_GAS: u64 = 6_000;
/// Gas charged for BN254 addition at address `0x06`.
pub const MONAD_EC_ADD_GAS: u64 = 300;
/// Gas charged for BN254 multiplication at address `0x07`.
pub const MONAD_EC_MUL_GAS: u64 = 30_000;
/// Base gas charged for BN254 pairing at address `0x08`.
pub const MONAD_EC_PAIRING_BASE_GAS: u64 = 225_000;
/// Additional gas charged per 192-byte point pair for BN254 pairing.
pub const MONAD_EC_PAIRING_PER_POINT_GAS: u64 = 170_000;
/// Gas charged per Blake2 compression round at address `0x09`.
pub const MONAD_BLAKE2F_ROUND_GAS: u64 = 2;
/// Gas charged for KZG point evaluation at address `0x0a`.
pub const MONAD_POINT_EVALUATION_GAS: u64 = 200_000;

/// Builds the cryptographic precompile provider for `spec`.
///
/// Inherits the base fork's implementations, including ModExp and BLS12-381,
/// replaces the six repriced operations, and enables P-256 at `0x0100`.
/// The provider also supplies these addresses for transaction prewarming.
pub fn monad_precompiles(spec: MonadHardfork) -> Precompiles<MonadEvmTypes> {
    let mut precompiles = Precompiles::base(spec.into_eth_spec());
    precompiles.as_map_mut().extend([
        Precompile::new(Address::with_last_byte(0x01), PrecompileId::EcRec, ecrecover),
        Precompile::new(Address::with_last_byte(0x06), PrecompileId::Bn254Add, ec_add),
        Precompile::new(Address::with_last_byte(0x07), PrecompileId::Bn254Mul, ec_mul),
        Precompile::new(Address::with_last_byte(0x08), PrecompileId::Bn254Pairing, ec_pairing),
        Precompile::new(Address::with_last_byte(0x09), PrecompileId::Blake2F, blake2f),
        Precompile::new(
            Address::with_last_byte(0x0a),
            PrecompileId::KzgPointEvaluation,
            point_evaluation,
        ),
        P256VERIFY_OSAKA(),
    ]);
    precompiles
}

fn ecrecover(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    // The upstream implementation charges the remaining 3,000 before validation.
    gas.spend(MONAD_ECRECOVER_GAS - 3_000)?;
    secp256k1::run(&message.input, gas)
}

fn ec_add(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    bn254::run_add(&message.input, MONAD_EC_ADD_GAS, gas)
}

fn ec_mul(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    bn254::run_mul(&message.input, MONAD_EC_MUL_GAS, gas)
}

fn ec_pairing(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    bn254::run_pair(&message.input, MONAD_EC_PAIRING_PER_POINT_GAS, MONAD_EC_PAIRING_BASE_GAS, gas)
}

fn blake2f(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    // Length validation precedes gas charging; final-flag validation follows it.
    let input = message.input.as_ref();
    if input.len() != 213 {
        return Err(PrecompileHalt::Blake2WrongLength.into());
    }
    let rounds = u32::from_be_bytes(input[..4].try_into().expect("four round-count bytes"));
    // The upstream implementation charges one more gas per round.
    gas.spend(u64::from(rounds) * (MONAD_BLAKE2F_ROUND_GAS - 1))?;
    blake2::run(input, gas)
}

fn point_evaluation(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    // The upstream implementation charges the remaining 50,000 before validation.
    gas.spend(MONAD_POINT_EVALUATION_GAS - 50_000)?;
    kzg_point_evaluation::run(&message.input, gas)
}
