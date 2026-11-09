//! Per-frame fork selection against monad-revm 0.8.0.

mod common;

use alloy_consensus::{TxEip2930, TxLegacy, transaction::Recovered};
use alloy_monad_evm2::{
    MonadBlockEnv, MonadEvm, MonadEvmFactory, MonadEvmTypes, MonadHardfork, MonadTxEnvelope,
    frame_hardfork, monad_version,
};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use common::FORKS;
use evm2::{
    AccountInfo, ExecutionConfig, ExecutionError, Inspector, OpcodeConfig, SpecId, TxResult,
    bytecode::Bytecode,
    evm::{InMemoryDB, SystemTx},
    inspector::InspectorConfig,
    interpreter::{GasTracker, InstrStop, Interpreter, Message, MessageResult, op},
    version::GasId,
};
use monad_revm::{
    MonadBuilder, MonadCfgEnv, MonadHardfork as ReferenceHardfork, api::default_ctx::MonadContext,
    monad_context_with_db,
};
use revm::{
    InspectEvm,
    context::{BlockEnv, TxEnv},
    context_interface::{result::ExecutionResult, transaction::AccessListItem},
    database::InMemoryDB as ReferenceDB,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome},
    state::{AccountInfo as ReferenceAccount, Bytecode as ReferenceBytecode},
};
use std::panic::{AssertUnwindSafe, catch_unwind};

const SENDER: Address = Address::repeat_byte(0x11);
const ROOT: Address = Address::repeat_byte(0x22);
const CHILD: Address = Address::repeat_byte(0x33);
const GRANDCHILD: Address = Address::repeat_byte(0x44);
const GAS: u64 = 2_000_000;

fn reference_spec(spec: MonadHardfork) -> ReferenceHardfork {
    FORKS.into_iter().find(|(fork, _)| *fork == spec).unwrap().1
}

struct Selector {
    root: MonadHardfork,
    child: MonadHardfork,
    grandchild: Option<MonadHardfork>,
    steps: Vec<(u16, MonadHardfork)>,
    ends: Vec<(u16, MonadHardfork, InstrStop)>,
}

impl Selector {
    const fn new(root: MonadHardfork, child: MonadHardfork) -> Self {
        Self { root, child, grandchild: None, steps: Vec::new(), ends: Vec::new() }
    }
    const fn select(&self, message: &mut Message<MonadEvmTypes>) {
        if message.depth == 1 {
            message.ext.hardfork = Some(self.child);
        } else if message.depth == 2 {
            message.ext.hardfork = self.grandchild;
        }
    }
    fn end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &MessageResult<MonadEvmTypes>,
    ) {
        self.ends.push((message.depth, frame_hardfork(interp.host()), result.stop));
    }
}

impl Inspector<MonadEvmTypes> for Selector {
    fn inspector_config(&self) -> InspectorConfig {
        InspectorConfig::new()
            .with_steps([op::MSTORE, op::SLOAD, op::SSTORE, op::CLZ].into_iter().collect())
    }
    fn call(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.select(message);
        None
    }
    fn create(
        &mut self,
        _: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.select(message);
        None
    }
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        let depth = interp.message().depth;
        let actual = frame_hardfork(interp.host());
        assert_eq!(
            actual,
            match depth {
                0 => self.root,
                1 => self.child,
                _ => self.grandchild.unwrap_or(self.child),
            }
        );
        self.steps.push((depth, actual));
    }
    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.end(interp, message, result);
    }
    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &Message<MonadEvmTypes>,
        result: &mut MessageResult<MonadEvmTypes>,
    ) {
        self.end(interp, message, result);
    }
}

struct ReferenceSelector {
    root: MonadHardfork,
    child: MonadHardfork,
    grandchild: Option<MonadHardfork>,
}

impl revm::Inspector<MonadContext<ReferenceDB>> for ReferenceSelector {
    fn call(
        &mut self,
        context: &mut MonadContext<ReferenceDB>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        let spec = if inputs.bytecode_address == ROOT {
            self.root
        } else if inputs.bytecode_address == GRANDCHILD {
            self.grandchild.unwrap_or(self.child)
        } else {
            self.child
        };
        context.cfg = MonadCfgEnv::new_with_spec(reference_spec(spec));
        None
    }
    fn create(
        &mut self,
        context: &mut MonadContext<ReferenceDB>,
        _: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        context.cfg = MonadCfgEnv::new_with_spec(reference_spec(self.child));
        None
    }
}

