//! Relocation policy for native and cryptographic precompiles.

use alloy_primitives::{Address, Bytes};
use evm2::{
    ExecutionConfig, Precompiles,
    evm::{InMemoryDB, PrewarmSet, SystemTx},
    interpreter::{GasTracker, Message},
    precompile::{PrecompileOutput, PrecompileProvider},
    precompiles::{MovePrecompileError, Precompile, PrecompileId, PrecompileResult},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvmBuilder, MonadEvmTypes, MonadHardfork, MonadHost, MonadPrecompiles,
    monad_precompiles, reserve_balance::RESERVE_BALANCE_ADDRESS, staking::STAKING_ADDRESS,
    syscall_snapshot_tx,
};
use std::borrow::Cow;

const IDENTITY: Address = Address::with_last_byte(4);
const DESTINATION: Address = Address::repeat_byte(0x55);

fn ids(provider: &impl PrecompileProvider<MonadEvmTypes>) -> Vec<(Address, PrecompileId)> {
    let mut ids = provider.precompile_ids();
    ids.sort_unstable_by_key(|(address, _)| *address);
    ids
}

#[test]
fn native_sources_reject_entire_batches_on_every_fork() {
    for spec in MonadHardfork::ALL {
        let mut provider = MonadPrecompiles::new(spec);
        assert_eq!(provider.spec(), spec);
        let before = ids(&provider);
        for native in [STAKING_ADDRESS, RESERVE_BALANCE_ADDRESS] {
            for moves in [
                vec![(native, DESTINATION)],
                vec![(IDENTITY, DESTINATION), (native, IDENTITY)],
                vec![(native, IDENTITY), (IDENTITY, native)],
            ] {
                assert_eq!(
                    provider.move_precompiles(&moves),
                    Err(MovePrecompileError::NotAPrecompile(native))
                );
                assert_eq!(ids(&provider), before);
                assert!(!provider.contains(&DESTINATION));
            }
            // Self-moves are no-ops, including inactive addresses.
            provider.move_precompiles(&[(native, native)]).unwrap();
            assert_eq!(ids(&provider), before);
        }
        let mut warm = PrewarmSet::new();
        provider.warm_addresses(&mut warm);
        assert!(warm.is_warm(&STAKING_ADDRESS));
        assert_eq!(warm.is_warm(&RESERVE_BALANCE_ADDRESS), spec >= MonadHardfork::MonadNine);
        assert!(warm.is_warm(&IDENTITY));
        assert!(!warm.is_warm(&DESTINATION));
    }
}

#[test]
fn crypto_moves_retain_execution_identity_and_warmth() {
    for spec in MonadHardfork::ALL {
        let mut provider = monad_precompiles(spec);
        let identity_id = provider.as_map().get(&IDENTITY).unwrap().id().clone();
        provider
            .move_precompiles(&[(STAKING_ADDRESS, STAKING_ADDRESS), (IDENTITY, DESTINATION)])
            .unwrap();
        assert!(!provider.contains(&IDENTITY));
        assert_eq!(provider.as_map().get(&DESTINATION).unwrap().id(), &identity_id);
        let mut warm = PrewarmSet::new();
        provider.warm_addresses(&mut warm);
        assert!(warm.is_warm(&DESTINATION));
        assert!(!warm.is_warm(&IDENTITY));
        let mut evm = MonadEvmBuilder::new(1).create_evm(
            spec,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        evm.set_precompiles(provider);
        let input = Bytes::from_static(b"relocated identity");
        let output = evm.system_call(SystemTx::new(DESTINATION, input.clone())).unwrap().discard();
        assert!(output.is_success());
        assert_eq!(output.output, input);
        assert_eq!(output.total_gas_spent, 18);
        assert!(evm.system_call(syscall_snapshot_tx()).unwrap().discard().is_success());
    }
}

fn identity(
    _: &mut MonadHost<'_>,
    message: &Message<MonadEvmTypes>,
    _: &mut GasTracker,
) -> PrecompileResult {
    Ok(PrecompileOutput::new(message.input.to_bytes()))
}

#[test]
fn custom_provider_installation_retains_native_address_guards() {
    for spec in MonadHardfork::ALL {
        for mode in 0..5 {
            let mut evm = MonadEvmBuilder::new(1).create_evm(
                spec,
                MonadBlockEnv::default(),
                InMemoryDB::default(),
            );
            let mut custom = Precompiles::new(Cow::Owned(monad_precompiles(spec).as_map().clone()));
            // Protection is attached to the protocol source address, even after replacement.
            custom.as_map_mut().insert(Precompile::new(
                STAKING_ADDRESS,
                PrecompileId::custom("Replacement"),
                identity,
            ));
            let config = ExecutionConfig::for_spec_and_version(spec, *evm.version());
            let registry = evm.registry().clone();
            match mode {
                0 => {}
                1 => evm.set_precompiles(custom),
                2 => evm.set_boxed_precompiles(Box::new(custom)),
                3 => evm.set_execution_config(config, spec, registry, custom),
                _ => evm.set_block_and_execution_config(
                    MonadBlockEnv::default(),
                    config,
                    spec,
                    registry,
                    custom,
                ),
            }
            for native in [STAKING_ADDRESS, RESERVE_BALANCE_ADDRESS] {
                assert_eq!(
                    evm.precompiles_mut()
                        .move_precompiles(&[(IDENTITY, DESTINATION), (native, IDENTITY)]),
                    Err(MovePrecompileError::NotAPrecompile(native))
                );
                assert!(evm.precompiles().contains(&IDENTITY));
                assert!(!evm.precompiles().contains(&DESTINATION));
            }
            evm.precompiles_mut().move_precompiles(&[(IDENTITY, DESTINATION)]).unwrap();
            let result = evm
                .system_call(SystemTx::new(DESTINATION, Bytes::from_static(b"hello")))
                .unwrap()
                .discard();
            assert_eq!(result.output.as_ref(), b"hello");
            assert!(result.is_success());
            evm.reset_precompiles();
            assert!(evm.precompiles().contains(&IDENTITY));
            assert!(!evm.precompiles().contains(&DESTINATION));
        }
    }
}

#[test]
fn rejected_unknown_sources_and_crypto_swaps_are_atomic() {
    for spec in MonadHardfork::ALL {
        let mut provider = monad_precompiles(spec);
        let before = ids(&provider);
        let unknown = Address::repeat_byte(0x66);
        assert_eq!(
            provider.move_precompiles(&[(IDENTITY, DESTINATION), (unknown, IDENTITY)]),
            Err(MovePrecompileError::NotAPrecompile(unknown))
        );
        assert_eq!(ids(&provider), before);
        let sha256 = Address::with_last_byte(2);
        let identity_id = provider.as_map().get(&IDENTITY).unwrap().id().clone();
        let sha_id = provider.as_map().get(&sha256).unwrap().id().clone();
        provider.move_precompiles(&[(IDENTITY, sha256), (sha256, IDENTITY)]).unwrap();
        assert_eq!(provider.as_map().get(&IDENTITY).unwrap().id(), &sha_id);
        assert_eq!(provider.as_map().get(&sha256).unwrap().id(), &identity_id);
    }
}
