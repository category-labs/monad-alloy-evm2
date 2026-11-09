//! Monad staking queries and journaled storage at address 0x1000.
//!
//! Validator registration follows monad-revm 0.8.0: it does not verify signatures
//! and uses simplified public-key lookup identifiers. It is not equivalent to
//! validator admission in the C++ consensus client.

pub mod abi;
pub mod constants;
mod decode;
pub mod interface;
pub mod result;
pub mod storage;
pub mod types;
pub mod write;

pub use storage::STAKING_ADDRESS;
pub use types::{Delegator, EpochInfo, ListNode, Validator, WithdrawalRequest};

use crate::MonadEvmTypes;
use abi::gas;
use alloc::{format, string::ToString, vec, vec::Vec};
use alloy_primitives::{Address, Bytes, Log, U256};
use alloy_sol_types::SolCall;
use decode::decode_fixed;
use evm2::{
    Evm, ExecutionError, HostError,
    interpreter::{GasTracker, Host, Message, MessageKind},
    precompiles::{PrecompileError, PrecompileHalt, PrecompileResult},
};
use interface::IMonadStaking::*;
use result::{Gas, InstructionResult, InterpreterResult, PrecompileHaltExt};
use storage::{
    delegator_key, delegator_offsets, global_slots, validator_key, validator_offsets, valset_slots,
    withdrawal_key, withdrawal_offsets,
};

/// Maximum validator IDs returned by one validator-set query.
const MAX_VALIDATORS_PER_CALL: u32 = 100;
/// Maximum entries returned by one delegation-list query.
const LINKED_LIST_PAGINATION: u32 = 50;

/// Executes a staking operation with journaled storage and fixed protocol gas.
pub fn run_staking_precompile(
    evm: &mut Evm<'_, MonadEvmTypes>,
    message: &Message<MonadEvmTypes>,
    gas: &mut GasTracker,
) -> PrecompileResult {
    if message.kind != MessageKind::Call
        || message.caller_is_static
        || message.destination != message.code_address
    {
        gas.spend_all();
        return Err(PrecompileError::Revert(Bytes::new()));
    }
    let mut storage = EvmStakingStorage { evm, error: None };
    let result = if message
        .input
        .get(..4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .is_some_and(write::is_write_selector)
    {
        write::run_staking_write(
            &message.input,
            message.gas_limit,
            &mut storage,
            &message.caller,
            message.value,
        )
    } else {
        run_staking_with_reader(&message.input, message.gas_limit, &mut storage, message.value)
    };
    if let Some(error) = storage.error.take() {
        return Err(error);
    }
    result.apply(gas)
}

/// Converts a handler outcome into a precompile result, consuming all gas on errors.
fn handler_result(
    gas_limit: u64,
    result: Result<(u64, Bytes), PrecompileHalt>,
) -> InterpreterResult {
    match result {
        Ok((gas_used, output)) => {
            let mut interpreter_result = InterpreterResult {
                result: InstructionResult::Return,
                gas: Gas::new(gas_limit),
                output,
            };
            if !interpreter_result.gas.record_regular_cost(gas_used) {
                interpreter_result.result = InstructionResult::PrecompileOOG;
            }
            interpreter_result
        }
        Err(e) => {
            // C++ returns EVMC_REVERT with gas_left=0 (consumes all gas on revert).
            let mut gas = Gas::new(gas_limit);
            let _ = gas.record_regular_cost(gas_limit);
            InterpreterResult {
                result: if e.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::Revert
                },
                gas,
                output: Bytes::copy_from_slice(e.to_string().as_bytes()),
            }
        }
    }
}

/// Staking storage backed by an evm2 host.
///
/// Storage accesses go through the host so they are journaled and reverted with the call frame.
/// Database and fatal errors are recorded and surfaced after the handler returns.
#[derive(Debug)]
struct EvmStakingStorage<'e, 'a> {
    evm: &'e mut Evm<'a, MonadEvmTypes>,
    error: Option<PrecompileError>,
}

impl EvmStakingStorage<'_, '_> {
    fn record(&mut self, error: ExecutionError) {
        if self.error.is_none() {
            self.error = Some(match error {
                ExecutionError::Database(error) => PrecompileError::Database(error),
                ExecutionError::Fatal(error) => PrecompileError::Fatal(error),
            });
        }
    }

