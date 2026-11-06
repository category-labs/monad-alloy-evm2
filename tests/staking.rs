//! Differential staking execution against monad-revm 0.8.0.

mod common;

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_monad_evm2::{
    MonadBlockEnv, MonadEvmFactory, MonadEvmTypes, MonadHardfork, MonadTxEnvelope,
    staking::{
        STAKING_ADDRESS,
        abi::gas,
        constants::{MON, UNIT_BIAS},
        interface::IMonadStaking::*,
        storage::{
            accumulator_key, delegator_key, delegator_offsets, global_slots, validator_key,
            validator_offsets, valset_slots,
        },
    },
};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use common::FORKS;
use evm2::{AccountInfo, TxResult, evm::InMemoryDB, interpreter::InstrStop};
use monad_revm::{MonadBuilder, MonadCfgEnv, monad_context_with_db};
use revm::{
    DatabaseCommit, ExecuteEvm, context::TxEnv, context_interface::result::ExecutionResult,
    database::InMemoryDB as ReferenceDB, state::AccountInfo as ReferenceAccount,
};
use std::collections::BTreeMap;

const SENDER: Address = Address::repeat_byte(0x11);
const VALIDATOR: u64 = 7;

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
    let reference_spec = FORKS.iter().find(|(fork, _)| *fork == spec).unwrap().1;
    let mut db = InMemoryDB::default();
    let mut reference_db = ReferenceDB::default();
    for address in [SENDER, STAKING_ADDRESS] {
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
        let expected = reference
            .transact(
                TxEnv::builder()
                    .caller(SENDER)
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
            .unwrap();
        let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), SENDER);
        let pending = evm.transact(&tx).unwrap();
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
        assert_eq!(
            result.total_gas_spent,
            expected.result.gas().total_gas_spent(),
            "{spec} call {nonce}"
        );
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
