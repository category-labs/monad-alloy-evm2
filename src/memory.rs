//! Memory expansion pricing and limits shared by active call frames.

use crate::MonadEvmTypes;
use evm2::{
    interpreter::{Gas, InstrStop, InterpreterState, Result},
    utils::num_words,
};

/// Total MIP-3 memory cost for a number of 32-byte words.
///
/// Expansion charges the difference between the new and previous totals. Growing
/// to one word costs zero gas; growing to 8 MiB costs 131,072 gas.
pub const fn monad_memory_cost(words: usize) -> u64 {
    (words as u64) / 2
}

pub(crate) fn resize<const MIP3: bool>(
    gas: &mut Gas,
    state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
    offset: usize,
    len: usize,
) -> Result {
    resize_with_gas_reserve::<MIP3>(gas, state, offset, len, 0)
}

/// Leaves enough gas for a charge the inherited instruction will make before expansion.
///
/// CREATE's initcode charge precedes memory expansion. Reserving that gas here
/// lets evm2 charge it once while retaining the same memory failure boundary.
pub(crate) fn resize_with_gas_reserve<const MIP3: bool>(
    gas: &mut Gas,
    state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
    offset: usize,
    len: usize,
    reserved_gas: u64,
) -> Result {
    let words = num_words(offset.saturating_add(len));
    #[cfg(feature = "memory_limit")]
    {
        let limit = state.version().memory_limit;
        let ancestors = state.host().ext().memory_pool_offset;
        if ancestors.saturating_add((words as u64).saturating_mul(32)) > limit {
            return Err(InstrStop::MemoryLimitOOG);
        }
    }
    if words <= gas.memory().words_num {
        return Ok(());
    }
    let size = words.checked_mul(32).ok_or(InstrStop::MemoryOOG)?;
    let cost = if MIP3 { monad_memory_cost(words) } else { state.gas_params().memory_cost(words) };
    let delta = gas.memory_mut().set_words_num(words, cost).ok_or(InstrStop::MemoryOOG)?;
    if delta > gas.remaining().saturating_sub(reserved_gas) {
        return Err(InstrStop::MemoryOOG);
    }
    gas.spend(delta).map_err(|_| InstrStop::MemoryOOG)?;
    state.memory().resize(0, size)
}
