//! Cryptographic precompile vectors and compatibility with monad-revm 0.8.0.

mod common;

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use common::FORKS;
use evm2::{
    AccountInfo, Precompiles, TxResult,
    bytecode::Bytecode,
    evm::{InMemoryDB, PrewarmSet},
    interpreter::{GasTracker, Message, op},
    precompile::PrecompileProvider,
    precompiles::{PrecompileHalt, PrecompileResult},
};
use monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork,
    MonadTxEnvelope, monad_precompiles,
};
use monad_revm::{
    MonadHardfork as ReferenceHardfork, precompiles::MonadPrecompiles as ReferencePrecompiles,
};
use revm::precompile::PrecompileStatus;
use serde_json::Value;
use std::collections::BTreeSet;

const SENDER: Address = Address::repeat_byte(0x11);
const CONTRACT: Address = Address::repeat_byte(0x22);
const TX_GAS: u64 = 3_000_000;

#[derive(Debug)]
struct Vector {
    name: String,
    address: u16,
    input: Bytes,
    output: Bytes,
    gas: u64,
    success: bool,
}

fn vectors() -> Vec<Vector> {
    // Each fixture records its original source alongside the known answer.
    let fixtures =
        serde_json::from_str::<Vec<Value>>(include_str!("fixtures/precompiles.json")).unwrap();
    fixtures
        .into_iter()
        .map(|v| Vector {
            name: v["name"].as_str().unwrap().into(),
            address: u16::try_from(v["address"].as_u64().unwrap()).unwrap(),
            input: v["input"].as_str().unwrap().parse().unwrap(),
            output: v["output"].as_str().unwrap().parse().unwrap(),
            gas: v["gas"].as_u64().unwrap(),
            success: v["success"].as_bool().unwrap(),
        })
        .collect()
}

fn address(value: u16) -> Address {
    Address::from_word(U256::from(value).into())
}

fn evm(spec: MonadHardfork) -> MonadEvm<'static> {
    MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID).create_evm(
        spec,
        MonadBlockEnv::default(),
        InMemoryDB::default(),
    )
}

fn run_precompile(
    evm: &mut MonadEvm<'_>,
    provider: &mut Precompiles<MonadEvmTypes>,
    target: u16,
    input: Bytes,
    limit: u64,
) -> (PrecompileResult, GasTracker) {
    let target = address(target);
    let message = Message::<MonadEvmTypes> {
        destination: target,
        call_target: target,
        code_address: target,
        input: input.into(),
        gas_limit: limit,
        ..Default::default()
    };
    let mut gas = GasTracker::new(limit);
    let result = provider.execute(evm, &message, &mut gas).expect("registered precompile");
    (result, gas)
}

fn transact(
    spec: MonadHardfork,
    target: Address,
    input: Bytes,
    code: Bytes,
) -> TxResult<MonadEvmTypes> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&CONTRACT, AccountInfo::default().with_code(Bytecode::new_legacy(code)));
    let mut evm =
        MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID).create_evm(spec, MonadBlockEnv::default(), db);
    let tx = TxLegacy { to: TxKind::Call(target), input, gas_limit: TX_GAS, ..Default::default() };
    evm.call_tx(&Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), SENDER)).unwrap()
}

#[test]
fn cryptographic_addresses_match_release_and_are_prewarmed() {
    let expected = (1..=0x11).chain([0x100]).map(address).collect::<BTreeSet<_>>();
    for (spec, reference_spec) in FORKS {
        let provider = monad_precompiles(spec);
        let reference = ReferencePrecompiles::new_with_spec(reference_spec);
        let mut all_addresses = expected.clone();
        all_addresses.insert(address(0x1000));
        if spec >= MonadHardfork::MonadNine {
            all_addresses.insert(address(0x1001));
        }
        assert_eq!(
            provider.addresses().into_iter().collect::<BTreeSet<_>>(),
            all_addresses,
            "{spec}"
        );
        assert_eq!(
            reference.precompiles().addresses().copied().collect::<BTreeSet<_>>(),
            expected,
            "{spec}"
        );
        let mut warm = PrewarmSet::new();
        provider.warm_addresses(&mut warm);
        for target in &expected {
            assert!(provider.contains(target), "{spec}: {target}");
            assert!(warm.is_warm(target), "{spec}: {target}");
        }
        // Mutating the Monad map must not change evm2's cached Ethereum map.
        if spec == MonadHardfork::MonadEight {
            assert!(
                !Precompiles::<MonadEvmTypes>::base(spec.into_eth_spec()).contains(&address(0x100))
            );
        }
    }
}

