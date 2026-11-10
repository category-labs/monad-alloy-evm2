//! Reserve-balance input and policy compatibility with monad-revm 0.8.0.

mod common;

use alloy_primitives::{Address, U256};
use common::FORKS;
use evm2::evm::InMemoryDB;
use monad_evm2::{
    MonadBlockEnv, MonadChainContext, MonadEvmBuilder, MonadHardfork, ReserveAccount,
    ReserveBalanceInit, default_max_reserve_balance, reserve_balance::DEFAULT_RESERVE_BALANCE,
};
use monad_revm::{
    MonadChainContext as ReferenceChainContext,
    reserve_balance::tracker::{
        ReserveBalanceInit as ReferenceInit, ReserveBalanceTracker as ReferenceTracker,
    },
};
use revm::state::{Account, AccountInfo};

const SENDER: Address = Address::repeat_byte(0x11);
const OTHER: Address = Address::repeat_byte(0x22);

fn reference(chain: &MonadChainContext) -> ReferenceChainContext {
    ReferenceChainContext {
        parent_senders_and_authorities: chain.parent_senders_and_authorities.clone(),
        grandparent_senders_and_authorities: chain.grandparent_senders_and_authorities.clone(),
        current_block_senders: chain.current_block_senders.clone(),
        current_block_authorities: chain.current_block_authorities.clone(),
        current_tx_index: chain.current_tx_index,
        max_reserve_balance: chain.max_reserve_balance,
    }
}

fn reference_account(original_balance: U256, balance: U256) -> Account {
    let mut account = Account::from(AccountInfo::default().with_balance(original_balance));
    account.info.balance = balance;
    account
}

#[test]
fn reserve_policy_defaults_and_overrides_match_release() {
    let actual = MonadChainContext::default();
    assert_eq!(reference(&actual), ReferenceChainContext::default());
    assert_eq!(DEFAULT_RESERVE_BALANCE, U256::from(10_000_000_000_000_000_000u64));
    assert_eq!(DEFAULT_RESERVE_BALANCE, monad_revm::reserve_balance::abi::DEFAULT_RESERVE_BALANCE);
    assert_eq!(default_max_reserve_balance(), monad_revm::chain::default_max_reserve_balance());

    for cap in [U256::ZERO, U256::ONE, DEFAULT_RESERVE_BALANCE, U256::MAX] {
        let chain = MonadChainContext { max_reserve_balance: cap, ..Default::default() };
        for address in [Address::ZERO, SENDER, OTHER] {
            assert_eq!(chain.max_reserve_balance(address), cap);
            assert_eq!(
                chain.max_reserve_balance(address),
                reference(&chain).max_reserve_balance(address)
            );
        }
    }
}

#[test]
fn sender_and_authority_windows_have_distinct_current_transaction_boundaries() {
    let mut chain = MonadChainContext {
        current_block_senders: vec![OTHER, SENDER, SENDER],
        current_block_authorities: vec![vec![], vec![], vec![SENDER]],
        current_tx_index: 1,
        ..Default::default()
    };
    // Current and future sender entries, and future authorities, do not block this sender.
    assert!(chain.sender_can_dip(SENDER, false));
    // An authorization in the current transaction already counts.
    chain.current_block_authorities[1].push(SENDER);
    assert!(!chain.sender_can_dip(SENDER, false));
    chain.current_block_authorities[1].clear();
    chain.current_block_senders[0] = SENDER;
    assert!(!chain.sender_can_dip(SENDER, false));
    chain.current_block_senders[0] = OTHER;
    chain.current_block_authorities[0].push(SENDER);
    assert!(!chain.sender_can_dip(SENDER, false));
}

