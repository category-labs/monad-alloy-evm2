//! Monad transaction admission and gas settlement.
//!
//! The sender must cover the full gas limit at the effective gas price. Value
//! transfers take place during execution and can fail after the fee is paid.
//! Receipts count the full gas limit even when the interpreter finishes early.

use crate::{
    MonadEvmTypes, MonadHaltReason, MonadHardfork, MonadHost, MonadTxEnvelope, MonadTxResultExt,
    page::{PageAccess, StoragePageKey},
    reserve_balance::{begin_reserve_balance_tx, end_reserve_balance_tx, sync_reserve_balance},
};
use alloy_consensus::{Transaction, TxType, Typed2718};
use alloy_primitives::{Address, Bytes, U256, address};
use core::fmt;
use evm2::{
    EvmFeatures, TxResult,
    env::TxEnvExt,
    ethereum::{
        access_list_counts, charge_upfront, default_settle_gas, effective_gas_price,
        eip7702::{AuthRefunds, apply_auth_list, eip7702_authorization_gas},
        execute_initial_frame, initial_gas_and_reservoir, intrinsic_and_floor_gas,
        prepare_initial_frame, validate_block_gas_limit, validate_chain_id,
        validate_create_initcode, validate_execution_gas_limit_cap, validate_floor_gas,
        validate_gas_price, validate_intrinsic_gas, validate_nonce_not_overflow,
        validate_priority_fee, validate_sender_and_bump_nonce, validate_tx_gas_limit_cap,
        warm_access_list, warm_base_accounts,
    },
    handler::GasSettlement,
    interpreter::{GasTracker, InstrStop},
    registry::{HandlerError, HandlerResult, TxRegistry, TxRequest, handler},
};

/// Monad's protocol system sender. This account cannot authorize EIP-7702 delegation.
pub const SYSTEM_ADDRESS: Address = address!("6f49a8f621353f12378d0046e7d7e4b9b249dc9e");

/// An EIP-7702 authorization was signed by the Monad system sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemSenderIsAuthority;

impl fmt::Display for SystemSenderIsAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("system transaction sender is authority")
    }
}

impl core::error::Error for SystemSenderIsAuthority {}

pub(crate) fn tx_registry() -> TxRegistry<MonadEvmTypes, TxResult<MonadEvmTypes>> {
    let mut registry = TxRegistry::new();
    for ty in [TxType::Legacy, TxType::Eip2930, TxType::Eip1559, TxType::Eip7702] {
        let id = ty as u8;
        registry.register(id, move |tx| (tx.ty() == id).then_some(tx), handler(prepare, execute));
    }
    registry
}

type Request<'a, 'host> = TxRequest<'a, 'host, MonadEvmTypes, MonadTxEnvelope>;

struct PreparedTx {
    gas_price: U256,
    intrinsic: u64,
}

enum PreparedRequest {
    Transaction(PreparedTx),
    System(crate::system::SystemCall),
}

fn prepare(req: &mut Request<'_, '_>) -> HandlerResult<PreparedRequest> {
    let host = &mut *req.host;
    // Only the wrapper can install this private request. System execution bypasses
    // ordinary admission, nonce updates, fees, authorizations and reserve checks.
    if let Some(system) = host.ext_mut().system_call.take() {
        return Ok(PreparedRequest::System(system));
    }
    end_reserve_balance_tx(host);
    if host.ext().preserve_reserve_balance_tracker && !host.ext().reserve_balance.tracking_enabled()
    {
        return Err(HandlerError::external(MissingReserveContext));
    }
    let tx = req.envelope;
    let caller = req.tx.signer();
    let version = host.version();
    let basefee = host.block().basefee;
    let max_fee = U256::from(tx.max_fee_per_gas());
    let gas_price = if let Some(tip) = tx.max_priority_fee_per_gas() {
        let tip = U256::from(tip);
        validate_priority_fee(version, max_fee, tip)?;
        effective_gas_price(max_fee, tip, basefee)
    } else {
        max_fee
    };
    validate_gas_price(version, gas_price, basefee)?;
    validate_chain_id(version, tx.chain_id(), tx.as_legacy().is_some())?;
    validate_tx_gas_limit_cap(version, tx.gas_limit())?;
    validate_block_gas_limit(version, tx.gas_limit(), host.block().gas_limit)?;
    validate_create_initcode(version, tx.kind(), tx.input())?;
    validate_nonce_not_overflow(tx.nonce())?;

    let (accounts, slots) = tx.access_list().map(access_list_counts).unwrap_or_default();
    let (mut intrinsic, floor) = intrinsic_and_floor_gas(
        version,
        caller,
        tx.kind(),
        tx.input(),
        accounts,
        slots,
        tx.value(),
    );
    if let Some(set_code) = tx.as_eip7702() {
        let auths = &set_code.authorization_list;
        if auths.is_empty() {
            return Err(HandlerError::EmptyAuthorizationList);
        }
        if auths.iter().any(|auth| auth.authority() == Some(SYSTEM_ADDRESS)) {
            return Err(HandlerError::external(SystemSenderIsAuthority));
        }
        intrinsic = intrinsic.saturating_add(eip7702_authorization_gas(version, auths.len()));
    }
    validate_intrinsic_gas(tx.gas_limit(), intrinsic, 0)?;
    validate_floor_gas(tx.gas_limit(), floor)?;
    validate_execution_gas_limit_cap(version, tx.gas_limit(), intrinsic, floor)?;

    charge_sender(host, caller, tx, gas_price)?;
    warm_base_accounts(host, tx.kind());
    if let Some(access_list) = tx.access_list() {
        warm_access_list(host, access_list);
        if host.config_spec_id() >= MonadHardfork::MonadTen {
            for item in &access_list.0 {
                for slot in &item.storage_keys {
                    let key = StoragePageKey::from_slot(item.address, U256::from_be_bytes(slot.0));
                    PageAccess::load(host, key).record_read(host);
                }
            }
        }
    }
    Ok(PreparedRequest::Transaction(PreparedTx { gas_price, intrinsic }))
}

