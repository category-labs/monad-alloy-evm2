# monad-evm2

Monad execution on [evm2](https://github.com/alloy-rs/evm2), including transaction
fees, memory and storage pricing, precompiles, staking, reserve balances, and
inspectors. Import the library as `monad_evm2`.

Supports MonadEight, MonadNine, and MonadTen. `MonadNext` retains MonadTen's rules;
see [hardfork definitions](src/spec.rs) for activation schedules and EIP selections.

## Runtime usage

Execute a local transfer with an explicitly supplied sender:

```rust
use alloy_consensus::{TxLegacy, transaction::Recovered};
use alloy_primitives::{Address, U256};
use evm2::evm::{AccountInfo, InMemoryDB};
use monad_evm2::{
    MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvmBuilder, MonadHardfork, MonadTxEnvelope,
};

let sender = Address::repeat_byte(0x11);
let mut db = InMemoryDB::default();
db.insert_account_info(&sender, AccountInfo::default().with_balance(U256::from(1_000_000)));

let mut evm = MonadEvmBuilder::new(MONAD_MAINNET_CHAIN_ID).create_evm(
    MonadHardfork::MonadTen,
    MonadBlockEnv::default(),
    db,
);
let tx = Recovered::new_unchecked(
    MonadTxEnvelope::Legacy(TxLegacy {
        chain_id: Some(MONAD_MAINNET_CHAIN_ID),
        to: Address::repeat_byte(0x22).into(),
        value: U256::from(1),
        gas_limit: 100_000,
        gas_price: 1,
        ..Default::default()
    }),
    sender,
);

let result = evm.transact(&tx).unwrap().commit();
assert!(result.is_success());
assert_eq!(result.ext.charged_gas, 100_000);
assert_eq!(result.total_gas_spent, 21_000);
```

`transact()` returns pending state: `commit()` accepts it; `discard()` or dropping
the handle rolls it back. Supply block history through `evm.ext_mut().chain` and
runtime overrides through the builder's `limits`. Use `transact_with_inspector()`
to inspect one transaction or `set_inspector()` to install an inspector.

Use `result.ext.charged_gas` for Monad receipts and block gas accounting. Ordinary
transactions charge their full gas limit; system calls charge zero. Fee settlement
uses this charge even when execution finishes early or reverts.

`total_gas_spent` retains execution consumption including intrinsic gas.
`floor_gas` is the EIP-7623 calldata floor, and `tx_gas_used()` applies that floor
to consumption for simulation and estimation. These evm2 fields do not represent
Monad's full-limit receipt charge.

## Execution modes

| Entry point | Reserve rejection | Lifecycle ownership |
| --- | --- | --- |
| `MonadEvmBuilder` | Enabled by default. | Standalone wrapper composes inspectors and manages synthetic-call context. |
| `MonadEvmFactory` | Disabled by default; tracking and queries remain available. | Embedder owns inspectors, snapshots, and state resolution. |

To enforce reserves on a native host, set
`host.ext_mut().revert_on_reserve_balance_violation = true` and pass a
`reserve_balance::reserve_inspector()` adapter to `transact_with_inspector()`.
Preserved synthetic calls defer rejection to their enclosing transaction.
Reserve rejection rolls back execution while retaining fees, nonce updates, and
authorizations. The result identifies it as `MonadHaltReason::ReserveBalanceViolation`.

`MonadEvmBuilder::create_host()` exposes the standalone `(host, execution)` pair.
The [execution API](src/execution.rs) documents inspector composition, per-frame
hardfork selection, and synthetic state handling. For fork switches, use the
[reserve rebase helpers](src/reserve_balance/sync.rs) to retain original balances.
[System calls](src/system.rs) and [trusted protocol replay](src/protocol.rs)
provide staking lifecycle execution.

Validator registration retains monad-revm 0.8.0's unverified signatures and
simplified public-key identifiers.

## Cargo features

| Feature | Purpose | Default |
| --- | --- | --- |
| `std` | Standard-library support. | Yes |
| `serde` | Serialization for local public types; supports `no_std`. | No |
| `c-kzg` | C KZG backend. | Yes |
| `secp256k1` | Native secp256k1 backend. | Yes |
| `blst` | BLST backend. | Yes |
| `portable` | Portable KZG and BLST builds. | Yes |
| `memory_limit` | Compatibility flag; memory limits always apply. | No |
| `compare-monad-revm` | Development tests against monad-revm 0.8.0. | No |

Memory is shared by active frames and capped at 8 MiB from MonadNine onward.
Disable default features for `no_std` builds.

## Local development

Run from the workspace root. [rust-toolchain.toml](../../rust-toolchain.toml)
selects Rust 1.96.0 with rustfmt and Clippy. LLVM is not required.

```sh
cargo test --locked -p monad-evm2
cargo test --locked -p monad-evm2 --features serde,memory_limit
cargo test --locked -p monad-evm2 --no-default-features --features std,serde
cargo clippy --locked -p monad-evm2 --all-targets --features serde,memory_limit -- -D warnings
cargo fmt --all -- --check
cargo doc --locked -p monad-evm2 --no-deps --features serde --open
```

Default tests check protocol behavior, execution gas, receipt charges, and rollback
without compiling monad-revm or revm. Add the optional comparison checks with:

```sh
cargo test --locked -p monad-evm2 --features compare-monad-revm
```

`--all-features` also enables these comparisons. They retain documented differences
from the reference; they do not override protocol expectations. CI runs independent
checks on pushes and pull requests. The manual
[Reference comparisons workflow](../../.github/workflows/reference.yml) runs the
reference audit for both crates.

Check without the standard library:

```sh
rustup target add thumbv7em-none-eabi
cargo check --locked -p monad-evm2 --lib --no-default-features --target thumbv7em-none-eabi
cargo check --locked -p monad-evm2 --lib --no-default-features --features serde --target thumbv7em-none-eabi
```

Licensed under [MIT](LICENSE).