fn evm(
    spec: MonadHardfork,
    accounts: &[(Address, Bytes)],
    memory_limit: Option<u64>,
) -> MonadEvm<'static> {
    let mut db = InMemoryDB::default();
    db.insert_account_info(&SENDER, AccountInfo::default().with_balance(U256::from(GAS)));
    for (address, code) in accounts {
        db.insert_account_info(
            address,
            AccountInfo::default().with_code(Bytecode::new_legacy(code.clone())),
        );
    }
    let mut factory = MonadEvmFactory::new(1);
    factory.limits.memory_limit = memory_limit;
    factory.create_evm(spec, MonadBlockEnv::default(), db)
}

fn tx() -> Recovered<MonadTxEnvelope> {
    Recovered::new_unchecked(
        MonadTxEnvelope::Legacy(TxLegacy { to: ROOT.into(), gas_limit: GAS, ..Default::default() }),
        SENDER,
    )
}

fn compare(
    root: MonadHardfork,
    child: MonadHardfork,
    accounts: &[(Address, Bytes)],
) -> (TxResult<MonadEvmTypes>, Selector) {
    compare_tree(root, child, None, accounts)
}

fn compare_tree(
    root: MonadHardfork,
    child: MonadHardfork,
    grandchild: Option<MonadHardfork>,
    accounts: &[(Address, Bytes)],
) -> (TxResult<MonadEvmTypes>, Selector) {
    compare_with_warmth(root, child, grandchild, accounts, Address::ZERO, vec![])
}

fn compare_with_warmth(
    root: MonadHardfork,
    child: MonadHardfork,
    grandchild: Option<MonadHardfork>,
    accounts: &[(Address, Bytes)],
    beneficiary: Address,
    access_list: Vec<AccessListItem>,
) -> (TxResult<MonadEvmTypes>, Selector) {
    let mut actual = evm(root, accounts, None);
    actual.set_block(MonadBlockEnv { beneficiary, ..Default::default() });
    let mut selector = Selector::new(root, child);
    selector.grandchild = grandchild;
    let transaction = if access_list.is_empty() {
        tx()
    } else {
        Recovered::new_unchecked(
            MonadTxEnvelope::Eip2930(TxEip2930 {
                chain_id: 1,
                to: ROOT.into(),
                gas_limit: GAS,
                access_list: access_list.clone().into(),
                ..Default::default()
            }),
            SENDER,
        )
    };
    let result = actual.transact_with_inspector(&transaction, &mut selector).unwrap().commit();
    let mut db = ReferenceDB::default();
    db.insert_account_info(SENDER, ReferenceAccount::default().with_balance(U256::from(GAS)));
    for (address, code) in accounts {
        db.insert_account_info(
            *address,
            ReferenceAccount::default().with_code(ReferenceBytecode::new_raw(code.clone())),
        );
    }
    let mut reference = monad_context_with_db(db)
        .with_cfg(MonadCfgEnv::new_with_spec(reference_spec(root)).with_chain_id(1))
        .with_block(BlockEnv { beneficiary, basefee: 0, ..Default::default() })
        .build_monad_with_inspector(ReferenceSelector { root, child, grandchild });
    let expected = reference
        .inspect_one_tx(
            TxEnv::builder()
                .caller(SENDER)
                .kind(TxKind::Call(ROOT))
                .gas_limit(GAS)
                .gas_price(0)
                .access_list(access_list.into())
                .build_fill(),
        )
        .unwrap();
    assert_eq!(result.is_success(), expected.is_success(), "{root} -> {child}: {expected:?}");
    assert_eq!(
        result.output.as_ref(),
        expected.output().map_or(&[][..], |bytes| bytes.as_ref()),
        "{root} -> {child}"
    );
    if matches!(expected, ExecutionResult::Success { .. }) {
        assert_eq!(result.logs, *expected.logs());
    }
    assert_eq!(frame_hardfork(&actual), root);
    assert_eq!(actual.ext().memory_pool_offset(), 0);
    (result, selector)
}

