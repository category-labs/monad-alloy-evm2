//! Differential staking execution against monad-revm 0.8.0.

mod common;

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_monad_evm2::{
    MonadBlockEnv, MonadEvmFactory, MonadEvmTypes, MonadHardfork, MonadTxEnvelope, SYSTEM_ADDRESS,
    staking::{
        STAKING_ADDRESS,
        abi::gas,
        constants::{MIN_AUTH_ADDRESS_STAKE, MON, UNIT_BIAS},
        interface::IMonadStaking::*,
        storage::{
            accumulator_key, consensus_view_key, delegator_key, delegator_offsets, global_slots,
            snapshot_view_key, val_id_secp_key, validator_key, validator_offsets, valset_slots,
            withdrawal_key, withdrawal_offsets,
        },
    },
    staking_system_tx, syscall_on_epoch_change_calldata, syscall_reward_calldata,
    syscall_snapshot_calldata,
};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use common::FORKS;
use evm2::{AccountInfo, TxResult, evm::InMemoryDB, interpreter::InstrStop};
use monad_revm::{MonadBuilder, MonadCfgEnv, monad_context_with_db};
use revm::{
    DatabaseCommit, ExecuteEvm, context::TxEnv, context_interface::result::ExecutionResult,
    database::InMemoryDB as ReferenceDB, handler::SystemCallEvm,
    state::AccountInfo as ReferenceAccount,
};
use std::collections::BTreeMap;

const SENDER: Address = Address::repeat_byte(0x11);
const VALIDATOR: u64 = 7;

const AUTHOR: Address = Address::repeat_byte(0x55);

fn system_prestate() -> Vec<(U256, U256)> {
    let mut slots = prestate();
    slots.extend([
        (val_id_secp_key(&AUTHOR), U256::from(VALIDATOR) << 192),
        (valset_slots::EXECUTION, U256::ONE << 192),
        (valset_slots::EXECUTION + U256::ONE, U256::from(VALIDATOR) << 192),
        (valset_slots::CONSENSUS, U256::ONE << 192),
        (valset_slots::CONSENSUS + U256::ONE, U256::from(VALIDATOR) << 192),
        (consensus_view_key(VALIDATOR, 0), MON * U256::from(100)),
        (consensus_view_key(VALIDATOR, 1), MON / U256::from(10)),
        (validator_key(VALIDATOR, validator_offsets::COMMISSION), MON / U256::from(5)),
        (accumulator_key(3, VALIDATOR, 1), U256::ONE << 192),
    ]);
    slots
}

#[test]
fn system_rewards_and_epoch_transitions_match_release() {
    for spec in MonadHardfork::ALL {
        let reward = call(syscall_reward_calldata(AUTHOR, MON * U256::from(10)));
        let results = assert_calls_as(
            spec,
            &system_prestate(),
            &[
                reward.clone(),
                call(syscall_snapshot_calldata()),
                reward.clone(),
                call(syscall_on_epoch_change_calldata(3)),
                reward,
                call(getValidatorCall { validatorId: VALIDATOR }.abi_encode()),
                call(getDelegatorCall { validatorId: VALIDATOR, delegator: SENDER }.abi_encode()),
                call(getEpochCall {}.abi_encode()),
                call(getProposerValIdCall {}.abi_encode()),
            ],
            SYSTEM_ADDRESS,
            true,
        );
        assert!(results.iter().all(|r| r.is_success()));
        assert_eq!(results[0].total_gas_spent, gas::SYSCALL_REWARD);
        assert_eq!(results[1].total_gas_spent, gas::SYSCALL_SNAPSHOT);
        assert_eq!(results[3].total_gas_spent, gas::SYSCALL_ON_EPOCH_CHANGE);
        let validator = getValidatorCall::abi_decode_returns(&results[5].output).unwrap();
        assert_eq!(validator.unclaimedRewards, MON * U256::from(26));
        let delegator = getDelegatorCall::abi_decode_returns(&results[6].output).unwrap();
        assert_eq!(delegator.unclaimedRewards, MON * U256::from(4));
        let epoch = getEpochCall::abi_decode_returns(&results[7].output).unwrap();
        assert_eq!(epoch.epoch, 3);
        assert!(!epoch.inEpochDelayPeriod);
        assert_eq!(
            getProposerValIdCall::abi_decode_returns(&results[8].output).unwrap(),
            VALIDATOR
        );
    }
}