    fn host_error(&mut self, context: &str, error: HostError) -> PrecompileHalt {
        let message = format!("{context}: {error:?}");
        if let HostError::Execution(error) = error {
            self.record(error);
        }
        PrecompileHalt::Other(message.into())
    }
}

impl StorageReader for EvmStakingStorage<'_, '_> {
    fn sload(&mut self, key: U256) -> Result<U256, PrecompileHalt> {
        match Host::sload(self.evm, &STAKING_ADDRESS, &key, false) {
            Ok(load) => Ok(load.value),
            Err(error) => Err(self.host_error("Storage read failed", error)),
        }
    }
}

impl write::StakingStorage for EvmStakingStorage<'_, '_> {
    fn sstore(&mut self, key: U256, value: U256) -> Result<(), PrecompileHalt> {
        match Host::sstore(self.evm, &STAKING_ADDRESS, &key, &value, false) {
            Ok(_) => Ok(()),
            Err(error) => Err(self.host_error("Storage write failed", error)),
        }
    }

    fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<(), PrecompileHalt> {
        if amount.is_zero() {
            return Ok(());
        }
        match self.evm.state_mut().transfer(&from, &to, &amount) {
            Ok(true) => Ok(()),
            Ok(false) => Err(PrecompileHalt::Other("Transfer failed: OutOfFunds".into())),
            Err(error) => {
                let message = format!("Transfer error: {error:?}");
                self.record(ExecutionError::Database(error));
                Err(PrecompileHalt::Other(message.into()))
            }
        }
    }

    fn emit_log(&mut self, log: Log) -> Result<(), PrecompileHalt> {
        self.evm.log(log);
        Ok(())
    }
}

/// Storage access for staking queries.
pub trait StorageReader {
    /// Read a U256 value from storage at the given key.
    fn sload(&mut self, key: U256) -> Result<U256, PrecompileHalt>;
}

/// Evaluates staking queries using a storage reader.
///
/// This reads stored delegator fields without settling pending rewards. EVM calls
/// use the mutating delegator handler instead.
pub fn run_staking_with_reader<R: StorageReader>(
    input: &[u8],
    gas_limit: u64,
    reader: &mut R,
    call_value: U256,
) -> InterpreterResult {
    // C++ routes short input to fallback with 40k gas cost
    let selector: [u8; 4] = match input.get(..4).and_then(|s| s.try_into().ok()) {
        Some(s) => s,
        None => return reader_fallback_result(gas_limit),
    };

    // All read handlers are non-payable. Payability checked per-method (dispatch-first).
    // Unknown selectors hit fallback without payability check.
    let result = match selector {
        getEpochCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_epoch_reader(reader, input, gas_limit)),
        getProposerValIdCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_proposer_val_id_reader(reader, gas_limit)),
        getValidatorCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_validator_reader(reader, input, gas_limit)),
        getDelegatorCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_delegator_reader(reader, input, gas_limit)),
        getWithdrawalRequestCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_withdrawal_request_reader(reader, input, gas_limit)),
        getConsensusValidatorSetCall::SELECTOR => {
            function_not_payable(&call_value).and_then(|_| {
                handle_get_validator_set_reader(reader, input, gas_limit, valset_slots::CONSENSUS)
            })
        }
        getSnapshotValidatorSetCall::SELECTOR => function_not_payable(&call_value).and_then(|_| {
            handle_get_validator_set_reader(reader, input, gas_limit, valset_slots::SNAPSHOT)
        }),
        getExecutionValidatorSetCall::SELECTOR => {
            function_not_payable(&call_value).and_then(|_| {
                handle_get_validator_set_reader(reader, input, gas_limit, valset_slots::EXECUTION)
            })
        }
        getDelegationsCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_delegations_reader(reader, input, gas_limit)),
        getDelegatorsCall::SELECTOR => function_not_payable(&call_value)
            .and_then(|_| handle_get_delegators_reader(reader, input, gas_limit)),
        // Unknown selector → fallback (no payability check)
        _ => return reader_fallback_result(gas_limit),
    };

    handler_result(gas_limit, result)
}