fn grow(code: &mut Vec<u8>, offset: u32) {
    code.extend([op::PUSH0, op::PUSH3]);
    code.extend(&offset.to_be_bytes()[1..]);
    code.push(op::MSTORE);
}

fn return_gas(code: &mut Vec<u8>, stop: u8) {
    code.extend([op::GAS, op::PUSH0, op::MSTORE, op::PUSH1, 32, op::PUSH0, stop]);
}

fn call(code: &mut Vec<u8>, opcode: u8, target: Address) {
    code.extend([op::PUSH1, 32, op::PUSH0, op::PUSH0, op::PUSH0]);
    if matches!(opcode, op::CALL | op::CALLCODE) {
        code.push(op::PUSH0);
    }
    code.push(op::PUSH20);
    code.extend(target.as_slice());
    code.extend([op::GAS, opcode]);
}

fn finish_parent(code: &mut Vec<u8>) {
    // Return the child's output, call success and remaining parent gas.
    code.extend([op::PUSH1, 32, op::MSTORE]);
    grow(code, 0x2000);
    code.extend([op::GAS, op::PUSH1, 64, op::MSTORE, op::PUSH1, 96, op::PUSH0, op::RETURN]);
}

#[test]
fn supported_forks_share_the_inherited_engine_rules() {
    let first = monad_version(MonadHardfork::MonadEight, 1);
    for spec in MonadHardfork::ALL {
        let version = monad_version(spec, 1);
        assert_eq!(version.features, first.features);
        assert_eq!(version.max_code_size, first.max_code_size);
        assert_eq!(version.max_initcode_size, first.max_initcode_size);
        assert_eq!(version.max_call_depth(), first.max_call_depth());
        for index in 0..GasId::COUNT {
            let id = GasId::from_usize(index).unwrap();
            assert_eq!(version.gas_params.get(id), first.gas_params.get(id), "{spec}: {id:?}");
        }
    }
    let prague = OpcodeConfig::<MonadEvmTypes>::base_for_spec(SpecId::PRAGUE);
    let osaka = OpcodeConfig::<MonadEvmTypes>::base_for_spec(SpecId::OSAKA);
    for opcode in 0..=u8::MAX {
        if opcode != op::CLZ {
            assert_eq!(prague.static_gas(opcode), osaka.static_gas(opcode));
            assert_eq!(prague.is_defined(opcode), osaka.is_defined(opcode));
        }
    }
}

#[test]
fn nested_calls_restore_memory_and_storage_rules_across_all_forks() {
    for opcode in [op::CALL, op::CALLCODE, op::DELEGATECALL, op::STATICCALL] {
        for stop in [op::RETURN, op::REVERT, op::INVALID] {
            let mut parent = vec![];
            call(&mut parent, opcode, CHILD);
            // Touch a page after the child returns, including shared storage contexts.
            parent.extend([op::PUSH0, op::SLOAD, op::POP, op::PUSH1, 1, op::SLOAD, op::POP]);
            finish_parent(&mut parent);
            let mut child = vec![];
            grow(&mut child, 0x2000);
            child.extend([op::PUSH0, op::SLOAD, op::POP, op::PUSH1, 1, op::SLOAD, op::POP]);
            if opcode != op::STATICCALL {
                child.extend([op::PUSH1, 1, op::PUSH0, op::SSTORE]);
            }
            return_gas(&mut child, stop);
            let accounts = [(ROOT, parent.into()), (CHILD, child.into())];
            for root in MonadHardfork::ALL {
                for child in MonadHardfork::ALL {
                    let (_, trace) = compare(root, child, &accounts);
                    assert!(trace.steps.contains(&(1, child)));
                    assert_eq!(trace.steps.last(), Some(&(0, root)));
                    assert_eq!(trace.ends[0].1, child);
                    assert_eq!(trace.ends.last().unwrap().1, root);
                }
            }
        }
    }
}