#[test]
fn known_answers_and_gas_boundaries_match_release() {
    for (spec, reference_spec) in FORKS {
        let mut evm = evm(spec);
        let mut provider = monad_precompiles(spec);
        let reference = ReferencePrecompiles::new_with_spec(reference_spec);
        for vector in vectors() {
            let precompile = reference.precompiles().get(&address(vector.address)).unwrap();
            for limit in [0, vector.gas - 1, vector.gas, vector.gas + 123] {
                let label = format!("{spec}/{}/{limit}", vector.name);
                let (actual, gas) = run_precompile(
                    &mut evm,
                    &mut provider,
                    vector.address,
                    vector.input.clone(),
                    limit,
                );
                let expected = precompile.execute(&vector.input, limit, 0).unwrap();
                assert_eq!(actual.is_ok(), expected.status.is_success(), "{label}");
                assert_eq!(actual.is_ok(), vector.success && limit >= vector.gas, "{label}");
                match actual {
                    Ok(output) => {
                        assert_eq!(output.bytes(), vector.output.as_ref(), "{label}");
                        assert_eq!(output.bytes(), expected.bytes.as_ref(), "{label}");
                        assert_eq!(gas.spent(), vector.gas, "{label}");
                        assert_eq!(gas.spent(), expected.gas_used, "{label}");
                    }
                    Err(error) => {
                        // The engines expose separate types for the same precompile halt reasons.
                        assert_eq!(
                            format!("{:?}", error.as_halt().expect("non-fatal halt")),
                            format!("{:?}", expected.status.halt_reason().unwrap()),
                            "{label}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn factory_dispatches_top_level_calls_and_preserves_execution_gas() {
    for (spec, _) in FORKS {
        for vector in vectors() {
            let label = format!("{spec}/{}", vector.name);
            let intrinsic =
                21_000 + vector.input.iter().map(|&b| if b == 0 { 4 } else { 16 }).sum::<u64>();
            let result = transact(spec, address(vector.address), vector.input, Bytes::new());
            assert_eq!(result.is_success(), vector.success, "{label}");
            assert_eq!(result.output, vector.output, "{label}");
            assert_eq!(
                result.total_gas_spent,
                if vector.success { intrinsic + vector.gas } else { TX_GAS },
                "{label}"
            );
            // Precompile execution still follows Monad's full-limit transaction charging.
            assert_eq!(result.tx_gas_used(), TX_GAS, "{label}");
            assert_eq!(result.refunded, 0, "{label}");
        }
    }
}

#[test]
fn nested_calls_enforce_exact_gas_and_return_unused_gas_to_the_caller() {
    for (spec, _) in FORKS {
        for vector in vectors() {
            for limit in [vector.gas - 1, vector.gas, vector.gas + 123] {
                let label = format!("{spec}/{}/{limit}", vector.name);
                let code = staticcall_code(vector.address, limit);
                let result = transact(spec, CONTRACT, vector.input.clone(), code);
                assert!(result.is_success(), "{label}: parent survives child failure");
                let success = vector.success && limit >= vector.gas;
                let mut expected = U256::from(u8::from(success)).to_be_bytes_vec();
                if success {
                    expected.extend_from_slice(&vector.output);
                }
                assert_eq!(result.output, expected, "{label}");
                assert_eq!(result.tx_gas_used(), TX_GAS, "{label}");

                // Re-run with a larger allowance: a successful child returns all
                // unused gas, while a halted child consumes its whole allowance.
                let extra = 100;
                if success || !vector.success {
                    let code = staticcall_code(vector.address, limit + extra);
                    let more = transact(spec, CONTRACT, vector.input.clone(), code);
                    assert_eq!(more.output, result.output, "{label}");
                    assert_eq!(
                        more.total_gas_spent - result.total_gas_spent,
                        if success { 0 } else { extra },
                        "{label}"
                    );
                }
            }
        }
    }
}

fn staticcall_code(target: u16, limit: u64) -> Bytes {
    // Copy calldata, STATICCALL with an exact allowance, then return a
    // success word followed by the entire precompile output.
    hex::decode(format!("365f5f375f5f365f61{target:04x}63{limit:08x}fa5f523d5f60203e3d6020015ff3"))
        .unwrap()
        .into()
}

#[test]
fn factory_warms_precompiles_before_contract_execution() {
    for (spec, _) in FORKS {
        for target in (1..=0x11u16).chain([0x100, 0x101]) {
            let [high, low] = target.to_be_bytes();
            let code = Bytes::from(vec![op::PUSH2, high, low, op::BALANCE, op::POP, op::STOP]);
            let result = transact(spec, CONTRACT, Bytes::new(), code);
            assert!(result.is_success());
            let access = if target == 0x101 { 10_100 } else { 100 };
            assert_eq!(result.total_gas_spent, 21_000 + 3 + access + 2, "{spec}/{target:#x}");
        }
    }
}

fn modexp_input(base: &[u8], exponent: &[u8], modulus: &[u8]) -> Bytes {
    let mut input = Vec::new();
    for part in [base, exponent, modulus] {
        input.extend_from_slice(&U256::from(part.len()).to_be_bytes::<32>());
    }
    for part in [base, exponent, modulus] {
        input.extend_from_slice(part);
    }
    input.into()
}

fn compare_modexp(
    (spec, reference_spec): (MonadHardfork, ReferenceHardfork),
    input: Bytes,
    limit: u64,
) -> (PrecompileResult, GasTracker) {
    let mut evm = evm(spec);
    let (actual, gas) =
        run_precompile(&mut evm, &mut monad_precompiles(spec), 5, input.clone(), limit);
    let expected = ReferencePrecompiles::new_with_spec(reference_spec)
        .precompiles()
        .get(&address(5))
        .unwrap()
        .execute(&input, limit, 0)
        .unwrap();
    match &actual {
        Ok(output) => {
            assert_eq!(expected.status, PrecompileStatus::Success);
            assert_eq!(output.bytes(), expected.bytes.as_ref());
            assert_eq!(gas.spent(), expected.gas_used);
        }
        Err(error) => assert_eq!(
            format!("{:?}", error.as_halt().unwrap()),
            format!("{:?}", expected.status.halt_reason().unwrap()),
        ),
    }
    (actual, gas)
}

#[test]
fn modexp_pricing_changes_at_nine() {
    for forks in FORKS {
        let eight = forks.0 == MonadHardfork::MonadEight;
        for (input, cost) in [
            (modexp_input(&[3], &[5], &[7]), if eight { 200 } else { 500 }),
            (
                modexp_input(&[0xff; 32], &[0xff; 32], &[0xfd; 32]),
                if eight { 1_360 } else { 4_080 },
            ),
            (Bytes::new(), if eight { 200 } else { 500 }),
        ] {
            for limit in [cost - 1, cost, cost + 123] {
                let (result, gas) = compare_modexp(forks, input.clone(), limit);
                if limit < cost {
                    assert!(result.unwrap_err().is_oog());
                } else {
                    assert!(result.is_ok());
                    assert_eq!(gas.spent(), cost);
                }
            }
        }
    }
}

#[test]
fn modexp_limits_each_operand_to_1024_bytes_from_nine() {
    for forks in FORKS {
        for operand in 0..3 {
            for length in [1_023, 1_024, 1_025] {
                let mut parts = [vec![0], vec![1], vec![1]];
                parts[operand] = vec![0; length];
                let input = modexp_input(&parts[0], &parts[1], &parts[2]);
                let (result, _) = compare_modexp(forks, input, 100_000_000);
                if length > 1_024 && forks.0 != MonadHardfork::MonadEight {
                    assert_eq!(
                        result.unwrap_err().as_halt(),
                        Some(&PrecompileHalt::ModexpEip7823LimitSize)
                    );
                } else {
                    assert!(result.is_ok(), "{forks:?}/{operand}/{length}: {result:?}");
                }
            }
        }
    }
}

#[test]
fn blake2_zero_rounds_and_maximum_rounds_do_not_overflow_pricing() {
    for (spec, _) in FORKS {
        let mut evm = evm(spec);
        let mut provider = monad_precompiles(spec);
        let input = Bytes::from(vec![0; 213]);
        let (result, gas) = run_precompile(&mut evm, &mut provider, 9, input, 0);
        assert!(result.is_ok());
        assert_eq!(gas.spent(), 0);

        // Stop before compression: the largest round count still needs 2 * u32::MAX gas.
        let mut input = vec![0; 213];
        input[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        let (result, _) =
            run_precompile(&mut evm, &mut provider, 9, input.into(), 2 * u64::from(u32::MAX) - 1);
        assert!(result.unwrap_err().is_oog());
    }
}
