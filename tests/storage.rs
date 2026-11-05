//! MIP-8 pricing, rollback and transaction isolation against monad-revm 0.8.0.

mod common;

use alloy_consensus::{TxEip2930, TxLegacy, transaction::Recovered};
use alloy_monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork,
    MonadTxEnvelope,
    page::{
        BASE_COST, PAGE_SHIFT, PAGE_WRITE_COST, STATE_GROWTH_COST, StoragePageKey, WORDS_PER_PAGE,
        page_index, page_offset,
    },
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use common::FORKS;
use evm2::{
    AccountInfo, ExecutionError, Inspector, TxResult,
    bytecode::Bytecode,
    evm::{Database, Db, InMemoryDB, SystemTx},
    interpreter::{InstrStop, Interpreter, Message, MessageResult, op},
};
use monad_revm::{
    MonadBuilder, MonadCfgEnv, MonadHardfork as ReferenceHardfork, monad_context_with_db,
};
use revm::{
    ExecuteEvm,
    context::{BlockEnv, TxEnv},
    context_interface::{
        result::{ExecutionResult, HaltReason},
        transaction::AccessListItem,
    },
    database::InMemoryDB as ReferenceDB,
    state::{AccountInfo as ReferenceAccount, Bytecode as ReferenceBytecode},
};
use std::io;

const SENDER: Address = Address::repeat_byte(0x11);
const ROOT: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);
const GRANDCHILD: Address = Address::repeat_byte(0x44);
const GAS: u64 = 5_000_000;

#[derive(Debug, Default)]
struct Trace {
    opcodes: Vec<u8>,
    accesses: Vec<(usize, Address, u8, u64)>,
    children: Vec<InstrStop>,
}

impl Inspector<MonadEvmTypes> for Trace {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let depth = usize::from(interp.message().depth);
        self.opcodes.resize(self.opcodes.len().max(depth + 1), op::STOP);
        self.opcodes[depth] = interp.opcode();
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let depth = usize::from(interp.message().depth);
        let opcode = self.opcodes[depth];
        if matches!(opcode, op::SLOAD | op::SSTORE) {
            self.accesses.push((depth, interp.message().destination, opcode, interp.step_cost()));
        }
    }

    fn call_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        if message.depth != 0 {
            self.children.push(result.stop);
        }
    }

    fn create_end(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.children.push(result.stop);
    }
}

impl Trace {
    fn costs(&self) -> Vec<u64> {
        self.accesses.iter().map(|access| access.3).collect()
    }
}

struct Run {
    evm: MonadEvm<'static>,
    result: TxResult<MonadEvmTypes>,
    trace: Trace,
}

struct Fixture {
    accounts: Vec<(Address, Bytecode)>,
    storage: Vec<(Address, U256, U256)>,
    access_list: Option<Vec<AccessListItem>>,
    gas: u64,
}

impl Fixture {
    fn new(code: Vec<u8>) -> Self {
        Self {
            accounts: vec![(ROOT, Bytecode::new_raw(code.into()))],
            storage: vec![],
            access_list: None,
            gas: GAS,
        }
    }

    fn contract(&mut self, address: Address, code: Vec<u8>) {
        self.accounts.push((address, Bytecode::new_raw(code.into())));
    }

    fn warm(&mut self, address: Address, slots: &[u64]) {
        self.access_list.get_or_insert_default().push(AccessListItem {
            address,
            storage_keys: slots.iter().map(|slot| U256::from(*slot).into()).collect(),
        });
    }

