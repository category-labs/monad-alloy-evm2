//! Independent hardfork, gas-table and runtime-limit expectations.

use evm2::{
    BaseEvmTypes, EvmFeatures, OpcodeConfig, SpecId, Version, interpreter::op, version::GasId,
};
use monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MONAD_MAX_CODE_SIZE, MONAD_MAX_INITCODE_SIZE, MONAD_MEMORY_LIMIT,
    MONAD_TESTNET_CHAIN_ID, MONAD_TX_GAS_LIMIT_CAP, MonadHardfork, MonadLimits,
    UnknownMonadHardfork, monad_gas_params, monad_memory_limit, monad_version,
};

#[test]
fn identifiers_and_names_are_stable() {
    let cases = [
        (MonadHardfork::MonadEight, 100, "MonadEight", SpecId::PRAGUE),
        (MonadHardfork::MonadNine, 101, "MonadNine", SpecId::OSAKA),
        (MonadHardfork::MonadTen, 102, "MonadTen", SpecId::OSAKA),
        (MonadHardfork::MonadNext, 103, "MonadNext", SpecId::OSAKA),
    ];
    assert_eq!(MonadHardfork::default(), MonadHardfork::MonadTen);
    assert_eq!(MonadHardfork::ALL, cases.map(|(fork, ..)| fork));
    assert_eq!(core::mem::size_of::<MonadHardfork>(), 1);
    for (fork, id, name, foundation) in cases {
        assert_eq!(fork as u8, id);
        assert_eq!(fork.to_string(), name);
        assert_eq!(fork.into_eth_spec(), foundation);
        assert_eq!(SpecId::from(fork), foundation);
        for spelling in [name.to_owned(), name.to_ascii_lowercase(), name.to_ascii_uppercase()] {
            assert_eq!(spelling.parse::<MonadHardfork>().unwrap(), fork);
        }
        for (active, active_id, ..) in cases {
            assert_eq!(fork.is_enabled_in(active), id <= active_id);
            assert_eq!(fork.cmp(&active), id.cmp(&active_id));
        }
    }
    for id in u8::MIN..=u8::MAX {
        let expected = cases.iter().find(|(_, value, ..)| *value == id).map(|(fork, ..)| *fork);
        assert_eq!(MonadHardfork::try_from_u8(id), expected);
    }
    for name in ["", "MonadSeven", "Prague", "100", " MonadNine", "MonadTen ", "monad-nine"] {
        assert_eq!(name.parse::<MonadHardfork>(), Err(UnknownMonadHardfork));
    }
}

#[test]
fn schedules_select_both_sides_of_each_activation() {
    assert_eq!((MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID), (143, 10_143));
    for (chain, activations) in [
        (143, [1_763_649_000, 1_773_930_600, 1_788_359_400]),
        (10_143, [1_763_562_600, 1_773_153_000, 1_786_545_000]),
    ] {
        for (index, fork) in MonadHardfork::ALL.into_iter().enumerate() {
            let scheduled = if chain == 143 {
                fork.mainnet_activation_timestamp()
            } else {
                fork.testnet_activation_timestamp()
            };
            assert_eq!(scheduled, activations.get(index).copied());
        }
        for (timestamp, expected) in [
            (0, MonadHardfork::MonadEight),
            (activations[0] - 1, MonadHardfork::MonadEight),
            (activations[0], MonadHardfork::MonadEight),
            (activations[0] + 1, MonadHardfork::MonadEight),
            (activations[1] - 1, MonadHardfork::MonadEight),
            (activations[1], MonadHardfork::MonadNine),
            (activations[1] + 1, MonadHardfork::MonadNine),
            (activations[2] - 1, MonadHardfork::MonadNine),
            (activations[2], MonadHardfork::MonadTen),
            (activations[2] + 1, MonadHardfork::MonadTen),
            (u64::MAX, MonadHardfork::MonadTen),
        ] {
            assert_eq!(MonadHardfork::from_chain_and_timestamp(chain, timestamp), Some(expected));
        }
    }
    for chain in [0, 1, 10, u64::MAX] {
        assert_eq!(MonadHardfork::from_chain_and_timestamp(chain, u64::MAX), None);
    }
}

#[test]
fn gas_tables_override_cold_access_prices_only() {
    for spec in MonadHardfork::ALL {
        let params = monad_gas_params(spec);
        let base = Version::base(spec.into_eth_spec());
        assert_eq!(params.get(GasId::ColdStorageCost), 8_100);
        assert_eq!(params.get(GasId::ColdStorageAdditionalCost), 8_000);
        assert_eq!(params.get(GasId::ColdAccountAdditionalCost), 10_000);
        assert_eq!(params.get(GasId::WarmStorageReadCost), 100);
        let mut restored = params;
        for id in [
            GasId::ColdStorageCost,
            GasId::ColdStorageAdditionalCost,
            GasId::ColdAccountAdditionalCost,
        ] {
            restored.set(id, base.gas_params.get(id).try_into().unwrap());
        }
        assert_eq!(restored, base.gas_params);
    }
}

