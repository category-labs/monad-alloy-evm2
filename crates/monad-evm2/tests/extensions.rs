//! MIP-7/MIP-18 protocol cases, with optional parity against pinned monad-revm main.

use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use evm2::{
    AccountInfo, Inspector, TxResult,
    bytecode::Bytecode,
    evm::InMemoryDB,
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageResult, op},
};
use monad_evm2::{
    MonadBlockEnv, MonadEvmBuilder, MonadEvmExt, MonadEvmFactory, MonadEvmTypes, MonadHardfork,
    MonadTxEnvelope,
    extension::{CALLERN_SELECTOR as CALLERN, CALLSTACKDEPTH_SELECTOR as DEPTH, EXTENSION},
};

const SENDER: Address = Address::repeat_byte(0x11);
const ROOT: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);
const LEAF: Address = Address::repeat_byte(0x44);
const SIBLING: Address = Address::repeat_byte(0x55);
const CHAIN_ID: u64 = 143;
const WORDS: usize = 11;

type Step = (usize, usize, u8, Vec<U256>);

#[derive(Default)]
struct Trace(Vec<Step>);

impl Inspector<MonadEvmTypes> for Trace {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        self.0.push((
            usize::from(interp.message().depth),
            interp.pc(),
            interp.opcode(),
            interp.stack().as_slice().to_vec(),
        ));
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    status: String,
    output: Bytes,
    created: Option<Address>,
    spent: u64,
    charged: u64,
    steps: Vec<Step>,
}

impl Outcome {
    fn new(result: TxResult<MonadEvmTypes>, trace: Trace) -> Self {
        Self {
            status: format!("{:?}", result.stop),
            output: result.output,
            created: result.created_address,
            spent: result.total_gas_spent,
            charged: result.ext.charged_gas,
            steps: trace.0,
        }
    }

    fn success(&self) {
        assert!(matches!(self.status.as_str(), "Stop" | "Return"), "{self:?}");
    }
}

fn database(accounts: &[(Address, Vec<u8>)]) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        &SENDER,
        AccountInfo::default().with_balance(U256::from(10).pow(U256::from(27))),
    );
    for (address, code) in accounts {
        db.insert_account_info(
            address,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::copy_from_slice(code))),
        );
    }
    db
}

fn transaction(gas_limit: u64) -> TxLegacy {
    TxLegacy { chain_id: Some(CHAIN_ID), to: ROOT.into(), gas_limit, ..Default::default() }
}

fn execute(
    spec: MonadHardfork,
    accounts: &[(Address, Vec<u8>)],
    tx: TxLegacy,
    native: bool,
    inspect: bool,
) -> Outcome {
    #[cfg(feature = "compare-monad-revm-main")]
    let expected = reference::execute(spec, accounts, tx.clone(), inspect);
    let envelope = Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), SENDER);
    let mut trace = Trace::default();
    let result = if native {
        let factory = MonadEvmFactory;
        let mut host = factory.create_evm(
            spec,
            factory.version(spec, CHAIN_ID),
            MonadBlockEnv::default(),
            database(accounts),
        );
        if inspect {
            host.transact_with_inspector(&envelope, &mut trace).unwrap().commit()
        } else {
            host.transact(&envelope).unwrap().commit()
        }
    } else {
        let mut evm = MonadEvmBuilder::new(CHAIN_ID).create_evm(
            spec,
            MonadBlockEnv::default(),
            database(accounts),
        );
        if inspect {
            evm.transact_with_inspector(&envelope, &mut trace).unwrap().commit()
        } else {
            evm.transact(&envelope).unwrap().commit()
        }
    };
    let actual = Outcome::new(result, trace);
    #[cfg(feature = "compare-monad-revm-main")]
    assert_eq!(actual, expected, "{spec} native={native}");
    actual
}

fn modes() -> impl Iterator<Item = (bool, bool)> {
    [false, true].into_iter().flat_map(|native| [false, true].map(|inspect| (native, inspect)))
}