#[test]
fn system_operations_require_the_reserved_sender_and_reject_invalid_transitions() {
    for spec in MonadHardfork::ALL {
        let operations = [
            call(syscall_reward_calldata(AUTHOR, MON)),
            call(syscall_snapshot_calldata()),
            call(syscall_on_epoch_change_calldata(3)),
        ];
        assert!(
            assert_calls_as(spec, &system_prestate(), &operations, SENDER, true)
                .iter()
                .all(|r| !r.is_success())
        );
        let results = assert_calls_as(
            spec,
            &system_prestate(),
            &[
                call(syscall_reward_calldata(Address::ZERO, MON)),
                call(syscall_on_epoch_change_calldata(2)),
                call(syscall_snapshot_calldata()),
                call(syscall_snapshot_calldata()),
                call(syscall_on_epoch_change_calldata(10)),
            ],
            SYSTEM_ADDRESS,
            true,
        );
        assert_eq!(
            results.iter().map(|r| r.is_success()).collect::<Vec<_>>(),
            vec![false, false, true, false, true]
        );
    }
}

#[test]
fn normal_system_sender_calls_can_supply_reward_value() {
    for spec in MonadHardfork::ALL {
        let results = assert_calls_as(
            spec,
            &system_prestate(),
            &[
                payable(
                    syscallRewardCall { blockAuthor: AUTHOR }.abi_encode(),
                    MON * U256::from(10),
                ),
                payable(syscallSnapshotCall {}.abi_encode(), MON),
                payable(syscallOnEpochChangeCall { epoch: 3 }.abi_encode(), MON),
            ],
            SYSTEM_ADDRESS,
            false,
        );
        assert!(results[0].is_success());
        assert!(!results[1].is_success());
        assert!(!results[2].is_success());
    }
}

#[test]
fn snapshot_ranks_top_two_hundred_and_removes_ineligible_validators() {
    let mut slots = system_prestate();
    slots.push((valset_slots::EXECUTION, U256::from(207) << 192));
    for id in 1..=207u64 {
        let mut flags = [0u8; 32];
        flags[..20].copy_from_slice(SENDER.as_slice());
        flags[20..28].copy_from_slice(&u64::from(id > 205).to_be_bytes());
        slots.extend([
            (valset_slots::EXECUTION + U256::from(id), U256::from(id) << 192),
            (validator_key(id, validator_offsets::ADDRESS_FLAGS), U256::from_be_bytes(flags)),
            (
                validator_key(id, validator_offsets::STAKE),
                MON * U256::from(if id % 2 == 0 { 2 } else { 1 }),
            ),
        ]);
    }
    for spec in MonadHardfork::ALL {
        let results = assert_calls_as(
            spec,
            &slots,
            &[
                call(syscall_snapshot_calldata()),
                call(getConsensusValidatorSetCall { startIndex: 0 }.abi_encode()),
                call(getConsensusValidatorSetCall { startIndex: 100 }.abi_encode()),
                call(getConsensusValidatorSetCall { startIndex: 200 }.abi_encode()),
                call(getExecutionValidatorSetCall { startIndex: 200 }.abi_encode()),
            ],
            SYSTEM_ADDRESS,
            true,
        );
        assert!(results.iter().all(|r| r.is_success()));
        let mut actual =
            getConsensusValidatorSetCall::abi_decode_returns(&results[1].output).unwrap().valIds;
        actual.extend(
            getConsensusValidatorSetCall::abi_decode_returns(&results[2].output).unwrap().valIds,
        );
        let expected = (1..=205u64)
            .filter(|id| id % 2 == 0)
            .chain((1..=205u64).filter(|id| id % 2 == 1))
            .take(200)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(
            getConsensusValidatorSetCall::abi_decode_returns(&results[3].output)
                .unwrap()
                .valIds
                .is_empty()
        );
        assert_eq!(
            getExecutionValidatorSetCall::abi_decode_returns(&results[4].output)
                .unwrap()
                .valIds
                .len(),
            5
        );
    }
}

