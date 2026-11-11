# Monad EVM2

Monad execution on [evm2](https://github.com/alloy-rs/evm2), with blockchain
fixture replay and execution parity checks.

## Crates

| Package | Purpose |
| --- | --- |
| [`monad-evm2`](crates/monad-evm2/README.md) | Runtime library: Monad hardforks, transaction fees, memory and storage pricing, precompiles, reserve balances, staking, and inspected execution. |
| [`monad-evm2-fixtures`](crates/monad-fixtures/README.md) | CLI for replaying signed blockchain fixtures, verifying execution commitments, and comparing results with monad-revm 0.8.0. |

The root Cargo manifest defines the workspace and shared package settings,
dependency versions, and lint rules. Each crate owns its source, tests, and
documentation.

## Runtime usage

Import the runtime crate as `monad_evm2`:

```rust
use monad_evm2::{MONAD_MAINNET_CHAIN_ID, MonadBlockEnv, MonadEvmBuilder, MonadHardfork};
use evm2::evm::InMemoryDB;

let builder = MonadEvmBuilder::new(MONAD_MAINNET_CHAIN_ID);
let evm = builder.create_evm(
    MonadHardfork::MonadTen,
    MonadBlockEnv::default(),
    InMemoryDB::default(),
);
```

The [runtime documentation](crates/monad-evm2/README.md) describes reserve
enforcement, per-frame hardfork selection, inspectors, protocol replay, system
calls, and optional features.

Applications that own a native evm2 host use the stateless `MonadEvmFactory`.
Native hosts track reserves without automatically rejecting violations; opting
into rejection requires the reserve inspector. `MonadEvmBuilder` enables rejection
by default and supplies the standalone wrapper with per-frame fork selection and
synthetic-call context handling. Both paths share Monad's protocol handlers;
memory caps also apply with default features disabled.

## Fixture replay

```sh
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests --json
```

The [fixture guide](crates/monad-fixtures/README.md) covers supported inputs,
reports, known reference differences, and the pinned CI corpus.

## Development

Requires Rust 1.96 or later. Cargo selects the toolchain through
`rust-toolchain.toml`. LLVM is not required.

Run these commands from the workspace root:

```sh
cargo check --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace
cargo test --locked --workspace --all-features
cargo test --locked -p monad-evm2 --no-default-features --features std,serde
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
cargo doc --locked --workspace --no-deps --all-features
```

The fixture runner requires `std`. Target the runtime package
explicitly when checking feature configurations or `no_std` builds:

```sh
rustup target add thumbv7em-none-eabi
cargo check --locked -p monad-evm2 --lib --no-default-features --target thumbv7em-none-eabi
cargo check --locked -p monad-evm2 --lib --no-default-features --features serde --target thumbv7em-none-eabi
```

## License

MIT. See [LICENSE](LICENSE).