    fn evm(&self, spec: MonadHardfork) -> MonadEvm<'static> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(GAS)));
        for (address, code) in &self.accounts {
            db.insert_account_info(address, AccountInfo::default().with_code(code.clone()));
        }
        for (address, slot, value) in &self.storage {
            db.insert_account_storage(address, slot, value);
        }
        MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID).create_evm(spec, MonadBlockEnv::default(), db)
    }

    fn tx(&self, nonce: u64) -> Recovered<MonadTxEnvelope> {
        let tx = if let Some(list) = &self.access_list {
            MonadTxEnvelope::Eip2930(TxEip2930 {
                chain_id: MONAD_MAINNET_CHAIN_ID,
                nonce,
                to: ROOT.into(),
                gas_limit: self.gas,
                access_list: list.clone().into(),
                ..Default::default()
            })
        } else {
            MonadTxEnvelope::Legacy(TxLegacy {
                nonce,
                to: ROOT.into(),
                gas_limit: self.gas,
                ..Default::default()
            })
        };
        Recovered::new_unchecked(tx, SENDER)
    }

    fn run(&self, spec: MonadHardfork, inspect: bool) -> Run {
        let mut evm = self.evm(spec);
        let mut trace = Trace::default();
        let tx = self.tx(0);
        let result = if inspect {
            evm.transact_with_inspector(&tx, &mut trace).unwrap().commit()
        } else {
            evm.transact(&tx).unwrap().commit()
        };
        assert_eq!(result.tx_gas_used(), self.gas);
        assert_eq!(result.refunded, 0);
        Run { evm, result, trace }
    }

    fn compare(&self, forks: (MonadHardfork, ReferenceHardfork)) -> Run {
        let mut db = ReferenceDB::default();
        db.insert_account_info(SENDER, ReferenceAccount::default().with_balance(U256::from(GAS)));
        for (address, code) in &self.accounts {
            db.insert_account_info(
                *address,
                ReferenceAccount::default()
                    .with_code(ReferenceBytecode::new_raw(code.original_bytes())),
            );
        }
        for (address, slot, value) in &self.storage {
            db.insert_account_storage(*address, *slot, *value).unwrap();
        }
        let mut reference = monad_context_with_db(db)
            .with_cfg(MonadCfgEnv::new_with_spec(forks.1).with_chain_id(MONAD_MAINNET_CHAIN_ID))
            .with_block(BlockEnv { basefee: 0, ..Default::default() })
            .build_monad();
        let mut tx = TxEnv::builder()
            .caller(SENDER)
            .to(ROOT)
            .gas_limit(self.gas)
            .gas_price(0)
            .chain_id(None);
        if let Some(list) = &self.access_list {
            tx = tx.access_list(list.clone().into()).chain_id(Some(MONAD_MAINNET_CHAIN_ID));
        }
        let expected = reference.transact(tx.build().unwrap()).unwrap();
        let mut actual = self.run(forks.0, true);
        let mut plain = self.run(forks.0, false);
        assert_eq!(actual.result, plain.result, "{forks:?}");
        assert_eq!(
            actual.result.is_success(),
            expected.result.is_success(),
            "{forks:?}: {:?}",
            expected.result
        );
        assert_eq!(
            actual.result.is_revert(),
            matches!(expected.result, ExecutionResult::Revert { .. })
        );
        assert_eq!(
            actual.result.total_gas_spent,
            expected.result.gas().total_gas_spent(),
            "{forks:?}"
        );
        assert_eq!(
            actual.result.output.as_ref(),
            expected.result.output().map_or(&[][..], Bytes::as_ref)
        );
        assert_eq!(actual.result.logs, expected.result.logs());
        if let ExecutionResult::Halt { reason: HaltReason::OutOfGas(_), .. } = expected.result {
            assert!(actual.result.stop.is_out_of_gas());
        }
        for (address, account) in expected.state {
            for (slot, value) in account.storage {
                assert_eq!(
                    actual.evm.state_mut().storage_slot_untracked(&address, &slot).unwrap(),
                    value.present_value,
                    "{forks:?}: {address} slot {slot}"
                );
                assert_eq!(
                    plain.evm.state_mut().storage_slot_untracked(&address, &slot).unwrap(),
                    value.present_value,
                    "{forks:?}: uninspected {address} slot {slot}"
                );
            }
        }
        actual
    }
}

fn push(code: &mut Vec<u8>, value: U256) {
    if value.is_zero() {
        code.push(op::PUSH0);
    } else {
        let bytes = value.to_be_bytes::<32>();
        let first = bytes.iter().position(|byte| *byte != 0).unwrap();
        code.push(op::PUSH1 + (31 - first) as u8);
        code.extend_from_slice(&bytes[first..]);
    }
}

fn instruction(code: &mut Vec<u8>, opcode: u8, args: &[U256]) {
    for arg in args.iter().rev() {
        push(code, *arg);
    }
    code.push(opcode);
}

