//! Staking arithmetic and validator-set regression tests.

use super::*;
use crate::staking::result::InstructionResult;
use alloc::collections::BTreeMap;

#[derive(Default)]
struct MockStorage {
    slots: BTreeMap<U256, U256>,
    logs: Vec<Log>,
    transfers: Vec<(Address, Address, U256)>,
}

impl MockStorage {
    fn set_u64_left(&mut self, key: U256, value: u64) {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&value.to_be_bytes());
        self.slots.insert(key, U256::from_be_bytes(bytes));
    }

    fn get_u64_left(&self, key: U256) -> u64 {
        let value = self.slots.get(&key).copied().unwrap_or(U256::ZERO);
        let bytes = value.to_be_bytes::<32>();
        u64::from_be_bytes(bytes[..8].try_into().unwrap())
    }
}

impl StorageReader for MockStorage {
    fn sload(&mut self, key: U256) -> Result<U256, PrecompileHalt> {
        Ok(self.slots.get(&key).copied().unwrap_or(U256::ZERO))
    }
}

impl StakingStorage for MockStorage {
    fn sstore(&mut self, key: U256, value: U256) -> Result<(), PrecompileHalt> {
        self.slots.insert(key, value);
        Ok(())
    }

    fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<(), PrecompileHalt> {
        self.transfers.push((from, to, amount));
        Ok(())
    }

    fn emit_log(&mut self, log: Log) -> Result<(), PrecompileHalt> {
        self.logs.push(log);
        Ok(())
    }
}

fn address_flags_slot(auth: Address, flags: u64) -> U256 {
    let mut bytes = [0u8; 32];
    bytes[..20].copy_from_slice(auth.as_slice());
    bytes[20..28].copy_from_slice(&flags.to_be_bytes());
    U256::from_be_bytes(bytes)
}

#[test]
fn valset_bitset_handles_bucket_boundaries() {
    let mut storage = MockStorage::default();

    for val_id in [0, 255, 256] {
        assert!(add_to_valset(&mut storage, val_id).expect("validator should be inserted"));
        assert!(!add_to_valset(&mut storage, val_id).expect("duplicate should be ignored"));
    }

    assert_eq!(
        storage.slots.get(&bitset_bucket_key(0)).copied().unwrap_or_default(),
        U256::from(1) | (U256::from(1) << 255usize)
    );
    assert_eq!(
        storage.slots.get(&bitset_bucket_key(256)).copied().unwrap_or_default(),
        U256::from(1)
    );
    assert_eq!(storage.get_u64_left(valset_slots::EXECUTION), 3);

    remove_from_valset(&mut storage, 255).expect("validator should be removed");
    assert_eq!(
        storage.slots.get(&bitset_bucket_key(0)).copied().unwrap_or_default(),
        U256::from(1)
    );

    remove_from_valset(&mut storage, 0).expect("validator should be removed");
    remove_from_valset(&mut storage, 256).expect("validator should be removed");
    assert_eq!(storage.slots.get(&bitset_bucket_key(0)).copied().unwrap_or_default(), U256::ZERO);
    assert_eq!(storage.slots.get(&bitset_bucket_key(256)).copied().unwrap_or_default(), U256::ZERO);
}