fn push(code: &mut Vec<u8>, value: usize) {
    code.push(op::PUSH2);
    code.extend(u16::try_from(value).unwrap().to_be_bytes());
}

fn store(code: &mut Vec<u8>, index: usize) {
    push(code, index * 32);
    code.push(op::MSTORE);
}

fn return_memory(code: &mut Vec<u8>, size: usize) {
    push(code, size);
    code.extend([op::PUSH0, op::RETURN]);
}

fn probe() -> Vec<u8> {
    let mut code = vec![EXTENSION, DEPTH];
    store(&mut code, 0);
    for (index, n) in [
        U256::ZERO,
        U256::from(1),
        U256::from(2),
        U256::from(3),
        U256::ONE << 32,
        U256::ONE << 64,
        U256::MAX,
    ]
    .into_iter()
    .enumerate()
    {
        code.push(op::PUSH32);
        code.extend(n.to_be_bytes::<32>());
        code.extend([EXTENSION, CALLERN]);
        store(&mut code, index + 1);
    }
    code.extend([EXTENSION, DEPTH, EXTENSION, CALLERN]);
    store(&mut code, 8);
    code.push(op::CALLER);
    store(&mut code, 9);
    code.push(op::ORIGIN);
    store(&mut code, 10);
    return_memory(&mut code, WORDS * 32);
    code
}

fn assert_probe(result: &Outcome, callers: &[Address]) {
    assert_probe_with_origin(result, callers, callers[0]);
}

fn assert_probe_with_origin(result: &Outcome, callers: &[Address], origin: Address) {
    result.success();
    let mut expected = vec![U256::from(callers.len() - 1)];
    for n in 0..4 {
        expected.push(callers.iter().rev().nth(n).map_or(U256::ZERO, |a| a.into_word().into()));
    }
    expected.extend([
        U256::ZERO,
        U256::ZERO,
        U256::ZERO,
        callers[0].into_word().into(),
        callers.last().unwrap().into_word().into(),
        origin.into_word().into(),
    ]);
    assert_eq!(result.output.chunks(32).map(U256::from_be_slice).collect::<Vec<_>>(), expected);
}

fn call(target: Address, opcode: u8) -> Vec<u8> {
    let mut code = vec![op::PUSH0; if matches!(opcode, op::CALL | op::CALLCODE) { 5 } else { 4 }];
    code.push(op::PUSH20);
    code.extend(target.as_slice());
    code.extend([op::GAS, opcode]);
    code
}

fn forward(target: Address, opcode: u8) -> Vec<u8> {
    let mut code = call(target, opcode);
    code.extend([
        op::POP,
        op::RETURNDATASIZE,
        op::PUSH0,
        op::PUSH0,
        op::RETURNDATACOPY,
        op::RETURNDATASIZE,
        op::PUSH0,
        op::RETURN,
    ]);
    code
}

#[test]
fn selectors_and_missing_bytes_are_fork_gated() {
    for spec in MonadHardfork::ALL {
        let expected =
            if spec == MonadHardfork::MonadNext { "InvalidFEOpcode" } else { "OpcodeNotFound" };
        for (native, inspect) in modes() {
            let first = if spec == MonadHardfork::MonadNext { 2 } else { 0 };
            for selector in first..=u8::MAX {
                let result = execute(
                    spec,
                    &[(ROOT, vec![EXTENSION, selector])],
                    transaction(100_000),
                    native,
                    inspect,
                );
                assert_eq!(result.status, expected);
                assert_eq!((result.charged, result.spent), (100_000, 100_000));
            }
            for code in [vec![EXTENSION], vec![op::PUSH0, EXTENSION]] {
                assert_eq!(
                    execute(spec, &[(ROOT, code)], transaction(100_000), native, inspect).status,
                    expected
                );
            }
        }
    }
}