fn registration(stake: U256, commission: U256) -> Vec<u8> {
    let mut payload = vec![2; 33];
    payload.extend([3; 48]);
    payload.extend(SENDER.as_slice());
    payload.extend(stake.to_be_bytes::<32>());
    payload.extend(commission.to_be_bytes::<32>());
    // The 0.8.0 compatibility path accepts these without signature verification.
    addValidatorCall {
        payload: payload.into(),
        signedSecpMessage: Bytes::new(),
        signedBlsMessage: Bytes::new(),
    }
    .abi_encode()
}

#[test]
fn validator_registration_matches_release_compatibility_rules() {
    let stake = MIN_AUTH_ADDRESS_STAKE;
    for spec in MonadHardfork::ALL {
        let add = payable(registration(stake, MON / U256::from(10)), stake);
        let results = assert_calls(
            spec,
            &prestate(),
            &[
                add.clone(),
                add,
                call(getValidatorCall { validatorId: 1 }.abi_encode()),
                call(getDelegatorCall { validatorId: 1, delegator: SENDER }.abi_encode()),
            ],
        );
        assert!(results[0].is_success());
        assert_eq!(addValidatorCall::abi_decode_returns(&results[0].output).unwrap(), 1);
        assert!(!results[1].is_success());
        let validator = getValidatorCall::abi_decode_returns(&results[2].output).unwrap();
        assert_eq!(validator.authAddress, SENDER);
        assert_eq!(validator.stake, stake);
        assert_eq!(validator.secpPubkey.as_ref(), &[2; 33]);
        assert_eq!(validator.blsPubkey.as_ref(), &[3; 48]);
        let delegator = getDelegatorCall::abi_decode_returns(&results[3].output).unwrap();
        assert_eq!(delegator.deltaStake, stake);
        for invalid in [
            payable(registration(stake, MON), stake - U256::ONE),
            payable(registration(stake - U256::ONE, MON), stake - U256::ONE),
            payable(registration(stake, MON + U256::ONE), stake),
            payable(
                addValidatorCall {
                    payload: Bytes::new(),
                    signedSecpMessage: Bytes::new(),
                    signedBlsMessage: Bytes::new(),
                }
                .abi_encode(),
                stake,
            ),
        ] {
            assert!(!assert_calls(spec, &prestate(), &[invalid])[0].is_success());
        }
    }
}

#[test]
fn commissions_require_authority_and_emit_only_when_changed() {
    for spec in MonadHardfork::ALL {
        let change =
            call(changeCommissionCall { validatorId: VALIDATOR, commission: MON }.abi_encode());
        let results = assert_calls(
            spec,
            &prestate(),
            &[
                change.clone(),
                change.clone(),
                call(
                    changeCommissionCall { validatorId: VALIDATOR, commission: MON + U256::ONE }
                        .abi_encode(),
                ),
                call(getValidatorCall { validatorId: VALIDATOR }.abi_encode()),
            ],
        );
        assert!(results[0].is_success());
        assert_eq!(results[0].logs.len(), 1);
        assert!(results[1].is_success());
        assert!(results[1].logs.is_empty());
        assert!(!results[2].is_success());
        let validator = getValidatorCall::abi_decode_returns(&results[3].output).unwrap();
        assert_eq!(validator.commission, MON);
        let mut slots = prestate();
        slots.push((
            validator_key(VALIDATOR, validator_offsets::ADDRESS_FLAGS),
            U256::from_be_slice(Address::repeat_byte(0x55).as_slice()) << 96,
        ));
        let results = assert_calls(spec, &slots, &[change]);
        assert_eq!(results[0].output.as_ref(), b"requires auth address");
    }
}

