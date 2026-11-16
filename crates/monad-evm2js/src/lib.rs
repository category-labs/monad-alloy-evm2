//! JavaScript-accessible execution, state and inspection for a local Monad VM.

mod database;
mod staking;
mod trace;
mod transaction;

use alloy_consensus::{TxEip1559, TxEip2930, TxEip7702, TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex};
use database::LocalDB;
use evm2::{
    bytecode::Bytecode,
    ethereum::{LazyTxEip7702, TxEnvelope},
    evm::AccountInfo,
};
use monad_evm2::{MonadBlockEnv, MonadEvmBuilder, MonadEvmTypes, MonadHardfork};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, str::FromStr};
use trace::Trace;
use transaction::{AccessEntry, AuthorizationEntry, ChainContext};
use wasm_bindgen::prelude::*;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Transaction {
    #[serde(default, rename = "type")]
    tx_type: Option<u8>,
    #[serde(default)]
    access_list: Vec<AccessEntry>,
    #[serde(default)]
    authorization_list: Vec<AuthorizationEntry>,
    #[serde(default)]
    chain_context: ChainContext,
    from: String,
    to: Option<String>,
    data: String,
    value: String,
    gas: u64,
    gas_price: String,
    #[serde(default)]
    priority_fee: Option<String>,
    nonce: u64,
    block_number: u64,
    timestamp: u64,
    block_gas_limit: u64,
    base_fee: String,
    beneficiary: String,
    prevrandao: String,
    commit: bool,
    trace: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    balance: String,
    nonce: u64,
    code: String,
    storage: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    version: u32,
    spec: String,
    chain_id: u64,
    accounts: BTreeMap<String, Account>,
    block_hashes: BTreeMap<String, String>,
}

/// Owns local state; simulation and tracing do not mutate that state.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Engine {
    db: LocalDB,
    spec: MonadHardfork,
    chain_id: u64,
}

fn address(value: &str) -> Result<Address, String> {
    Address::from_str(value).map_err(|e| format!("Invalid address: {e}"))
}

fn word(value: &str) -> Result<U256, String> {
    U256::from_str(value).map_err(|e| format!("Invalid 256-bit quantity: {e}"))
}

#[wasm_bindgen]
impl Engine {
    /// Create an empty local VM with an explicit specification and chain ID.
    #[wasm_bindgen(constructor)]
    pub fn new(spec: &str, chain_id: u32) -> Result<Self, String> {
        let spec = match spec {
            "monad-eight" => MonadHardfork::MonadEight,
            "monad-nine" => MonadHardfork::MonadNine,
            "monad-ten" => MonadHardfork::MonadTen,
            "monad-next" => MonadHardfork::MonadNext,
            _ => {
                return Err(
                    "Supported specifications: monad-eight, monad-nine, monad-ten, monad-next"
                        .into(),
                );
            }
        };
        Ok(Self { db: LocalDB::new(database::EmptyLocalDB), spec, chain_id: chain_id.into() })
    }

    /// Set a development account's balance without replacing its code or nonce.
    pub fn fund(&mut self, account: &str, balance: &str) -> Result<(), String> {
        let account = address(account)?;
        let balance = word(balance)?;
        let mut info = self.db.account_info(&account).cloned().unwrap_or_default();
        info.balance = balance;
        self.db.insert_account_info(&account, info);
        Ok(())
    }

    /// Read an account as JSON, including its bytecode and nonzero storage slots.
    pub fn account(&self, account: &str) -> Result<String, String> {
        serde_json::to_string(&self.read_account(address(account)?)).map_err(|e| e.to_string())
    }

    /// Supply a historical hash for the BLOCKHASH instruction.
    /// Unsupplied hashes return zero, including after restoring a snapshot.
    pub fn set_block_hash(&mut self, number: u32, hash: &str) -> Result<(), String> {
        let hash = B256::from_str(hash).map_err(|e| e.to_string())?;
        self.db.cache.block_hashes.insert(U256::from(number), hash);
        Ok(())
    }

    /// Apply or simulate one local staking system transition without a user transaction.
    /// Failed transitions, including reward minting, never change the database.
    pub fn staking_transition(&mut self, request: &str) -> Result<String, String> {
        staking::execute(self, request)
    }