#[test]
fn extension_gas_stack_and_end_of_code_boundaries() {
    for (native, inspect) in modes() {
        for (code, gas, expected) in [
            (vec![EXTENSION, CALLERN], 1, "OutOfGas"),
            (vec![EXTENSION, 0xff], 0, "InvalidFEOpcode"),
            (vec![EXTENSION], 0, "InvalidFEOpcode"),
        ] {
            assert_eq!(
                execute(
                    MonadHardfork::MonadNext,
                    &[(ROOT, code)],
                    transaction(21_000 + gas),
                    native,
                    inspect
                )
                .status,
                expected,
            );
        }
        for (code, gas) in [(vec![EXTENSION, DEPTH], 2), (vec![op::PUSH0, EXTENSION, CALLERN], 4)] {
            let result = execute(
                MonadHardfork::MonadNext,
                &[(ROOT, code.clone())],
                transaction(21_000 + gas),
                native,
                inspect,
            );
            result.success();
            assert_eq!(result.spent, 21_000 + gas);
            assert_eq!(
                execute(
                    MonadHardfork::MonadNext,
                    &[(ROOT, code)],
                    transaction(21_000 + gas - 1),
                    native,
                    inspect
                )
                .status,
                "OutOfGas"
            );
        }
        for (code, expected) in [
            (vec![EXTENSION, CALLERN], "StackUnderflow"),
            ([vec![op::PUSH0; 1024], vec![EXTENSION, DEPTH]].concat(), "StackOverflow"),
            ([vec![op::PUSH0; 1024], vec![EXTENSION, CALLERN]].concat(), "Stop"),
            ([vec![op::PUSH0; 1023], vec![EXTENSION, DEPTH]].concat(), "Stop"),
            (vec![EXTENSION, DEPTH, EXTENSION], "InvalidFEOpcode"),
        ] {
            assert_eq!(
                execute(
                    MonadHardfork::MonadNext,
                    &[(ROOT, code)],
                    transaction(100_000),
                    native,
                    inspect
                )
                .status,
                expected
            );
        }
    }
}

#[test]
fn legacy_jump_analysis_and_push_data_are_unchanged() {
    for spec in MonadHardfork::ALL {
        for (native, inspect) in modes() {
            for (code, expected) in [
                (vec![op::PUSH1, 4, op::JUMP, EXTENSION, op::JUMPDEST, op::STOP], "Stop"),
                (
                    vec![op::PUSH1, 5, op::JUMP, EXTENSION, op::PUSH1, op::JUMPDEST, op::STOP],
                    "InvalidJump",
                ),
            ] {
                assert_eq!(
                    execute(spec, &[(ROOT, code)], transaction(100_000), native, inspect).status,
                    expected
                );
            }
            let code = vec![
                op::PUSH3,
                EXTENSION,
                0,
                op::JUMPDEST,
                op::PUSH0,
                op::MSTORE,
                op::PUSH1,
                32,
                op::PUSH0,
                op::RETURN,
            ];
            let result = execute(spec, &[(ROOT, code)], transaction(100_000), native, inspect);
            result.success();
            assert_eq!(U256::from_be_slice(&result.output), U256::from(0xae005b));
        }
    }
}

#[test]
fn root_call_and_constructor_observe_zero_depth() {
    for (native, inspect) in modes() {
        for create in [false, true] {
            let mut tx = transaction(1_000_000);
            if create {
                tx.to = TxKind::Create;
                tx.input = probe().into();
            }
            assert_probe(
                &execute(MonadHardfork::MonadNext, &[(ROOT, probe())], tx, native, inspect),
                &[SENDER],
            );
        }
    }
}

