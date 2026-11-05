//! MIP-8 SLOAD and SSTORE, enabled from Monad Ten.

use crate::{
    MonadEvmTypes,
    page::{PageAccess, StoragePageKey},
};
use evm2::{
    interpreter::{Host, InstrStop},
    version::GasId,
};
use evm2_macros::instruction;

#[instruction(EvmTypes = MonadEvmTypes, dynamic_gas)]
pub(super) fn sload(cx: _, [key]: [Word]) -> Result<out> {
    let address = cx.state.message().destination;
    let mut page = PageAccess::load(cx.state.host(), StoragePageKey::from_slot(address, *key));
    let cold_cost = if page.is_cold() {
        cx.state.gas_params().get(GasId::ColdStorageAdditionalCost)
    } else {
        0
    };
    if cx.gas.remaining() < cold_cost {
        return Err(InstrStop::OutOfGas);
    }
    // Slot warmth cannot reject a load: another slot may have warmed this page.
    let loaded =
        cx.state.host().sload(&address, key, false).map_err(|error| cx.state.fail(error))?;
    page.record_read(cx.state.host());
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
    let mut page = PageAccess::load(cx.state.host(), StoragePageKey::from_slot(address, key));
    let cold_cost = if page.is_cold() {
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
    let cost = page.record_write(cx.state.host(), stored.present_value, stored.new_value);
    // Monad does not give Ethereum's clearing or reset-to-original refunds.
    cx.gas.spend(cost + cold_cost)?;
}
