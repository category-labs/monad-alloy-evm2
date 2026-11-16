//! MIP-7 extension decoding and MIP-18 call-stack introspection on MonadNext.
//!
//! The prefix and selector form one instruction. Both assigned selectors cost two
//! gas; other selectors and a missing selector halt exceptionally. Legacy jump
//! destination analysis is unchanged.

use crate::{MonadEvmTypes, MonadHardfork, frame_hardfork};
use alloy_primitives::U256;
use evm2::interpreter::InstrStop;
use evm2_macros::instruction;

/// MIP-7 extension opcode prefix.
pub const EXTENSION: u8 = 0xae;
/// MIP-18 selector for the executing frame's zero-based depth.
pub const CALLSTACKDEPTH_SELECTOR: u8 = 0x00;
/// MIP-18 selector for the caller of the frame `n` levels above the current frame.
pub const CALLERN_SELECTOR: u8 = 0x01;

#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
pub(crate) fn extension(cx: _) -> Result {
    if frame_hardfork(cx.state.host()) != MonadHardfork::MonadNext {
        return Err(InstrStop::OpcodeNotFound);
    }
    let offset = cx.state.bytecode().pc_offset(*cx.pc);
    let code = cx.state.message().code.original_byte_slice();
    let selector = *code.get(offset + 1).ok_or(InstrStop::InvalidFEOpcode)?;
    if !matches!(selector, CALLSTACKDEPTH_SELECTOR | CALLERN_SELECTOR) {
        return Err(InstrStop::InvalidFEOpcode);
    }
    let terminal = offset + 2 == code.len();
    cx.gas.spend(2)?;
    let message = cx.state.message();
    let depth = usize::from(message.depth);
    if selector == CALLSTACKDEPTH_SELECTOR {
        stack.push(U256::from(depth))?;
    } else {
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
    }
    if terminal {
        // A final zero selector can be the last allocated byte. Stop without
        // moving the PC so step_end inspectors can still safely read opcode().
        return Err(InstrStop::Stop);
    }
    // SAFETY: The selector and a following opcode are in the original code.
    // Dispatch advances one more byte after this instruction succeeds.
    unsafe { cx.pc.advance_unchecked(1) };
    Ok(())
}