/// Fallback result for unknown/short selectors in the reader dispatch.
fn reader_fallback_result(gas_limit: u64) -> InterpreterResult {
    if gas_limit < gas::FALLBACK {
        return InterpreterResult {
            result: InstructionResult::PrecompileOOG,
            output: Bytes::new(),
            gas: Gas::new(gas_limit),
        };
    }
    let mut gas = Gas::new(gas_limit);
    let _ = gas.record_regular_cost(gas_limit);
    InterpreterResult {
        result: InstructionResult::Revert,
        output: Bytes::from("method not supported"),
        gas,
    }
}

/// Check that msg.value is zero (non-payable method guard).
fn function_not_payable(call_value: &U256) -> Result<(), PrecompileHalt> {
    if !call_value.is_zero() {
        return Err(PrecompileHalt::Other("value non-zero".into()));
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Reader-based handlers
// ═══════════════════════════════════════════════════════════════════════════════

fn handle_get_epoch_reader<R: StorageReader>(
    reader: &mut R,
    _input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    if gas_limit < gas::GET_EPOCH {
        return Err(PrecompileHalt::OutOfGas);
    }
    let epoch_info = read_epoch_info_reader(reader)?;
    let encoded = getEpochCall::abi_encode_returns(&getEpochReturn {
        epoch: epoch_info.epoch,
        inEpochDelayPeriod: epoch_info.in_delay_period,
    });
    Ok((gas::GET_EPOCH, encoded.into()))
}

fn handle_get_proposer_val_id_reader<R: StorageReader>(
    reader: &mut R,
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    if gas_limit < gas::GET_PROPOSER_VAL_ID {
        return Err(PrecompileHalt::OutOfGas);
    }
    let val_id = read_storage_u64_reader(reader, global_slots::PROPOSER_VAL_ID)?;
    let encoded = getProposerValIdCall::abi_encode_returns(&val_id);
    Ok((gas::GET_PROPOSER_VAL_ID, encoded.into()))
}

fn handle_get_validator_reader<R: StorageReader>(
    reader: &mut R,
    input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    if gas_limit < gas::GET_VALIDATOR {
        return Err(PrecompileHalt::OutOfGas);
    }
    let call = decode_fixed::<getValidatorCall>(input)?;
    let val_id = call.validatorId;
    let validator = read_validator_reader(reader, val_id)?;

    // Read consensus and snapshot stakes and commissions
    let consensus_stake = read_storage_u256_reader(reader, storage::consensus_view_key(val_id, 0))?;
    let consensus_commission =
        read_storage_u256_reader(reader, storage::consensus_view_key(val_id, 1))?;
    let snapshot_stake = read_storage_u256_reader(reader, storage::snapshot_view_key(val_id, 0))?;
    let snapshot_commission =
        read_storage_u256_reader(reader, storage::snapshot_view_key(val_id, 1))?;

    let encoded = getValidatorCall::abi_encode_returns(&getValidatorReturn {
        authAddress: validator.auth_address,
        flags: validator.flags,
        stake: validator.stake,
        accRewardPerToken: validator.accumulated_reward_per_token,
        commission: validator.commission,
        unclaimedRewards: validator.unclaimed_rewards,
        consensusStake: consensus_stake,
        consensusCommission: consensus_commission,
        snapshotStake: snapshot_stake,
        snapshotCommission: snapshot_commission,
        secpPubkey: Bytes::copy_from_slice(&validator.secp_pubkey),
        blsPubkey: Bytes::copy_from_slice(&validator.bls_pubkey),
    });
    Ok((gas::GET_VALIDATOR, encoded.into()))
}

fn handle_get_delegator_reader<R: StorageReader>(
    reader: &mut R,
    input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    if gas_limit < gas::GET_DELEGATOR {
        return Err(PrecompileHalt::OutOfGas);
    }
    let call = decode_fixed::<getDelegatorCall>(input)?;
    let delegator = read_delegator_reader(reader, call.validatorId, &call.delegator)?;
    let encoded = getDelegatorCall::abi_encode_returns(&getDelegatorReturn {
        stake: delegator.stake,
        accRewardPerToken: delegator.accumulated_reward_per_token,
        unclaimedRewards: delegator.rewards,
        deltaStake: delegator.delta_stake,
        nextDeltaStake: delegator.next_delta_stake,
        deltaEpoch: delegator.delta_epoch,
        nextDeltaEpoch: delegator.next_delta_epoch,
    });
    Ok((gas::GET_DELEGATOR, encoded.into()))
}

fn handle_get_withdrawal_request_reader<R: StorageReader>(
    reader: &mut R,
    input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    if gas_limit < gas::GET_WITHDRAWAL_REQUEST {
        return Err(PrecompileHalt::OutOfGas);
    }
    let call = decode_fixed::<getWithdrawalRequestCall>(input)?;
    let withdrawal =
        read_withdrawal_request_reader(reader, call.validatorId, &call.delegator, call.withdrawId)?;
    let encoded = getWithdrawalRequestCall::abi_encode_returns(&getWithdrawalRequestReturn {
        withdrawalAmount: withdrawal.amount,
        accRewardPerToken: withdrawal.accumulator,
        withdrawEpoch: withdrawal.epoch,
    });
    Ok((gas::GET_WITHDRAWAL_REQUEST, encoded.into()))
}

/// Handler for validator set functions using StorageReader.
fn handle_get_validator_set_reader<R: StorageReader>(
    reader: &mut R,
    input: &[u8],
    gas_limit: u64,
    base_slot: U256,
) -> Result<(u64, Bytes), PrecompileHalt> {
    // Flat gas cost for all validator set getters (C++ static_assert: 814,000)
    let gas_cost = gas::GET_CONSENSUS_VALIDATOR_SET;
    if gas_limit < gas_cost {
        return Err(PrecompileHalt::OutOfGas);
    }

    let call = decode_fixed::<getConsensusValidatorSetCall>(input)?;
    let start_index = call.startIndex;

    // Read array length from base slot
    let length = read_storage_u64_reader(reader, base_slot)?;

    // Calculate how many elements to read
    let start = start_index as u64;
    let remaining = length.saturating_sub(start);
    let count = remaining.min(MAX_VALIDATORS_PER_CALL as u64) as u32;

    // Read validator IDs
    let mut val_ids = Vec::with_capacity(count as usize);
    for i in 0..count {
        let slot = base_slot + U256::from(1 + start_index + i);
        let val_id = read_storage_u64_reader(reader, slot)?;
        val_ids.push(val_id);
    }

    // Determine if we're done
    let next_index = start_index + count;
    let is_done = (next_index as u64) >= length;

    let encoded =
        getConsensusValidatorSetCall::abi_encode_returns(&getConsensusValidatorSetReturn {
            isDone: is_done,
            nextIndex: next_index,
            valIds: val_ids,
        });
    Ok((gas_cost, encoded.into()))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Reader-based storage functions
// ═══════════════════════════════════════════════════════════════════════════════

fn read_epoch_info_reader<R: StorageReader>(reader: &mut R) -> Result<EpochInfo, PrecompileHalt> {
    let epoch = read_storage_u64_reader(reader, global_slots::EPOCH)?;
    let in_delay_raw = read_storage_u256_reader(reader, global_slots::IN_BOUNDARY)?;
    let in_delay_period = in_delay_raw != U256::ZERO;
    Ok(EpochInfo { epoch, in_delay_period })
}

fn read_validator_reader<R: StorageReader>(
    reader: &mut R,
    val_id: u64,
) -> Result<Validator, PrecompileHalt> {
    let stake = read_storage_u256_reader(reader, validator_key(val_id, validator_offsets::STAKE))?;
    let accumulated_reward_per_token = read_storage_u256_reader(
        reader,
        validator_key(val_id, validator_offsets::ACCUMULATED_REWARD_PER_TOKEN),
    )?;
    let commission =
        read_storage_u256_reader(reader, validator_key(val_id, validator_offsets::COMMISSION))?;

    let keys_slot_0 =
        read_storage_u256_reader(reader, validator_key(val_id, validator_offsets::KEYS))?
            .to_be_bytes::<32>();
    let keys_slot_1 =
        read_storage_u256_reader(reader, validator_key(val_id, validator_offsets::KEYS + 1))?
            .to_be_bytes::<32>();
    let keys_slot_2 =
        read_storage_u256_reader(reader, validator_key(val_id, validator_offsets::KEYS + 2))?
            .to_be_bytes::<32>();

    let mut keys_concat = [0u8; 96];
    keys_concat[0..32].copy_from_slice(&keys_slot_0);
    keys_concat[32..64].copy_from_slice(&keys_slot_1);
    keys_concat[64..96].copy_from_slice(&keys_slot_2);

    let mut secp_pubkey = [0u8; 33];
    let mut bls_pubkey = [0u8; 48];
    secp_pubkey.copy_from_slice(&keys_concat[0..33]);
    bls_pubkey.copy_from_slice(&keys_concat[33..81]);

    let address_flags_raw =
        read_storage_u256_reader(reader, validator_key(val_id, validator_offsets::ADDRESS_FLAGS))?
            .to_be_bytes::<32>();
    let auth_address = Address::from_slice(&address_flags_raw[0..20]);
    let flags = u64::from_be_bytes(address_flags_raw[20..28].try_into().unwrap());

    let unclaimed_rewards = read_storage_u256_reader(
        reader,
        validator_key(val_id, validator_offsets::UNCLAIMED_REWARDS),
    )?;

    Ok(Validator {
        stake,
        accumulated_reward_per_token,
        commission,
        secp_pubkey,
        bls_pubkey,
        auth_address,
        flags,
        unclaimed_rewards,
    })
}

fn read_delegator_reader<R: StorageReader>(
    reader: &mut R,
    val_id: u64,
    delegator_addr: &Address,
) -> Result<Delegator, PrecompileHalt> {
    let stake = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::STAKE),
    )?;
    let accumulated_reward_per_token = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::ACCUMULATED_REWARD_PER_TOKEN),
    )?;
    let rewards = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::REWARDS),
    )?;
    let delta_stake = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::DELTA_STAKE),
    )?;
    let next_delta_stake = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::NEXT_DELTA_STAKE),
    )?;

    let epochs_raw = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::EPOCHS),
    )?
    .to_be_bytes::<32>();
    let delta_epoch = u64::from_be_bytes(epochs_raw[0..8].try_into().unwrap());
    let next_delta_epoch = u64::from_be_bytes(epochs_raw[8..16].try_into().unwrap());

    Ok(Delegator {
        stake,
        accumulated_reward_per_token,
        rewards,
        delta_stake,
        next_delta_stake,
        delta_epoch,
        next_delta_epoch,
    })
}