#[test]
fn clz_activation_belongs_to_each_frame() {
    for root in MonadHardfork::ALL {
        for child in MonadHardfork::ALL {
            for parent_clz in [false, true] {
                let mut parent = vec![];
                call(&mut parent, op::CALL, CHILD);
                if parent_clz {
                    parent.extend([op::PUSH1, 1, op::CLZ, op::POP]);
                }
                finish_parent(&mut parent);
                let child_code = Bytes::from_static(&[
                    op::PUSH1,
                    1,
                    op::CLZ,
                    op::PUSH0,
                    op::MSTORE,
                    op::PUSH1,
                    32,
                    op::PUSH0,
                    op::RETURN,
                ]);
                let (result, trace) =
                    compare(root, child, &[(ROOT, parent.into()), (CHILD, child_code)]);
                assert_eq!(result.is_success(), !parent_clz || root >= MonadHardfork::MonadNine);
                assert_eq!(trace.ends[0].2.is_success(), child >= MonadHardfork::MonadNine);
            }
        }
    }
}

#[test]
fn immediate_precompiles_follow_child_rules_and_restore_the_parent() {
    for target in [Address::with_last_byte(4), Address::with_last_byte(5)] {
        let mut parent = vec![];
        call(&mut parent, op::CALL, target);
        finish_parent(&mut parent);
        for root in MonadHardfork::ALL {
            for child in MonadHardfork::ALL {
                let (_, trace) = compare(root, child, &[(ROOT, Bytes::copy_from_slice(&parent))]);
                assert_eq!(trace.ends[0].1, child);
                assert_eq!(trace.steps.last(), Some(&(0, root)));
            }
        }
    }
}

#[test]
fn reserve_precompile_activation_and_contract_fallback_follow_the_child() {
    let target = alloy_monad_evm2::reserve_balance::RESERVE_BALANCE_ADDRESS;
    let fallback = Bytes::from_static(&[
        op::PUSH1,
        0x42,
        op::PUSH0,
        op::MSTORE,
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ]);
    let mut parent = vec![];
    call(&mut parent, op::CALL, target);
    finish_parent(&mut parent);
    let accounts = [(ROOT, parent.into()), (target, fallback)];
    for root in MonadHardfork::ALL {
        for child in MonadHardfork::ALL {
            let (result, trace) = compare(root, child, &accounts);
            assert!(result.is_success());
            // Empty calldata reverts in the precompile and returns 0x42 in bytecode.
            assert_eq!(trace.ends[0].2.is_success(), child == MonadHardfork::MonadEight);
        }
    }
}

#[test]
fn grandchildren_inherit_or_override_and_unwind_each_parent_separately() {
    let mut parent = vec![];
    call(&mut parent, op::CALL, CHILD);
    finish_parent(&mut parent);
    let mut child = vec![];
    call(&mut child, op::CALL, GRANDCHILD);
    finish_parent(&mut child);
    let mut grandchild = vec![];
    grow(&mut grandchild, 0x2000);
    grandchild.extend([op::PUSH0, op::SLOAD, op::POP, op::PUSH1, 1, op::SLOAD, op::POP]);
    return_gas(&mut grandchild, op::RETURN);
    let accounts = [(ROOT, parent.into()), (CHILD, child.into()), (GRANDCHILD, grandchild.into())];
    for root in MonadHardfork::ALL {
        for child in MonadHardfork::ALL {
            for grandchild in [None, Some(MonadHardfork::MonadEight), Some(MonadHardfork::MonadTen)]
            {
                let (_, trace) = compare_tree(root, child, grandchild, &accounts);
                assert_eq!(
                    trace.ends.iter().map(|(depth, spec, _)| (*depth, *spec)).collect::<Vec<_>>(),
                    [(2, grandchild.unwrap_or(child)), (1, child), (0, root)]
                );
            }
        }
    }
}

