//! Configuration compatibility with the published monad-revm 0.8.0 release.

use alloy_monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MONAD_MAX_CODE_SIZE, MONAD_MAX_INITCODE_SIZE, MONAD_MEMORY_LIMIT,
    MONAD_TESTNET_CHAIN_ID, MONAD_TX_GAS_LIMIT_CAP, MonadHardfork, MonadLimits,
    UnknownMonadHardfork, monad_gas_params, monad_memory_limit, monad_version,
};
use evm2::{
    BaseEvmTypes, EvmFeatures, OpcodeConfig, SpecId, Version, interpreter::op, version::GasId,
};
use monad_revm::{MonadCfgEnv, MonadHardfork as ReferenceHardfork};
use revm::{
    context::Cfg, context_interface::cfg::GasId as ReferenceGasId,
    primitives::hardfork::SpecId as ReferenceSpecId,
};

const FORKS: [(MonadHardfork, ReferenceHardfork); 4] = [
    (MonadHardfork::MonadEight, ReferenceHardfork::MonadEight),
    (MonadHardfork::MonadNine, ReferenceHardfork::MonadNine),
    (MonadHardfork::MonadTen, ReferenceHardfork::MonadTen),
    (MonadHardfork::MonadNext, ReferenceHardfork::MonadNext),
];

#[test]
fn identifiers_names_and_ordering_match_release() {
    assert_eq!(MonadHardfork::default() as u8, ReferenceHardfork::default() as u8);
    assert_eq!(MonadHardfork::ALL, FORKS.map(|(ours, _)| ours));
    assert_eq!(core::mem::size_of::<MonadHardfork>(), 1);

    for (ours, reference) in FORKS {
        assert_eq!(ours as u8, reference as u8);
        let name: &str = ours.into();
        assert_eq!(name, reference.to_string());
        assert_eq!(ours.to_string(), reference.to_string());
        for spelling in [name.to_owned(), name.to_ascii_lowercase(), name.to_ascii_uppercase()] {
            assert_eq!(spelling.parse::<MonadHardfork>().unwrap(), ours);
            assert_eq!(spelling.parse::<ReferenceHardfork>().unwrap(), reference);
        }

        let expected_base = match reference.into_eth_spec() {
            ReferenceSpecId::PRAGUE => SpecId::PRAGUE,
            ReferenceSpecId::OSAKA => SpecId::OSAKA,
            unexpected => panic!("unexpected Ethereum foundation: {unexpected:?}"),
        };
        assert_eq!(ours.into_eth_spec(), expected_base);
        assert_eq!(SpecId::from(ours), expected_base);

        for (active, reference_active) in FORKS {
            assert_eq!(ours.is_enabled_in(active), reference.is_enabled_in(reference_active));
            assert_eq!(ours.cmp(&active), reference.cmp(&reference_active));
        }
    }
}

#[test]
fn unknown_identifiers_are_rejected() {
    for raw in u8::MIN..=u8::MAX {
        let expected = FORKS.iter().find(|(_, reference)| *reference as u8 == raw);
        assert_eq!(MonadHardfork::try_from_u8(raw), expected.map(|(ours, _)| *ours));
    }
    for name in ["", "MonadSeven", "Prague", "100", " MonadNine", "MonadTen ", "monad-nine"] {
        assert!(name.parse::<ReferenceHardfork>().is_err());
        assert_eq!(name.parse::<MonadHardfork>(), Err(UnknownMonadHardfork));
    }
}

#[test]
fn schedules_match_release_at_every_boundary() {
    assert_eq!(MONAD_MAINNET_CHAIN_ID, monad_revm::MONAD_MAINNET_CHAIN_ID);
    assert_eq!(MONAD_TESTNET_CHAIN_ID, monad_revm::MONAD_TESTNET_CHAIN_ID);

    for chain_id in [MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID] {
        let mut timestamps = vec![0, u64::MAX];
        for (ours, reference) in FORKS {
            let (actual, expected) = if chain_id == MONAD_MAINNET_CHAIN_ID {
                (ours.mainnet_activation_timestamp(), reference.mainnet_activation_timestamp())
            } else {
                (ours.testnet_activation_timestamp(), reference.testnet_activation_timestamp())
            };
            assert_eq!(actual, expected, "{ours} chain {chain_id}");
            if let Some(activation) = expected {
                timestamps.extend([activation - 1, activation, activation + 1]);
            }
        }
        for timestamp in timestamps {
            assert_eq!(
                MonadHardfork::from_chain_and_timestamp(chain_id, timestamp).map(|spec| spec as u8),
                ReferenceHardfork::from_chain_and_timestamp(chain_id, timestamp)
                    .map(|spec| spec as u8),
                "chain {chain_id} timestamp {timestamp}"
            );
        }
    }

    for chain_id in [0, 1, 10, u64::MAX] {
        for timestamp in [0, 1_788_359_400, u64::MAX] {
            assert_eq!(MonadHardfork::from_chain_and_timestamp(chain_id, timestamp), None);
        }
    }
}

