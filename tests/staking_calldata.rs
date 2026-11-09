//! Staking input boundaries from the C++ precompile, including its permissive padding rules.
//!
//! Reference: category-labs/monad at e8f793d145fad654b3a8b0b77c9826f0f2572312,
//! category/execution/monad/staking/staking_contract.cpp and
//! category/execution/ethereum/core/contract/abi_decode.hpp.

use alloy_monad_evm2::{
    MonadBlockEnv, MonadEvmFactory, MonadEvmTypes, MonadHardfork, SYSTEM_ADDRESS,
    staking::{
        STAKING_ADDRESS, StorageReader,
        abi::gas,
        constants::DUST_THRESHOLD,
        interface::IMonadStaking::*,
        result::InstructionResult,
        run_staking_precompile, run_staking_with_reader,
        write::{StakingStorage, run_staking_write},
    },
};
use alloy_primitives::{Address, Bytes, Log, U256};
use alloy_sol_types::SolCall;
use evm2::{
    evm::InMemoryDB,
    interpreter::{GasTracker, Message},
    precompiles::{PrecompileError, PrecompileHalt},
};
use std::collections::BTreeMap;

const GAS_LIMIT: u64 = 1_000_000;
const DELEGATOR: Address = Address::repeat_byte(0x11);

#[derive(Clone, Copy, Debug)]
enum Route {
    Native,
    Adapter,
}

const ROUTES: [Route; 2] = [Route::Native, Route::Adapter];

#[derive(Default)]
struct Storage {
    slots: BTreeMap<U256, U256>,
    accesses: usize,
}

impl StorageReader for Storage {
    fn sload(&mut self, key: U256) -> Result<U256, PrecompileHalt> {
        self.accesses += 1;
        Ok(self.slots.get(&key).copied().unwrap_or_default())
    }
}

impl StakingStorage for Storage {
    fn sstore(&mut self, key: U256, value: U256) -> Result<(), PrecompileHalt> {
        self.accesses += 1;
        self.slots.insert(key, value);
        Ok(())
    }

    fn transfer(
        &mut self,
        _from: Address,
        _to: Address,
        _amount: U256,
    ) -> Result<(), PrecompileHalt> {
        self.accesses += 1;
        Ok(())
    }

    fn emit_log(&mut self, _log: Log) -> Result<(), PrecompileHalt> {
        self.accesses += 1;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    result: InstructionResult,
    output: Bytes,
    remaining: u64,
}

fn run(
    route: Route,
    reader: bool,
    input: &[u8],
    gas_limit: u64,
    caller: Address,
    value: U256,
) -> (Outcome, usize) {
    match route {
        Route::Native => {
            let mut evm = MonadEvmFactory::new(1).create_evm(
                MonadHardfork::MonadTen,
                MonadBlockEnv::default(),
                InMemoryDB::default(),
            );
            let message = Message::<MonadEvmTypes> {
                destination: STAKING_ADDRESS,
                code_address: STAKING_ADDRESS,
                call_target: STAKING_ADDRESS,
                caller,
                value,
                input: Bytes::copy_from_slice(input),
                gas_limit,
                ..Default::default()
            };
            let mut gas = GasTracker::new(gas_limit);
            let (result, output) = match run_staking_precompile(&mut evm, &message, &mut gas) {
                Ok(output) => (InstructionResult::Return, output.into_bytes()),
                Err(PrecompileError::Revert(output)) => (InstructionResult::Revert, output),
                Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)) => {
                    (InstructionResult::PrecompileOOG, Bytes::new())
                }
                other => panic!("unexpected precompile result: {other:?}"),
            };
            let accesses = evm.state().loaded_accounts().map(|(_, slots)| slots.count()).sum();
            (Outcome { result, output, remaining: gas.remaining() }, accesses)
        }
        Route::Adapter => {
            let mut storage = Storage::default();
            let result = if reader {
                run_staking_with_reader(input, gas_limit, &mut storage, value)
            } else {
                run_staking_write(input, gas_limit, &mut storage, &caller, value)
            };
            (
                Outcome {
                    result: result.result,
                    output: result.output,
                    remaining: result.gas.remaining(),
                },
                storage.accesses,
            )
        }
    }
}

