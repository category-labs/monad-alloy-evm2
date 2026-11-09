//! Memory-expanding instructions with fork-specific pricing and pooled limits.

use crate::{MonadEvmTypes, memory::resize};
use alloy_primitives::{B256, Bytes, KECCAK256_EMPTY, keccak256 as hash};
use evm2::{
    interpreter::{Gas, Host, InstrStop, InterpreterState, Result, Word},
    utils::WordExt,
};
use evm2_macros::instruction;

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn mload<const MIP3: bool>(cx: _, [offset]: [Word]) -> Result<out> {
    let offset = offset.to_usize()?;
    resize::<MIP3>(cx.gas, cx.state, offset, 32)?;
    *out = cx.state.memory().get_word(offset);
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn mstore<const MIP3: bool>(cx: _, [offset, value]: [Word]) -> Result {
    let offset = offset.to_usize()?;
    resize::<MIP3>(cx.gas, cx.state, offset, 32)?;
    cx.state.memory().set(offset, &value.to_be_bytes::<32>());
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn mstore8<const MIP3: bool>(cx: _, [offset, value]: [Word]) -> Result {
    let offset = offset.to_usize()?;
    resize::<MIP3>(cx.gas, cx.state, offset, 1)?;
    cx.state.memory().set(offset, &[value.byte(0)]);
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn mcopy<const MIP3: bool>(cx: _, [dst, src, len]: [Word]) -> Result {
    let len = len.to_usize()?;
    cx.gas.spend(cx.state.gas_params().mcopy_cost(len))?;
    if len != 0 {
        let dst = dst.to_usize()?;
        let src = src.to_usize()?;
        resize::<MIP3>(cx.gas, cx.state, dst.max(src), len)?;
        cx.state.memory().copy(dst, src, len);
    }
    Ok(())
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn keccak256<const MIP3: bool>(cx: _, [offset, len]: [Word]) -> Result<out> {
    let len = len.to_usize()?;
    cx.gas.spend(cx.state.gas_params().keccak256_word_cost(len))?;
    let digest = if len == 0 {
        KECCAK256_EMPTY
    } else {
        let offset = offset.to_usize()?;
        resize::<MIP3>(cx.gas, cx.state, offset, len)?;
        hash(cx.state.memory().slice(offset, len))
    };
    *out = Word::from_be_bytes(digest.0);
}

fn copy<const MIP3: bool>(
    gas: &mut Gas,
    state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
    offset: Word,
    source: Word,
    len: usize,
    data: &[u8],
) -> Result {
    gas.spend(state.gas_params().copy_cost(len))?;
    if len != 0 {
        let offset = offset.to_usize()?;
        resize::<MIP3>(gas, state, offset, len)?;
        state.memory().set_data(offset, source.to_usize_saturated(), len, data);
    }
    Ok(())
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn calldatacopy<const MIP3: bool>(cx: _, [offset, source, len]: [Word]) -> Result {
    let len = len.to_usize()?;
    let input = cx.state.message().input.clone();
    copy::<MIP3>(cx.gas, cx.state, *offset, *source, len, &input)
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn returndatacopy<const MIP3: bool>(cx: _, [offset, source, len]: [Word]) -> Result {
    let len = len.to_usize()?;
    let source = source.to_usize_saturated();
    if source.saturating_add(len) > cx.state.return_data().len() {
        return Err(InstrStop::OutOfOffset);
    }
    let data = cx.state.return_data().clone();
    copy::<MIP3>(cx.gas, cx.state, *offset, Word::from(source), len, &data)
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn extcodecopy<const MIP3: bool>(
    cx: _,
    [address, offset, source, len]: [Word],
) -> Result {
    let len = len.to_usize()?;
    cx.gas.spend(cx.state.gas_params().extcodecopy_cost(len))?;
    let offset = if len == 0 {
        0
    } else {
        let offset = offset.to_usize()?;
        resize::<MIP3>(cx.gas, cx.state, offset, len)?;
        offset
    };
    let cold_cost = cx.state.gas_params().cold_account_additional_cost();
    let skip_cold_load = cx.gas.remaining() < cold_cost;
    let account = cx
        .state
        .host()
        .load_account(&address.to_address(), true, skip_cold_load)
        .map_err(|error| cx.state.fail(error))?;
    if account.is_cold {
        cx.gas.spend(cold_cost)?;
    }
    cx.state.memory().set_data(
        offset,
        source.to_usize_saturated(),
        len,
        account.code.original_byte_slice(),
    );
}

#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
pub(super) fn log<const TOPICS: usize, const MIP3: bool>(cx: _) -> Result {
    if cx.state.is_static() {
        return Err(InstrStop::StateChangeDuringStaticCall);
    }
    let [offset, len] = stack.popn()?;
    let len = len.to_usize()?;
    cx.gas.spend(cx.state.gas_params().log_cost(TOPICS as u8, len))?;
    let data = if len == 0 {
        Bytes::new()
    } else {
        let offset = offset.to_usize()?;
        resize::<MIP3>(cx.gas, cx.state, offset, len)?;
        Bytes::copy_from_slice(cx.state.memory().slice(offset, len))
    };
    let mut topics = [B256::ZERO; TOPICS];
    for (topic, word) in topics.iter_mut().zip(stack.popn_dyn(TOPICS)?) {
        *topic = B256::from(word.to_be_bytes::<32>());
    }
    let address = cx.state.message().destination;
    Host::log(cx.state.host(), &address, topics.as_slice().into(), data.as_ref().into());
    Ok(())
}

fn finish<const MIP3: bool>(
    gas: &mut Gas,
    state: &mut InterpreterState<'_, '_, MonadEvmTypes>,
    offset: Word,
    len: Word,
    stop: InstrStop,
) -> Result {
    let len = len.to_usize()?;
    let output = if len == 0 {
        0..0
    } else {
        let offset = offset.to_usize()?;
        let end = offset.checked_add(len).ok_or(InstrStop::MemoryOOG)?;
        if end > u32::MAX as usize {
            return Err(InstrStop::MemoryLimitOOG);
        }
        resize::<MIP3>(gas, state, offset, len)?;
        offset as u32..end as u32
    };
    state.set_output(output);
    Err(stop)
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn ret<const MIP3: bool>(cx: _, [offset, len]: [Word]) -> Result {
    finish::<MIP3>(cx.gas, cx.state, *offset, *len, InstrStop::Return)
}

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn revert<const MIP3: bool>(cx: _, [offset, len]: [Word]) -> Result {
    finish::<MIP3>(cx.gas, cx.state, *offset, *len, InstrStop::Revert)
}
