//! Staking argument decoding matching the C++ precompile's input consumption.

use super::interface::IMonadStaking::addValidatorCall;
use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall, SolType};
use evm2::precompiles::PrecompileHalt;

/// Decode a fixed-size call, ignoring dirty high bits but rejecting leftover bytes.
pub(super) fn decode_fixed<C: SolCall>(input: &[u8]) -> Result<C, PrecompileHalt> {
    let data = input.get(4..).ok_or_else(|| PrecompileHalt::Other("input too short".into()))?;
    let size = <C::Parameters<'_> as SolType>::ENCODED_SIZE
        .expect("fixed staking arguments must have a static ABI size");
    if data.len() < size {
        return Err(PrecompileHalt::Other("input too short".into()));
    }
    if data.len() > size {
        return Err(PrecompileHalt::Other("invalid input".into()));
    }

    // C++ reads only the low bytes of integers and addresses, without padding validation.
    C::abi_decode_raw(data).map_err(|_| PrecompileHalt::Other("invalid input".into()))
}

/// Decode the three sequential fixed-length byte tails, ignoring head offsets and padding.
pub(super) fn decode_add_validator(input: &[u8]) -> Result<addValidatorCall, PrecompileHalt> {
    let mut data = input.get(4..).ok_or_else(|| PrecompileHalt::Other("input too short".into()))?;
    take_bytes(&mut data, 3 * 32)?;
    let payload = decode_bytes_tail(&mut data, 165)?;
    let signed_secp_message = decode_bytes_tail(&mut data, 64)?;
    let signed_bls_message = decode_bytes_tail(&mut data, 96)?;
    if !data.is_empty() {
        return Err(PrecompileHalt::Other("invalid input".into()));
    }
    Ok(addValidatorCall {
        payload,
        signedSecpMessage: signed_secp_message,
        signedBlsMessage: signed_bls_message,
    })
}

fn decode_bytes_tail(data: &mut &[u8], size: usize) -> Result<Bytes, PrecompileHalt> {
    let length = U256::from_be_slice(take_bytes(data, 32)?);
    if length != U256::from(size) {
        return Err(PrecompileHalt::Other("length mismatch".into()));
    }
    let padded = take_bytes(data, size.next_multiple_of(32))?;
    Ok(Bytes::copy_from_slice(&padded[..size]))
}

fn take_bytes<'a>(data: &mut &'a [u8], size: usize) -> Result<&'a [u8], PrecompileHalt> {
    let (head, tail) = data
        .split_at_checked(size)
        .ok_or_else(|| PrecompileHalt::Other("input too short".into()))?;
    *data = tail;
    Ok(head)
}