fn read_withdrawal_request_reader<R: StorageReader>(
    reader: &mut R,
    val_id: u64,
    delegator_addr: &Address,
    withdrawal_id: u8,
) -> Result<WithdrawalRequest, PrecompileHalt> {
    let amount = read_storage_u256_reader(
        reader,
        withdrawal_key(val_id, delegator_addr, withdrawal_id, withdrawal_offsets::AMOUNT),
    )?;
    let accumulator = read_storage_u256_reader(
        reader,
        withdrawal_key(val_id, delegator_addr, withdrawal_id, withdrawal_offsets::ACCUMULATOR),
    )?;

    let epoch_raw = read_storage_u256_reader(
        reader,
        withdrawal_key(val_id, delegator_addr, withdrawal_id, withdrawal_offsets::EPOCH),
    )?
    .to_be_bytes::<32>();
    let epoch = u64::from_be_bytes(epoch_raw[0..8].try_into().unwrap());

    Ok(WithdrawalRequest { amount, accumulator, epoch })
}

fn read_storage_u256_reader<R: StorageReader>(
    reader: &mut R,
    key: U256,
) -> Result<U256, PrecompileHalt> {
    reader.sload(key)
}

fn read_storage_u64_reader<R: StorageReader>(
    reader: &mut R,
    key: U256,
) -> Result<u64, PrecompileHalt> {
    let value = read_storage_u256_reader(reader, key)?;
    // Monad stores u64 values left-aligned (big-endian in first 8 bytes of slot)
    let bytes = value.to_be_bytes::<32>();
    Ok(u64::from_be_bytes(bytes[0..8].try_into().unwrap()))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Reader-based linked list functions
// ═══════════════════════════════════════════════════════════════════════════════

fn handle_get_delegations_reader<R: StorageReader>(
    reader: &mut R,
    input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    let gas_cost = gas::GET_DELEGATIONS;
    if gas_limit < gas_cost {
        return Err(PrecompileHalt::OutOfGas);
    }

    let call = decode_fixed::<getDelegationsCall>(input)?;

    let (done, next_val_id, val_ids) = traverse_validators_for_delegator_reader(
        reader,
        &call.delegator,
        call.startValId,
        LINKED_LIST_PAGINATION,
    )?;

    let encoded = getDelegationsCall::abi_encode_returns(&getDelegationsReturn {
        isDone: done,
        nextValId: next_val_id,
        valIds: val_ids,
    });
    Ok((gas_cost, encoded.into()))
}

fn handle_get_delegators_reader<R: StorageReader>(
    reader: &mut R,
    input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    let gas_cost = gas::GET_DELEGATORS;
    if gas_limit < gas_cost {
        return Err(PrecompileHalt::OutOfGas);
    }

    let call = decode_fixed::<getDelegatorsCall>(input)?;

    let (done, next_delegator, delegators) = traverse_delegators_for_validator_reader(
        reader,
        call.validatorId,
        &call.startDelegator,
        LINKED_LIST_PAGINATION,
    )?;

    let encoded = getDelegatorsCall::abi_encode_returns(&getDelegatorsReturn {
        isDone: done,
        nextDelegator: next_delegator,
        delegators,
    });
    Ok((gas_cost, encoded.into()))
}

fn read_list_node_reader<R: StorageReader>(
    reader: &mut R,
    val_id: u64,
    delegator_addr: &Address,
) -> Result<ListNode, PrecompileHalt> {
    let slot6 = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::LIST_NODE),
    )?
    .to_be_bytes::<32>();
    let slot7 = read_storage_u256_reader(
        reader,
        delegator_key(val_id, delegator_addr, delegator_offsets::LIST_NODE + 1),
    )?
    .to_be_bytes::<32>();
    Ok(ListNode::from_slots(slot6, slot7))
}