    /// Execute against an isolated cache, applying state only for committed transactions.
    pub fn execute(&mut self, request: &str) -> Result<String, String> {
        let request: Transaction = serde_json::from_str(request).map_err(|e| e.to_string())?;
        let gas_price =
            word(&request.gas_price)?.try_into().map_err(|_| "Gas price exceeds u128")?;
        let priority_fee = request
            .priority_fee
            .as_deref()
            .map(word)
            .transpose()?
            .map(u128::try_from)
            .transpose()
            .map_err(|_| "Priority fee exceeds u128")?;
        let tx_type = request.tx_type.unwrap_or_else(|| {
            if !request.authorization_list.is_empty() {
                4
            } else if priority_fee.is_some() {
                2
            } else if !request.access_list.is_empty() {
                1
            } else {
                0
            }
        });
        if !matches!(tx_type, 0 | 1 | 2 | 4) {
            return Err("Unsupported transaction type; expected 0, 1, 2 or 4".into());
        }
        if (tx_type == 0 && !request.access_list.is_empty())
            || (tx_type != 4 && !request.authorization_list.is_empty())
            || (matches!(tx_type, 0 | 1) && priority_fee.is_some())
        {
            return Err("Transaction fields do not match its type".into());
        }
        if matches!(tx_type, 2 | 4) && priority_fee.is_none() {
            return Err("Invalid transaction fields: missing gas priority fee for EIP-1559".into());
        }
        if tx_type == 4 && request.authorization_list.is_empty() {
            return Err(
                "Invalid transaction fields: missing authorization list for EIP-7702".into()
            );
        }
        let access_list =
            request.access_list.iter().map(AccessEntry::parse).collect::<Result<Vec<_>, _>>()?;
        let authorizations = request
            .authorization_list
            .iter()
            .map(AuthorizationEntry::parse)
            .collect::<Result<Vec<_>, _>>()?;
        // Consensus participants include recoverable authorities even when an
        // authorization is later skipped for its chain ID or nonce.
        let authorities: Vec<_> =
            authorizations.iter().filter_map(|auth| auth.recover_authority().ok()).collect();
        let chain = request.chain_context.parse(address(&request.from)?, authorities.clone())?;
        let to =
            request.to.as_deref().map(address).transpose()?.map_or(TxKind::Create, TxKind::Call);
        let value = word(&request.value)?;
        let input = Bytes::from_str(&request.data).map_err(|e| e.to_string())?;
        let tx = match tx_type {
            0 => TxEnvelope::Legacy(TxLegacy {
                chain_id: Some(self.chain_id),
                nonce: request.nonce,
                gas_limit: request.gas,
                gas_price,
                to,
                value,
                input,
            }),
            1 => TxEnvelope::Eip2930(TxEip2930 {
                chain_id: self.chain_id,
                nonce: request.nonce,
                gas_limit: request.gas,
                gas_price,
                to,
                value,
                input,
                access_list: access_list.into(),
            }),
            2 => TxEnvelope::Eip1559(TxEip1559 {
                chain_id: self.chain_id,
                nonce: request.nonce,
                gas_limit: request.gas,
                max_fee_per_gas: gas_price,
                max_priority_fee_per_gas: priority_fee.unwrap_or(gas_price),
                to,
                value,
                input,
                access_list: access_list.into(),
            }),
            4 => TxEnvelope::Eip7702(LazyTxEip7702::from(TxEip7702 {
                chain_id: self.chain_id,
                nonce: request.nonce,
                gas_limit: request.gas,
                max_fee_per_gas: gas_price,
                max_priority_fee_per_gas: priority_fee.unwrap_or(gas_price),
                to: to.to().copied().ok_or(
                    "Invalid transaction fields: derive tx type error: missing target for EIP-7702",
                )?,
                value,
                input,
                access_list: access_list.into(),
                authorization_list: authorizations,
            })),
            _ => unreachable!("transaction type was validated above"),
        };
        let basefee: u64 =
            word(&request.base_fee)?.try_into().map_err(|_| "Base fee exceeds u64")?;
        let block = MonadBlockEnv {
            number: U256::from(request.block_number),
            timestamp: U256::from(request.timestamp),
            gas_limit: U256::from(request.block_gas_limit),
            basefee: U256::from(basefee),
            beneficiary: address(&request.beneficiary)?,
            prevrandao: U256::from_be_bytes(
                B256::from_str(&request.prevrandao).map_err(|e| e.to_string())?.0,
            ),
            ..Default::default()
        };
        let mut trace = Trace::new(self.spec);
        let mut evm =
            MonadEvmBuilder::new(self.chain_id).create_evm(self.spec, block, self.db.clone());
        evm.ext_mut().chain = chain;
        let tx = Recovered::new_unchecked(tx, address(&request.from)?);
        let output = if request.trace {
            evm.transact_with_inspector(&tx, &mut trace)
        } else {
            evm.transact(&tx)
        }
        .map_err(|e| format!("Transaction validation failed: {e:?}"))?
        .detach();
        let result = &output.result;
        let error = execution_error(result);
        let logs: Vec<Value> = result.logs.iter().map(|log| json!({
            "address": format!("{:#x}", log.address),
            "topics": log.data.topics().iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>(),
            "data": hex::encode_prefixed(&log.data.data)
        })).collect();
        let response = json!({
            "success": result.is_success(), "error": error,
            "returnValue": hex::encode_prefixed(&result.output),
            "createdAddress": result.created_address.map(|a| format!("{a:#x}")),
            "gasUsed": result.ext.charged_gas, "gasSpent": result.total_gas_spent,
            "logs": logs, "structLogs": trace.steps, "preimages": trace.preimages,
            "codeByAddress": trace.code_by_address,
            "consoleCalls": trace.console_calls,
            "authorizationAuthorities": authorities.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>(),
            "traceTruncated": trace.truncated
        });
        if request.commit {
            self.db.commit_pending(&output.pending_state);
        }
        serde_json::to_string(&response).map_err(|e| e.to_string())
    }