#[test]
fn creation_selects_child_rules_and_restores_the_parent() {
    for opcode in [op::CREATE, op::CREATE2] {
        for stop in [op::RETURN, op::REVERT] {
            let mut initcode = vec![];
            grow(&mut initcode, 0x2000);
            initcode.extend([op::PUSH1, 1, op::PUSH0, op::SSTORE]);
            return_gas(&mut initcode, stop);
            let mut parent =
                vec![op::PUSH1, initcode.len() as u8, op::PUSH2, 0, 0, op::PUSH0, op::CODECOPY];
            if opcode == op::CREATE2 {
                parent.push(op::PUSH0);
            }
            parent.extend([op::PUSH1, initcode.len() as u8, op::PUSH0, op::PUSH0, opcode]);
            finish_parent(&mut parent);
            let offset = (parent.len() as u16).to_be_bytes();
            parent[3..5].copy_from_slice(&offset);
            parent.extend(initcode);
            for root in MonadHardfork::ALL {
                for child in MonadHardfork::ALL {
                    let (_, trace) =
                        compare(root, child, &[(ROOT, Bytes::copy_from_slice(&parent))]);
                    assert!(trace.steps.contains(&(1, child)));
                    assert_eq!(trace.steps.last(), Some(&(0, root)));
                }
            }
        }
    }
}

#[test]
fn memory_caps_can_be_tightened_and_relaxed_between_frames() {
    let mut parent = vec![];
    call(&mut parent, op::CALL, CHILD);
    finish_parent(&mut parent);
    let mut child = vec![];
    grow(&mut child, 8 * 1024 * 1024);
    return_gas(&mut child, op::RETURN);
    let accounts = [(ROOT, parent.into()), (CHILD, child.into())];
    for root in MonadHardfork::ALL {
        for child in MonadHardfork::ALL {
            let mut actual = evm(root, &accounts, None);
            let mut selector = Selector::new(root, child);
            // Quadratic expansion on MonadEight needs more than the ordinary tx cap.
            let result = actual
                .system_call_with_inspector(
                    SystemTx::new(ROOT, Bytes::new()).with_gas_limit(200_000_000),
                    &mut selector,
                )
                .unwrap()
                .commit();
            assert!(result.is_success());
            assert_eq!(
                selector.ends[0].2.is_success(),
                !cfg!(feature = "memory_limit") || child == MonadHardfork::MonadEight
            );
            assert_eq!(frame_hardfork(&actual), root);
        }
    }
}

struct FailChild {
    panic: bool,
    selector: Selector,
}

impl Inspector<MonadEvmTypes> for FailChild {
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        self.selector.call(interp, message)
    }
    fn step(&mut self, interp: &mut Interpreter<'_, '_, MonadEvmTypes>) {
        if interp.message().depth == 1 {
            assert_eq!(frame_hardfork(interp.host()), self.selector.child);
            assert!(!self.panic, "injected frame panic");
            interp.fail(ExecutionError::Fatal("injected frame failure".into()));
        }
    }
}

#[test]
fn fatal_errors_and_panics_do_not_leak_a_child_fork() {
    for system in [false, true] {
        for panic in [false, true] {
            let root = MonadHardfork::MonadEight;
            let mut parent = vec![];
            call(&mut parent, op::CALL, CHILD);
            finish_parent(&mut parent);
            let accounts = [(ROOT, parent.into()), (CHILD, Bytes::from_static(&[op::STOP]))];
            let mut actual = evm(root, &accounts, None);
            let mut inspector =
                FailChild { panic, selector: Selector::new(root, MonadHardfork::MonadTen) };
            let run = catch_unwind(AssertUnwindSafe(|| {
                let pending = if system {
                    actual.system_call_with_inspector(
                        SystemTx::new(ROOT, Bytes::new()),
                        &mut inspector,
                    )
                } else {
                    actual.transact_with_inspector(&tx(), &mut inspector)
                };
                pending.map(|pending| pending.commit())
            }));
            if panic {
                assert!(run.is_err());
            } else {
                assert!(run.unwrap().unwrap_err().to_string().contains("injected frame failure"));
            }
            assert_eq!(frame_hardfork(&actual), root);
            assert_eq!(actual.ext().memory_pool_offset(), 0);
            assert!(
                !actual
                    .precompiles()
                    .contains(&alloy_monad_evm2::reserve_balance::RESERVE_BALANCE_ADDRESS)
            );
            assert!(actual.transact(&tx()).unwrap().discard().is_success());
        }
    }
}

