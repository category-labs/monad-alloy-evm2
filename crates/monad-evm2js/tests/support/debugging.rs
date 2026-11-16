use super::Engine;
use serde_json::{Value, json};

const SENDER: &str = "0x1111111111111111111111111111111111111111";
const ROOT: &str = "0x2222222222222222222222222222222222222222";
const CHILD: &str = "0x3333333333333333333333333333333333333333";
const LEAF: &str = "0x4444444444444444444444444444444444444444";
const RETURN_STATUS: &str = "5f5260205ff3";

fn call(target: &str, opcode: &str) -> String {
    // Empty input/output, no value, a bounded allowance for the child.
    let value = if matches!(opcode, "f1" | "f2") { "5f" } else { "" };
    format!("5f5f5f5f{value}73{}6207a120{opcode}", &target[2..])
}

fn factory(create2: bool, reverting: bool) -> String {
    let init = if reverting {
        "602a5f555f5ffd"
    } else {
        "602a5f556008601060003960086000f3602a5f5260205ff3"
    };
    // CODECOPY the initcode appended after the factory's return instruction.
    let tail = format!(
        "{}60{:02x}5f5f{}{}",
        if create2 { "6001" } else { "" },
        init.len() / 2,
        if create2 { "f5" } else { "f0" },
        RETURN_STATUS
    );
    let offset = 6 + tail.len() / 2;
    format!("60{:02x}60{offset:02x}5f39{tail}{init}", init.len() / 2)
}

