//! MIP-8 SLOAD and SSTORE, enabled from Monad Ten.

use crate::{
    MonadEvmTypes,
    page::{PageAccess, StoragePageKey, is_page_cold},
};
use evm2::{
    interpreter::{Host, InstrStop},
    version::GasId,
};
use evm2_macros::instruction;

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn sload(cx: _, [key]: [Word]) -> Result<out> {
    let address = cx.state.message().destination;
    let cold =
        is_page_cold(cx.state.host(), address, *key).map_err(|error| cx.state.fail(error))?;
    let cold_cost =
        if cold { cx.state.gas_params().get(GasId::ColdStorageAdditionalCost) } else { 0 };
    if cx.gas.remaining() < cold_cost {
        return Err(InstrStop::OutOfGas);
    }
    // Slot warmth cannot reject a load: another slot may have warmed this page.
    let loaded =
        cx.state.host().sload(&address, key, false).map_err(|error| cx.state.fail(error))?;
    cx.gas.spend(cold_cost)?;
    *out = loaded.value;
}

#[instruction(EvmTypes = MonadEvmTypes, no_stack_preamble, dynamic_gas)]
pub(super) fn sstore(cx: _) -> Result {
    if cx.state.is_static() {
        return Err(InstrStop::StateChangeDuringStaticCall);
    }
    let [key, value] = stack.popn()?;
    if cx.gas.remaining() <= cx.state.gas_params().get(GasId::CallStipend) {
        return Err(InstrStop::ReentrancySentryOOG);
    }
    let address = cx.state.message().destination;
    let cold = is_page_cold(cx.state.host(), address, key).map_err(|error| cx.state.fail(error))?;
    let cold_cost = if cold {
        if cx.gas.remaining() < cx.state.gas_params().get(GasId::ColdStorageCost) {
            return Err(InstrStop::OutOfGas);
        }
        cx.state.gas_params().get(GasId::ColdStorageAdditionalCost)
    } else {
        0
    };
    let stored = cx
        .state
        .host()
        .sstore(&address, &key, &value, false)
        .map_err(|error| cx.state.fail(error))?;
    let cost = PageAccess::load(cx.state.host(), StoragePageKey::from_slot(address, key))
        .record_write(cx.state.host(), stored.present_value, stored.new_value);
    // Monad does not give Ethereum's clearing or reset-to-original refunds.
    cx.gas.spend(cost + cold_cost)?;
}