    /// Export versioned JSON state, independent of any browser persistence mechanism.
    pub fn snapshot(&self) -> Result<String, String> {
        let accounts = self
            .db
            .cache
            .accounts
            .iter()
            .filter(|(_, account)| account.is_some())
            .map(|(addr, _)| (format!("{addr:#x}"), self.read_account(*addr)))
            .collect();
        let block_hashes = self
            .db
            .cache
            .block_hashes
            .iter()
            .map(|(number, hash)| (format!("{number:#x}"), format!("{hash:#x}")))
            .collect();
        serde_json::to_string(&Snapshot {
            version: 1,
            spec: self.spec.to_string(),
            chain_id: self.chain_id,
            accounts,
            block_hashes,
        })
        .map_err(|e| e.to_string())
    }

    /// Validate completely before replacing the current database.
    pub fn restore(&mut self, snapshot: &str) -> Result<(), String> {
        let snapshot: Snapshot = serde_json::from_str(snapshot).map_err(|e| e.to_string())?;
        if snapshot.version != 1
            || snapshot.spec != self.spec.to_string()
            || snapshot.chain_id != self.chain_id
        {
            return Err("Incompatible Monad state version, specification or chain ID".into());
        }
        let mut db = LocalDB::new(database::EmptyLocalDB);
        for (addr, account) in snapshot.accounts {
            let addr = address(&addr)?;
            let code = Bytes::from_str(&account.code).map_err(|e| e.to_string())?;
            let code = Bytecode::new_raw_checked(code)
                .map_err(|e| format!("Invalid bytecode for {addr:#x}: {e}"))?;
            db.insert_account_info(
                &addr,
                AccountInfo {
                    balance: word(&account.balance)?,
                    nonce: account.nonce,
                    ..Default::default()
                }
                .with_code(code),
            );
            for (slot, value) in account.storage {
                db.insert_account_storage(&addr, &word(&slot)?, &word(&value)?);
            }
        }
        for (number, hash) in snapshot.block_hashes {
            db.cache
                .block_hashes
                .insert(word(&number)?, B256::from_str(&hash).map_err(|e| e.to_string())?);
        }
        self.db = db;
        Ok(())
    }
}

impl Engine {
    fn read_account(&self, address: Address) -> Account {
        let Some(account) = self.db.account_info(&address) else {
            return Account {
                balance: "0x0".into(),
                nonce: 0,
                code: "0x".into(),
                storage: BTreeMap::new(),
            };
        };
        let code = self
            .db
            .cache
            .contracts
            .get(&account.code_hash)
            .map(|c| hex::encode_prefixed(c.original_bytes()))
            .unwrap_or_else(|| "0x".into());
        Account {
            balance: format!("{:#x}", account.balance),
            nonce: account.nonce,
            code,
            storage: self
                .db
                .cache
                .storage
                .get(&address)
                .into_iter()
                .flat_map(|storage| storage.slots.iter())
                .filter(|(_, v)| !v.is_zero())
                .map(|(k, v)| (format!("0x{k:064x}"), format!("0x{v:064x}")))
                .collect(),
        }
    }
}

fn execution_error(result: &evm2::TxResult<MonadEvmTypes>) -> Option<String> {
    if let Some(reason) = result.ext.halt_reason {
        Some(format!("{reason:?}"))
    } else if result.is_success() {
        None
    } else if result.is_revert() {
        Some("revert".into())
    } else {
        Some(trace::halt_error(result.stop))
    }
}