/// Semantic gas identifiers, independent of either engine's internal table indices.
const GAS_IDS: &[(GasId, ReferenceGasId)] = &[
    (GasId::ExpByte, ReferenceGasId::exp_byte_gas()),
    (GasId::ExtcodecopyPerWord, ReferenceGasId::extcodecopy_per_word()),
    (GasId::CopyPerWord, ReferenceGasId::copy_per_word()),
    (GasId::Logdata, ReferenceGasId::logdata()),
    (GasId::Logtopic, ReferenceGasId::logtopic()),
    (GasId::McopyPerWord, ReferenceGasId::mcopy_per_word()),
    (GasId::Keccak256PerWord, ReferenceGasId::keccak256_per_word()),
    (GasId::MemoryLinearCost, ReferenceGasId::memory_linear_cost()),
    (GasId::MemoryQuadraticReduction, ReferenceGasId::memory_quadratic_reduction()),
    (GasId::InitcodePerWord, ReferenceGasId::initcode_per_word()),
    (GasId::Create, ReferenceGasId::create()),
    (GasId::CallStipendReduction, ReferenceGasId::call_stipend_reduction()),
    (GasId::TransferValueCost, ReferenceGasId::transfer_value_cost()),
    (GasId::ColdAccountAdditionalCost, ReferenceGasId::cold_account_additional_cost()),
    (GasId::NewAccountCost, ReferenceGasId::new_account_cost()),
    (GasId::WarmStorageReadCost, ReferenceGasId::warm_storage_read_cost()),
    (GasId::SstoreStatic, ReferenceGasId::sstore_static()),
    (GasId::SstoreSetWithoutLoadCost, ReferenceGasId::sstore_set_without_load_cost()),
    (GasId::SstoreResetWithoutColdLoadCost, ReferenceGasId::sstore_reset_without_cold_load_cost()),
    (GasId::SstoreClearingSlotRefund, ReferenceGasId::sstore_clearing_slot_refund()),
    (GasId::SelfdestructRefund, ReferenceGasId::selfdestruct_refund()),
    (GasId::CallStipend, ReferenceGasId::call_stipend()),
    (GasId::MaxRefundQuotient, ReferenceGasId::max_refund_quotient()),
    (GasId::ColdStorageAdditionalCost, ReferenceGasId::cold_storage_additional_cost()),
    (GasId::ColdStorageCost, ReferenceGasId::cold_storage_cost()),
    (GasId::NewAccountCostForSelfdestruct, ReferenceGasId::new_account_cost_for_selfdestruct()),
    (GasId::CodeDepositCost, ReferenceGasId::code_deposit_cost()),
    (GasId::TxEip7702PerEmptyAccountCost, ReferenceGasId::tx_eip7702_regular_gas()),
    (GasId::TxTokenNonZeroByteMultiplier, ReferenceGasId::tx_token_non_zero_byte_multiplier()),
    (GasId::TxTokenCost, ReferenceGasId::tx_token_cost()),
    (GasId::TxFloorCostPerToken, ReferenceGasId::tx_floor_cost_per_token()),
    (GasId::TxFloorCostBase, ReferenceGasId::tx_floor_cost_base_gas()),
    (GasId::TxFloorZeroByteMultiplier, ReferenceGasId::tx_floor_token_zero_byte_multiplier()),
    (GasId::TxAccessListAddressCost, ReferenceGasId::tx_access_list_address_cost()),
    (GasId::TxAccessListStorageKeyCost, ReferenceGasId::tx_access_list_storage_key_cost()),
    (
        GasId::TxAccessListFloorByteMultiplier,
        ReferenceGasId::tx_access_list_floor_byte_multiplier(),
    ),
    (GasId::TxBaseStipend, ReferenceGasId::tx_base_stipend()),
    (GasId::TxCreateCost, ReferenceGasId::tx_create_cost()),
    (GasId::TxInitcodeCost, ReferenceGasId::tx_initcode_cost()),
    (GasId::SstoreSetRefund, ReferenceGasId::sstore_set_refund()),
    (GasId::SstoreResetRefund, ReferenceGasId::sstore_reset_refund()),
    (GasId::TxEip7702AuthRefund, ReferenceGasId::tx_eip7702_regular_refund()),
    (GasId::SstoreSetState, ReferenceGasId::sstore_set_state_gas()),
    (GasId::NewAccountState, ReferenceGasId::new_account_state_gas()),
    (GasId::CodeDepositState, ReferenceGasId::code_deposit_state_gas()),
    (GasId::CreateState, ReferenceGasId::create_state_gas()),
    (GasId::TxEip7702PerAuthState, ReferenceGasId::tx_eip7702_state_gas_bytecode()),
    (GasId::TxValueCost, ReferenceGasId::tx_account_write_cost()),
    (GasId::TxCreateAccessCost, ReferenceGasId::tx_create_access_cost()),
];