#[test]
fn external_rewards_use_the_consensus_or_snapshot_stake() {
    for spec in MonadHardfork::ALL {
        for boundary in [false, true] {
            let mut slots = prestate();
            slots.extend([
                (global_slots::IN_BOUNDARY, U256::from(u8::from(boundary)) << 248),
                (consensus_view_key(VALIDATOR, 0), MON * U256::from(100)),
                (snapshot_view_key(VALIDATOR, 0), MON * U256::from(50)),
            ]);
            let results = assert_calls(
                spec,
                &slots,
                &[
                    payable(externalRewardCall { validatorId: VALIDATOR }.abi_encode(), MON),
                    call(getValidatorCall { validatorId: VALIDATOR }.abi_encode()),
                ],
            );
            assert!(results[0].is_success());
            let validator = getValidatorCall::abi_decode_returns(&results[1].output).unwrap();
            assert_eq!(validator.unclaimedRewards, MON);
            assert_eq!(
                validator.accRewardPerToken,
                UNIT_BIAS / U256::from(if boundary { 50 } else { 100 })
            );
        }
    }
}

#[test]
fn external_reward_overflow_reverts_accumulator_and_value_transfer() {
    let mut slots = prestate();
    slots.extend([
        (consensus_view_key(VALIDATOR, 0), MON),
        (validator_key(VALIDATOR, validator_offsets::UNCLAIMED_REWARDS), U256::MAX),
    ]);
    for spec in MonadHardfork::ALL {
        let reward = payable(externalRewardCall { validatorId: VALIDATOR }.abi_encode(), MON);
        let results = assert_calls(spec, &slots, &[reward.clone(), reward]);
        for result in results {
            assert_eq!(result.output.as_ref(), b"internal error");
            assert!(result.logs.is_empty());
        }
    }
}

fn payable(input: impl Into<Bytes>, value: U256) -> Call {
    Call { value, ..call(input) }
}

#[test]
fn delegation_updates_pending_stake_and_lists_without_duplicate_entries() {
    for spec in MonadHardfork::ALL {
        for boundary in [false, true] {
            let mut slots = prestate();
            slots.push((global_slots::IN_BOUNDARY, U256::from(u8::from(boundary)) << 248));
            let results = assert_calls(
                spec,
                &slots,
                &[
                    payable(delegateCall { validatorId: VALIDATOR }.abi_encode(), MON),
                    payable(delegateCall { validatorId: VALIDATOR }.abi_encode(), MON),
                    call(
                        getDelegatorCall { validatorId: VALIDATOR, delegator: SENDER }.abi_encode(),
                    ),
                    call(getDelegationsCall { delegator: SENDER, startValId: 0 }.abi_encode()),
                    call(
                        getDelegatorsCall { validatorId: VALIDATOR, startDelegator: Address::ZERO }
                            .abi_encode(),
                    ),
                ],
            );
            assert!(results.iter().all(|r| r.is_success()));
            let del = getDelegatorCall::abi_decode_returns(&results[2].output).unwrap();
            assert_eq!(del.deltaStake + del.nextDeltaStake, MON * U256::from(2));
            assert_eq!(del.deltaEpoch.max(del.nextDeltaEpoch), if boundary { 4 } else { 3 });
            let delegations = getDelegationsCall::abi_decode_returns(&results[3].output).unwrap();
            assert_eq!(delegations.valIds, vec![VALIDATOR]);
        }
    }
}

