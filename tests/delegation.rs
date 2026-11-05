//! Delegated call contexts, contract creation and inspector observations.

mod common;

use alloy_consensus::{TxEip2930, TxEip7702, TxLegacy, transaction::Recovered};
use alloy_monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork,
    MonadTxEnvelope,
};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use common::FORKS;
use evm2::{
    AccountInfo, Inspector, TxResult,
    bytecode::Bytecode,
    ethereum::LazyTxEip7702,
    evm::InMemoryDB,
    interpreter::{InstrStop, Interpreter, Message, MessageKind, MessageResult, op},
};
use monad_revm::{
    MonadBuilder, MonadCfgEnv, MonadHardfork as ReferenceHardfork, monad_context_with_db,
};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, TxEnv},
    context_interface::{
        result::{ExecutionResult, HaltReason},
        transaction::{AccessListItem, Authorization, RecoveredAuthority, RecoveredAuthorization},
    },
    database::InMemoryDB as ReferenceDB,
    state::{AccountInfo as ReferenceAccount, Bytecode as ReferenceBytecode},
};

const SENDER: Address = Address::repeat_byte(0x11);
const AUTHORITY: Address = Address::repeat_byte(0x22);
const CODE: Address = Address::repeat_byte(0x33);
const PROXY: Address = Address::repeat_byte(0x44);
const CREATOR: Address = Address::repeat_byte(0x55);
const TX_GAS: u64 = 1_000_000;
const CALL_GAS: u64 = 200_000;
const BALANCE: u64 = 10_000_000;
const CALLS: [(u8, MessageKind); 4] = [
    (op::CALL, MessageKind::Call),
    (op::CALLCODE, MessageKind::CallCode),
    (op::DELEGATECALL, MessageKind::DelegateCall),
    (op::STATICCALL, MessageKind::StaticCall),
];

#[derive(Debug, Default)]
struct Trace {
    calls: Vec<Message<MonadEvmTypes>>,
    call_ends: Vec<(Message<MonadEvmTypes>, MessageResult<MonadEvmTypes>)>,
    creates: Vec<Message<MonadEvmTypes>>,
    create_ends: Vec<(Message<MonadEvmTypes>, MessageResult<MonadEvmTypes>)>,
}

impl Inspector<MonadEvmTypes> for Trace {
    fn call(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.calls.push(message.clone());
        None
    }

    fn call_end(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.call_ends.push((message.clone(), result.clone()));
    }

    fn create(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.creates.push(message.clone());
        None
    }

    fn create_end(
        &mut self,
        _interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.create_ends.push((message.clone(), result.clone()));
    }
}

struct Run {
    evm: MonadEvm<'static>,
    result: TxResult<MonadEvmTypes>,
    trace: Trace,
}

fn execute(
    spec: MonadHardfork,
    contracts: &[(Address, Bytecode)],
    tx: MonadTxEnvelope,
    inspect: bool,
) -> Run {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(BALANCE)));
    for (address, code) in contracts {
        db.insert_account_info(
            address,
            AccountInfo::default().with_balance(U256::from(BALANCE)).with_code(code.clone()),
        );
    }
    let mut evm =
        MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID).create_evm(spec, MonadBlockEnv::default(), db);
    let tx = Recovered::new_unchecked(tx, SENDER);
    let mut trace = Trace::default();
    let result = if inspect {
        evm.transact_with_inspector(&tx, &mut trace).unwrap().commit()
    } else {
        evm.transact(&tx).unwrap().commit()
    };
    assert_eq!(result.tx_gas_used(), TX_GAS);
    Run { evm, result, trace }
}

fn legacy(target: Address) -> TxLegacy {
    TxLegacy {
        chain_id: Some(MONAD_MAINNET_CHAIN_ID),
        to: TxKind::Call(target),
        gas_limit: TX_GAS,
        ..Default::default()
    }
}

fn account(evm: &mut MonadEvm<'_>, address: Address) -> AccountInfo {
    evm.state_mut().account_info_untracked(&address).unwrap().unwrap_or_default()
}

