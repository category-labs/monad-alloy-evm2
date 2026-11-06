//! Settles delegator stake and rewards against the current epoch.

use super::{
    StorageReader,
    abi::gas,
    constants::UNIT_BIAS,
    interface::IMonadStaking::*,
    storage::{
        accumulator_key, delegator_key, delegator_offsets, global_slots, validator_key,
        validator_offsets,
    },
    types::{Delegator, RefCountedAccumulator},
};
use alloc::format;
use alloy_primitives::{Address, Bytes, Log, U256};
use alloy_sol_types::SolCall;
use evm2::precompiles::PrecompileHalt;

/// Extended storage trait for state-mutating staking operations.
///
/// Extends [`StorageReader`] with write capabilities needed by delegate,
/// undelegate, and other state-changing functions.
pub trait StakingStorage: StorageReader {
    /// Write a U256 value to storage at the given key.
    fn sstore(&mut self, key: U256, value: U256) -> Result<(), PrecompileHalt>;

    /// Transfer balance from one address to another.
    fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<(), PrecompileHalt>;

    /// Emit a log entry.
    fn emit_log(&mut self, log: Log) -> Result<(), PrecompileHalt>;
}

/// Checked addition for U256.
///
/// C++ parity: checked math failures map to staking internal error.
fn checked_add_u256(a: U256, b: U256) -> Result<U256, PrecompileHalt> {
    a.checked_add(b).ok_or_else(|| PrecompileHalt::Other("internal error".into()))
}

/// Checked subtraction for U256.
///
/// C++ parity: checked math failures map to staking internal error.
fn checked_sub_u256(a: U256, b: U256) -> Result<U256, PrecompileHalt> {
    a.checked_sub(b).ok_or_else(|| PrecompileHalt::Other("internal error".into()))
}

/// Checked multiply-then-divide for U256. Returns `PrecompileHalt` on
/// overflow (in the multiplication) or division by zero.
///
/// C++ parity: checked math failures map to staking internal error.
fn checked_mul_div_u256(a: U256, b: U256, d: U256) -> Result<U256, PrecompileHalt> {
    if d.is_zero() {
        return Err(PrecompileHalt::Other("internal error".into()));
    }
    let product = a.checked_mul(b).ok_or_else(|| PrecompileHalt::Other("internal error".into()))?;
    Ok(product / d)
}

fn write_storage_u256<S: StakingStorage>(
    s: &mut S,
    key: U256,
    value: U256,
) -> Result<(), PrecompileHalt> {
    s.sstore(key, value)
}

fn read_u256<S: StorageReader>(s: &mut S, key: U256) -> Result<U256, PrecompileHalt> {
    s.sload(key)
}

fn read_u64<S: StorageReader>(s: &mut S, key: U256) -> Result<u64, PrecompileHalt> {
    let value = s.sload(key)?;
    let bytes = value.to_be_bytes::<32>();
    Ok(u64::from_be_bytes(bytes[0..8].try_into().unwrap()))
}

fn read_epoch<S: StorageReader>(s: &mut S) -> Result<u64, PrecompileHalt> {
    read_u64(s, global_slots::EPOCH)
}