fn read(code: &mut Vec<u8>, slot: U256) {
    instruction(code, op::SLOAD, &[slot]);
    code.push(op::POP);
}

fn write(code: &mut Vec<u8>, slot: u64, value: u64) {
    instruction(code, op::SSTORE, &[U256::from(slot), U256::from(value)]);
}

fn call(code: &mut Vec<u8>, opcode: u8, address: Address) {
    call_with_gas(code, opcode, address, 200_000);
}

fn call_with_gas(code: &mut Vec<u8>, opcode: u8, address: Address, gas: u64) {
    let mut args = vec![U256::from(gas), address.into_word().into()];
    if matches!(opcode, op::CALL | op::CALLCODE) {
        args.push(U256::ZERO);
    }
    args.extend([U256::ZERO; 4]);
    instruction(code, opcode, &args);
    code.push(op::POP);
}

fn finish(code: &mut Vec<u8>, stop: u8) {
    if stop == op::REVERT {
        instruction(code, stop, &[U256::ZERO; 2]);
    } else {
        code.push(stop);
    }
}

#[test]
fn page_identifiers_cover_the_full_storage_key_space() {
    assert_eq!(
        (PAGE_SHIFT, WORDS_PER_PAGE, BASE_COST, PAGE_WRITE_COST, STATE_GROWTH_COST),
        (7, 128, 100, 2_800, 17_000)
    );
    for slot in [
        U256::ZERO,
        U256::ONE,
        U256::from(127),
        U256::from(128),
        U256::from(u64::MAX),
        U256::ONE << 200,
        U256::MAX,
    ] {
        assert_eq!((page_index(slot) << PAGE_SHIFT) + page_offset(slot), slot);
        assert!(page_offset(slot) < U256::from(WORDS_PER_PAGE));
        assert_eq!(page_index(slot), monad_revm::page::page_index(slot));
        assert_eq!(page_offset(slot), monad_revm::page::page_offset(slot));
        assert_ne!(StoragePageKey::from_slot(ROOT, slot), StoragePageKey::from_slot(CHILD, slot));
    }
}

#[test]
fn reads_warm_pages_only_from_monad_ten() {
    let slots = [
        U256::ZERO,
        U256::from(127),
        U256::from(128),
        U256::from(255),
        U256::MAX,
        U256::MAX - U256::ONE,
    ];
    let mut code = vec![];
    for slot in slots {
        read(&mut code, slot);
    }
    for forks in FORKS {
        let run = Fixture::new(code.clone()).compare(forks);
        let expected = if forks.0 >= MonadHardfork::MonadTen {
            vec![8_100, 100, 8_100, 100, 8_100, 100]
        } else {
            vec![8_100; 6]
        };
        assert_eq!(run.trace.costs(), expected);
    }
}

#[test]
fn unchanged_writes_only_warm_the_page() {
    let mut code = vec![];
    write(&mut code, 0, 0);
    write(&mut code, 1, 1);
    write(&mut code, 1, 2);
    write(&mut code, 2, 0);
    read(&mut code, U256::from(127));
    for forks in FORKS {
        let run = Fixture::new(code.clone()).compare(forks);
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [8_100, 19_900, 100, 100, 100]);
        }
    }
}

#[test]
fn growth_charges_the_page_high_water_mark_without_refunds() {
    let mut code = vec![];
    for (slot, value) in [(0, 1), (1, 1), (0, 0), (0, 1), (1, 0), (2, 1), (3, 1)] {
        write(&mut code, slot, value);
    }
    for forks in FORKS {
        let run = Fixture::new(code.clone()).compare(forks);
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [27_900, 17_100, 100, 100, 100, 100, 17_100]);
        }
    }
}

#[test]
fn clearing_existing_slots_offsets_growth_only_within_the_same_page() {
    let mut code = vec![];
    for (slot, value) in [(0, 0), (1, 0), (2, 1), (3, 1), (128, 1), (4, 1)] {
        write(&mut code, slot, value);
    }
    let mut fixture = Fixture::new(code);
    fixture.storage = vec![(ROOT, U256::ZERO, U256::ONE), (ROOT, U256::ONE, U256::ONE)];
    for forks in FORKS {
        let run = fixture.compare(forks);
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [10_900, 100, 100, 100, 27_900, 17_100]);
        }
    }
}