fn assert_revert(result: &Outcome, message: &str) {
    assert_eq!(result.result, InstructionResult::Revert);
    assert_eq!(result.output.as_ref(), message.as_bytes());
    assert_eq!(result.remaining, 0);
}

#[test]
fn fixed_arguments_consume_exact_words() {
    let cases = [
        (getValidatorCall { validatorId: 1 }.abi_encode(), gas::GET_VALIDATOR, true, vec![8]),
        (
            getDelegatorCall { validatorId: 1, delegator: DELEGATOR }.abi_encode(),
            gas::GET_DELEGATOR,
            true,
            vec![8, 20],
        ),
        (
            getWithdrawalRequestCall { validatorId: 1, delegator: DELEGATOR, withdrawId: 7 }
                .abi_encode(),
            gas::GET_WITHDRAWAL_REQUEST,
            true,
            vec![8, 20, 1],
        ),
        (
            getConsensusValidatorSetCall { startIndex: 0 }.abi_encode(),
            gas::GET_CONSENSUS_VALIDATOR_SET,
            true,
            vec![4],
        ),
        (
            getSnapshotValidatorSetCall { startIndex: 0 }.abi_encode(),
            gas::GET_SNAPSHOT_VALIDATOR_SET,
            true,
            vec![4],
        ),
        (
            getExecutionValidatorSetCall { startIndex: 0 }.abi_encode(),
            gas::GET_EXECUTION_VALIDATOR_SET,
            true,
            vec![4],
        ),
        (
            getDelegationsCall { delegator: DELEGATOR, startValId: 1 }.abi_encode(),
            gas::GET_DELEGATIONS,
            true,
            vec![20, 8],
        ),
        (
            getDelegatorsCall { validatorId: 1, startDelegator: DELEGATOR }.abi_encode(),
            gas::GET_DELEGATORS,
            true,
            vec![8, 20],
        ),
        (delegateCall { validatorId: 1 }.abi_encode(), gas::DELEGATE, false, vec![8]),
        (
            undelegateCall { validatorId: 1, amount: U256::ZERO, withdrawId: 7 }.abi_encode(),
            gas::UNDELEGATE,
            false,
            vec![8, 32, 1],
        ),
        (
            withdrawCall { validatorId: 1, withdrawId: 7 }.abi_encode(),
            gas::WITHDRAW,
            false,
            vec![8, 1],
        ),
        (compoundCall { validatorId: 1 }.abi_encode(), gas::COMPOUND, false, vec![8]),
        (claimRewardsCall { validatorId: 1 }.abi_encode(), gas::CLAIM_REWARDS, false, vec![8]),
        (
            changeCommissionCall { validatorId: 1, commission: U256::ZERO }.abi_encode(),
            gas::CHANGE_COMMISSION,
            false,
            vec![8, 32],
        ),
        (externalRewardCall { validatorId: 1 }.abi_encode(), gas::EXTERNAL_REWARD, false, vec![8]),
        (
            getDelegatorCall { validatorId: 1, delegator: DELEGATOR }.abi_encode(),
            gas::GET_DELEGATOR,
            false,
            vec![8, 20],
        ),
        (
            syscallOnEpochChangeCall { epoch: 1 }.abi_encode(),
            gas::SYSCALL_ON_EPOCH_CHANGE,
            false,
            vec![8],
        ),
    ];
    for route in ROUTES {
        for (input, cost, reader, widths) in &cases {
            let value = if input.starts_with(&delegateCall::SELECTOR) {
                DUST_THRESHOLD
            } else {
                U256::ZERO
            };
            let (exact, _) = run(route, *reader, input, GAS_LIMIT, SYSTEM_ADDRESS, value);
            let mut dirty = input.clone();
            for (word, width) in widths.iter().enumerate() {
                dirty[4 + word * 32..4 + word * 32 + 32 - width].fill(0xff);
            }
            let (dirty_result, _) = run(route, *reader, &dirty, GAS_LIMIT, SYSTEM_ADDRESS, value);
            assert_eq!(dirty_result, exact, "{route:?} {input:?}");
            for length in 4..input.len() {
                let (result, accesses) =
                    run(route, *reader, &input[..length], GAS_LIMIT, SYSTEM_ADDRESS, value);
                assert_revert(&result, "input too short");
                assert_eq!(accesses, 0, "truncated calldata must fail before storage access");
            }
            for suffix in [&[0xff][..], &[0; 32][..], &[0xff; 33][..]] {
                let mut extra = input.clone();
                extra.extend_from_slice(suffix);
                let (result, accesses) =
                    run(route, *reader, &extra, GAS_LIMIT, SYSTEM_ADDRESS, value);
                assert_revert(&result, "invalid input");
                assert_eq!(accesses, 0, "trailing calldata must fail before storage access");
                let (result, accesses) =
                    run(route, *reader, &extra, cost - 1, SYSTEM_ADDRESS, value);
                assert_eq!(result.result, InstructionResult::PrecompileOOG);
                assert_eq!(accesses, 0);
            }
        }
    }
}

