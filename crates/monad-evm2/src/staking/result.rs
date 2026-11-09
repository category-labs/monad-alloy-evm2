//! Precompile result types used by the staking port.
//!
//! These model the fixed-cost staking handlers' gas, return and revert outcomes.
//! [`InterpreterResult::apply`] converts a result into evm2 precompile semantics.

use alloy_primitives::Bytes;
use evm2::{
    interpreter::GasTracker,
    precompile::PrecompileOutput,
    precompiles::{PrecompileError, PrecompileHalt, PrecompileResult},
};

/// Out-of-gas classification for [`PrecompileHalt`], matching revm's `PrecompileHalt::is_oog`.
pub trait PrecompileHaltExt {
    /// Returns whether the halt is an out-of-gas halt.
    fn is_oog(&self) -> bool;
}

impl PrecompileHaltExt for PrecompileHalt {
    fn is_oog(&self) -> bool {
        matches!(self, Self::OutOfGas)
    }
}

/// Gas accounting for a single staking precompile call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gas {
    limit: u64,
    remaining: u64,
}

impl Gas {
    /// Creates gas accounting with `limit` gas available.
    pub const fn new(limit: u64) -> Self {
        Self { limit, remaining: limit }
    }

    /// Creates gas accounting with all `limit` gas spent.
    pub const fn new_spent_with_reservoir(limit: u64, _reservoir: u64) -> Self {
        Self { limit, remaining: 0 }
    }

    /// Records a regular gas cost, returning `false` when it exceeds the remaining gas.
    pub const fn record_regular_cost(&mut self, cost: u64) -> bool {
        if cost > self.remaining {
            return false;
        }
        self.remaining -= cost;
        true
    }

    /// Returns the remaining gas.
    pub const fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Returns the spent gas.
    pub const fn spent(&self) -> u64 {
        self.limit - self.remaining
    }
}

/// Outcome of a staking precompile call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionResult {
    /// Successful return.
    Return,
    /// Revert with output.
    Revert,
    /// Out-of-gas halt.
    PrecompileOOG,
}

/// Result of a staking precompile call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpreterResult {
    /// Call outcome.
    pub result: InstructionResult,
    /// Gas accounting.
    pub gas: Gas,
    /// Return or revert output.
    pub output: Bytes,
}

impl InterpreterResult {
    /// Applies this result to an evm2 gas tracker and converts it into a precompile result.
    pub fn apply(self, gas: &mut GasTracker) -> PrecompileResult {
        match self.result {
            InstructionResult::Return => {
                gas.spend(self.gas.spent())?;
                Ok(PrecompileOutput::new(self.output))
            }
            InstructionResult::Revert => {
                gas.spend(self.gas.spent())?;
                Err(PrecompileError::Revert(self.output))
            }
            InstructionResult::PrecompileOOG => Err(PrecompileHalt::OutOfGas.into()),
        }
    }
}
