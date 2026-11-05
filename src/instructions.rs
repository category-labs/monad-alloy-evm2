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

pub(crate) const fn configure_mip8(config: &mut OpcodeConfig<MonadEvmTypes>) {
    config.set_instruction::<storage::sload>(op::SLOAD, crate::page::BASE_COST as u16);
    config.set_instruction::<storage::sstore>(op::SSTORE, 0);
}

pub(crate) const fn configure<const MIP3: bool>(config: &mut OpcodeConfig<MonadEvmTypes>) {
    config.set_instruction::<memory::mload<MIP3>>(op::MLOAD, gas::VERYLOW as u16);
    config.set_instruction::<memory::mstore<MIP3>>(op::MSTORE, gas::VERYLOW as u16);
    config.set_instruction::<memory::mstore8<MIP3>>(op::MSTORE8, gas::VERYLOW as u16);
    config.set_instruction::<memory::mcopy<MIP3>>(op::MCOPY, gas::VERYLOW as u16);
    config.set_instruction::<memory::keccak256<MIP3>>(op::KECCAK256, gas::KECCAK256 as u16);
    config.set_instruction::<memory::calldatacopy<MIP3>>(op::CALLDATACOPY, gas::VERYLOW as u16);
    config.set_instruction::<codecopy<MIP3>>(op::CODECOPY, gas::VERYLOW as u16);
    config.set_instruction::<memory::returndatacopy<MIP3>>(op::RETURNDATACOPY, gas::VERYLOW as u16);
    config.set_instruction::<memory::extcodecopy<MIP3>>(
        op::EXTCODECOPY,
        gas::WARM_STORAGE_READ_COST as u16,
    );
    config.set_instruction::<memory::log<0, MIP3>>(op::LOG0, gas::LOG as u16);
    config.set_instruction::<memory::log<1, MIP3>>(op::LOG1, gas::LOG as u16);
    config.set_instruction::<memory::log<2, MIP3>>(op::LOG2, gas::LOG as u16);
    config.set_instruction::<memory::log<3, MIP3>>(op::LOG3, gas::LOG as u16);
    config.set_instruction::<memory::log<4, MIP3>>(op::LOG4, gas::LOG as u16);
    config.set_instruction::<memory::ret<MIP3>>(op::RETURN, 0);
    config.set_instruction::<memory::revert<MIP3>>(op::REVERT, 0);
    config
        .set_instruction::<call<{ op::CALL }, MIP3>>(op::CALL, gas::WARM_STORAGE_READ_COST as u16);
    config.set_instruction::<call<{ op::CALLCODE }, MIP3>>(
        op::CALLCODE,
        gas::WARM_STORAGE_READ_COST as u16,
    );
    config.set_instruction::<call<{ op::DELEGATECALL }, MIP3>>(
        op::DELEGATECALL,
        gas::WARM_STORAGE_READ_COST as u16,
    );
    config.set_instruction::<call<{ op::STATICCALL }, MIP3>>(
        op::STATICCALL,
        gas::WARM_STORAGE_READ_COST as u16,
    );
    config.set_instruction::<create<false, MIP3>>(op::CREATE, 0);
    config.set_instruction::<create<true, MIP3>>(op::CREATE2, 0);
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
