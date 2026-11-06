//! Cryptographic precompiles with Monad gas pricing and fork activation.
//!
//! Monad Eight uses Berlin ModExp pricing. Monad Nine and later use Osaka's
//! EIP-7823 input limits and EIP-7883 pricing. P-256 verification uses EIP-7951
//! pricing on every supported Monad hardfork.

use crate::reserve_balance::{RESERVE_BALANCE_ADDRESS, run_reserve_balance_precompile};
use crate::{MonadEvm, MonadEvmTypes, MonadHardfork};
use alloy_primitives::{Address, Bytes, hex};
use evm2::{
    Precompiles, crypto,
    interpreter::{GasTracker, Message},
    precompile::PrecompileOutput,
    precompiles::{
        P256VERIFY_OSAKA, Precompile, PrecompileHalt, PrecompileId, PrecompileResult, bn254,
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

/// Encoded field elements per blob and BLS scalar modulus, as specified by EIP-4844.
const POINT_EVALUATION_RETURN_VALUE: [u8; 64] = hex!(
    "0000000000000000000000000000000000000000000000000000000000001000"
    "73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001"
);

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
    if spec >= MonadHardfork::MonadNine {
        precompiles.as_map_mut().insert(Precompile::new(
            RESERVE_BALANCE_ADDRESS,
            PrecompileId::custom("MonadReserveBalance"),
            run_reserve_balance_precompile,
        ));
    }
    precompiles
}

fn ecrecover(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    gas.spend(MONAD_ECRECOVER_GAS)?;

    // Short inputs are right-padded with zeroes; bytes after the first 128 are ignored.
    let mut input = [0u8; 128];
    let len = message.input.len().min(input.len());
    input[..len].copy_from_slice(&message.input[..len]);

    // `v` must be a 32-byte big-endian integer equal to 27 or 28.
    if !(input[32..63].iter().all(|&b| b == 0) && matches!(input[63], 27 | 28)) {
        return Ok(PrecompileOutput::default());
    }

    let hash = input[..32].try_into().expect("32 hash bytes");
    let signature = input[64..].try_into().expect("64 signature bytes");
    let output = crypto()
        .secp256k1_ecrecover(signature, input[63] - 27, hash)
        .map(|address| Bytes::copy_from_slice(&address))
        .unwrap_or_default();
    Ok(PrecompileOutput::new(output))
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
    gas.spend(u64::from(rounds) * MONAD_BLAKE2F_ROUND_GAS)?;

    let final_block = match input[212] {
        0 => false,
        1 => true,
        _ => return Err(PrecompileHalt::Blake2WrongFinalIndicatorFlag.into()),
    };

    let mut state = [0u64; 8];
    for (word, bytes) in state.iter_mut().zip(input[4..68].as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    let mut block = [0u64; 16];
    for (word, bytes) in block.iter_mut().zip(input[68..196].as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    let counters = [
        u64::from_le_bytes(input[196..204].try_into().expect("eight counter bytes")),
        u64::from_le_bytes(input[204..212].try_into().expect("eight counter bytes")),
    ];
    crypto().blake2_compress(rounds, &mut state, &block, &counters, final_block);

    let mut output = [0u8; 64];
    for (bytes, word) in output.as_chunks_mut::<8>().0.iter_mut().zip(state) {
        *bytes = word.to_le_bytes();
    }
    Ok(PrecompileOutput::new(output.into()))
}

fn point_evaluation(
    _evm: &mut MonadEvm<'_>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    gas.spend(MONAD_POINT_EVALUATION_GAS)?;

    let input = message.input.as_ref();
    if input.len() != 192 {
        return Err(PrecompileHalt::BlobInvalidInputLength.into());
    }

    // The versioned hash must match the commitment before verifying the proof.
    let commitment = &input[96..144];
    let mut versioned_hash = crypto().sha256(commitment);
    versioned_hash[0] = 0x01;
    if versioned_hash != input[..32] {
        return Err(PrecompileHalt::BlobMismatchedVersion.into());
    }

    let z = input[32..64].try_into().expect("32 evaluation-point bytes");
    let y = input[64..96].try_into().expect("32 evaluation-result bytes");
    let commitment = commitment.try_into().expect("48 commitment bytes");
    let proof = input[144..192].try_into().expect("48 proof bytes");
    crypto().verify_kzg_proof(z, y, commitment, proof)?;

    Ok(PrecompileOutput::new(Bytes::from_static(&POINT_EVALUATION_RETURN_VALUE)))
}
