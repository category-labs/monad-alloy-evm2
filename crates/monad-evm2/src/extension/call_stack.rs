//! MIP-18 call-stack introspection instructions.

use crate::MonadEvmTypes;
use alloy_primitives::U256;
use evm2_macros::instruction;

/// MIP-18 selector for the executing frame's zero-based depth.
pub const CALLSTACKDEPTH_SELECTOR: u8 = 0x00;
/// MIP-18 selector for the caller of the frame `n` levels above the current frame.
pub const CALLERN_SELECTOR: u8 = 0x01;

#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
pub(super) fn callstackdepth(cx: _) -> Result {
    cx.gas.spend(2)?;
    stack.push(U256::from(cx.state.message().depth))?;
    Ok(())
}

#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
pub(super) fn callern(cx: _) -> Result {
    cx.gas.spend(2)?;
    let message = cx.state.message();
    let depth = usize::from(message.depth);
    let (_, n) = stack.popn_top::<0>()?;
    *n = if *n > U256::from(depth) {
        U256::ZERO
    } else if n.is_zero() {
        message.caller.into_word().into()
    } else {
        // Synthetic hosts may start above depth zero without ancestor context.
        cx.state
            .host()
            .ext()
            .frame
            .callers
            .get(depth - n.to::<usize>())
            .copied()
            .unwrap_or_default()
            .into_word()
            .into()
    };
    Ok(())
}