#[test]
fn callers_follow_call_delegatecall_callcode_and_staticcall() {
    for (native, inspect) in modes() {
        for opcode in [op::CALL, op::CALLCODE, op::DELEGATECALL, op::STATICCALL] {
            let accounts =
                [(ROOT, forward(CHILD, opcode)), (CHILD, forward(LEAF, op::CALL)), (LEAF, probe())];
            let child_caller = if opcode == op::DELEGATECALL { SENDER } else { ROOT };
            let leaf_caller =
                if matches!(opcode, op::DELEGATECALL | op::CALLCODE) { ROOT } else { CHILD };
            assert_probe(
                &execute(
                    MonadHardfork::MonadNext,
                    &accounts,
                    transaction(1_000_000),
                    native,
                    inspect,
                ),
                &[SENDER, child_caller, leaf_caller],
            );
        }
        let delegated = Bytecode::new_eip7702(LEAF).original_byte_slice().to_vec();
        let accounts = [(ROOT, forward(CHILD, op::CALL)), (CHILD, delegated), (LEAF, probe())];
        assert_probe(
            &execute(MonadHardfork::MonadNext, &accounts, transaction(1_000_000), native, inspect),
            &[SENDER, ROOT],
        );
    }
}

#[test]
fn create_and_create2_record_constructor_callers() {
    for (native, inspect) in modes() {
        for opcode in [op::CREATE, op::CREATE2] {
            let initcode = probe();
            let mut code = Vec::new();
            push(&mut code, initcode.len());
            push(&mut code, 0);
            code.extend([op::PUSH0, op::CODECOPY]);
            if opcode == op::CREATE2 {
                code.push(op::PUSH0);
            }
            push(&mut code, initcode.len());
            code.extend([op::PUSH0, op::PUSH0, opcode, op::PUSH0, op::MSTORE]);
            push(&mut code, WORDS * 32);
            code.extend([op::PUSH0, op::PUSH0, op::PUSH0, op::MLOAD, op::EXTCODECOPY]);
            return_memory(&mut code, WORDS * 32);
            let offset = u16::try_from(code.len()).unwrap();
            code[4..6].copy_from_slice(&offset.to_be_bytes());
            code.extend(initcode);
            assert_probe(
                &execute(
                    MonadHardfork::MonadNext,
                    &[(ROOT, code)],
                    transaction(1_000_000),
                    native,
                    inspect,
                ),
                &[SENDER, ROOT],
            );
        }
    }
}

#[test]
fn completed_failed_and_immediate_calls_do_not_leak_ancestry() {
    for (native, inspect) in modes() {
        for terminal in [op::STOP, op::REVERT, op::INVALID] {
            let mut child = call(LEAF, op::CALL);
            child.extend([op::POP, op::PUSH0, op::PUSH0, terminal]);
            let mut root = call(CHILD, op::DELEGATECALL);
            root.push(op::POP);
            root.extend(forward(SIBLING, op::CALL));
            let accounts =
                [(ROOT, root), (CHILD, child), (SIBLING, forward(LEAF, op::CALL)), (LEAF, probe())];
            assert_probe(
                &execute(
                    MonadHardfork::MonadNext,
                    &accounts,
                    transaction(10_000_000),
                    native,
                    inspect,
                ),
                &[SENDER, ROOT, SIBLING],
            );
        }
        for (target, child) in [
            (Address::with_last_byte(4), vec![]),
            (CHILD, vec![]),
            (CHILD, probe()),
            (CHILD, vec![op::PUSH0, op::PUSH0, op::REVERT]),
            (CHILD, vec![op::INVALID]),
        ] {
            let mut root = call(target, op::CALL);
            root.push(op::POP);
            root.extend(probe());
            assert_probe(
                &execute(
                    MonadHardfork::MonadNext,
                    &[(ROOT, root), (CHILD, child)],
                    transaction(10_000_000),
                    native,
                    inspect,
                ),
                &[SENDER],
            );
        }
    }
}

#[test]
fn inspection_reports_one_step_per_extension_and_original_pcs() {
    for native in [false, true] {
        let code = vec![EXTENSION, DEPTH, EXTENSION, CALLERN, op::POP, op::PC, op::STOP];
        let result =
            execute(MonadHardfork::MonadNext, &[(ROOT, code)], transaction(100_000), native, true);
        result.success();
        assert_eq!(result.steps.iter().map(|s| s.1).collect::<Vec<_>>(), [0, 2, 4, 5, 6]);
        assert_eq!(result.steps[1].3, [U256::ZERO]);
        assert_eq!(result.steps[2].3, [U256::from_be_slice(SENDER.as_slice())]);
        assert_eq!(result.steps[4].3, [U256::from(5)]);
    }
}

