//! Complete state snapshots and execution commitments.

use crate::fixture::{Account, Allocation};
use alloy_primitives::{B256, keccak256};
use alloy_trie::{
    TrieAccount,
    root::{state_root_unhashed, storage_root_unhashed},
};
use evm2::{bytecode::Bytecode, evm::InMemoryDB};
use eyre::{Result, ensure, eyre};
use revm::{database::InMemoryDB as ReferenceDB, state::Bytecode as ReferenceBytecode};
use std::collections::BTreeSet;

pub(crate) fn snapshot(db: &InMemoryDB) -> Allocation {
    db.cache
        .accounts
        .iter()
        .filter_map(|(address, info)| {
            let info = info.as_ref()?;
            let code = db
                .cache
                .contracts
                .get(&info.code_hash)
                .map(Bytecode::original_bytes)
                .unwrap_or_default();
            let storage = db
                .cache
                .storage
                .get(address)
                .map(|storage| {
                    storage
                        .slots
                        .iter()
                        .filter(|(_, value)| !value.is_zero())
                        .map(|(key, value)| (*key, *value))
                        .collect()
                })
                .unwrap_or_default();
            Some((*address, Account { nonce: info.nonce, balance: info.balance, code, storage }))
        })
        .collect()
}

pub(crate) fn reference_snapshot(db: &ReferenceDB) -> Allocation {
    db.cache
        .accounts
        .iter()
        .filter_map(|(address, account)| {
            let info = account.info()?;
            let code = db
                .cache
                .contracts
                .get(&info.code_hash)
                .map(ReferenceBytecode::original_bytes)
                .unwrap_or_default();
            let storage = account
                .storage
                .iter()
                .filter(|(_, value)| !value.is_zero())
                .map(|(k, v)| (*k, *v))
                .collect();
            Some((*address, Account { nonce: info.nonce, balance: info.balance, code, storage }))
        })
        .collect()
}

pub(crate) fn root(accounts: &Allocation) -> B256 {
    state_root_unhashed(accounts.iter().map(|(address, account)| {
        let storage_root = storage_root_unhashed(
            account
                .storage
                .iter()
                .map(|(key, value)| (B256::from(key.to_be_bytes::<32>()), *value)),
        );
        (
            *address,
            TrieAccount {
                nonce: account.nonce,
                balance: account.balance,
                code_hash: keccak256(&account.code),
                storage_root,
            },
        )
    }))
}

pub(crate) fn compare(actual: &Allocation, expected: &Allocation) -> Result<()> {
    let addresses = actual.keys().chain(expected.keys()).collect::<BTreeSet<_>>();
    for address in addresses {
        match (actual.get(address), expected.get(address)) {
            (Some(actual), Some(expected)) => {
                ensure!(
                    actual.nonce == expected.nonce,
                    "account {address}: nonce expected {}, got {}",
                    expected.nonce,
                    actual.nonce
                );
                ensure!(
                    actual.balance == expected.balance,
                    "account {address}: balance expected {}, got {}",
                    expected.balance,
                    actual.balance
                );
                ensure!(actual.code == expected.code, "account {address}: code mismatch");
                let slots =
                    actual.storage.keys().chain(expected.storage.keys()).collect::<BTreeSet<_>>();
                for slot in slots {
                    let actual = actual.storage.get(slot).copied().unwrap_or_default();
                    let expected = expected.storage.get(slot).copied().unwrap_or_default();
                    ensure!(
                        actual == expected,
                        "account {address}, storage {slot:#x}: expected {expected:#x}, got {actual:#x}"
                    );
                }
            }
            (Some(_), None) => return Err(eyre!("unexpected account {address}")),
            (None, Some(_)) => return Err(eyre!("missing account {address}")),
            (None, None) => unreachable!(),
        }
    }
    Ok(())
}
