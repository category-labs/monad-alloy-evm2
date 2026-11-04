//! Monad instruction overrides for delegated execution.

use crate::MonadEvmTypes;
use evm2::interpreter::{
    Host, InstrStop, InterpreterState, Pc, Result, StackMut, instructions, private::Instruction,
};

/// CREATE/CREATE2 with Monad's restriction on delegated execution contexts.
pub(crate) struct Create<const IS_CREATE2: bool>;

impl<const IS_CREATE2: bool> Instruction<MonadEvmTypes> for Create<IS_CREATE2> {
    const MESSAGE: bool = true;

    fn execute(
        pc: &mut Pc,
        stack: StackMut<'_>,
        state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
    ) -> Result {
        if state.is_static() {
            return Err(InstrStop::StateChangeDuringStaticCall);
        }

        // Inspect the storage context, which survives CALLCODE and DELEGATECALL.
        let target = state.message().destination;
        let account =
            state.host().load_account(&target, true, false).map_err(|error| state.fail(error))?;
        if account.code.is_eip7702() {
            return Err(InstrStop::NotActivated);
        }

        instructions::create::<MonadEvmTypes, IS_CREATE2>::execute(pc, stack, state)
    }
}
