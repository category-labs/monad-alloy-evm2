//! Monad memory and storage pricing, pooled limits and delegation restrictions.

use crate::{MonadEvmTypes, memory::resize_with_gas_reserve};
use evm2::{
    EvmFeatures, OpcodeConfig,
    interpreter::{
        Host, InstrStop, InterpreterState, Pc, Result, StackMut, gas, instructions, op,
        private::Instruction,
    },
    utils::WordExt,
};
use evm2_macros::instruction;

mod memory;
mod storage;

type Mip3Instruction<New, Old> =
    ForkInstruction<New, Old, { crate::MonadHardfork::MonadNine as u8 }>;
type Mip8Instruction<New, Old> =
    ForkInstruction<New, Old, { crate::MonadHardfork::MonadTen as u8 }>;

pub(crate) const fn configure(config: &mut OpcodeConfig<MonadEvmTypes>) {
    config.set_instruction::<Mip3Instruction<memory::mload<true>, memory::mload<false>>>(
        op::MLOAD,
        gas::VERYLOW as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::mstore<true>, memory::mstore<false>>>(
        op::MSTORE,
        gas::VERYLOW as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::mstore8<true>, memory::mstore8<false>>>(
        op::MSTORE8,
        gas::VERYLOW as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::mcopy<true>, memory::mcopy<false>>>(
        op::MCOPY,
        gas::VERYLOW as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::keccak256<true>, memory::keccak256<false>>>(
        op::KECCAK256,
        gas::KECCAK256 as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::calldatacopy<true>, memory::calldatacopy<false>>>(op::CALLDATACOPY, gas::VERYLOW as u16);
    config.set_instruction::<Mip3Instruction<codecopy<true>, codecopy<false>>>(
        op::CODECOPY,
        gas::VERYLOW as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::returndatacopy<true>, memory::returndatacopy<false>>>(op::RETURNDATACOPY, gas::VERYLOW as u16);
    config
        .set_instruction::<Mip3Instruction<memory::extcodecopy<true>, memory::extcodecopy<false>>>(
            op::EXTCODECOPY,
            gas::WARM_STORAGE_READ_COST as u16,
        );
    config.set_instruction::<Mip3Instruction<memory::log<0, true>, memory::log<0, false>>>(
        op::LOG0,
        gas::LOG as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::log<1, true>, memory::log<1, false>>>(
        op::LOG1,
        gas::LOG as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::log<2, true>, memory::log<2, false>>>(
        op::LOG2,
        gas::LOG as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::log<3, true>, memory::log<3, false>>>(
        op::LOG3,
        gas::LOG as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::log<4, true>, memory::log<4, false>>>(
        op::LOG4,
        gas::LOG as u16,
    );
    config.set_instruction::<Mip3Instruction<memory::ret<true>, memory::ret<false>>>(op::RETURN, 0);
    config.set_instruction::<Mip3Instruction<memory::revert<true>, memory::revert<false>>>(
        op::REVERT,
        0,
    );
    config.set_instruction::<Mip3Instruction<call<{ op::CALL }, true>, call<{ op::CALL }, false>>>(
        op::CALL,
        gas::WARM_STORAGE_READ_COST as u16,
    );
    config.set_instruction::<Mip3Instruction<call<{ op::CALLCODE }, true>, call<{ op::CALLCODE }, false>>>(op::CALLCODE, gas::WARM_STORAGE_READ_COST as u16);
    config.set_instruction::<Mip3Instruction<call<{ op::DELEGATECALL }, true>, call<{ op::DELEGATECALL }, false>>>(op::DELEGATECALL, gas::WARM_STORAGE_READ_COST as u16);
    config.set_instruction::<Mip3Instruction<call<{ op::STATICCALL }, true>, call<{ op::STATICCALL }, false>>>(op::STATICCALL, gas::WARM_STORAGE_READ_COST as u16);
    config.set_instruction::<Mip3Instruction<create<false, true>, create<false, false>>>(
        op::CREATE,
        0,
    );
    config.set_instruction::<Mip3Instruction<create<true, true>, create<true, false>>>(
        op::CREATE2,
        0,
    );
    config.set_instruction::<Mip8Instruction<storage::sload, instructions::sload<MonadEvmTypes>>>(
        op::SLOAD,
        gas::WARM_STORAGE_READ_COST as u16,
    );
    config
        .set_instruction::<Mip8Instruction<storage::sstore, instructions::sstore<MonadEvmTypes>>>(
            op::SSTORE,
            0,
        );
    config.set_instruction::<clz>(op::CLZ, 0);
}

/// Dispatches only the rules that differ between the supported Monad frames.
/// Their inherited feature flags and dynamic gas parameters are identical.
struct ForkInstruction<New, Old, const SINCE: u8>(core::marker::PhantomData<(New, Old)>);

impl<New: Instruction<MonadEvmTypes>, Old: Instruction<MonadEvmTypes>, const SINCE: u8>
    Instruction<MonadEvmTypes> for ForkInstruction<New, Old, SINCE>
{
    const DYNAMIC_GAS: bool = New::DYNAMIC_GAS || Old::DYNAMIC_GAS;
    const MESSAGE: bool = New::MESSAGE || Old::MESSAGE;

    fn execute(
        pc: &mut Pc,
        stack: StackMut<'_>,
        state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
    ) -> Result {
        if crate::frame_hardfork(state.host()) as u8 >= SINCE {
            New::execute(pc, stack, state)
        } else {
            Old::execute(pc, stack, state)
        }
    }
}

/// CLZ activation is frame-local; unavailable frames fail before stack checks.
#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
fn clz(cx: _) -> Result {
    if crate::frame_hardfork(cx.state.host()) < crate::MonadHardfork::MonadNine {
        return Err(InstrStop::NotActivated);
    }
    cx.gas.spend(u64::from(gas::LOW))?;
    instructions::clz::<MonadEvmTypes>::execute(cx.pc, stack, cx.state)
}

/// Keeps the suspended frame's allocation in the pool until the inherited handler returns.
fn execute_message<I: Instruction<MonadEvmTypes>>(
    pc: &mut Pc,
    stack: StackMut<'_>,
    state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
) -> Result {
    let frame_memory = state.memory().len() as u64;
    let ancestors = state.host().ext().memory_pool_offset;
    state.host().ext_mut().memory_pool_offset =
        ancestors.checked_add(frame_memory).ok_or(InstrStop::MemoryLimitOOG)?;
    let result = I::execute(pc, stack, state);
    // Restore on success, exceptional halt, inspector override and database error.
    state.host().ext_mut().memory_pool_offset = ancestors;
    result
}

/// Expands input and output memory before evm2 prepares and executes a call.
#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas, message)]
fn call<const OP: u8, const MIP3: bool>(cx: _) -> Result {
    let has_value = matches!(OP, op::CALL | op::CALLCODE);
    let args = if has_value { 7 } else { 6 };
    if stack.len() >= args
        && !(OP == op::CALL && cx.state.is_static() && stack.peek(2).is_some_and(|v| !v.is_zero()))
    {
        let first = if has_value { 3 } else { 2 };
        let prepared = (|| {
            for index in [first, first + 2] {
                let len = stack.peek(index + 1).unwrap().to_usize()?;
                if len != 0 {
                    let offset = stack.peek(index).unwrap().to_usize()?;
                    crate::memory::resize::<MIP3>(cx.gas, cx.state, offset, len)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            stack.popn_dyn(args)?.for_each(drop);
            return Err(error);
        }
    }
    match OP {
        op::CALL => execute_message::<instructions::call<MonadEvmTypes>>(cx.pc, stack, cx.state),
        op::CALLCODE => {
            execute_message::<instructions::callcode<MonadEvmTypes>>(cx.pc, stack, cx.state)
        }
        op::DELEGATECALL => {
            execute_message::<instructions::delegatecall<MonadEvmTypes>>(cx.pc, stack, cx.state)
        }
        op::STATICCALL => {
            execute_message::<instructions::staticcall<MonadEvmTypes>>(cx.pc, stack, cx.state)
        }
        _ => unreachable!("unsupported call opcode"),
    }
}

/// CREATE/CREATE2 with Monad's restriction on delegated execution contexts.
#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas, message)]
fn create<const IS_CREATE2: bool, const MIP3: bool>(cx: _) -> Result {
    if cx.state.is_static() {
        return Err(InstrStop::StateChangeDuringStaticCall);
    }
    // Inspect the storage context, which survives CALLCODE and DELEGATECALL.
    let target = cx.state.message().destination;
    let account =
        cx.state.host().load_account(&target, true, false).map_err(|error| cx.state.fail(error))?;
    if account.code.is_eip7702() {
        return Err(InstrStop::NotActivated);
    }

    let args = if IS_CREATE2 { 4 } else { 3 };
    if stack.len() >= args {
        let prepared = (|| {
            let len = stack.peek(2).unwrap().to_usize()?;
            let initcode_cost = if cx.state.feature(EvmFeatures::EIP3860) {
                if len > cx.state.version().max_initcode_size {
                    return Err(InstrStop::CreateInitCodeSizeLimit);
                }
                cx.state.gas_params().initcode_cost(len)
            } else {
                0
            };
            if cx.gas.remaining() < initcode_cost {
                return Err(InstrStop::OutOfGas);
            }
            if len != 0 {
                let offset = stack.peek(1).unwrap().to_usize()?;
                resize_with_gas_reserve::<MIP3>(cx.gas, cx.state, offset, len, initcode_cost)?;
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            stack.popn_dyn(args)?.for_each(drop);
            return Err(error);
        }
    }
    execute_message::<instructions::create<MonadEvmTypes, IS_CREATE2>>(cx.pc, stack, cx.state)
}

/// Uses the active interpreter bytecode, including changes made by an inspector.
#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
fn codecopy<const MIP3: bool>(cx: _) -> Result {
    if let Some([offset, _, len]) = stack.peekn::<3>() {
        let prepared = (|| {
            let len = len.to_usize()?;
            let copy_cost = cx.state.gas_params().copy_cost(len);
            if cx.gas.remaining() < copy_cost {
                return Err(InstrStop::OutOfGas);
            }
            if len != 0 {
                resize_with_gas_reserve::<MIP3>(
                    cx.gas,
                    cx.state,
                    offset.to_usize()?,
                    len,
                    copy_cost,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            stack.popn::<3>()?;
            return Err(error);
        }
    }
    instructions::codecopy::<MonadEvmTypes>::execute(cx.pc, stack, cx.state)
}