#[test]
fn terminal_extension_keeps_inspector_opcode_reads_in_bounds() {
    #[derive(Default)]
    struct End(Vec<(usize, u8, Vec<U256>)>);
    impl Inspector<MonadEvmTypes> for End {
        fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
            assert!(interp.pc() < interp.active_bytecode().bytes_slice().len());
            self.0.push((interp.pc(), interp.opcode(), interp.stack().as_slice().to_vec()));
        }
    }
    for code in [vec![EXTENSION, DEPTH], vec![op::PUSH0, EXTENSION, CALLERN]] {
        let mut evm = MonadEvmBuilder::new(CHAIN_ID).create_evm(
            MonadHardfork::MonadNext,
            MonadBlockEnv::default(),
            database(&[(ROOT, code.clone())]),
        );
        let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(transaction(100_000)), SENDER);
        let mut end = End::default();
        let result = evm.transact_with_inspector(&tx, &mut end).unwrap().commit();
        assert!(result.is_success());
        let expected =
            if code.len() == 2 { U256::ZERO } else { U256::from_be_slice(SENDER.as_slice()) };
        assert_eq!(end.0.last(), Some(&(code.len() - 2, EXTENSION, vec![expected])));
    }
}

#[test]
fn fork_overrides_gate_extensions_and_preserve_ancestor_callers() {
    struct Select(MonadHardfork, Option<MonadHardfork>);
    impl Inspector<MonadEvmTypes> for Select {
        fn call(
            &mut self,
            _: &mut Interpreter<'_, '_, MonadEvmTypes>,
            message: &mut Message<MonadEvmTypes>,
        ) -> Option<MessageResult<MonadEvmTypes>> {
            message.ext.hardfork = match message.depth {
                1 => Some(self.0),
                2 => self.1,
                _ => None,
            };
            None
        }
    }
    for root in MonadHardfork::ALL {
        let accounts =
            [(ROOT, forward(CHILD, op::CALL)), (CHILD, forward(LEAF, op::CALL)), (LEAF, probe())];
        let mut evm = MonadEvmBuilder::new(CHAIN_ID).create_evm(
            root,
            MonadBlockEnv::default(),
            database(&accounts),
        );
        let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(transaction(1_000_000)), SENDER);
        // A Next descendant must see callers from its older-fork ancestors too.
        let result = evm
            .transact_with_inspector(
                &tx,
                &mut Select(MonadHardfork::MonadTen, Some(MonadHardfork::MonadNext)),
            )
            .unwrap()
            .discard();
        assert_probe(&Outcome::new(result, Trace::default()), &[SENDER, ROOT, CHILD]);
        let result = evm
            .transact_with_inspector(
                &tx,
                &mut Select(MonadHardfork::MonadNext, Some(MonadHardfork::MonadTen)),
            )
            .unwrap()
            .discard();
        assert!(result.is_success());
        assert!(result.output.is_empty());
    }
}