#[test]
fn dynamic_gas_params_match_release() {
    assert_eq!(GAS_IDS.len(), GasId::Custom0.as_usize());
    for (index, (id, _)) in GAS_IDS.iter().enumerate() {
        assert_eq!(id.as_usize(), index, "missing or repeated gas parameter");
    }
    for (ours, reference) in FORKS {
        let params = monad_gas_params(ours);
        let expected = monad_revm::instructions::monad_gas_params(reference);
        for &(id, reference_id) in GAS_IDS {
            assert_eq!(params.get(id), expected.get(reference_id), "{ours} {id:?}");
        }
        assert_eq!(params.selfdestruct_cold_cost(), expected.selfdestruct_cold_cost(), "{ours}");
    }
}

#[test]
fn default_limits_match_release() {
    assert_eq!(MONAD_TX_GAS_LIMIT_CAP, monad_revm::cfg::MONAD_TX_GAS_LIMIT_CAP);
    assert_eq!(MONAD_MAX_CODE_SIZE, monad_revm::cfg::MONAD_MAX_CODE_SIZE);
    assert_eq!(MONAD_MAX_INITCODE_SIZE, monad_revm::cfg::MONAD_MAX_INITCODE_SIZE);
    assert_eq!(MONAD_MEMORY_LIMIT, monad_revm::cfg::MONAD_MEMORY_LIMIT);

    for (spec, reference) in FORKS {
        for chain_id in [MONAD_MAINNET_CHAIN_ID, MONAD_TESTNET_CHAIN_ID, 0, u64::MAX] {
            let version = monad_version(spec, chain_id);
            let cfg = MonadCfgEnv::new_with_spec(reference).with_chain_id(chain_id);
            assert_eq!(version.chain_id, cfg.chain_id());
            assert_eq!(version.tx_gas_limit_cap, cfg.tx_gas_limit_cap(), "{spec}");
            assert_eq!(version.max_code_size, cfg.max_code_size(), "{spec}");
            assert_eq!(version.max_initcode_size, cfg.max_initcode_size(), "{spec}");
            assert_eq!(version.memory_limit, cfg.memory_limit(), "{spec}");
            assert_eq!(version.gas_params, monad_gas_params(spec));
            assert_eq!(version.tx_gas_caps(), (cfg.tx_gas_limit_cap(), u64::MAX));

            let base = Version::base(spec.into_eth_spec());
            assert_eq!(version.features, base.features);
            assert_eq!(version.max_blobs_per_tx, base.max_blobs_per_tx);
            assert_eq!(version.initial_call_depth, base.initial_call_depth);
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
fn explicit_limits_match_release_including_zero_and_overflow() {
    for (spec, reference) in FORKS {
        for gas_cap in [None, Some(0), Some(12_345_678), Some(u64::MAX)] {
            for code_size in [None, Some(0), Some(100_000), Some(usize::MAX)] {
                for initcode_size in [None, Some(0), Some(300_000), Some(usize::MAX)] {
                    let limits = MonadLimits {
                        tx_gas_limit_cap: gas_cap,
                        max_code_size: code_size,
                        max_initcode_size: initcode_size,
                        ..Default::default()
                    };
                    let version = limits.version(spec, MONAD_MAINNET_CHAIN_ID);
                    let mut cfg = MonadCfgEnv::new_with_spec(reference);
                    cfg.0.tx_gas_limit_cap = gas_cap;
                    cfg.0.limit_contract_code_size = code_size;
                    cfg.0.limit_contract_initcode_size = initcode_size;
                    assert_eq!(
                        version.tx_gas_limit_cap,
                        cfg.tx_gas_limit_cap(),
                        "{spec} {limits:?}"
                    );
                    assert_eq!(version.max_code_size, cfg.max_code_size(), "{spec} {limits:?}");
                    assert_eq!(
                        version.max_initcode_size,
                        cfg.max_initcode_size(),
                        "{spec} {limits:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn memory_limits_survive_hardfork_changes() {
    for configured in
        [0, 16_000, MONAD_MEMORY_LIMIT, MONAD_MEMORY_LIMIT + 1, 128 * 1024 * 1024, u64::MAX]
    {
        let limits = MonadLimits { memory_limit: Some(configured), ..Default::default() };
        let mut cfg = MonadCfgEnv::new();
        #[cfg(feature = "memory_limit")]
        {
            cfg.0.memory_limit = configured;
        }
        for (spec, reference) in FORKS.into_iter().chain(FORKS.into_iter().rev()) {
            cfg.0.spec = reference;
            let version = limits.version(spec, MONAD_MAINNET_CHAIN_ID);
            assert_eq!(version.memory_limit, cfg.memory_limit(), "{spec} configured {configured}");
            assert_eq!(monad_memory_limit(spec, configured), cfg.memory_limit());
        }
        assert_eq!(limits.memory_limit, Some(configured));
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
