//! Native system-call lifecycle, failure handling and funding.

use alloy_primitives::{Address, B256, U256};
use evm2::{
    AccountInfo, Inspector,
    bytecode::Bytecode,
    evm::{Database, Db, InMemoryDB},
    interpreter::{Interpreter, Message, MessageResult},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork, SYSTEM_ADDRESS,
    apply_epoch_boundary, apply_syscall_on_epoch_change, apply_syscall_reward,
    apply_syscall_snapshot,
    staking::{
        STAKING_ADDRESS,
        constants::MON,
        storage::{
            consensus_view_key, global_slots, val_id_secp_key, validator_key, validator_offsets,
        },
    },
    staking_system_tx, syscall_on_epoch_change_calldata, syscall_snapshot_tx,
};
use std::{cell::Cell, io, rc::Rc};

const AUTHOR: Address = Address::repeat_byte(0x55);
const AUTH: Address = Address::repeat_byte(0x66);

fn db() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&STAKING_ADDRESS, AccountInfo::default().with_balance(MON));
    db.insert_account_info(&SYSTEM_ADDRESS, AccountInfo::default().with_nonce(7).with_balance(MON));
    for (key, value) in [
        (global_slots::EPOCH, U256::from(2) << 192),
        (val_id_secp_key(&AUTHOR), U256::ONE << 192),
        (consensus_view_key(1, 0), MON * U256::from(100)),
        (validator_key(1, validator_offsets::STAKE), MON * U256::from(100)),
        (
            validator_key(1, validator_offsets::ADDRESS_FLAGS),
            U256::from_be_slice(AUTH.as_slice()) << 96,
        ),
    ] {
        db.insert_account_storage(&STAKING_ADDRESS, &key, &value);
    }
    db
}

fn evm() -> MonadEvm<'static> {
    MonadEvmFactory::new(1).create_evm(MonadHardfork::MonadTen, MonadBlockEnv::default(), db())
}

fn slot(evm: &mut MonadEvm<'_>, key: U256) -> U256 {
    evm.state_mut().storage_slot_untracked(&STAKING_ADDRESS, &key).unwrap()
}

struct CallCounter(Rc<Cell<usize>>);

impl Inspector<MonadEvmTypes> for CallCounter {
    fn call(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        assert_eq!(message.caller, SYSTEM_ADDRESS);
        assert_eq!(message.destination, STAKING_ADDRESS);
        self.0.set(self.0.get() + 1);
        None
    }
}

#[test]
fn block_lifecycle_helpers_use_the_installed_inspector() {
    let calls = Rc::new(Cell::new(0));
    let mut evm = evm();
    evm.set_inspector(CallCounter(calls.clone()));
    assert!(apply_syscall_reward(&mut evm, AUTHOR, MON).unwrap().commit().is_success());
    let (snapshot, change) = apply_epoch_boundary(&mut evm, 3).unwrap();
    assert!(snapshot.is_success() && change.is_success());
    assert_eq!(calls.get(), 3);
    assert!(evm.inspector().is_some());
}

#[test]
fn prefunded_reward_is_not_minted_twice_and_handles_can_discard() {
    let mut evm = evm();
    let reward_key = validator_key(1, validator_offsets::UNCLAIMED_REWARDS);
    let discarded = apply_syscall_reward(&mut evm, AUTHOR, MON).unwrap().discard();
    assert!(discarded.is_success());
    assert_eq!(slot(&mut evm, reward_key), U256::ZERO);
    let result = apply_syscall_reward(&mut evm, AUTHOR, MON).unwrap().commit();
    assert!(result.is_success());
    assert_eq!(slot(&mut evm, reward_key), MON);
    assert_eq!(evm.state_mut().account(&STAKING_ADDRESS).unwrap().balance(), MON);
    let sender = evm.state_mut().account_info_untracked(&SYSTEM_ADDRESS).unwrap().unwrap();
    assert_eq!((sender.balance, sender.nonce), (MON, 7));
    assert_eq!(result.refunded, 0);
    assert!(!evm.ext().reserve_balance.tracking_enabled());
}

#[test]
fn epoch_helper_stops_after_failed_snapshot() {
    let mut evm = evm();
    assert!(apply_syscall_snapshot(&mut evm).unwrap().commit().is_success());
    assert!(apply_epoch_boundary(&mut evm, 3).is_err());
    assert_eq!(slot(&mut evm, global_slots::EPOCH), U256::from(2) << 192);
    assert_ne!(slot(&mut evm, global_slots::IN_BOUNDARY), U256::ZERO);
}