#[test]
fn growth_counters_cover_all_128_slots_in_both_directions() {
    for initially_full in [false, true] {
        let mut code = vec![];
        for value in if initially_full { [0, 1, 0] } else { [1, 0, 1] } {
            for slot in 0..128 {
                write(&mut code, slot, value);
            }
        }
        let mut fixture = Fixture::new(code);
        if initially_full {
            fixture.storage = (0..128).map(|slot| (ROOT, U256::from(slot), U256::ONE)).collect();
        }
        for forks in FORKS {
            let run = fixture.compare(forks);
            if forks.0 >= MonadHardfork::MonadTen {
                let costs = run.trace.costs();
                assert_eq!(costs[0], if initially_full { 10_900 } else { 27_900 });
                assert!(
                    costs[1..128]
                        .iter()
                        .all(|cost| *cost == if initially_full { 100 } else { 17_100 })
                );
                assert!(costs[128..].iter().all(|cost| *cost == 100));
            }
        }
    }
}

#[test]
fn access_lists_warm_whole_pages_but_still_charge_each_listed_key() {
    let mut code = vec![];
    read(&mut code, U256::from(127));
    write(&mut code, 1, 1);
    write(&mut code, 128, 1);
    let mut fixture = Fixture::new(code);
    fixture.warm(ROOT, &[0, 0, 2]);
    fixture.warm(ROOT, &[3]);
    for forks in FORKS {
        let run = fixture.compare(forks);
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [100, 19_900, 27_900]);
            // Two address entries, four storage keys, and 17 gas for PUSH/POP.
            assert_eq!(
                run.result.total_gas_spent,
                21_000 + 2 * 2_400 + 4 * 1_900 + 17 + 100 + 19_900 + 27_900
            );
        }
    }
}

#[test]
fn reads_and_growth_are_isolated_by_storage_context() {
    for opcode in [op::CALL, op::CALLCODE, op::DELEGATECALL] {
        let mut root = vec![];
        write(&mut root, 0, 0);
        call(&mut root, opcode, CHILD);
        write(&mut root, 1, 1);
        let mut child = vec![];
        write(&mut child, 0, 0);
        write(&mut child, 2, 1);
        let mut fixture = Fixture::new(root);
        fixture.contract(CHILD, child);
        fixture.storage.push((ROOT, U256::ZERO, U256::ONE));
        for forks in FORKS {
            let run = fixture.compare(forks);
            if forks.0 >= MonadHardfork::MonadTen {
                assert_eq!(
                    run.trace.costs(),
                    if opcode == op::CALL {
                        vec![10_900, 8_100, 19_900, 100]
                    } else {
                        vec![10_900, 100, 100, 17_100]
                    }
                );
                assert_eq!(run.trace.accesses[1].1, if opcode == op::CALL { CHILD } else { ROOT });
            }
        }
    }
}