fn reference(
    spec: ReferenceHardfork,
    contracts: &[(Address, Bytecode)],
    tx: &TxLegacy,
) -> ExecutionResult {
    let mut db = ReferenceDB::default();
    db.insert_account_info(SENDER, ReferenceAccount::default().with_balance(U256::from(BALANCE)));
    for (address, code) in contracts {
        db.insert_account_info(
            *address,
            ReferenceAccount::default()
                .with_balance(U256::from(BALANCE))
                .with_code(ReferenceBytecode::new_raw(code.original_bytes())),
        );
    }
    let mut evm = monad_context_with_db(db)
        .with_cfg(MonadCfgEnv::new_with_spec(spec).with_chain_id(MONAD_MAINNET_CHAIN_ID))
        .with_block(BlockEnv { basefee: 0, ..Default::default() })
        .build_monad();
    let tx = TxEnv::builder()
        .caller(SENDER)
        .nonce(tx.nonce)
        .kind(tx.to)
        .value(tx.value)
        .data(tx.input.clone())
        .chain_id(tx.chain_id)
        .gas_limit(tx.gas_limit)
        .gas_price(tx.gas_price)
        .build()
        .unwrap();
    evm.transact(tx).unwrap().result
}

fn assert_matches_release(
    forks: (MonadHardfork, ReferenceHardfork),
    contracts: &[(Address, Bytecode)],
    tx: &TxLegacy,
    actual: &TxResult<MonadEvmTypes>,
) {
    let expected = reference(forks.1, contracts, tx);
    assert_eq!(actual.is_success(), expected.is_success(), "{forks:?}: {expected:?}");
    assert_eq!(
        actual.is_revert(),
        matches!(expected, ExecutionResult::Revert { .. }),
        "{forks:?}: {expected:?}"
    );
    assert_eq!(actual.output.as_ref(), expected.output().map_or(&[][..], Bytes::as_ref));
    // Gas parity for Ten+ also depends on MIP-8 storage pricing.
    if forks.0 < MonadHardfork::MonadTen {
        assert_eq!(actual.total_gas_spent, expected.gas().total_gas_spent());
    }
}

fn code(bytes: impl Into<Bytes>) -> Bytecode {
    Bytecode::new_legacy(bytes.into())
}

fn forward(target: Address, opcode: u8, gas: u64, value: u8) -> Bytecode {
    let mut bytes = vec![
        op::CALLDATASIZE,
        op::PUSH0,
        op::PUSH0,
        op::CALLDATACOPY,
        op::PUSH0,
        op::PUSH0,
        op::CALLDATASIZE,
        op::PUSH0,
    ];
    if matches!(opcode, op::CALL | op::CALLCODE) {
        bytes.extend([op::PUSH1, value]);
    }
    bytes.push(op::PUSH20);
    bytes.extend_from_slice(target.as_slice());
    bytes.push(op::PUSH8);
    bytes.extend_from_slice(&gas.to_be_bytes());
    // Return the success word followed by all child returndata.
    bytes.extend([
        opcode,
        op::PUSH0,
        op::MSTORE,
        op::RETURNDATASIZE,
        op::PUSH0,
        op::PUSH1,
        32,
        op::RETURNDATACOPY,
        op::RETURNDATASIZE,
        op::PUSH1,
        32,
        op::ADD,
        op::PUSH0,
        op::RETURN,
    ]);
    code(bytes)
}

fn create_code(create2: bool) -> Bytecode {
    let mut bytes = vec![op::PUSH0; if create2 { 4 } else { 3 }];
    bytes.extend([
        if create2 { op::CREATE2 } else { op::CREATE },
        op::PUSH0,
        op::MSTORE,
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ]);
    code(bytes)
}

fn context_code() -> Bytecode {
    code(vec![
        op::ADDRESS,
        op::PUSH0,
        op::MSTORE,
        op::CALLER,
        op::PUSH1,
        32,
        op::MSTORE,
        op::CALLVALUE,
        op::PUSH1,
        64,
        op::MSTORE,
        op::ORIGIN,
        op::PUSH1,
        96,
        op::MSTORE,
        op::PUSH1,
        128,
        op::PUSH0,
        op::RETURN,
    ])
}

fn context_output(destination: Address, caller: Address, value: u64) -> Vec<u8> {
    [destination.into_word(), caller.into_word(), U256::from(value).into(), SENDER.into_word()]
        .into_iter()
        .flat_map(|word| word.0)
        .collect()
}