#[test]
fn undelegation_records_delayed_withdrawal_and_rejects_duplicate_id() {
    let mut slots = prestate();
    slots.push((delegator_key(VALIDATOR, &SENDER, delegator_offsets::STAKE), MON));
    for spec in MonadHardfork::ALL {
        let results = assert_calls(
            spec,
            &slots,
            &[
                call(
                    undelegateCall {
                        validatorId: VALIDATOR,
                        amount: MON / U256::from(2),
                        withdrawId: 7,
                    }
                    .abi_encode(),
                ),
                call(
                    undelegateCall { validatorId: VALIDATOR, amount: U256::ONE, withdrawId: 7 }
                        .abi_encode(),
                ),
                call(withdrawCall { validatorId: VALIDATOR, withdrawId: 7 }.abi_encode()),
                call(
                    getWithdrawalRequestCall {
                        validatorId: VALIDATOR,
                        delegator: SENDER,
                        withdrawId: 7,
                    }
                    .abi_encode(),
                ),
            ],
        );
        assert!(results[0].is_success());
        assert!(!results[1].is_success());
        assert!(!results[2].is_success());
        let wr = getWithdrawalRequestCall::abi_decode_returns(&results[3].output).unwrap();
        assert_eq!(wr.withdrawalAmount, MON / U256::from(2));
        assert_eq!(wr.withdrawEpoch, 3);
    }
}

#[test]
fn mature_withdrawal_pays_principal_and_rewards_once() {
    let mut slots = prestate();
    slots.extend([
        (withdrawal_key(VALIDATOR, &SENDER, 1, withdrawal_offsets::AMOUNT), MON),
        (withdrawal_key(VALIDATOR, &SENDER, 1, withdrawal_offsets::EPOCH), U256::ONE << 192),
        (accumulator_key(1, VALIDATOR, 0), UNIT_BIAS),
        (accumulator_key(1, VALIDATOR, 1), U256::ONE << 192),
        (validator_key(VALIDATOR, validator_offsets::UNCLAIMED_REWARDS), MON),
    ]);
    for spec in MonadHardfork::ALL {
        let withdraw = call(withdrawCall { validatorId: VALIDATOR, withdrawId: 1 }.abi_encode());
        let results = assert_calls(spec, &slots, &[withdraw.clone(), withdraw]);
        assert!(results[0].is_success());
        assert!(!results[1].is_success());
        assert_eq!(results[0].logs.len(), 1);
    }
}

#[test]
fn reward_claims_and_compounding_settle_the_same_rewards() {
    let mut slots = prestate();
    slots.extend([
        (delegator_key(VALIDATOR, &SENDER, delegator_offsets::STAKE), MON),
        (validator_key(VALIDATOR, validator_offsets::ACCUMULATED_REWARD_PER_TOKEN), UNIT_BIAS),
        (validator_key(VALIDATOR, validator_offsets::UNCLAIMED_REWARDS), MON),
    ]);
    for spec in MonadHardfork::ALL {
        for input in [
            claimRewardsCall { validatorId: VALIDATOR }.abi_encode(),
            compoundCall { validatorId: VALIDATOR }.abi_encode(),
        ] {
            let operation = call(input);
            let results = assert_calls(
                spec,
                &slots,
                &[
                    operation.clone(),
                    operation,
                    call(
                        getDelegatorCall { validatorId: VALIDATOR, delegator: SENDER }.abi_encode(),
                    ),
                ],
            );
            assert!(results.iter().all(|r| r.is_success()));
            let del = getDelegatorCall::abi_decode_returns(&results[2].output).unwrap();
            assert_eq!(del.unclaimedRewards, U256::ZERO);
        }
    }
}