fn read_delegator<S: StorageReader>(
    s: &mut S,
    val_id: u64,
    addr: &Address,
) -> Result<Delegator, PrecompileHalt> {
    let stake = read_u256(s, delegator_key(val_id, addr, delegator_offsets::STAKE))?;
    let accumulated_reward_per_token =
        read_u256(s, delegator_key(val_id, addr, delegator_offsets::ACCUMULATED_REWARD_PER_TOKEN))?;
    let rewards = read_u256(s, delegator_key(val_id, addr, delegator_offsets::REWARDS))?;
    let delta_stake = read_u256(s, delegator_key(val_id, addr, delegator_offsets::DELTA_STAKE))?;
    let next_delta_stake =
        read_u256(s, delegator_key(val_id, addr, delegator_offsets::NEXT_DELTA_STAKE))?;
    let epochs_raw =
        read_u256(s, delegator_key(val_id, addr, delegator_offsets::EPOCHS))?.to_be_bytes::<32>();
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

fn read_accumulator<S: StorageReader>(
    s: &mut S,
    epoch: u64,
    val_id: u64,
) -> Result<RefCountedAccumulator, PrecompileHalt> {
    let value = read_u256(s, accumulator_key(epoch, val_id, 0))?;
    // Refcount is stored as u256 (right-aligned standard integer encoding)
    let refcount_u256 = read_u256(s, accumulator_key(epoch, val_id, 1))?;
    let refcount = refcount_u256.as_limbs()[0];
    Ok(RefCountedAccumulator { value, refcount })
}

fn write_validator_unclaimed_rewards<S: StakingStorage>(
    s: &mut S,
    val_id: u64,
    rewards: U256,
) -> Result<(), PrecompileHalt> {
    write_storage_u256(s, validator_key(val_id, validator_offsets::UNCLAIMED_REWARDS), rewards)
}

fn write_delegator<S: StakingStorage>(
    s: &mut S,
    val_id: u64,
    addr: &Address,
    del: &Delegator,
) -> Result<(), PrecompileHalt> {
    write_storage_u256(s, delegator_key(val_id, addr, delegator_offsets::STAKE), del.stake)?;
    write_storage_u256(
        s,
        delegator_key(val_id, addr, delegator_offsets::ACCUMULATED_REWARD_PER_TOKEN),
        del.accumulated_reward_per_token,
    )?;
    write_storage_u256(s, delegator_key(val_id, addr, delegator_offsets::REWARDS), del.rewards)?;
    write_storage_u256(
        s,
        delegator_key(val_id, addr, delegator_offsets::DELTA_STAKE),
        del.delta_stake,
    )?;
    write_storage_u256(
        s,
        delegator_key(val_id, addr, delegator_offsets::NEXT_DELTA_STAKE),
        del.next_delta_stake,
    )?;
    // Pack delta_epoch + next_delta_epoch into one slot
    let mut epochs = [0u8; 32];
    epochs[0..8].copy_from_slice(&del.delta_epoch.to_be_bytes());
    epochs[8..16].copy_from_slice(&del.next_delta_epoch.to_be_bytes());
    write_storage_u256(
        s,
        delegator_key(val_id, addr, delegator_offsets::EPOCHS),
        U256::from_be_bytes(epochs),
    )?;
    Ok(())
}

fn write_accumulator<S: StakingStorage>(
    s: &mut S,
    epoch: u64,
    val_id: u64,
    acc: &RefCountedAccumulator,
) -> Result<(), PrecompileHalt> {
    write_storage_u256(s, accumulator_key(epoch, val_id, 0), acc.value)?;
    // Refcount stored as u256 (right-aligned standard integer encoding)
    write_storage_u256(s, accumulator_key(epoch, val_id, 1), U256::from(acc.refcount))?;
    Ok(())
}

/// Check if an epoch is active (has passed).
const fn is_epoch_active(current_epoch: u64, active_epoch: u64) -> bool {
    active_epoch != 0 && active_epoch <= current_epoch
}

/// Calculate rewards using the accumulator formula.
///
/// `reward = (stake * (epoch_acc - last_acc)) / UNIT_BIAS`
fn calculate_rewards(stake: U256, epoch_acc: U256, last_acc: U256) -> Result<U256, PrecompileHalt> {
    // Mirror C++ ordering exactly:
    // 1) checked_sub(current_acc, last_checked_acc)
    // 2) checked_mul_div(delta, stake, UNIT_BIAS)
    let diff = checked_sub_u256(epoch_acc, last_acc)?;
    checked_mul_div_u256(diff, stake, UNIT_BIAS)
}

/// Decrement the accumulator refcount and return the snapshotted value.
///
/// If refcount reaches 0, clears the accumulator storage.
fn decrement_accumulator_refcount<S: StakingStorage>(
    s: &mut S,
    epoch: u64,
    val_id: u64,
) -> Result<U256, PrecompileHalt> {
    let mut acc = read_accumulator(s, epoch, val_id)?;
    let value = acc.value;
    if acc.refcount == 0 {
        return Ok(U256::ZERO);
    }
    acc.refcount -= 1;
    if acc.refcount == 0 {
        // Clear storage
        write_storage_u256(s, accumulator_key(epoch, val_id, 0), U256::ZERO)?;
        write_storage_u256(s, accumulator_key(epoch, val_id, 1), U256::ZERO)?;
    } else {
        write_accumulator(s, epoch, val_id, &acc)?;
    }
    Ok(value)
}

/// Apply compound: calculate rewards from delta track activation and fold into active stake.
fn apply_compound<S: StakingStorage>(
    s: &mut S,
    val_id: u64,
    del: &mut Delegator,
) -> Result<U256, PrecompileHalt> {
    let epoch_acc = decrement_accumulator_refcount(s, del.delta_epoch, val_id)?;
    let rewards = calculate_rewards(del.stake, epoch_acc, del.accumulated_reward_per_token)?;
    del.accumulated_reward_per_token = epoch_acc;

    // Compound: active_stake += delta_stake
    del.stake = checked_add_u256(del.stake, del.delta_stake)?;

    // Promote next_delta → delta
    del.delta_stake = del.next_delta_stake;
    del.next_delta_stake = U256::ZERO;
    del.delta_epoch = del.next_delta_epoch;
    del.next_delta_epoch = 0;

    Ok(rewards)
}

/// Pull delegator state up to date.
///
/// Promotes pending delta stakes if their activation epochs have passed,
/// and calculates accumulated rewards.
fn pull_delegator_up_to_date<S: StakingStorage>(
    s: &mut S,
    val_id: u64,
    addr: &Address,
) -> Result<Delegator, PrecompileHalt> {
    let mut del = read_delegator(s, val_id, addr)?;
    let current_epoch = read_epoch(s)?;

    // Can promote next_delta → delta?
    let can_promote = del.delta_epoch == 0 && del.next_delta_epoch <= current_epoch + 1;
    if can_promote && del.next_delta_epoch != 0 {
        del.delta_stake = del.next_delta_stake;
        del.next_delta_stake = U256::ZERO;
        del.delta_epoch = del.next_delta_epoch;
        del.next_delta_epoch = 0;
    }

    // Track validator unclaimed_rewards for reward_invariant
    let mut unclaimed_rewards =
        read_u256(s, validator_key(val_id, validator_offsets::UNCLAIMED_REWARDS))?;

    // Check which delta tracks can be compounded.
    // Both checks use the state BEFORE any compounding.
    let can_compound = is_epoch_active(current_epoch, del.delta_epoch) && del.delta_epoch != 0;
    let can_compound_boundary =
        is_epoch_active(current_epoch, del.next_delta_epoch) && del.next_delta_epoch != 0;

    // Compound boundary track first.
    // When both tracks are active, apply_compound is called twice:
    //   1st: uses delta_epoch, promotes next_delta → delta
    //   2nd: uses the newly promoted delta (was next_delta)
    if can_compound_boundary {
        let rewards = apply_compound(s, val_id, &mut del)?;
        // reward_invariant: check solvency and deduct from unclaimed
        if unclaimed_rewards < rewards {
            return Err(PrecompileHalt::Other("solvency error".into()));
        }
        unclaimed_rewards = checked_sub_u256(unclaimed_rewards, rewards)?;
        del.rewards = checked_add_u256(del.rewards, rewards)?;
    }

    // Compound main delta track
    if can_compound {
        let rewards = apply_compound(s, val_id, &mut del)?;
        // reward_invariant: check solvency and deduct from unclaimed
        if unclaimed_rewards < rewards {
            return Err(PrecompileHalt::Other("solvency error".into()));
        }
        unclaimed_rewards = checked_sub_u256(unclaimed_rewards, rewards)?;
        del.rewards = checked_add_u256(del.rewards, rewards)?;
    }

    // Accrue rewards for active stake
    if !del.stake.is_zero() {
        let val_acc =
            read_u256(s, validator_key(val_id, validator_offsets::ACCUMULATED_REWARD_PER_TOKEN))?;
        let rewards = calculate_rewards(del.stake, val_acc, del.accumulated_reward_per_token)?;
        // reward_invariant: check solvency and deduct from unclaimed
        if unclaimed_rewards < rewards {
            return Err(PrecompileHalt::Other("solvency error".into()));
        }
        unclaimed_rewards = checked_sub_u256(unclaimed_rewards, rewards)?;
        del.accumulated_reward_per_token = val_acc;
        del.rewards = checked_add_u256(del.rewards, rewards)?;
    }

    // Write back updated unclaimed_rewards
    write_validator_unclaimed_rewards(s, val_id, unclaimed_rewards)?;
    write_delegator(s, val_id, addr, &del)?;
    Ok(del)
}

/// Handle getDelegator as a write operation.
///
/// The canonical implementation calls `pull_delegator_up_to_date` which writes settled
/// state to storage. The gas cost (184,900) includes warm_sstores.
pub fn handle_get_delegator_write<S: StakingStorage>(
    s: &mut S,
    input: &[u8],
    gas_limit: u64,
) -> Result<(u64, Bytes), PrecompileHalt> {
    if gas_limit < gas::GET_DELEGATOR {
        return Err(PrecompileHalt::OutOfGas);
    }

    let call = getDelegatorCall::abi_decode_raw(&input[4..])
        .map_err(|e| PrecompileHalt::Other(format!("Invalid input: {e}").into()))?;

    // pull_delegator_up_to_date persists settled state
    let del = pull_delegator_up_to_date(s, call.validatorId, &call.delegator)?;

    let encoded = getDelegatorCall::abi_encode_returns(&getDelegatorReturn {
        stake: del.stake,
        accRewardPerToken: del.accumulated_reward_per_token,
        unclaimedRewards: del.rewards,
        deltaStake: del.delta_stake,
        nextDeltaStake: del.next_delta_stake,
        deltaEpoch: del.delta_epoch,
        nextDeltaEpoch: del.next_delta_epoch,
    });
    Ok((gas::GET_DELEGATOR, encoded.into()))
}