pub(crate) fn report() -> Value {
    let mut report = serde_json::Map::new();
    for fork in ["monad-eight", "monad-nine", "monad-ten", "monad-next"] {
        let leaf_success = "602a5f55602a5f5260205fa060205ff3";
        let middle = format!("{}{}", call(LEAF, "f1"), RETURN_STATUS);
        let parent = format!("{}{}", call(CHILD, "f1"), RETURN_STATUS);
        let mut cases: Vec<(&str, String, String, String, bool, u64)> = vec![
            ("nested-success", parent.clone(), middle.clone(), leaf_success.into(), true, 3),
            ("nested-revert", parent.clone(), middle.clone(), "602a5f555f5ffd".into(), true, 3),
            ("nested-invalid", parent.clone(), middle.clone(), "fe".into(), true, 3),
            (
                "nested-out-of-gas",
                parent.clone(),
                middle.replace("6207a120", "6103e8"),
                "5b5f56".into(),
                true,
                3,
            ),
            (
                "root-revert",
                format!("{}5f5ffd", call(CHILD, "f1")),
                leaf_success.into(),
                "00".into(),
                false,
                2,
            ),
            (
                "static-write",
                format!("{}{}", call(CHILD, "fa"), RETURN_STATUS),
                "602a5f5500".into(),
                "00".into(),
                true,
                2,
            ),
            (
                "delegatecall",
                format!("{}{}", call(CHILD, "f4"), RETURN_STATUS),
                leaf_success.into(),
                "00".into(),
                true,
                2,
            ),
            (
                "callcode",
                format!("{}{}", call(CHILD, "f2"), RETURN_STATUS),
                leaf_success.into(),
                "00".into(),
                true,
                2,
            ),
            ("create", factory(false, false), "00".into(), "00".into(), true, 2),
            ("create2", factory(true, false), "00".into(), "00".into(), true, 2),
            ("create-revert", factory(false, true), "00".into(), "00".into(), true, 2),
            ("create2-revert", factory(true, true), "00".into(), "00".into(), true, 2),
        ];
        if fork == "monad-next" {
            cases.push((
                "nested-extensions",
                parent,
                format!("{}{}", call(LEAF, "f1"), RETURN_STATUS),
                "ae005f526001ae0160205260405ff3".into(),
                true,
                3,
            ));
        }
        for (name, root, child, leaf, success, depth) in cases {
            let label = format!("{fork}/{name}");
            let mut vm = Engine::new(fork, 1337).unwrap();
            let mut initial: Value = serde_json::from_str(&vm.snapshot().unwrap()).unwrap();
            for (address, code) in [(SENDER, ""), (ROOT, &root), (CHILD, &child), (LEAF, &leaf)] {
                initial["accounts"][address] = json!({ "balance": if address == SENDER { "0x3635c9adc5dea00000" } else { "0x0" },
                    "nonce": if address == SENDER { 0 } else { 1 }, "code": format!("0x{code}"), "storage": {} });
            }
            vm.restore(&initial.to_string()).unwrap();
            let mut request = json!({
                "from": SENDER, "to": ROOT, "data": "0x", "value": "0x0", "gas": 1_000_000,
                "gasPrice": "0x1", "nonce": 0, "blockNumber": 1, "timestamp": 1800000000,
                "blockGasLimit": 30000000, "baseFee": "0x0", "beneficiary": format!("0x{:040x}", 0),
                "prevrandao": format!("0x{:064x}", 0), "commit": false, "trace": true
            });
            let before = vm.snapshot().unwrap();
            let result: Value =
                serde_json::from_str(&vm.execute(&request.to_string()).unwrap()).unwrap();
            assert_eq!(result["success"], success, "{label}: status");
            assert_eq!(before, vm.snapshot().unwrap(), "{label}: simulation");
            let steps = result["structLogs"].as_array().unwrap();
            assert_eq!(
                steps.iter().filter_map(|s| s["depth"].as_u64()).max(),
                Some(depth),
                "{label}: depth"
            );
            assert!(!result["traceTruncated"].as_bool().unwrap(), "{label}");
            let entry = steps.iter().position(|s| s["depth"] == 2).unwrap();
            let resume = steps.iter().rposition(|s| s["depth"] == 1).unwrap();
            assert!(entry < resume, "{label}: resume parent frame");
            assert!(steps[entry - 1]["gasCost"].as_u64().unwrap() > 0, "{label}: parent call gas");
            if name == "nested-extensions" {
                let extension = steps.iter().find(|s| s["op"] == "CALLSTACKDEPTH").unwrap();
                assert_eq!(extension["depth"], 3);
                assert_eq!(extension["gasCost"], 2);
                assert!(steps.iter().any(|s| s["pc"] == 2
                    && s["depth"] == 3
                    && s["stack"][0] == format!("{:064x}", 2)));
            }
            request["trace"] = json!(false);
            let plain: Value =
                serde_json::from_str(&vm.execute(&request.to_string()).unwrap()).unwrap();
            for field in ["success", "error", "returnValue", "gasSpent", "gasUsed", "logs"] {
                assert_eq!(plain[field], result[field], "{label}: tracing changed {field}");
            }
            request["trace"] = json!(true);
            request["commit"] = json!(true);
            let committed: Value =
                serde_json::from_str(&vm.execute(&request.to_string()).unwrap()).unwrap();
            assert_eq!(committed, result, "{label}: commit");
            let state: Value = serde_json::from_str(&vm.snapshot().unwrap()).unwrap();
            if matches!(
                name,
                "nested-revert"
                    | "nested-invalid"
                    | "nested-out-of-gas"
                    | "root-revert"
                    | "static-write"
                    | "create-revert"
                    | "create2-revert"
            ) {
                assert_eq!(result["logs"], json!([]), "{label}: reverted logs");
                for address in [ROOT, CHILD, LEAF] {
                    assert_eq!(
                        state["accounts"][address]["storage"],
                        json!({}),
                        "{label}: reverted storage"
                    );
                }
            }
            let mut followup = Value::Null;
            if matches!(name, "create" | "create2") {
                let created = format!("0x{}", &result["returnValue"].as_str().unwrap()[26..]);
                assert_eq!(
                    state["accounts"][&created]["code"], "0x602a5f5260205ff3",
                    "{label}: child runtime"
                );
                request["to"] = json!(created);
                request["nonce"] = json!(1);
                request["commit"] = json!(false);
                let output: Value =
                    serde_json::from_str(&vm.execute(&request.to_string()).unwrap()).unwrap();
                assert_eq!(
                    output["returnValue"],
                    format!("0x{:064x}", 42),
                    "{label}: child execution"
                );
                assert_eq!(output["structLogs"][0]["pc"], 0);
                followup = json!({ "request": request, "result": output });
            }
            request["to"] = json!(ROOT);
            request["nonce"] = json!(0);
            request["commit"] = json!(true);
            report.insert(label, json!({ "initial": initial, "request": request, "result": result, "state": state, "followup": followup }));
        }
    }
    Value::Object(report)
}