#[test]
fn delegated_root_context_and_inspector_inputs_match_execution() {
    let contracts = [(AUTHORITY, Bytecode::new_eip7702(CODE)), (CODE, context_code())];
    for forks in FORKS {
        let tx = TxLegacy { value: U256::from(7), ..legacy(AUTHORITY) };
        let actual = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
        assert_matches_release(forks, &contracts, &tx, &actual.result);
        assert_eq!(actual.result.output, context_output(AUTHORITY, SENDER, 7));
        let message = &actual.trace.calls[0];
        assert_eq!(
            (message.destination, message.call_target, message.code_address),
            (AUTHORITY, AUTHORITY, CODE)
        );
        assert_eq!(message.caller, SENDER);
        assert_eq!(message.value, U256::from(7));
        assert_eq!(message.code, contracts[1].1);
        assert_eq!(actual.trace.calls.len(), 1);
        assert_eq!(actual.trace.call_ends[0].0, *message);
        let plain = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx), false);
        assert_eq!(plain.result, actual.result);
    }
}

#[test]
fn nested_calls_keep_callee_context_and_code_addresses_distinct() {
    for forks in FORKS {
        for (opcode, kind) in CALLS {
            let contracts = [
                (AUTHORITY, Bytecode::new_eip7702(CODE)),
                (CODE, context_code()),
                (PROXY, forward(AUTHORITY, opcode, CALL_GAS, 13)),
            ];
            let tx = TxLegacy { value: U256::from(7), ..legacy(PROXY) };
            let mut actual =
                execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            assert_matches_release(forks, &contracts, &tx, &actual.result);
            let destination =
                if matches!(opcode, op::CALLCODE | op::DELEGATECALL) { PROXY } else { AUTHORITY };
            let caller = if opcode == op::DELEGATECALL { SENDER } else { PROXY };
            let value = match opcode {
                op::STATICCALL => 0,
                op::DELEGATECALL => 7,
                _ => 13,
            };
            let mut expected = U256::ONE.to_be_bytes_vec();
            expected.extend(context_output(destination, caller, value));
            assert_eq!(actual.result.output, expected, "{forks:?}/{kind:?}");
            let message = &actual.trace.calls[1];
            assert_eq!(message.kind, kind);
            assert_eq!(
                (message.destination, message.call_target, message.code_address),
                (destination, AUTHORITY, CODE)
            );
            assert_eq!(message.caller, caller);
            assert_eq!(message.value, U256::from(value));
            assert_eq!(actual.trace.call_ends[0].0, *message);
            assert_eq!(actual.trace.calls.len(), 2);
            assert_eq!(
                account(&mut actual.evm, AUTHORITY).balance,
                U256::from(BALANCE + if opcode == op::CALL { 13 } else { 0 })
            );
            let plain = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx), false);
            assert_eq!(plain.result, actual.result);
        }
    }
}

#[test]
fn delegated_storage_writes_use_the_execution_context() {
    for forks in FORKS {
        for (opcode, kind) in CALLS {
            let contracts = [
                (AUTHORITY, Bytecode::new_eip7702(CODE)),
                (CODE, code(vec![op::PUSH1, 42, op::PUSH0, op::SSTORE, op::STOP])),
                (PROXY, forward(AUTHORITY, opcode, CALL_GAS, 0)),
            ];
            let tx = legacy(PROXY);
            let mut actual =
                execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            assert_matches_release(forks, &contracts, &tx, &actual.result);
            let destination = if opcode == op::CALL { AUTHORITY } else { PROXY };
            for address in [AUTHORITY, CODE, PROXY] {
                let expected =
                    if opcode != op::STATICCALL && address == destination { 42 } else { 0 };
                assert_eq!(
                    actual.evm.state_mut().storage_slot_untracked(&address, &U256::ZERO).unwrap(),
                    U256::from(expected),
                    "{forks:?}/{kind:?}/{address}"
                );
            }
            assert_eq!(
                actual.result.output.as_ref(),
                U256::from(u8::from(opcode != op::STATICCALL)).to_be_bytes::<32>()
            );
        }
    }
}

#[test]
fn create_and_create2_halt_in_a_delegated_context_without_changing_nonce() {
    for forks in FORKS {
        for create2 in [false, true] {
            let contracts =
                [(AUTHORITY, Bytecode::new_eip7702(CODE)), (CODE, create_code(create2))];
            let tx = legacy(AUTHORITY);
            let mut actual =
                execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            assert_eq!(actual.result.stop, InstrStop::NotActivated);
            assert_eq!(actual.result.total_gas_spent, TX_GAS);
            assert!(matches!(
                reference(forks.1, &contracts, &tx),
                ExecutionResult::Halt { reason: HaltReason::NotActivated, .. }
            ));
            assert_eq!(account(&mut actual.evm, AUTHORITY).nonce, 0);
            assert_eq!(account(&mut actual.evm, CODE).nonce, 0);
            assert!(actual.trace.creates.is_empty());
            assert!(actual.trace.create_ends.is_empty());
            let plain = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx), false);
            assert_eq!(plain.result, actual.result);
        }
    }
}