#[test]
fn test_snapshot_clears_stale_snapshot_and_consensus_views() {
    let mut storage = MockStorage::default();

    let old_snapshot_val = 11u64;
    let old_consensus_val = 22u64;
    let new_consensus_val = 33u64;

    // Old snapshot set with stale view.
    storage.set_u64_left(valset_slots::SNAPSHOT, 1);
    storage.set_u64_left(valset_slots::SNAPSHOT + U256::from(1u64), old_snapshot_val);
    storage.slots.insert(snapshot_view_key(old_snapshot_val, 0), U256::from(101u64));
    storage.slots.insert(snapshot_view_key(old_snapshot_val, 1), U256::from(202u64));

    // Old consensus set and view.
    storage.set_u64_left(valset_slots::CONSENSUS, 1);
    storage.set_u64_left(valset_slots::CONSENSUS + U256::from(1u64), old_consensus_val);
    storage.slots.insert(consensus_view_key(old_consensus_val, 0), U256::from(303u64));
    storage.slots.insert(consensus_view_key(old_consensus_val, 1), U256::from(404u64));

    // Execution set with one OK validator to become the new consensus entry.
    storage.set_u64_left(valset_slots::EXECUTION, 1);
    storage.set_u64_left(valset_slots::EXECUTION + U256::from(1u64), new_consensus_val);
    storage
        .slots
        .insert(validator_key(new_consensus_val, validator_offsets::STAKE), U256::from(999u64));
    storage
        .slots
        .insert(validator_key(new_consensus_val, validator_offsets::COMMISSION), U256::from(77u64));
    storage.slots.insert(
        validator_key(new_consensus_val, validator_offsets::ADDRESS_FLAGS),
        address_flags_slot(Address::new([0x33; 20]), validator_flags::OK),
    );

    let (_gas, output) =
        handle_syscall_snapshot(&mut storage, &[], gas::SYSCALL_SNAPSHOT + 1_000, &SYSTEM_ADDRESS)
            .expect("snapshot should succeed");
    assert!(output.is_empty());

    // Old snapshot val view cleared.
    assert_eq!(
        storage.slots.get(&snapshot_view_key(old_snapshot_val, 0)).copied().unwrap_or(U256::ZERO),
        U256::ZERO
    );
    assert_eq!(
        storage.slots.get(&snapshot_view_key(old_snapshot_val, 1)).copied().unwrap_or(U256::ZERO),
        U256::ZERO
    );

    // Old consensus val view cleared.
    assert_eq!(
        storage.slots.get(&consensus_view_key(old_consensus_val, 0)).copied().unwrap_or(U256::ZERO),
        U256::ZERO
    );
    assert_eq!(
        storage.slots.get(&consensus_view_key(old_consensus_val, 1)).copied().unwrap_or(U256::ZERO),
        U256::ZERO
    );

    // Snapshot now contains previous consensus val.
    assert_eq!(storage.get_u64_left(valset_slots::SNAPSHOT), 1);
    assert_eq!(storage.get_u64_left(valset_slots::SNAPSHOT + U256::from(1u64)), old_consensus_val);
    assert_eq!(
        storage.slots.get(&snapshot_view_key(old_consensus_val, 0)).copied().unwrap_or(U256::ZERO),
        U256::from(303u64)
    );
    assert_eq!(
        storage.slots.get(&snapshot_view_key(old_consensus_val, 1)).copied().unwrap_or(U256::ZERO),
        U256::from(404u64)
    );

    // Consensus rebuilt from execution.
    assert_eq!(storage.get_u64_left(valset_slots::CONSENSUS), 1);
    assert_eq!(storage.get_u64_left(valset_slots::CONSENSUS + U256::from(1u64)), new_consensus_val);
    assert_eq!(
        storage.slots.get(&consensus_view_key(new_consensus_val, 0)).copied().unwrap_or(U256::ZERO),
        U256::from(999u64)
    );
    assert_eq!(
        storage.slots.get(&consensus_view_key(new_consensus_val, 1)).copied().unwrap_or(U256::ZERO),
        U256::from(77u64)
    );
}