#[test]
fn nested_calls_commit_or_revert_every_page_field() {
    for opcode in [op::CALL, op::CALLCODE, op::DELEGATECALL] {
        for stop in [op::STOP, op::REVERT, op::INVALID] {
            let mut root = vec![];
            call(&mut root, opcode, CHILD);
            call(&mut root, opcode, CHILD);
            let mut child = vec![];
            write(&mut child, 0, 1);
            write(&mut child, 1, 1);
            finish(&mut child, stop);
            let mut fixture = Fixture::new(root);
            fixture.contract(CHILD, child);
            for forks in FORKS {
                let run = fixture.compare(forks);
                if forks.0 >= MonadHardfork::MonadTen {
                    assert_eq!(
                        run.trace.costs(),
                        if stop == op::STOP {
                            vec![27_900, 17_100, 100, 100]
                        } else {
                            vec![27_900, 17_100, 27_900, 17_100]
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn ancestor_revert_undoes_successful_grandchildren_but_preserves_access_lists() {
    for warm in [false, true] {
        let mut root = vec![];
        call(&mut root, op::DELEGATECALL, CHILD);
        call(&mut root, op::DELEGATECALL, GRANDCHILD);
        let mut child = vec![];
        call(&mut child, op::DELEGATECALL, GRANDCHILD);
        finish(&mut child, op::REVERT);
        let mut grandchild = vec![];
        write(&mut grandchild, 0, 1);
        let mut fixture = Fixture::new(root);
        fixture.contract(CHILD, child);
        fixture.contract(GRANDCHILD, grandchild);
        if warm {
            fixture.warm(ROOT, &[127]);
        }
        for forks in FORKS {
            let run = fixture.compare(forks);
            if forks.0 >= MonadHardfork::MonadTen {
                assert_eq!(run.trace.costs(), [if warm { 19_900 } else { 27_900 }; 2]);
            }
        }
    }
}

#[test]
fn static_reads_share_pages_and_static_writes_fail() {
    for writing in [false, true] {
        let mut root = vec![];
        call(&mut root, op::STATICCALL, CHILD);
        call(&mut root, op::CALL, CHILD);
        let mut child = vec![];
        if writing {
            write(&mut child, 0, 1);
        } else {
            read(&mut child, U256::ZERO);
        }
        let mut fixture = Fixture::new(root);
        fixture.contract(CHILD, child);
        for forks in FORKS {
            let run = fixture.compare(forks);
            assert_eq!(
                run.trace.children[0],
                if writing { InstrStop::StateChangeDuringStaticCall } else { InstrStop::Stop }
            );
            if !writing {
                assert_eq!(run.trace.costs(), [8_100, 100]);
            }
        }
    }
}

#[test]
fn failed_code_deposit_reverts_initcode_page_accounting() {
    let mut initcode = vec![];
    write(&mut initcode, 0, 1);
    instruction(&mut initcode, op::MSTORE8, &[U256::ZERO, U256::from(0xef)]);
    instruction(&mut initcode, op::RETURN, &[U256::ZERO, U256::ONE]);
    let mut root = vec![];
    instruction(&mut root, op::MSTORE, &[U256::ZERO, U256::from_be_slice(&initcode)]);
    for _ in 0..2 {
        instruction(
            &mut root,
            op::CREATE2,
            &[U256::ZERO, U256::from(32 - initcode.len()), U256::from(initcode.len()), U256::ONE],
        );
        root.push(op::POP);
    }
    let created = ROOT.create2(U256::ONE.to_be_bytes::<32>(), keccak256(&initcode));
    for forks in FORKS {
        let mut run = Fixture::new(root.clone()).compare(forks);
        assert!(run.trace.children.iter().all(|stop| !stop.is_success()));
        assert_eq!(
            run.evm.state_mut().storage_slot_untracked(&created, &U256::ZERO).unwrap(),
            U256::ZERO
        );
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [27_900; 2]);
        }
    }
}

#[test]
fn exact_storage_gas_boundaries_and_sstore_sentry_match_release() {
    let forks = FORKS[2];
    for (writing, value, cost) in [(false, 0, 8_100), (true, 0, 8_100), (true, 1, 27_900)] {
        let mut code = vec![];
        if writing {
            write(&mut code, 0, value);
        } else {
            instruction(&mut code, op::SLOAD, &[U256::ZERO]);
        }
        let pushes = if writing { if value == 0 { 4 } else { 5 } } else { 2 };
        for short in [false, true] {
            let mut fixture = Fixture::new(code.clone());
            fixture.gas = 21_000 + pushes + cost - u64::from(short);
            assert_eq!(fixture.compare(forks).result.is_success(), !short);
        }
    }
    let mut code = vec![];
    write(&mut code, 0, 0);
    for remaining in [2_300, 2_301] {
        let mut fixture = Fixture::new(code.clone());
        fixture.warm(ROOT, &[127]);
        fixture.gas = 21_000 + 2_400 + 1_900 + 4 + remaining;
        let run = fixture.compare(forks);
        assert_eq!(run.result.is_success(), remaining > 2_300);
        if remaining == 2_300 {
            assert_eq!(run.result.stop, InstrStop::ReentrancySentryOOG);
        }
    }
    // Slot 127 is cold in the native slot tracker, but its page is already warm.
    let mut fixture = Fixture::new(vec![op::PUSH1, 127, op::SLOAD]);
    fixture.warm(ROOT, &[0]);
    fixture.gas = 21_000 + 2_400 + 1_900 + 3 + 100;
    assert!(fixture.compare(forks).result.is_success());
}

#[test]
fn page_state_does_not_escape_commit_discard_validation_or_system_calls() {
    let mut code = vec![];
    write(&mut code, 0, 1);
    let fixture = Fixture::new(code);
    let mut evm = fixture.evm(MonadHardfork::MonadTen);
    let tx = fixture.tx(0);
    evm.validate_tx(&tx).unwrap();
    let mut warmed = Fixture::new(vec![]);
    warmed.warm(ROOT, &[0]);
    evm.validate_tx(&warmed.tx(0)).unwrap();
    let first = evm.call_tx(&tx).unwrap();
    assert_eq!(first.total_gas_spent, 21_000 + 5 + 27_900);
    assert_eq!(evm.call_tx(&tx).unwrap(), first);
    drop(evm.transact(&tx).unwrap());
    assert_eq!(evm.transact(&tx).unwrap().commit(), first);
    // A fresh transaction still pays a cold read for its unchanged write.
    assert_eq!(evm.transact(&fixture.tx(1)).unwrap().commit().total_gas_spent, 21_000 + 5 + 8_100);
    for _ in 0..2 {
        let result = evm.system_call(SystemTx::new(ROOT, Bytes::new())).unwrap().commit();
        assert_eq!(result.total_gas_spent, 5 + 8_100);
    }
}

struct AbortAfterStorage;

impl Inspector<MonadEvmTypes> for AbortAfterStorage {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if interp.opcode() == op::STOP {
            interp.fail(ExecutionError::Fatal("injected after storage".into()));
        }
    }
}

#[test]
fn fatal_execution_errors_clear_page_state() {
    let mut code = vec![];
    write(&mut code, 0, 1);
    code.push(op::STOP);
    let fixture = Fixture::new(code);
    let mut evm = fixture.evm(MonadHardfork::MonadTen);
    let tx = fixture.tx(0);
    assert!(evm.transact_with_inspector(&tx, &mut AbortAfterStorage).is_err());
    assert_eq!(evm.transact(&tx).unwrap().commit().total_gas_spent, 21_000 + 5 + 27_900);
}

#[test]
fn contract_transient_storage_cannot_modify_page_bookkeeping() {
    let mut input = [0; 57];
    input[..5].copy_from_slice(b"MIP-8");
    input[5..25].copy_from_slice(ROOT.as_slice());
    let tracker_slot = U256::from_be_bytes(keccak256(input).0);
    let mut code = vec![];
    read(&mut code, U256::ZERO);
    // Even the exact tracker key is private to the precompile's storage context.
    instruction(&mut code, op::TSTORE, &[tracker_slot, U256::ZERO]);
    read(&mut code, U256::ONE);
    instruction(&mut code, op::TLOAD, &[tracker_slot]);
    push(&mut code, U256::ZERO);
    code.push(op::MSTORE);
    instruction(&mut code, op::RETURN, &[U256::ZERO, U256::from(32)]);
    for forks in FORKS {
        let run = Fixture::new(code.clone()).compare(forks);
        assert_eq!(run.result.output, Bytes::from(vec![0; 32]));
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [8_100, 100]);
        }
    }
}

#[test]
fn reverted_clears_cannot_subsidize_subsequent_growth() {
    for prewarm in [false, true] {
        for stop in [op::STOP, op::REVERT, op::INVALID] {
            let mut root = vec![];
            if prewarm {
                read(&mut root, U256::ZERO);
            }
            call(&mut root, op::DELEGATECALL, CHILD);
            write(&mut root, 1, 1);
            let mut child = vec![];
            write(&mut child, 0, 0);
            finish(&mut child, stop);
            let mut fixture = Fixture::new(root);
            fixture.contract(CHILD, child);
            fixture.storage.push((ROOT, U256::ZERO, U256::ONE));
            for forks in FORKS {
                let run = fixture.compare(forks);
                if forks.0 >= MonadHardfork::MonadTen {
                    let final_cost = if stop == op::STOP {
                        100
                    } else if prewarm {
                        19_900
                    } else {
                        27_900
                    };
                    assert_eq!(run.trace.costs().last(), Some(&final_cost));
                }
            }
        }
    }
}

#[test]
fn out_of_gas_writes_revert_page_tracking_and_persistent_storage() {
    let mut root = vec![];
    call_with_gas(&mut root, op::DELEGATECALL, CHILD, 27_904);
    call(&mut root, op::DELEGATECALL, CHILD);
    let mut child = vec![];
    write(&mut child, 0, 1);
    let mut fixture = Fixture::new(root);
    fixture.contract(CHILD, child);
    for forks in &FORKS[2..] {
        let run = fixture.compare(*forks);
        assert_eq!(run.trace.children, [InstrStop::OutOfGas, InstrStop::Stop]);
        assert_eq!(run.trace.costs().last(), Some(&27_900));
    }
}

#[test]
fn static_write_rejection_precedes_stack_underflow() {
    let mut root = vec![];
    call(&mut root, op::STATICCALL, CHILD);
    let mut fixture = Fixture::new(root);
    fixture.contract(CHILD, vec![op::SSTORE]);
    for forks in &FORKS[2..] {
        assert_eq!(
            fixture.compare(*forks).trace.children,
            [InstrStop::StateChangeDuringStaticCall]
        );
    }
}

#[test]
fn code_at_the_tracker_address_cannot_execute_as_a_contract() {
    let precompile = Address::with_last_byte(1);
    let mut root = vec![];
    read(&mut root, U256::ZERO);
    call(&mut root, op::CALL, precompile);
    read(&mut root, U256::ONE);
    let mut fixture = Fixture::new(root);
    // Simulate code installed by a state override at the permanent precompile.
    fixture.contract(precompile, vec![op::INVALID]);
    for forks in FORKS {
        let run = fixture.compare(forks);
        assert!(run.trace.children[0].is_success());
        if forks.0 >= MonadHardfork::MonadTen {
            assert_eq!(run.trace.costs(), [8_100, 100]);
        }
    }
}

struct FailingStorageDb {
    inner: InMemoryDB,
    reads: usize,
}

impl Database for FailingStorageDb {
    type Error = io::Error;

    fn get_account(&mut self, address: &Address) -> io::Result<Option<AccountInfo>> {
        Ok(self.inner.get_account(address).unwrap())
    }

    fn get_code_by_hash(&mut self, hash: &B256) -> io::Result<Bytecode> {
        Ok(self.inner.get_code_by_hash(hash).unwrap())
    }

    fn get_storage(&mut self, _: &Address, _: &U256) -> io::Result<U256> {
        self.reads += 1;
        Err(io::Error::other("injected storage read failure"))
    }

    fn get_block_hash(&mut self, number: &U256) -> io::Result<B256> {
        Ok(self.inner.get_block_hash(number).unwrap())
    }
}

#[test]
fn unaffordable_cold_accesses_do_not_read_the_database() {
    for writing in [false, true] {
        for affordable in [false, true] {
            let mut code = vec![];
            if writing {
                write(&mut code, 0, 0);
            } else {
                instruction(&mut code, op::SLOAD, &[U256::ZERO]);
            }
            let mut db = FailingStorageDb { inner: InMemoryDB::default(), reads: 0 };
            db.inner.insert_account_info(&SENDER, AccountInfo::default());
            db.inner.insert_account_info(
                &ROOT,
                AccountInfo::default().with_code(Bytecode::new_raw(code.clone().into())),
            );
            let mut fixture = Fixture::new(code);
            fixture.gas = 21_000 + if writing { 4 } else { 2 } + 8_099 + u64::from(affordable);
            let mut evm = MonadEvmFactory::new(MONAD_MAINNET_CHAIN_ID).create_evm(
                MonadHardfork::MonadTen,
                MonadBlockEnv::default(),
                Db::new(&mut db),
            );
            let result = evm.call_tx(&fixture.tx(0));
            if affordable {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().stop, InstrStop::OutOfGas);
            }
            drop(evm);
            assert_eq!(db.reads, usize::from(affordable));
        }
    }
}