#[test]
fn installed_inspectors_and_detached_system_state_keep_frame_selection_scoped() {
    let mut parent = vec![];
    call(&mut parent, op::CALL, CHILD);
    finish_parent(&mut parent);
    let child = Bytes::from_static(&[
        op::PUSH1,
        1,
        op::CLZ,
        op::PUSH0,
        op::MSTORE,
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ]);
    let accounts = [(ROOT, parent.into()), (CHILD, child)];
    for system in [false, true] {
        let root = MonadHardfork::MonadEight;
        let mut actual = evm(root, &accounts, None);
        actual.set_inspector(Selector::new(root, MonadHardfork::MonadTen));
        let pending = if system {
            actual.system_call(SystemTx::new(ROOT, Bytes::new())).unwrap()
        } else {
            actual.transact(&tx()).unwrap()
        };
        assert_eq!(U256::from_be_slice(&pending.result().output[..32]), U256::from(255));
        let detached = pending.detach();
        assert!(detached.result.is_success());
        assert_eq!(frame_hardfork(&actual), root);
        assert!(actual.clear_inspector().is_some());
        let next = actual.transact(&tx()).unwrap().discard();
        assert_eq!(U256::from_be_slice(&next.output[32..64]), U256::ZERO);
    }
}

struct OverrideChild;

impl Inspector<MonadEvmTypes> for OverrideChild {
    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        message: &mut Message<MonadEvmTypes>,
    ) -> Option<MessageResult<MonadEvmTypes>> {
        assert_eq!(frame_hardfork(interp.host()), MonadHardfork::MonadEight);
        if message.depth == 1 {
            message.ext.hardfork = Some(MonadHardfork::MonadTen);
            return Some(MessageResult::<MonadEvmTypes> {
                stop: InstrStop::Stop,
                gas: GasTracker::new(message.gas_limit),
                ..Default::default()
            });
        }
        None
    }
    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MonadEvmTypes>,
        _: &Message<MonadEvmTypes>,
        _: &mut MessageResult<MonadEvmTypes>,
    ) {
        assert_eq!(frame_hardfork(interp.host()), MonadHardfork::MonadEight);
    }
}

#[test]
fn inspector_overrides_do_not_enter_a_frame_or_change_parent_rules() {
    let mut parent = vec![];
    call(&mut parent, op::CALL, CHILD);
    parent.extend([op::POP, op::PUSH1, 1, op::CLZ, op::STOP]);
    let mut actual = evm(
        MonadHardfork::MonadEight,
        &[(ROOT, parent.into()), (CHILD, Bytes::from_static(&[op::INVALID]))],
        None,
    );
    let result = actual.transact_with_inspector(&tx(), &mut OverrideChild).unwrap().commit();
    assert_eq!(result.stop, InstrStop::NotActivated);
    assert_eq!(frame_hardfork(&actual), MonadHardfork::MonadEight);
}

#[test]
fn frame_memory_limits_follow_explicit_runtime_configuration_updates() {
    let mut parent = vec![];
    call(&mut parent, op::CALL, CHILD);
    parent.extend([op::PUSH0, op::MSTORE, op::PUSH1, 32, op::PUSH0, op::RETURN]);
    let mut child = vec![];
    grow(&mut child, 64);
    child.push(op::STOP);
    let accounts = [(ROOT, parent.into()), (CHILD, child.into())];
    for root in MonadHardfork::ALL {
        let mut actual = evm(root, &accounts, None);
        let mut version = *actual.version();
        version.memory_limit = 64;
        actual.set_block_and_execution_config_within_spec(
            MonadBlockEnv::default(),
            ExecutionConfig::for_spec_and_version(root, version),
        );
        let mut selector = Selector::new(root, MonadHardfork::MonadEight);
        let result = actual.transact_with_inspector(&tx(), &mut selector).unwrap().discard();
        assert!(result.is_success());
        assert_eq!(selector.ends[0].2.is_success(), !cfg!(feature = "memory_limit"));
    }
}