#[test]
fn test_syscall_reward_commission_mul_overflow_reverts() {
    let mut storage = MockStorage::default();
    let block_author = Address::new([0x11; 20]);
    let auth_address = Address::new([0x22; 20]);
    let val_id = 1u64;

    // Setup validator
    storage.set_u64_left(val_id_secp_key(&block_author), val_id);
    storage.slots.insert(consensus_view_key(val_id, 0), U256::from(10u64));
    // Commission rate near MAX to trigger mul overflow with large reward
    storage.slots.insert(consensus_view_key(val_id, 1), U256::MAX);
    storage.slots.insert(
        validator_key(val_id, validator_offsets::ADDRESS_FLAGS),
        address_flags_slot(auth_address, validator_flags::OK),
    );

    let input = syscallRewardCall { blockAuthor: block_author }.abi_encode();
    // block_reward near MAX * commission_rate near MAX → overflow
    let result = run_staking_write(
        &input,
        gas::SYSCALL_REWARD + 1_000,
        &mut storage,
        &SYSTEM_ADDRESS,
        U256::MAX / U256::from(2),
    );

    // Should revert (not silently clamp)
    assert_eq!(result.result, InstructionResult::Revert);
}

#[test]
fn test_external_reward_accumulator_overflow_reverts() {
    let mut storage = MockStorage::default();
    let caller = Address::new([0xAA; 20]);
    let val_id = 1u64;

    // Setup validator with accumulator near MAX
    storage.slots.insert(
        validator_key(val_id, validator_offsets::ADDRESS_FLAGS),
        address_flags_slot(caller, validator_flags::OK),
    );
    storage
        .slots
        .insert(validator_key(val_id, validator_offsets::ACCUMULATED_REWARD_PER_TOKEN), U256::MAX);
    storage.slots.insert(consensus_view_key(val_id, 0), U256::from(1u64));
    storage.slots.insert(consensus_view_key(val_id, 1), U256::ZERO);

    // call_value * UNIT_BIAS overflows when call_value is large
    let call_value = MAX_EXTERNAL_REWARD;
    let input = externalRewardCall { validatorId: val_id }.abi_encode();
    let result = handle_external_reward(
        &mut storage,
        &input,
        gas::EXTERNAL_REWARD + 1_000,
        &caller,
        call_value,
    );

    // Should fail because new_acc = MAX + reward_acc overflows
    assert!(result.is_err());
}

#[test]
fn test_withdraw_payout_overflow_reverts() {
    let mut storage = MockStorage::default();
    let caller = Address::new([0xCC; 20]);
    let val_id = 1u64;
    let wid = 0u8;

    // Withdrawal request with near-MAX amount
    storage
        .slots
        .insert(withdrawal_key(val_id, &caller, wid, withdrawal_offsets::AMOUNT), U256::MAX);
    // Accumulator with diff that would produce non-zero rewards
    storage.slots.insert(
        withdrawal_key(val_id, &caller, wid, withdrawal_offsets::ACCUMULATOR),
        U256::from(1u64),
    );
    // Withdrawal epoch set to far past
    let mut wr_epoch_bytes = [0u8; 32];
    wr_epoch_bytes[..8].copy_from_slice(&1u64.to_be_bytes());
    storage.slots.insert(
        withdrawal_key(val_id, &caller, wid, withdrawal_offsets::EPOCH),
        U256::from_be_bytes(wr_epoch_bytes),
    );

    // Current epoch well past withdrawal delay
    storage.set_u64_left(global_slots::EPOCH, 100);

    // Accumulator for epoch 1, val_id: has refcount and value > wr_acc
    storage.slots.insert(accumulator_key(1, val_id, 0), U256::from(100u64));
    storage.slots.insert(accumulator_key(1, val_id, 1), U256::from(1u64));

    // Validator unclaimed_rewards large enough for solvency
    storage.slots.insert(validator_key(val_id, validator_offsets::UNCLAIMED_REWARDS), U256::MAX);
    storage.slots.insert(
        validator_key(val_id, validator_offsets::ADDRESS_FLAGS),
        address_flags_slot(caller, validator_flags::OK),
    );

    let input = withdrawCall { validatorId: val_id, withdrawId: wid }.abi_encode();
    let result = handle_withdraw(&mut storage, &input, gas::WITHDRAW + 1_000, &caller);

    // calculate_rewards with stake=MAX and acc diff will overflow in mul_div
    assert!(result.is_err());
}