#[test]
fn sender_eligibility_matches_release_across_history_and_delegation() {
    for pending in 0..4 {
        for senders in 0..8 {
            for authorities in 0..8 {
                let mut chain = MonadChainContext {
                    current_block_senders: (0..3)
                        .map(|i| if senders & (1 << i) != 0 { SENDER } else { OTHER })
                        .collect(),
                    current_block_authorities: (0..3)
                        .map(|i| {
                            if authorities & (1 << i) != 0 {
                                vec![OTHER, SENDER, SENDER]
                            } else {
                                vec![]
                            }
                        })
                        .collect(),
                    ..Default::default()
                };
                if pending & 1 != 0 {
                    chain.parent_senders_and_authorities.insert(SENDER);
                }
                if pending & 2 != 0 {
                    chain.grandparent_senders_and_authorities.insert(SENDER);
                }
                for index in [0, 1, 2, 3, usize::MAX] {
                    chain.current_tx_index = index;
                    let expected = reference(&chain);
                    for delegated in [false, true] {
                        assert_eq!(
                            chain.sender_can_dip(SENDER, delegated),
                            expected.sender_can_dip(SENDER, delegated),
                            "pending={pending} senders={senders} authorities={authorities} index={index} delegated={delegated}"
                        );
                        if delegated || pending != 0 {
                            assert!(!chain.sender_can_dip(SENDER, delegated));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn partial_history_matches_release_without_indexing_missing_entries() {
    for senders in [vec![], vec![OTHER], vec![SENDER]] {
        for authorities in [vec![], vec![vec![]], vec![vec![SENDER]]] {
            let chain = MonadChainContext {
                current_block_senders: senders.clone(),
                current_block_authorities: authorities,
                current_tx_index: usize::MAX,
                ..Default::default()
            };
            for sender in [Address::ZERO, SENDER, OTHER] {
                assert_eq!(
                    chain.sender_can_dip(sender, false),
                    reference(&chain).sender_can_dip(sender, false)
                );
            }
        }
    }
}

#[test]
fn original_balance_sets_the_reserve_baseline_across_debits_and_credits() {
    let originals = [
        U256::ZERO,
        U256::ONE,
        DEFAULT_RESERVE_BALANCE - U256::ONE,
        DEFAULT_RESERVE_BALANCE,
        DEFAULT_RESERVE_BALANCE + U256::ONE,
        U256::MAX,
    ];
    for cap in [U256::ZERO, U256::ONE, DEFAULT_RESERVE_BALANCE, U256::MAX] {
        let chain = MonadChainContext { max_reserve_balance: cap, ..Default::default() };
        let reference_chain = reference(&chain);
        for original_balance in originals {
            let mut view = ReserveAccount { original_balance, ..Default::default() };
            let reserve = view.pretx_reserve(cap);
            for balance in [
                U256::ZERO,
                original_balance,
                U256::MAX,
                reserve,
                reserve.saturating_sub(U256::ONE),
            ] {
                view.balance = balance;
                assert_eq!(view.pretx_reserve(cap), reserve);
                for (_, spec) in FORKS {
                    let mut tracker = ReferenceTracker::default();
                    tracker.init(ReferenceInit {
                        chain: &reference_chain,
                        spec,
                        sender: SENDER,
                        effective_gas_price: 0,
                        gas_limit: 0,
                        sender_is_delegated: false,
                        sender_account: None,
                    });
                    tracker.on_debit(Some(&reference_account(original_balance, balance)), OTHER);
                    assert_eq!(
                        balance < reserve,
                        tracker.has_violation(),
                        "spec={spec:?} cap={cap} original={original_balance} current={balance}"
                    );
                }
            }
        }
    }
}

#[test]
fn full_limit_fees_are_widened_before_multiplication() {
    let chain = MonadChainContext::default();
    let mut init = ReserveBalanceInit {
        chain: &chain,
        spec: MonadHardfork::MonadEight,
        sender: SENDER,
        effective_gas_price: 0,
        gas_limit: 0,
        sender_is_delegated: false,
        sender_account: None,
    };
    let cases = [
        (0, u64::MAX, U256::ZERO),
        (u128::MAX, 0, U256::ZERO),
        (10, 30_000_000, U256::from(300_000_000)),
        (u128::MAX, 2, U256::from_limbs([u64::MAX - 1, u64::MAX, 1, 0])),
        (u128::MAX, u64::MAX, U256::from_limbs([1, u64::MAX, u64::MAX - 1, 0])),
    ];
    for (price, limit, expected) in cases {
        init.effective_gas_price = price;
        init.gas_limit = limit;
        assert_eq!(init.gas_fees(), expected);
    }
}

#[test]
fn transaction_inputs_match_the_release_sender_fee_allowance() {
    let chain = MonadChainContext {
        parent_senders_and_authorities: [SENDER].into_iter().collect(),
        max_reserve_balance: U256::MAX,
        ..Default::default()
    };
    let reference_chain = reference(&chain);
    for (spec, reference_spec) in FORKS {
        for (price, gas_limit) in [(0, 0), (7, 21_000), (u128::MAX, u64::MAX)] {
            let mut sender = ReserveAccount { original_balance: U256::MAX, ..Default::default() };
            let init = ReserveBalanceInit {
                chain: &chain,
                spec,
                sender: SENDER,
                effective_gas_price: price,
                gas_limit,
                sender_is_delegated: false,
                sender_account: Some(&sender),
            };
            let threshold =
                sender.pretx_reserve(chain.max_reserve_balance(SENDER)) - init.gas_fees();
            for shortfall in [U256::ZERO, U256::ONE] {
                sender.balance = threshold - shortfall;
                let mut tracker = ReferenceTracker::default();
                tracker.init(ReferenceInit {
                    chain: &reference_chain,
                    spec: reference_spec,
                    sender: SENDER,
                    effective_gas_price: price,
                    gas_limit,
                    sender_is_delegated: false,
                    sender_account: Some(&reference_account(
                        sender.original_balance,
                        sender.balance,
                    )),
                });
                assert_eq!(tracker.has_violation(), !shortfall.is_zero());
            }
        }
    }
}

#[test]
fn evm_extension_holds_caller_supplied_block_history() {
    for (spec, _) in FORKS {
        let mut evm = MonadEvmBuilder::new(143).create_evm(
            spec,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        assert_eq!(evm.ext().chain, MonadChainContext::default());
        evm.ext_mut().chain = MonadChainContext {
            current_block_senders: vec![SENDER, OTHER],
            current_block_authorities: vec![vec![], vec![SENDER]],
            current_tx_index: 1,
            ..Default::default()
        };
        assert!(!evm.ext().chain.sender_can_dip(SENDER, false));
        assert!(evm.ext().chain.sender_can_dip(OTHER, false));
        assert_eq!(evm.ext().memory_pool_offset(), 0);
    }
}