#[test]
fn failed_epoch_change_keeps_successful_snapshot_for_retry() {
    let mut evm = evm();
    assert!(apply_epoch_boundary(&mut evm, 2).is_err());
    assert_eq!(slot(&mut evm, global_slots::EPOCH), U256::from(2) << 192);
    assert_ne!(slot(&mut evm, global_slots::IN_BOUNDARY), U256::ZERO);
    assert!(apply_syscall_on_epoch_change(&mut evm, 3).unwrap().commit().is_success());
    assert_eq!(slot(&mut evm, global_slots::EPOCH), U256::from(3) << 192);
    assert_eq!(slot(&mut evm, global_slots::IN_BOUNDARY), U256::ZERO);
    let (snapshot, change) = apply_epoch_boundary(&mut evm, 4).unwrap();
    assert!(snapshot.is_success() && change.is_success());
}

#[test]
fn maximum_epoch_uses_unsigned_next_epoch_arithmetic() {
    let mut evm = evm();
    let result = apply_syscall_on_epoch_change(&mut evm, u64::MAX).unwrap().commit();
    assert!(result.is_success());
    assert_eq!(slot(&mut evm, global_slots::EPOCH), U256::from(u64::MAX) << 192);
}

/// Fail a late snapshot read after the boundary flag has already been written.
struct FailingStorageDb {
    inner: InMemoryDB,
    fail: bool,
}

impl Database for FailingStorageDb {
    type Error = io::Error;
    fn get_account(&mut self, address: &Address) -> io::Result<Option<AccountInfo>> {
        Ok(self.inner.get_account(address).unwrap())
    }
    fn get_code_by_hash(&mut self, hash: &B256) -> io::Result<Bytecode> {
        Ok(self.inner.get_code_by_hash(hash).unwrap())
    }
    fn get_storage(&mut self, address: &Address, key: &U256) -> io::Result<U256> {
        if self.fail
            && *address == STAKING_ADDRESS
            && *key == monad_evm2::staking::storage::valset_slots::CONSENSUS
        {
            self.fail = false;
            return Err(io::Error::other("injected staking storage failure"));
        }
        Ok(self.inner.get_storage(address, key).unwrap())
    }
    fn get_block_hash(&mut self, number: &U256) -> io::Result<B256> {
        Ok(self.inner.get_block_hash(number).unwrap())
    }
}

#[test]
fn database_failure_aborts_instead_of_reverting_and_clears_prior_writes() {
    let mut evm = MonadEvmFactory::new(1).create_evm(
        MonadHardfork::MonadTen,
        MonadBlockEnv::default(),
        Db::new(FailingStorageDb { inner: db(), fail: true }),
    );
    assert!(apply_syscall_snapshot(&mut evm).is_err());
    assert_eq!(slot(&mut evm, global_slots::IN_BOUNDARY), U256::ZERO);
    assert!(!evm.ext().reserve_balance.tracking_enabled());
    assert!(apply_syscall_snapshot(&mut evm).unwrap().commit().is_success());
}

#[test]
fn borrowed_inspector_observes_precompile_database_failure_and_retry() {
    let mut evm = MonadEvmFactory::new(1).create_evm(
        MonadHardfork::MonadTen,
        MonadBlockEnv::default(),
        Db::new(FailingStorageDb { inner: db(), fail: true }),
    );
    let mut inspector = CallCounter(Rc::new(Cell::new(0)));
    let error = evm.system_call_with_inspector(syscall_snapshot_tx(), &mut inspector).unwrap_err();
    assert!(error.to_string().contains("injected staking storage failure"));
    assert_eq!(inspector.0.get(), 1);
    assert_eq!(slot(&mut evm, global_slots::IN_BOUNDARY), U256::ZERO);
    let result =
        evm.system_call_with_inspector(syscall_snapshot_tx(), &mut inspector).unwrap().commit();
    assert!(result.is_success());
    assert_eq!(inspector.0.get(), 2);
    assert_ne!(slot(&mut evm, global_slots::IN_BOUNDARY), U256::ZERO);
}

#[test]
fn exhausted_system_budget_cannot_change_the_epoch() {
    let mut evm = evm();
    let tx = staking_system_tx(syscall_on_epoch_change_calldata(3)).with_gas_limit(49_999);
    let result = evm.system_call(tx).unwrap().commit();
    assert!(!result.is_success());
    assert_eq!(slot(&mut evm, global_slots::EPOCH), U256::from(2) << 192);
}