#[test]
fn creation_checks_the_storage_context_across_all_call_kinds() {
    for forks in FORKS {
        for create2 in [false, true] {
            for (opcode, kind) in CALLS {
                for delegated in [false, true] {
                    let forwarding = forward(CREATOR, opcode, CALL_GAS, 0);
                    let contracts = [
                        (
                            AUTHORITY,
                            if delegated {
                                Bytecode::new_eip7702(CODE)
                            } else {
                                forwarding.clone()
                            },
                        ),
                        (CODE, forwarding),
                        (CREATOR, create_code(create2)),
                    ];
                    let tx = legacy(AUTHORITY);
                    let mut actual =
                        execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
                    assert_matches_release(forks, &contracts, &tx, &actual.result);
                    let allowed = opcode == op::CALL || (!delegated && opcode != op::STATICCALL);
                    let label =
                        format!("{forks:?}/create2={create2}/{kind:?}/delegated={delegated}");
                    assert!(actual.result.is_success(), "{label}");
                    assert_eq!(
                        &actual.result.output[..32],
                        U256::from(u8::from(allowed)).to_be_bytes::<32>(),
                        "{label}"
                    );
                    assert_eq!(actual.trace.creates.len(), usize::from(allowed), "{label}");
                    assert_eq!(actual.trace.create_ends.len(), usize::from(allowed), "{label}");
                    let creator = if opcode == op::CALL { CREATOR } else { AUTHORITY };
                    assert_eq!(
                        account(&mut actual.evm, creator).nonce,
                        u64::from(allowed),
                        "{label}"
                    );
                    if allowed {
                        let creation = &actual.trace.creates[0];
                        assert_eq!(creation.caller, creator);
                        assert_eq!(
                            creation.kind,
                            if create2 { MessageKind::Create2 } else { MessageKind::Create }
                        );
                        let (finished, result) = &actual.trace.create_ends[0];
                        let created = result.created_address.unwrap();
                        // The entry hook sees the creator; frame setup selects the new account.
                        assert_eq!(creation.code_address, creator);
                        assert_eq!(creation.destination, created);
                        assert_eq!(finished.caller, creator);
                        assert_eq!(
                            (finished.destination, finished.call_target, finished.code_address),
                            (created, created, created)
                        );
                        assert_ne!(U256::from_be_slice(&actual.result.output[32..]), U256::ZERO);
                    }
                    let plain = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx), false);
                    assert_eq!(plain.result, actual.result, "{label}");
                }
            }
        }
    }
}

#[test]
fn ordinary_context_can_create_using_code_from_a_delegated_callee() {
    for forks in FORKS {
        for create2 in [false, true] {
            for opcode in [op::CALLCODE, op::DELEGATECALL] {
                let contracts = [
                    (AUTHORITY, Bytecode::new_eip7702(CODE)),
                    (CODE, create_code(create2)),
                    (PROXY, forward(AUTHORITY, opcode, CALL_GAS, 0)),
                ];
                let tx = legacy(PROXY);
                let mut actual =
                    execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
                assert_matches_release(forks, &contracts, &tx, &actual.result);
                assert!(actual.result.is_success());
                assert_eq!(&actual.result.output[..32], U256::ONE.to_be_bytes::<32>());
                assert_eq!(account(&mut actual.evm, PROXY).nonce, 1);
                assert_eq!(account(&mut actual.evm, AUTHORITY).nonce, 0);
                assert_eq!(actual.trace.creates.len(), 1);
                assert_eq!(actual.trace.creates[0].caller, PROXY);
            }
        }
    }
}