#[test]
fn relocated_precompiles_retain_activation_and_failed_moves_are_atomic() {
    let original = alloy_monad_evm2::reserve_balance::RESERVE_BALANCE_ADDRESS;
    let relocated = Address::repeat_byte(0x55);
    let mut parent = vec![];
    call(&mut parent, op::CALL, relocated);
    finish_parent(&mut parent);
    let accounts = [(ROOT, parent.into())];
    for child in MonadHardfork::ALL {
        let root = MonadHardfork::MonadTen;
        let mut actual = evm(root, &accounts, None);
        actual.precompiles_mut().move_precompiles(&[(original, relocated)]).unwrap();
        let mut selector = Selector::new(root, child);
        assert!(
            actual.transact_with_inspector(&tx(), &mut selector).unwrap().discard().is_success()
        );
        assert_eq!(selector.ends[0].2.is_success(), child == MonadHardfork::MonadEight);
        assert!(actual.precompiles().contains(&relocated));
        assert!(!actual.precompiles().contains(&original));
        assert!(
            actual
                .precompiles_mut()
                .move_precompiles(&[(relocated, Address::ZERO), (Address::repeat_byte(0x77), ROOT)])
                .is_err()
        );
        assert!(actual.precompiles().contains(&relocated));
        actual.reset_precompiles();
        assert!(actual.precompiles().contains(&original));
        assert!(!actual.precompiles().contains(&relocated));
    }
}

#[test]
fn copied_extension_contexts_cannot_rebind_another_evms_precompiles() {
    let target = alloy_monad_evm2::reserve_balance::RESERVE_BALANCE_ADDRESS;
    let mut parent = vec![];
    call(&mut parent, op::CALL, target);
    finish_parent(&mut parent);
    let accounts = [(ROOT, parent.into())];
    let source = evm(MonadHardfork::MonadEight, &accounts, None);
    let mut destination = evm(MonadHardfork::MonadTen, &accounts, None);
    *destination.ext_mut() = source.ext().clone();
    let mut selector = Selector::new(MonadHardfork::MonadTen, MonadHardfork::MonadEight);
    let result = destination.transact_with_inspector(&tx(), &mut selector).unwrap().discard();
    assert!(result.is_success());
    assert!(selector.ends[0].2.is_success());
    assert!(!source.precompiles().contains(&target));
    assert!(destination.precompiles().contains(&target));
}

#[test]
fn precompile_warmth_follows_frame_activation_in_both_directions() {
    let target = alloy_monad_evm2::reserve_balance::RESERVE_BALANCE_ADDRESS;
    let mut parent = vec![];
    call(&mut parent, op::CALL, CHILD);
    parent.extend([op::PUSH20]);
    parent.extend(target.as_slice());
    parent.extend([op::BALANCE, op::POP]);
    finish_parent(&mut parent);
    for stop in [op::RETURN, op::REVERT, op::INVALID] {
        for access_in_child in [false, true] {
            let mut child = vec![];
            if access_in_child {
                child.push(op::PUSH20);
                child.extend(target.as_slice());
                child.extend([op::BALANCE, op::POP]);
            }
            return_gas(&mut child, stop);
            let accounts = [(ROOT, Bytes::copy_from_slice(&parent)), (CHILD, child.into())];
            for (root, child) in [
                (MonadHardfork::MonadEight, MonadHardfork::MonadNine),
                (MonadHardfork::MonadNine, MonadHardfork::MonadEight),
            ] {
                let _ = compare(root, child, &accounts);
            }
        }
    }
}

#[test]
fn frame_changes_preserve_beneficiary_and_access_list_warmth() {
    let target = alloy_monad_evm2::reserve_balance::RESERVE_BALANCE_ADDRESS;
    for child_address in [CHILD, target] {
        let mut parent = vec![];
        call(&mut parent, op::CALL, child_address);
        finish_parent(&mut parent);
        let mut child = vec![op::PUSH20];
        child.extend(target.as_slice());
        child.extend([op::BALANCE, op::POP, op::PUSH0, op::SLOAD, op::POP]);
        return_gas(&mut child, op::RETURN);
        let accounts = [(ROOT, parent.into()), (child_address, child.into())];
        for beneficiary in [Address::ZERO, target] {
            for access_list in
                [vec![], vec![AccessListItem { address: target, storage_keys: vec![B256::ZERO] }]]
            {
                let (result, _) = compare_with_warmth(
                    MonadHardfork::MonadNine,
                    MonadHardfork::MonadEight,
                    None,
                    &accounts,
                    beneficiary,
                    access_list,
                );
                assert!(result.is_success());
                assert_eq!(U256::from_be_slice(&result.output[32..64]), U256::ONE);
            }
        }
    }
}
