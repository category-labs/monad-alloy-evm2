//! Reserve-balance query ABI and native precompile.

use super::dipped_into_reserve;
use crate::{MonadEvmTypes, MonadHardfork};
use alloy_primitives::{Address, Bytes, address};
use evm2::{
    Evm,
    interpreter::{GasTracker, Message, MessageKind},
    precompile::PrecompileOutput,
    precompiles::{PrecompileError, PrecompileHalt, PrecompileResult},
};

/// Reserve-balance precompile address.
pub const RESERVE_BALANCE_ADDRESS: Address = address!("0x0000000000000000000000000000000000001001");

/// `dippedIntoReserve()` selector.
pub const DIPPED_INTO_RESERVE_SELECTOR: [u8; 4] = [0x3a, 0x61, 0x58, 0x4e];

/// Gas cost for `dippedIntoReserve()`.
pub const DIPPED_INTO_RESERVE_GAS: u64 = 100;

/// Fallback dispatch cost for invalid selectors.
pub const FALLBACK_GAS: u64 = 100;

alloy_sol_types::sol! {
    /// Monad reserve-balance precompile interface at address 0x1001.
    interface IReserveBalance {
        function dippedIntoReserve() external returns (bool);
    }
}

/// Reserve-balance precompile errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveBalanceError {
    /// Unknown or short selector.
    MethodNotSupported,
    /// Nonzero value supplied to a non-payable method.
    ValueNonZero,
    /// Extra calldata past the selector.
    InvalidInput,
}

impl ReserveBalanceError {
    /// Returns the revert message emitted by the precompile.
    pub const fn message(self) -> &'static str {
        match self {
            Self::MethodNotSupported => "method not supported",
            Self::ValueNonZero => "value is nonzero",
            Self::InvalidInput => "input is invalid",
        }
    }
}

impl core::fmt::Display for ReserveBalanceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

impl core::error::Error for ReserveBalanceError {}

/// Runs the reserve-balance precompile.
///
/// The caller must only dispatch messages whose code address is [`RESERVE_BALANCE_ADDRESS`] while
/// MonadNine or later is active.
pub fn run_reserve_balance_precompile(
    evm: &mut Evm<'_, MonadEvmTypes>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    debug_assert!(MonadHardfork::MonadNine.is_enabled_in(evm.config_spec_id()));
    let is_delegated_call =
        message.kind == MessageKind::Call && message.destination != message.code_address;
    let is_static = message.kind == MessageKind::StaticCall || message.caller_is_static;

    if message.kind != MessageKind::Call || is_static || is_delegated_call {
        return revert(gas, Bytes::new());
    }

    if message.gas_limit < DIPPED_INTO_RESERVE_GAS {
        return Err(PrecompileHalt::OutOfGas.into());
    }

    let input = &message.input;
    if input.get(..4) != Some(&DIPPED_INTO_RESERVE_SELECTOR[..]) {
        return revert_error(gas, ReserveBalanceError::MethodNotSupported);
    }

    if !message.value.is_zero() {
        return revert_error(gas, ReserveBalanceError::ValueNonZero);
    }

    if input.len() > 4 {
        return revert_error(gas, ReserveBalanceError::InvalidInput);
    }

    let has_violation = dipped_into_reserve(evm).map_err(PrecompileError::Database)?;
    gas.spend(DIPPED_INTO_RESERVE_GAS)?;
    let mut output = [0u8; 32];
    output[31] = u8::from(has_violation);
    Ok(PrecompileOutput::new(Bytes::copy_from_slice(&output)))
}

const fn revert_error(gas: &mut GasTracker, error: ReserveBalanceError) -> PrecompileResult {
    revert(gas, Bytes::from_static(error.message().as_bytes()))
}

const fn revert(gas: &mut GasTracker, output: Bytes) -> PrecompileResult {
    gas.spend_all();
    Err(PrecompileError::Revert(output))
}