#[test]
fn default_limits_and_features_follow_the_selected_fork() {
    assert_eq!(MONAD_TX_GAS_LIMIT_CAP, 30_000_000);
    assert_eq!(MONAD_MAX_CODE_SIZE, 131_072);
    assert_eq!(MONAD_MAX_INITCODE_SIZE, 262_144);
    assert_eq!(MONAD_MEMORY_LIMIT, 8_388_608);
    for spec in MonadHardfork::ALL {
        for chain in [0, 143, 10_143, u64::MAX] {
            let version = monad_version(spec, chain);
            assert_eq!(version.chain_id, chain);
            assert_eq!(version.tx_gas_caps(), (30_000_000, u64::MAX));
            assert_eq!(version.max_code_size, 131_072);
            assert_eq!(version.max_initcode_size, 262_144);
            assert_eq!(
                version.memory_limit,
                if spec == MonadHardfork::MonadEight { u32::MAX as u64 } else { 8_388_608 }
            );
            assert_eq!(version.gas_params, monad_gas_params(spec));
            let base = Version::base(spec.into_eth_spec());
            assert_eq!(version.features, base.features);
            assert_eq!(version.max_blobs_per_tx, base.max_blobs_per_tx);
            assert!(version.feature(EvmFeatures::EIP7623 | EvmFeatures::EIP7702));
            assert!(!version.features.intersects(
                EvmFeatures::EIP2780
                    | EvmFeatures::EIP8037
                    | EvmFeatures::EIP7708
                    | EvmFeatures::EIP8246
            ));
        }
    }
}

#[test]
fn explicit_limits_preserve_zero_saturate_sizes_and_restore_memory() {
    for spec in MonadHardfork::ALL {
        for gas_cap in [0, 12_345_678, u64::MAX] {
            for (code_size, derived_initcode) in
                [(0, 0), (100_000, 200_000), (usize::MAX, usize::MAX)]
            {
                for initcode_size in [None, Some(0), Some(300_000), Some(usize::MAX)] {
                    let limits = MonadLimits {
                        tx_gas_limit_cap: Some(gas_cap),
                        max_code_size: Some(code_size),
                        max_initcode_size: initcode_size,
                        ..Default::default()
                    };
                    let version = limits.version(spec, 143);
                    assert_eq!(version.tx_gas_limit_cap, gas_cap);
                    assert_eq!(version.max_code_size, code_size);
                    assert_eq!(
                        version.max_initcode_size,
                        initcode_size.unwrap_or(derived_initcode)
                    );
                }
            }
        }
    }
    for configured in [0, 16_000, 8_388_608, 8_388_609, 128 * 1024 * 1024, u64::MAX] {
        let limits = MonadLimits { memory_limit: Some(configured), ..Default::default() };
        for spec in MonadHardfork::ALL.into_iter().chain(MonadHardfork::ALL.into_iter().rev()) {
            let expected = if spec == MonadHardfork::MonadEight {
                configured
            } else {
                configured.min(8_388_608)
            };
            assert_eq!(limits.version(spec, 143).memory_limit, expected);
            assert_eq!(monad_memory_limit(spec, configured), expected);
        }
        assert_eq!(limits.memory_limit, Some(configured));
    }
}

#[test]
fn inherited_opcodes_follow_mip5_without_enabling_amsterdam() {
    for spec in MonadHardfork::ALL {
        let opcodes = OpcodeConfig::<BaseEvmTypes>::base_for_spec(spec.into_eth_spec());
        // MIP-5 activates CLZ at MonadNine; blob opcodes predate it despite blob
        // transactions being unsupported. C++ MonadTraits gates these separately.
        assert_eq!(opcodes.is_defined(op::CLZ), spec != MonadHardfork::MonadEight, "{spec}");
        for opcode in [op::BLOBHASH, op::BLOBBASEFEE] {
            assert!(opcodes.is_defined(opcode), "{spec} opcode {opcode:#x}");
        }
        // MonadNext follows the published Rust release, not C++ MONAD_NEXT.
        for opcode in [op::DUPN, op::SWAPN, op::EXCHANGE, op::SLOTNUM] {
            assert!(!opcodes.is_defined(opcode), "{spec} opcode {opcode:#x}");
        }
    }
}

#[test]
#[cfg(feature = "serde")]
fn serde_round_trips_hardforks_and_requested_limits() {
    for spec in MonadHardfork::ALL {
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(json, format!("\"{spec}\""));
        assert_eq!(serde_json::from_str::<MonadHardfork>(&json).unwrap(), spec);
    }
    assert!(serde_json::from_str::<MonadHardfork>("\"MonadSeven\"").is_err());

    for limits in [
        MonadLimits::default(),
        MonadLimits {
            tx_gas_limit_cap: Some(u64::MAX),
            max_code_size: Some(usize::MAX),
            max_initcode_size: Some(0),
            memory_limit: Some(128 * 1024 * 1024),
        },
    ] {
        let json = serde_json::to_string(&limits).unwrap();
        assert_eq!(serde_json::from_str::<MonadLimits>(&json).unwrap(), limits);
    }
}