#[test]
fn failed_settlement_rolls_back_earlier_accumulator_writes() {
    let mut slots = prestate();
    slots.extend([
        (delegator_key(VALIDATOR, &SENDER, delegator_offsets::DELTA_STAKE), MON),
        (delegator_key(VALIDATOR, &SENDER, delegator_offsets::EPOCHS), U256::from(2) << 192),
        (accumulator_key(2, VALIDATOR, 1), U256::ONE << 192),
        (validator_key(VALIDATOR, validator_offsets::ACCUMULATED_REWARD_PER_TOKEN), UNIT_BIAS),
    ]);
    for spec in MonadHardfork::ALL {
        let query =
            call(getDelegatorCall { validatorId: VALIDATOR, delegator: SENDER }.abi_encode());
        let results = assert_calls(spec, &slots, &[query.clone(), query]);
        for result in results {
            assert_eq!(result.stop, InstrStop::Revert);
            assert_eq!(result.output.as_ref(), b"solvency error");
            assert!(result.logs.is_empty());
        }
    }
}

#[test]
fn invalid_delegation_and_arithmetic_overflow_restore_transferred_value() {
    for spec in MonadHardfork::ALL {
        let results = assert_calls(
            spec,
            &prestate(),
            &[
                payable(delegateCall { validatorId: VALIDATOR }.abi_encode(), U256::ONE),
                payable(delegateCall { validatorId: VALIDATOR + 1 }.abi_encode(), MON),
                payable(claimRewardsCall { validatorId: VALIDATOR }.abi_encode(), MON),
            ],
        );
        assert!(results.iter().all(|r| !r.is_success()));
        let mut slots = prestate();
        slots.push((validator_key(VALIDATOR, validator_offsets::STAKE), U256::MAX));
        let result = assert_calls(
            spec,
            &slots,
            &[payable(delegateCall { validatorId: VALIDATOR }.abi_encode(), MON)],
        );
        assert_eq!(result[0].output.as_ref(), b"internal error");
    }
}

#[derive(Clone, Debug)]
struct Call {
    input: Bytes,
    value: U256,
    gas_limit: u64,
}

fn call(input: impl Into<Bytes>) -> Call {
    Call { input: input.into(), value: U256::ZERO, gas_limit: 2_000_000 }
}

fn prestate() -> Vec<(U256, U256)> {
    vec![
        (global_slots::EPOCH, U256::from(2) << 192),
        (validator_key(VALIDATOR, validator_offsets::STAKE), MON * U256::from(100)),
        (
            validator_key(VALIDATOR, validator_offsets::ADDRESS_FLAGS),
            U256::from_be_slice(SENDER.as_slice()) << 96,
        ),
    ]
}

/// Compare every changed slot, outcome, return data, logs and touched balance/nonce.
fn assert_calls(
    spec: MonadHardfork,
    slots: &[(U256, U256)],
    calls: &[Call],
) -> Vec<TxResult<MonadEvmTypes>> {
    assert_calls_as(spec, slots, calls, SENDER, false)
}