#[test]
fn zero_delegation_succeeds_without_accessing_staking_state() {
    for route in ROUTES {
        for validator_id in [0, 1, u64::MAX] {
            let input = delegateCall { validatorId: validator_id }.abi_encode();
            let (result, accesses) = run(route, false, &input, GAS_LIMIT, DELEGATOR, U256::ZERO);
            assert_eq!(result.result, InstructionResult::Return);
            assert!(delegateCall::abi_decode_returns(&result.output).unwrap());
            assert_eq!(result.remaining, GAS_LIMIT - gas::DELEGATE);
            assert_eq!(accesses, 0);
            let (result, accesses) =
                run(route, false, &input, gas::DELEGATE - 1, DELEGATOR, U256::ZERO);
            assert_eq!(result.result, InstructionResult::PrecompileOOG);
            assert_eq!(accesses, 0);
            let mut malformed = input;
            malformed.push(0xff);
            let (result, accesses) =
                run(route, false, &malformed, GAS_LIMIT, DELEGATOR, U256::ZERO);
            assert_revert(&result, "invalid input");
            assert_eq!(accesses, 0);
        }
    }
}

#[test]
fn parameterless_getters_ignore_argument_bytes() {
    for route in ROUTES {
        for (input, cost) in [
            (getEpochCall {}.abi_encode(), gas::GET_EPOCH),
            (getProposerValIdCall {}.abi_encode(), gas::GET_PROPOSER_VAL_ID),
        ] {
            let (exact, _) = run(route, true, &input, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
            assert_eq!(exact.result, InstructionResult::Return);
            assert_eq!(exact.remaining, GAS_LIMIT - cost);
            for suffix in [&[0xff][..], &[0xff; 32][..], &[0xff; 65][..]] {
                let mut extra = input.clone();
                extra.extend_from_slice(suffix);
                assert_eq!(
                    run(route, true, &extra, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO).0,
                    exact
                );
                let (result, accesses) =
                    run(route, true, &extra, GAS_LIMIT, SYSTEM_ADDRESS, U256::ONE);
                assert_revert(&result, "value non-zero");
                assert_eq!(accesses, 0);
            }
        }
    }
}

#[test]
fn snapshot_rejects_argument_bytes() {
    for route in ROUTES {
        let input = syscallSnapshotCall {}.abi_encode();
        assert_eq!(
            run(route, false, &input, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO).0.result,
            InstructionResult::Return
        );
        for suffix in [&[0xff][..], &[0; 32][..]] {
            let mut extra = input.clone();
            extra.extend_from_slice(suffix);
            let (result, accesses) =
                run(route, false, &extra, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
            assert_revert(&result, "invalid input");
            assert_eq!(accesses, 0);
        }
    }
}

fn registration_input() -> Vec<u8> {
    addValidatorCall {
        payload: vec![0; 165].into(),
        signedSecpMessage: vec![0; 64].into(),
        signedBlsMessage: vec![0; 96].into(),
    }
    .abi_encode()
}

#[test]
fn registration_ignores_offsets_and_padding() {
    for route in ROUTES {
        let input = registration_input();
        assert_eq!(input.len(), 548);
        for offsets in [input[4..100].to_vec(), vec![0; 96], vec![0xff; 96], vec![1; 96]] {
            let mut modified = input.clone();
            modified[4..100].copy_from_slice(&offsets);
            modified[297..324].fill(0xff);
            let (result, accesses) =
                run(route, false, &modified, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
            assert_revert(&result, "insufficient stake");
            assert_eq!(accesses, 0);
        }
    }
}

#[test]
fn registration_requires_complete_padded_tails() {
    for route in ROUTES {
        let input = registration_input();
        for length in 4..input.len() {
            let (result, accesses) =
                run(route, false, &input[..length], GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
            assert_revert(&result, "input too short");
            assert_eq!(accesses, 0);
        }
        for suffix in [&[0xff][..], &[0; 32][..]] {
            let mut extra = input.clone();
            extra.extend_from_slice(suffix);
            let (result, accesses) =
                run(route, false, &extra, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
            assert_revert(&result, "invalid input");
            assert_eq!(accesses, 0);
        }
    }
}

#[test]
fn registration_requires_exact_tail_lengths() {
    for route in ROUTES {
        for (offset, length) in [(100, 165), (324, 64), (420, 96)] {
            for invalid in [U256::ZERO, U256::from(length - 1), U256::from(length + 1), U256::MAX] {
                let mut input = registration_input();
                input[offset..offset + 32].copy_from_slice(&invalid.to_be_bytes::<32>());
                for end in [offset + 32, input.len()] {
                    let (result, accesses) =
                        run(route, false, &input[..end], GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
                    assert_revert(&result, "length mismatch");
                    assert_eq!(accesses, 0);
                }
            }
        }
    }
}

#[test]
fn syscall_reward_allows_only_the_complete_synthetic_extension() {
    for route in ROUTES {
        let input = syscallRewardCall { blockAuthor: DELEGATOR }.abi_encode();
        for length in 4..=101 {
            let mut modified = input.clone();
            modified.resize(length, 0xff);
            let (result, accesses) =
                run(route, false, &modified, GAS_LIMIT, SYSTEM_ADDRESS, U256::ZERO);
            let message = match length {
                4..36 => "input too short",
                36 | 68 => "not in validator set",
                _ => "invalid input",
            };
            assert_revert(&result, message);
            assert_eq!(accesses == 0, length != 36 && length != 68);
        }
    }
}

#[test]
fn payability_precedes_gas_and_decoding() {
    let cases = [
        (getValidatorCall::SELECTOR.to_vec(), true, gas::GET_VALIDATOR),
        (changeCommissionCall::SELECTOR.to_vec(), false, gas::CHANGE_COMMISSION),
        (
            [syscallSnapshotCall::SELECTOR.as_slice(), &[0xff]].concat(),
            false,
            gas::SYSCALL_SNAPSHOT,
        ),
        (syscallOnEpochChangeCall::SELECTOR.to_vec(), false, gas::SYSCALL_ON_EPOCH_CHANGE),
    ];
    for route in ROUTES {
        for (input, reader, cost) in &cases {
            for limit in [cost - 1, GAS_LIMIT] {
                let (result, accesses) =
                    run(route, *reader, input, limit, SYSTEM_ADDRESS, U256::ONE);
                assert_revert(&result, "value non-zero");
                assert_eq!(accesses, 0);
            }
        }
    }
}

#[test]
fn syscall_gas_and_authorization_precede_decoding() {
    for route in ROUTES {
        for (input, cost) in [
            (syscallRewardCall::SELECTOR.to_vec(), gas::SYSCALL_REWARD),
            ([syscallSnapshotCall::SELECTOR.as_slice(), &[0xff]].concat(), gas::SYSCALL_SNAPSHOT),
            (syscallOnEpochChangeCall::SELECTOR.to_vec(), gas::SYSCALL_ON_EPOCH_CHANGE),
        ] {
            let (result, accesses) = run(route, false, &input, cost - 1, DELEGATOR, U256::ZERO);
            assert_eq!(result.result, InstructionResult::PrecompileOOG);
            assert_eq!(accesses, 0);
            let (result, accesses) = run(route, false, &input, GAS_LIMIT, DELEGATOR, U256::ZERO);
            assert_revert(&result, "Unauthorized: not system address");
            assert_eq!(accesses, 0);
        }
    }
}
