//! Compare the public JSON contract with the pinned monad-revmjs implementation.
#![cfg(feature = "compare-monad-revmjs-main")]

mod current {
    use monad_evm2js::Engine;
    pub(super) mod precompiles {
        include!("support/precompiles.rs");
    }
    pub(super) mod transactions {
        include!("support/transactions.rs");
    }
    pub(super) mod debugging {
        include!("support/debugging.rs");
    }
    pub(super) mod staking {
        include!("support/staking.rs");
    }
}
mod reference {
    use monad_revmjs_reference::Engine;
    pub(super) mod precompiles {
        include!("support/precompiles.rs");
    }
    pub(super) mod transactions {
        include!("support/transactions.rs");
    }
    pub(super) mod debugging {
        include!("support/debugging.rs");
    }
    pub(super) mod staking {
        include!("support/staking.rs");
    }
}

fn compare(actual: &serde_json::Value, expected: &serde_json::Value, path: &str) {
    use serde_json::Value;
    // The native Osaka system-call budget is 30M. revm's shared constant now
    // also includes an Amsterdam state-gas reservoir, consumed on a halt.
    if path.starts_with("staking/") && path.ends_with("/result/gasSpent") && expected == 31_566_720
    {
        assert_eq!(actual, 30_000_000, "{path}");
        return;
    }
    // Both reject CLZ on Eight; evm2 checks activation before charging its gas.
    if path == "transactions/monad-eight/clz-activation/0/result/structLogs/1/gasCost" {
        assert_eq!(actual, 0);
        assert_eq!(expected, 5);
        return;
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            if path.ends_with("/state/accounts") {
                // revm retains a touched empty beneficiary even at zero gas price;
                // evm2 omits it. These entries have no observable account state.
                let populated = |account: &&Value| {
                    account["nonce"] != 0
                        || account["balance"] != "0x0"
                        || account["code"] != "0x"
                        || account["storage"] != serde_json::json!({})
                };
                let a: serde_json::Map<_, _> = a
                    .iter()
                    .filter(|(_, v)| populated(v))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let b: serde_json::Map<_, _> = b
                    .iter()
                    .filter(|(_, v)| populated(v))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                compare(&Value::Object(a), &Value::Object(b), &format!("{path}/populated"));
                return;
            }
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                b.keys().collect::<Vec<_>>(),
                "{path}: fields"
            );
            for (key, value) in a {
                compare(value, &b[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: length");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                compare(a, b, &format!("{path}/{i}"));
            }
        }
        // Validation exception text includes backend types; both fixture runners check
        // the expected error category and that rejection leaves state unchanged.
        (Value::String(a), Value::String(b))
            if a.starts_with("Transaction validation failed:")
                && b.starts_with("Transaction validation failed:") => {}
        _ => assert_eq!(actual, expected, "{path}"),
    }
}

#[test]
fn precompiles_match_reference() {
    compare(&current::precompiles::report(), &reference::precompiles::report(), "precompiles");
}
#[test]
fn transactions_match_reference() {
    compare(&current::transactions::report(), &reference::transactions::report(), "transactions");
}
#[test]
fn staking_matches_reference() {
    compare(&current::staking::report(), &reference::staking::report(), "staking");
}

#[test]
fn debugging_matches_reference() {
    compare(&current::debugging::report(), &reference::debugging::report(), "debugging");
}

#[test]
fn snapshots_resume_in_both_engines() {
    use serde_json::{Value, json};
    let sender = "0x1111111111111111111111111111111111111111";
    for fork in ["monad-eight", "monad-nine", "monad-ten", "monad-next"] {
        let mut old = monad_revmjs_reference::Engine::new(fork, 1337).unwrap();
        let mut new = monad_evm2js::Engine::new(fork, 1337).unwrap();
        old.fund(sender, "0x3635c9adc5dea00000").unwrap();
        old.set_block_hash(1, &format!("0x{}", "ab".repeat(32))).unwrap();
        let mut request = json!({
            "from": sender, "to": null, "data": "0x600c600c600039600c6000f3602a5f555f545f5260205ff3", "value": "0x0",
            "gas": 1_000_000, "gasPrice": "0x1", "nonce": 0, "blockNumber": 2, "timestamp": 1800000000,
            "blockGasLimit": 30000000, "baseFee": "0x0", "beneficiary": format!("0x{:040x}", 0),
            "prevrandao": format!("0x{:064x}", 0), "commit": true, "trace": true
        });
        let deploy: Value =
            serde_json::from_str(&old.execute(&request.to_string()).unwrap()).unwrap();
        assert_eq!(deploy["success"], true);
        new.restore(&old.snapshot().unwrap()).unwrap();
        assert_eq!(
            new.snapshot().unwrap(),
            old.snapshot().unwrap(),
            "{fork}: import reference snapshot"
        );
        request["to"] = deploy["createdAddress"].clone();
        request["data"] = json!("0x");
        request["nonce"] = json!(1);
        let actual: Value =
            serde_json::from_str(&new.execute(&request.to_string()).unwrap()).unwrap();
        let expected: Value =
            serde_json::from_str(&old.execute(&request.to_string()).unwrap()).unwrap();
        compare(&actual, &expected, fork);
        assert_eq!(actual["returnValue"], format!("0x{:064x}", 42));
        old.restore(&new.snapshot().unwrap()).unwrap();
        assert_eq!(
            new.snapshot().unwrap(),
            old.snapshot().unwrap(),
            "{fork}: export reference snapshot"
        );
        request["nonce"] = json!(2);
        request["commit"] = json!(false);
        assert_eq!(
            new.execute(&request.to_string()).unwrap(),
            old.execute(&request.to_string()).unwrap(),
            "{fork}: resume reference engine"
        );
    }
}