fn assert_calls_as(
    spec: MonadHardfork,
    slots: &[(U256, U256)],
    calls: &[Call],
    caller: Address,
    system: bool,
) -> Vec<TxResult<MonadEvmTypes>> {
    let reference_spec = FORKS.iter().find(|(fork, _)| *fork == spec).unwrap().1;
    let mut db = InMemoryDB::default();
    let mut reference_db = ReferenceDB::default();
    for address in [SENDER, STAKING_ADDRESS, SYSTEM_ADDRESS] {
        let balance = MON * U256::from(1_000_000_000u64);
        db.insert_account_info(&address, AccountInfo::default().with_balance(balance));
        reference_db
            .insert_account_info(address, ReferenceAccount::default().with_balance(balance));
    }
    for (key, value) in slots {
        db.insert_account_storage(&STAKING_ADDRESS, key, value);
        reference_db.insert_account_storage(STAKING_ADDRESS, *key, *value).unwrap();
    }
    let mut evm = MonadEvmFactory::new(1).create_evm(spec, MonadBlockEnv::default(), db);
    let mut reference = monad_context_with_db(reference_db)
        .with_cfg(MonadCfgEnv::new_with_spec(reference_spec).with_chain_id(1))
        .build_monad();
    let mut results = Vec::new();
    for (nonce, call) in calls.iter().enumerate() {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: nonce as u64,
            to: STAKING_ADDRESS.into(),
            input: call.input.clone(),
            value: call.value,
            gas_limit: call.gas_limit,
            ..Default::default()
        };
        let expected = if system {
            assert_eq!(call.value, U256::ZERO);
            reference.system_call_with_caller(caller, STAKING_ADDRESS, call.input.clone()).unwrap()
        } else {
            reference
                .transact(
                    TxEnv::builder()
                        .caller(caller)
                        .kind(tx.to)
                        .value(tx.value)
                        .data(tx.input.clone())
                        .gas_limit(tx.gas_limit)
                        .gas_price(0)
                        .nonce(tx.nonce)
                        .chain_id(Some(1))
                        .build()
                        .unwrap(),
                )
                .unwrap()
        };
        let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), caller);
        let pending = if system {
            evm.system_call(staking_system_tx(call.input.clone()).with_caller(caller))
        } else {
            evm.transact(&tx)
        }
        .unwrap();
        let actual_slots = pending
            .account_changes()
            .flat_map(|account| {
                account
                    .storage
                    .slots
                    .iter()
                    .filter(|(_, slot)| slot.value.original != slot.value.current)
                    .map(move |(key, slot)| ((account.address, *key), slot.value.current))
            })
            .collect::<BTreeMap<_, _>>();
        let expected_slots = expected
            .state
            .iter()
            .flat_map(|(address, account)| {
                account
                    .storage
                    .iter()
                    .filter(|(_, slot)| slot.is_changed())
                    .map(move |(key, slot)| ((*address, *key), slot.present_value))
            })
            .collect::<BTreeMap<_, _>>();
        let result = pending.commit();
        assert_eq!(actual_slots, expected_slots, "{spec} call {nonce}");
        assert_eq!(result.is_success(), expected.result.is_success(), "{spec} call {nonce}");
        assert_eq!(
            result.stop == InstrStop::Revert,
            matches!(expected.result, ExecutionResult::Revert { .. }),
            "{spec} call {nonce}"
        );
        assert_eq!(
            result.output.as_ref(),
            expected.result.output().map_or(&[][..], |b| b.as_ref()),
            "{spec} call {nonce}"
        );
        if !system {
            assert_eq!(
                result.total_gas_spent,
                expected.result.gas().total_gas_spent(),
                "{spec} call {nonce}"
            );
        }
        assert_eq!(&result.logs, expected.result.logs(), "{spec} call {nonce}");
        for (address, account) in &expected.state {
            let actual =
                evm.state_mut().account_info_untracked(address).unwrap().unwrap_or_default();
            assert_eq!(
                (actual.balance, actual.nonce),
                (account.info.balance, account.info.nonce),
                "{spec} call {nonce}: {address}"
            );
        }
        reference.0.ctx.journaled_state.database.commit(expected.state);
        results.push(result);
    }
    results
}