#[test]
fn creation_guard_preserves_static_error_precedence_and_rolls_back_child_state() {
    for forks in FORKS {
        for create2 in [false, true] {
            let opcode = if create2 { op::CREATE2 } else { op::CREATE };
            let mut contracts = vec![
                (AUTHORITY, Bytecode::new_eip7702(CODE)),
                (CODE, code(vec![opcode])),
                (PROXY, forward(AUTHORITY, op::STATICCALL, CALL_GAS, 0)),
            ];
            let tx = legacy(PROXY);
            let actual = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            assert_matches_release(forks, &contracts, &tx, &actual.result);
            assert_eq!(actual.trace.call_ends[0].1.stop, InstrStop::StateChangeDuringStaticCall);

            // A delegated context is rejected before attempting to pop CREATE operands.
            let actual =
                execute(forks.0, &contracts, MonadTxEnvelope::Legacy(legacy(AUTHORITY)), true);
            assert_eq!(actual.result.stop, InstrStop::NotActivated);

            contracts[1].1 = code(vec![op::PUSH1, 42, op::PUSH0, op::SSTORE, opcode]);
            contracts[2].1 = forward(AUTHORITY, op::CALL, CALL_GAS, 13);
            let mut actual =
                execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            assert_matches_release(forks, &contracts, &tx, &actual.result);
            assert_eq!(actual.trace.call_ends[0].1.stop, InstrStop::NotActivated);
            assert_eq!(account(&mut actual.evm, AUTHORITY).balance, U256::from(BALANCE));
            assert_eq!(
                actual.evm.state_mut().storage_slot_untracked(&AUTHORITY, &U256::ZERO).unwrap(),
                U256::ZERO
            );
            assert_eq!(account(&mut actual.evm, AUTHORITY).nonce, 0);
        }
    }
}

#[test]
fn delegation_is_resolved_once_and_empty_targets_succeed() {
    for forks in FORKS {
        for target in [AUTHORITY, CODE, Address::ZERO] {
            let contracts = [
                (AUTHORITY, Bytecode::new_eip7702(target)),
                (CODE, Bytecode::new_eip7702(CREATOR)),
                (CREATOR, code(vec![op::STOP])),
            ];
            let tx = legacy(AUTHORITY);
            let actual = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            assert_matches_release(forks, &contracts, &tx, &actual.result);
            assert_eq!(actual.result.is_success(), target == Address::ZERO);
            assert_eq!(actual.trace.calls.len(), 1);
            assert_eq!(actual.trace.calls[0].code_address, target);
        }
    }
}

#[test]
fn delegating_to_protocol_precompiles_executes_empty_code() {
    for (spec, _) in FORKS {
        for id in (1..=0x11u16).chain([0x100]) {
            let precompile = Address::from_word(U256::from(id).into());
            let mut contracts = vec![(AUTHORITY, Bytecode::new_eip7702(precompile))];
            let tx = TxLegacy {
                input: Bytes::from_static(b"hello"),
                value: U256::from(7),
                ..legacy(AUTHORITY)
            };
            let mut actual = execute(spec, &contracts, MonadTxEnvelope::Legacy(tx), true);
            // C++ and EIP-7702 suppress delegated Ethereum precompiles, including P-256.
            assert!(actual.result.is_success(), "{spec}/{id:#x}");
            assert!(actual.result.output.is_empty());
            assert_eq!(actual.result.total_gas_spent, 21_080);
            assert_eq!(account(&mut actual.evm, AUTHORITY).balance, U256::from(BALANCE + 7));
            assert_eq!(account(&mut actual.evm, precompile).balance, U256::ZERO);

            for (opcode, kind) in CALLS {
                contracts.truncate(1);
                contracts.push((PROXY, forward(AUTHORITY, opcode, 100, 0)));
                let tx = TxLegacy { input: Bytes::from_static(b"hello"), ..legacy(PROXY) };
                let actual = execute(spec, &contracts, MonadTxEnvelope::Legacy(tx), true);
                assert!(actual.result.is_success(), "{spec}/{id:#x}/{kind:?}");
                assert_eq!(actual.result.output.as_ref(), U256::ONE.to_be_bytes::<32>());
                let (message, result) = &actual.trace.call_ends[0];
                assert_eq!(message.call_target, AUTHORITY);
                assert_eq!(message.code_address, precompile);
                assert!(result.stop.is_success());
                assert_eq!(result.gas.remaining(), 100);
            }
        }
    }
}