fn traverse_validators_for_delegator_reader<R: StorageReader>(
    reader: &mut R,
    delegator: &Address,
    start_val_id: u64,
    limit: u32,
) -> Result<(bool, u64, Vec<u64>), PrecompileHalt> {
    let ptr = if start_val_id == 0 {
        let sentinel = read_list_node_reader(reader, ListNode::SENTINEL_VAL_ID, delegator)?;
        sentinel.inext
    } else {
        start_val_id
    };

    if ptr == 0 {
        return Ok((true, 0, vec![]));
    }

    let first_node = read_list_node_reader(reader, ptr, delegator)?;
    if first_node.iprev == 0 {
        return Ok((true, ptr, vec![]));
    }

    let mut results = Vec::with_capacity(limit as usize);
    let mut current_ptr = ptr;
    let mut current_node = first_node;
    let mut count = 0u32;

    while current_ptr != 0 && count < limit {
        results.push(current_ptr);
        let next = current_node.inext;
        count += 1;

        if next != 0 && count < limit {
            current_node = read_list_node_reader(reader, next, delegator)?;
        }
        current_ptr = next;
    }

    let done = current_ptr == 0;
    Ok((done, current_ptr, results))
}

fn traverse_delegators_for_validator_reader<R: StorageReader>(
    reader: &mut R,
    val_id: u64,
    start_delegator: &Address,
    limit: u32,
) -> Result<(bool, Address, Vec<Address>), PrecompileHalt> {
    let ptr = if *start_delegator == Address::ZERO {
        let sentinel = read_list_node_reader(reader, val_id, &ListNode::SENTINEL_ADDRESS)?;
        sentinel.anext
    } else {
        *start_delegator
    };

    if ptr == Address::ZERO {
        return Ok((true, Address::ZERO, vec![]));
    }

    let first_node = read_list_node_reader(reader, val_id, &ptr)?;
    if first_node.aprev == Address::ZERO {
        return Ok((true, ptr, vec![]));
    }

    let mut results = Vec::with_capacity(limit as usize);
    let mut current_ptr = ptr;
    let mut current_node = first_node;
    let mut count = 0u32;

    while current_ptr != Address::ZERO && count < limit {
        results.push(current_ptr);
        let next = current_node.anext;
        count += 1;

        if next != Address::ZERO && count < limit {
            current_node = read_list_node_reader(reader, val_id, &next)?;
        }
        current_ptr = next;
    }

    let done = current_ptr == Address::ZERO;
    Ok((done, current_ptr, results))
}