#[test]
fn inspector_overrides_and_context_restores_preserve_active_ancestry() {
    struct Restore;
    impl Inspector<MonadEvmTypes> for Restore {
        fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
            let host = interp.host();
            let mut context = MonadEvmExt::default();
            context.chain = host.ext().chain.clone();
            context.reserve_balance = host.ext().reserve_balance.clone();
            host.ext_mut().replace_while_executing(context);
        }
        fn call(
            &mut self,
            _: &mut Interpreter<'_, '_, MonadEvmTypes>,
            message: &mut Message<MonadEvmTypes>,
        ) -> Option<MessageResult<MonadEvmTypes>> {
            (message.call_target == SIBLING).then(|| MessageResult::<MonadEvmTypes> {
                stop: InstrStop::Revert,
                gas: GasTracker::new(message.gas_limit),
                ..Default::default()
            })
        }
    }
    let mut child = call(SIBLING, op::DELEGATECALL);
    child.push(op::POP);
    child.extend(forward(LEAF, op::CALL));
    let accounts = [(ROOT, forward(CHILD, op::CALL)), (CHILD, child), (LEAF, probe())];
    let mut evm = MonadEvmBuilder::new(CHAIN_ID).create_evm(
        MonadHardfork::MonadNext,
        MonadBlockEnv::default(),
        database(&accounts),
    );
    let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(transaction(1_000_000)), SENDER);
    let result = evm.transact_with_inspector(&tx, &mut Restore).unwrap().commit();
    assert_probe(&Outcome::new(result, Trace::default()), &[SENDER, ROOT, CHILD]);
}

#[test]
fn preserved_synthetic_context_retains_ancestor_callers() {
    #[derive(Default)]
    struct Capture(Option<MonadEvmExt>);
    impl Inspector<MonadEvmTypes> for Capture {
        fn call(
            &mut self,
            interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
            message: &mut Message<MonadEvmTypes>,
        ) -> Option<MessageResult<MonadEvmTypes>> {
            if message.depth == 1 {
                self.0 = Some(interp.host().ext().clone());
            }
            None
        }
    }
    let spec = MonadHardfork::MonadNext;
    let accounts = [(ROOT, forward(CHILD, op::CALL)), (CHILD, probe())];
    let mut evm = MonadEvmBuilder::new(CHAIN_ID).create_evm(
        spec,
        MonadBlockEnv::default(),
        database(&accounts),
    );
    let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(transaction(1_000_000)), SENDER);
    let mut capture = Capture::default();
    assert!(evm.transact_with_inspector(&tx, &mut capture).unwrap().commit().is_success());

    let mut nested = MonadEvmBuilder::new(CHAIN_ID).create_evm(
        spec,
        MonadBlockEnv::default(),
        database(&[(CHILD, probe())]),
    );
    *nested.ext_mut() = capture.0.unwrap();
    nested.ext_mut().set_preserve_reserve_balance_tracker(true);
    nested.set_initial_call_depth(1);
    let mut tx = transaction(1_000_000);
    tx.to = CHILD.into();
    let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), ROOT);
    let result = nested.transact(&tx).unwrap().commit();
    assert_probe_with_origin(&Outcome::new(result, Trace::default()), &[SENDER, ROOT], ROOT);
}

#[test]
fn transaction_reuse_and_nonzero_initial_depth_do_not_expose_stale_callers() {
    for native in [false, true] {
        let spec = MonadHardfork::MonadNext;
        let accounts =
            [(ROOT, forward(CHILD, op::CALL)), (CHILD, probe()), (LEAF, vec![op::INVALID])];
        let db = database(&accounts);
        let (mut host, execution) = if native {
            let factory = MonadEvmFactory;
            (
                factory.create_evm(
                    spec,
                    factory.version(spec, CHAIN_ID),
                    MonadBlockEnv::default(),
                    db,
                ),
                None,
            )
        } else {
            let (host, execution) =
                MonadEvmBuilder::new(CHAIN_ID).create_host(spec, MonadBlockEnv::default(), db);
            (host, Some(execution))
        };
        for (nonce, target, depth) in [(0, ROOT, 0), (1, LEAF, 0), (2, CHILD, 1024), (3, ROOT, 0)] {
            host.set_initial_call_depth(depth);
            let mut tx = transaction(1_000_000);
            tx.nonce = nonce;
            tx.to = target.into();
            let tx = Recovered::new_unchecked(MonadTxEnvelope::Legacy(tx), SENDER);
            let result = if let Some(execution) = &execution {
                execution.transact(&mut host, &tx, None).unwrap().commit()
            } else {
                host.transact(&tx).unwrap().commit()
            };
            let result = Outcome::new(result, Trace::default());
            if target == LEAF {
                assert_eq!(result.status, "InvalidFEOpcode");
            } else if depth == 0 {
                assert_probe(&result, &[SENDER, ROOT]);
            } else {
                result.success();
                let words = result.output.chunks(32).map(U256::from_be_slice).collect::<Vec<_>>();
                assert_eq!(words[0], U256::from(1024));
                assert_eq!(words[1], U256::from_be_slice(SENDER.as_slice()));
                // A synthetic initial depth supplies no implicit ancestor callers.
                assert!(words[2..9].iter().all(U256::is_zero));
                assert_eq!(words[9], U256::from_be_slice(SENDER.as_slice()));
                assert_eq!(words[10], U256::from_be_slice(SENDER.as_slice()));
            }
        }
    }
}