#[test]
fn direct_precompile_dispatch_is_distinct_from_delegation() {
    let identity = Address::with_last_byte(4);
    let ecrecover = Address::with_last_byte(1);
    for forks in FORKS {
        for target in [identity, ecrecover] {
            let contracts = [(AUTHORITY, Bytecode::new_eip7702(target))];
            let tx = TxLegacy { input: Bytes::from_static(b"hello"), ..legacy(AUTHORITY) };
            let actual = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
            let buggy = reference(forks.1, &contracts, &tx);
            // The release incorrectly invokes the delegated precompile. Corrected by
            // monad-revm commit 42d2cc57a95ed9e060123ba1bb4e7b4d827b5eb0.
            assert_eq!(actual.result.total_gas_spent, 21_080);
            assert_eq!(
                buggy.gas().total_gas_spent(),
                21_080 + if target == identity { 18 } else { 6_000 }
            );
            let direct = execute(
                forks.0,
                &[],
                MonadTxEnvelope::Legacy(TxLegacy { to: TxKind::Call(target), ..tx }),
                true,
            );
            assert_eq!(direct.result.total_gas_spent, buggy.gas().total_gas_spent());
            assert_eq!(direct.result.output.as_ref(), buggy.output().unwrap().as_ref());
        }
    }
}

fn set_code(authority: Address, delegate: Address, authority_nonce: u64) -> MonadTxEnvelope {
    let tx = TxEip7702 {
        chain_id: MONAD_MAINNET_CHAIN_ID,
        gas_limit: TX_GAS,
        to: authority,
        ..Default::default()
    };
    let authorization = RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::ZERO, address: delegate, nonce: authority_nonce },
        RecoveredAuthority::Valid(authority),
    );
    LazyTxEip7702::from_cached_recovered_authorizations(tx, vec![authorization]).into()
}

#[test]
fn same_transaction_authorization_is_visible_to_creation_and_precompile_dispatch() {
    for (spec, _) in FORKS {
        for create2 in [false, true] {
            let contracts = [(CODE, create_code(create2))];
            let mut actual = execute(spec, &contracts, set_code(AUTHORITY, CODE, 0), true);
            assert_eq!(actual.result.stop, InstrStop::NotActivated);
            assert_eq!(account(&mut actual.evm, AUTHORITY).nonce, 1);
            assert_eq!(
                actual
                    .evm
                    .state_mut()
                    .account(&AUTHORITY)
                    .unwrap()
                    .load_code()
                    .unwrap()
                    .eip7702_address(),
                Some(CODE)
            );
            assert!(actual.trace.creates.is_empty());
        }
        let identity = Address::with_last_byte(4);
        let actual = execute(spec, &[], set_code(AUTHORITY, identity, 0), true);
        assert!(actual.result.is_success());
        assert_eq!(actual.result.total_gas_spent, 46_000);
        assert_eq!(actual.trace.calls[0].code_address, identity);
    }
}

#[test]
fn delegated_sender_can_still_originate_a_contract_creation_transaction() {
    for forks in FORKS {
        let contracts = [(SENDER, Bytecode::new_eip7702(CODE)), (CODE, create_code(false))];
        let tx =
            TxLegacy { to: TxKind::Create, input: Bytes::from_static(&[op::STOP]), ..legacy(CODE) };
        let actual = execute(forks.0, &contracts, MonadTxEnvelope::Legacy(tx.clone()), true);
        assert_matches_release(forks, &contracts, &tx, &actual.result);
        assert!(actual.result.is_success());
        assert_eq!(actual.trace.creates.len(), 1);
        assert_eq!(actual.trace.creates[0].caller, SENDER);
    }
}

#[test]
fn access_lists_prewarm_the_delegation_target() {
    let contracts = [
        (AUTHORITY, Bytecode::new_eip7702(CODE)),
        (CODE, code(vec![op::STOP])),
        (PROXY, forward(AUTHORITY, op::CALL, CALL_GAS, 0)),
    ];
    for (spec, _) in FORKS {
        let cold = execute(spec, &contracts, MonadTxEnvelope::Legacy(legacy(PROXY)), false);
        let tx = TxEip2930 {
            chain_id: MONAD_MAINNET_CHAIN_ID,
            gas_limit: TX_GAS,
            to: TxKind::Call(PROXY),
            access_list: vec![AccessListItem { address: CODE, storage_keys: vec![] }].into(),
            ..Default::default()
        };
        let warm = execute(spec, &contracts, MonadTxEnvelope::Eip2930(tx), false);
        assert!(cold.result.is_success());
        assert_eq!(warm.result.output, cold.result.output);
        assert_eq!(cold.result.total_gas_spent - warm.result.total_gas_spent, 10_000 - 2_400);
    }
}