#[test]
fn query_abi_gas_and_error_ordering_match_release() {
    let queries = [
        (getEpochCall {}.abi_encode(), gas::GET_EPOCH),
        (getProposerValIdCall {}.abi_encode(), gas::GET_PROPOSER_VAL_ID),
        (getValidatorCall { validatorId: VALIDATOR }.abi_encode(), gas::GET_VALIDATOR),
        (
            getDelegatorCall { validatorId: VALIDATOR, delegator: SENDER }.abi_encode(),
            gas::GET_DELEGATOR,
        ),
        (
            getWithdrawalRequestCall { validatorId: VALIDATOR, delegator: SENDER, withdrawId: 1 }
                .abi_encode(),
            gas::GET_WITHDRAWAL_REQUEST,
        ),
        (
            getConsensusValidatorSetCall { startIndex: 0 }.abi_encode(),
            gas::GET_CONSENSUS_VALIDATOR_SET,
        ),
        (
            getSnapshotValidatorSetCall { startIndex: 0 }.abi_encode(),
            gas::GET_SNAPSHOT_VALIDATOR_SET,
        ),
        (
            getExecutionValidatorSetCall { startIndex: 0 }.abi_encode(),
            gas::GET_EXECUTION_VALIDATOR_SET,
        ),
        (
            getDelegationsCall { delegator: SENDER, startValId: 0 }.abi_encode(),
            gas::GET_DELEGATIONS,
        ),
        (
            getDelegatorsCall { validatorId: VALIDATOR, startDelegator: Address::ZERO }
                .abi_encode(),
            gas::GET_DELEGATORS,
        ),
    ];
    for spec in MonadHardfork::ALL {
        for (input, cost) in &queries {
            let intrinsic =
                21_000 + input.iter().map(|b| if *b == 0 { 4 } else { 16 }).sum::<u64>();
            for budget in [cost - 1, *cost, cost + 1] {
                let mut call = call(input.clone());
                call.gas_limit = intrinsic + budget;
                let results = assert_calls(spec, &prestate(), &[call]);
                assert_eq!(results[0].is_success(), budget >= *cost, "{spec} {cost} {budget}");
            }
            let mut payable = call(input.clone());
            payable.value = U256::ONE;
            assert!(!assert_calls(spec, &prestate(), &[payable])[0].is_success());
        }
        for input in [
            vec![],
            vec![0xde, 0xad],
            vec![0xde, 0xad, 0xbe, 0xef],
            getValidatorCall::SELECTOR.to_vec(),
        ] {
            assert!(!assert_calls(spec, &prestate(), &[call(input)])[0].is_success());
        }
    }
}

#[test]
fn delegator_query_settles_rewards_once_and_clears_accumulator_reference() {
    let mut slots = prestate();
    slots.extend([
        (delegator_key(VALIDATOR, &SENDER, delegator_offsets::DELTA_STAKE), MON),
        (delegator_key(VALIDATOR, &SENDER, delegator_offsets::EPOCHS), U256::from(2) << 192),
        (accumulator_key(2, VALIDATOR, 0), U256::ZERO),
        (accumulator_key(2, VALIDATOR, 1), U256::ONE << 192),
        (validator_key(VALIDATOR, validator_offsets::ACCUMULATED_REWARD_PER_TOKEN), UNIT_BIAS),
        (validator_key(VALIDATOR, validator_offsets::UNCLAIMED_REWARDS), MON),
    ]);
    let query = call(getDelegatorCall { validatorId: VALIDATOR, delegator: SENDER }.abi_encode());
    for spec in MonadHardfork::ALL {
        let results = assert_calls(spec, &slots, &[query.clone(), query.clone()]);
        let first = getDelegatorCall::abi_decode_returns(&results[0].output).unwrap();
        assert_eq!(first.stake, MON);
        assert_eq!(first.unclaimedRewards, MON);
        assert_eq!(first.deltaEpoch, 0);
        assert_eq!(results[0].output, results[1].output);
    }
}

#[test]
fn validator_set_queries_stop_at_page_boundaries() {
    let mut slots = prestate();
    slots.push((valset_slots::EXECUTION, U256::from(101) << 192));
    for id in 1..=101u64 {
        slots.push((valset_slots::EXECUTION + U256::from(id), U256::from(id) << 192));
    }
    for spec in MonadHardfork::ALL {
        let results = assert_calls(
            spec,
            &slots,
            &[
                call(getExecutionValidatorSetCall { startIndex: 0 }.abi_encode()),
                call(getExecutionValidatorSetCall { startIndex: 100 }.abi_encode()),
                call(getExecutionValidatorSetCall { startIndex: 101 }.abi_encode()),
            ],
        );
        assert!(results.iter().all(|r| r.is_success()));
        let page = getExecutionValidatorSetCall::abi_decode_returns(&results[0].output).unwrap();
        assert_eq!(page.valIds.len(), 100);
    }
}