fn charge_sender(
    host: &mut MonadHost<'_>,
    caller: Address,
    tx: &MonadTxEnvelope,
    gas_price: U256,
) -> HandlerResult<()> {
    let fee = U256::from(tx.gas_limit()) * gas_price;
    let check_balance = host.feature(EvmFeatures::BALANCE_CHECK);
    // Simulations can top up the transfer value after the fee is deducted.
    let required = if check_balance { fee } else { U256::ZERO };
    validate_sender_and_bump_nonce(host, caller, tx.nonce(), required)?;
    charge_upfront(host, caller, fee)?;
    if !check_balance && host.feature(EvmFeatures::BALANCE_TOP_UP) && !tx.value().is_zero() {
        let mut sender = host.state_mut().account(&caller)?;
        if sender.balance() < tx.value() {
            sender.set_balance(tx.value());
        }
    }
    Ok(())
}

fn execute(
    req: Request<'_, '_>,
    prepared: PreparedRequest,
) -> HandlerResult<TxResult<MonadEvmTypes>> {
    match prepared {
        PreparedRequest::System(system) => system.execute(req.host),
        PreparedRequest::Transaction(prepared) => {
            let scope = ReserveScope { host: req.host };
            execute_inner(scope.host, req.tx.signer(), req.envelope, prepared)
        }
    }
}

struct ReserveScope<'e, 'a> {
    host: &'e mut MonadHost<'a>,
}

#[derive(Debug)]
struct MissingReserveContext;

impl fmt::Display for MissingReserveContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("preserved synthetic calls require an initialized reserve tracker")
    }
}

impl core::error::Error for MissingReserveContext {}

#[derive(Debug)]
struct MissingMonadExecutor;

impl fmt::Display for MissingMonadExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Monad transactions must execute through MonadEvm")
    }
}

impl core::error::Error for MissingMonadExecutor {}

impl Drop for ReserveScope<'_, '_> {
    fn drop(&mut self) {
        end_reserve_balance_tx(self.host);
    }
}

fn execute_inner(
    host: &mut MonadHost<'_>,
    caller: Address,
    tx: &MonadTxEnvelope,
    prepared: PreparedTx,
) -> HandlerResult<TxResult<MonadEvmTypes>> {
    let gas_limit = tx.gas_limit();
    let chain_id = host.version().chain_id;
    let PreparedTx { gas_price, intrinsic } = prepared;
    *host.tx_env_mut() = TxEnvExt {
        origin: caller,
        gas_price,
        chain_id: U256::from(chain_id),
        ..Default::default()
    };
    if let Some(set_code) = tx.as_eip7702() {
        // Authorization changes survive a reverted call. Monad discards their gas refunds.
        let mut refunds = AuthRefunds::new(host.version());
        apply_auth_list(host, chain_id, &set_code.authorization_list, &mut refunds)?;
    }
    let (execution_gas, reservoir) =
        initial_gas_and_reservoir(host.version(), gas_limit, intrinsic, 0);
    let mut gas = GasTracker::new_with_execution_gas_and_reservoir(execution_gas, reservoir);
    let frame = prepare_initial_frame(
        host,
        caller,
        tx.nonce(),
        tx.kind(),
        tx.input(),
        tx.value(),
        &mut gas,
    )?;
    begin_reserve_balance_tx(host, caller, gas_price, gas_limit)?;
    if !host.ext().preserve_reserve_balance_tracker {
        host.ext_mut().reserve_execution.checkpoint = Some(host.state().checkpoint());
        host.ext_mut().reserve_execution.depth = host.version().initial_call_depth;
    }
    let entered_execution = frame.is_some();
    let mut result = execute_initial_frame(host, frame, &mut gas, execution_gas, reservoir)?;
    if entered_execution && !host.ext().reserve_execution.observed {
        // Direct native-host entrypoints cannot compose the mandatory observer. Abort so
        // bypassing the wrapper never accepts execution without reserve adjudication.
        return Err(HandlerError::external(MissingMonadExecutor));
    }
    sync_reserve_balance(host)?;
    let reverted_for_reserve_balance = host.ext().reserve_execution.rejected;
    if reverted_for_reserve_balance {
        // A tracing callback may edit the native outcome; protocol rejection remains final.
        result.stop = InstrStop::Revert;
        result.output = Bytes::new();
        result.created_address = None;
    }
    result.gas.set_refunded(0);
    // A full-limit floor makes the receipt and beneficiary use the charged gas while
    // preserving actual execution consumption and the gas observed inside the EVM.
    let mut result = default_settle_gas(
        host,
        GasSettlement {
            caller,
            gas_price,
            gas_limit,
            floor_gas: gas_limit,
            initial_state_gas: 0,
            state_refund: 0,
            result,
        },
    )?;
    sync_reserve_balance(host)?;
    result.ext = MonadTxResultExt {
        dipped_into_reserve: host.ext().reserve_balance.has_violation(),
        reverted_for_reserve_balance,
        halt_reason: reverted_for_reserve_balance
            .then_some(MonadHaltReason::ReserveBalanceViolation),
    };
    Ok(result)
}
