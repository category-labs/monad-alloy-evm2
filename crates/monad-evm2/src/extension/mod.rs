//! MIP-7 extension decoding and selector dispatch on MonadNext.
//!
//! The prefix and selector form one instruction. Each handler defines its gas and
//! stack rules. Missing or unassigned selectors halt exceptionally. Legacy jump
//! destination analysis is unchanged.

use crate::{MonadEvmTypes, MonadHardfork, frame_hardfork};
use evm2::interpreter::InstrStop;
use evm2_macros::instruction;

mod call_stack;
pub use call_stack::{CALLERN_SELECTOR, CALLSTACKDEPTH_SELECTOR};

/// MIP-7 extension opcode prefix.
pub const EXTENSION: u8 = 0xae;

#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
pub(crate) fn extension(cx: _) -> Result {
    if frame_hardfork(cx.state.host()) != MonadHardfork::MonadNext {
        return Err(InstrStop::OpcodeNotFound);
    }
    let offset = cx.state.bytecode().pc_offset(*cx.pc);
    let code = cx.state.message().code.original_byte_slice();
    let selector = *code.get(offset + 1).ok_or(InstrStop::InvalidFEOpcode)?;
    let terminal = offset + 2 == code.len();
    match selector {
        CALLSTACKDEPTH_SELECTOR => call_stack::callstackdepth::execute(cx.pc, stack, cx.state)?,
        CALLERN_SELECTOR => call_stack::callern::execute(cx.pc, stack, cx.state)?,
        _ => return Err(InstrStop::InvalidFEOpcode),
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