#[cfg(feature = "compare-monad-revm-main")]
mod reference {
    use super::*;
    use monad_revm_main::{MonadBuilder, MonadCfgEnv, MonadHaltReason, monad_context_with_db};
    use revm::{
        ExecuteEvm,
        context::{
            TxEnv,
            result::{ExecutionResult, HaltReason},
        },
        inspector::InspectEvm,
        interpreter::{
            Interpreter as RevmInterpreter,
            interpreter_types::{InputsTr, Jumps},
        },
        state::{AccountInfo as RevmAccount, Bytecode as RevmCode},
    };

    impl<CTX: revm::context_interface::ContextTr> revm::Inspector<CTX> for Trace {
        fn step(&mut self, interp: &mut RevmInterpreter, _: &mut CTX) {
            self.0.push((
                interp.input.depth(),
                interp.bytecode.pc(),
                interp.bytecode.opcode(),
                interp.stack.data().to_vec(),
            ));
        }
    }

    pub(super) fn execute(
        spec: MonadHardfork,
        accounts: &[(Address, Vec<u8>)],
        tx: TxLegacy,
        inspect: bool,
    ) -> Outcome {
        let mut db = revm::database::InMemoryDB::default();
        db.insert_account_info(
            SENDER,
            RevmAccount { balance: U256::from(10).pow(U256::from(27)), ..Default::default() },
        );
        for (address, code) in accounts {
            db.insert_account_info(
                *address,
                RevmAccount::default().with_code(RevmCode::new_raw(Bytes::copy_from_slice(code))),
            );
        }
        let cfg =
            MonadCfgEnv::new_with_spec(spec.to_string().parse().unwrap()).with_chain_id(CHAIN_ID);
        let mut evm =
            monad_context_with_db(db).with_cfg(cfg).build_monad_with_inspector(Trace::default());
        let mut request = TxEnv::builder()
            .caller(SENDER)
            .gas_limit(tx.gas_limit)
            .gas_price(tx.gas_price)
            .nonce(tx.nonce)
            .chain_id(tx.chain_id)
            .data(tx.input);
        request = match tx.to {
            TxKind::Create => request.create(),
            TxKind::Call(address) => request.to(address),
        };
        let request = request.build_fill();
        let result =
            if inspect { evm.inspect_tx(request) } else { evm.transact(request) }.unwrap().result;
        let status = match &result {
            ExecutionResult::Success { reason, .. } => format!("{reason:?}"),
            ExecutionResult::Revert { .. } => "Revert".into(),
            ExecutionResult::Halt {
                reason: MonadHaltReason::Ethereum(HaltReason::OutOfGas(_)),
                ..
            } => "OutOfGas".into(),
            ExecutionResult::Halt { reason: MonadHaltReason::Ethereum(reason), .. } => {
                format!("{reason:?}")
            }
            other => panic!("unexpected reference result: {other:?}"),
        };
        Outcome {
            status,
            output: result.output().cloned().unwrap_or_default(),
            created: result.created_address(),
            spent: result.gas().total_gas_spent(),
            charged: result.tx_gas_used(),
            steps: evm.0.inspector.0,
        }
    }
}
