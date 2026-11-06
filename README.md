# alloy-monad-evm2

Monad execution on [evm2](https://github.com/alloy-rs/evm2).

Ordinary transactions enforce reserve balances automatically. Reserve rejection
rolls back execution while retaining fees, the sender nonce and EIP-7702
authorizations. `result.ext.halt_reason` identifies
`MonadHaltReason::ReserveBalanceViolation`; evm2's native stop code is `Revert`
to preserve unused CALL gas. Inspectors see the rejection after execution state
has been rolled back.

`MonadEvm` manages transaction and inspector lifecycles around `MonadHost`, the
native evm2 host. The current adapter observes root instructions to detect reserve
violations before native rollback, so transactions use evm2's inspected interpreter.
Callers supply block history through `evm.ext_mut().chain`.

Internal-call simulations may set `preserve_reserve_balance_tracker` after
initializing or copying the enclosing transaction's reserve context. These
synthetic calls retain its original balance baselines and defer enforcement to
the enclosing transaction. Discarding a synthetic call restores its context;
committing keeps its tracking changes. The enclosing caller must check reserves
before accepting execution and restore the flag to `false` for ordinary
transactions. `MonadCheckpoint` and `MonadStateSnapshot` pair state with Monad
bookkeeping; `rebase_reserve_balance` rebuilds tracking against replacement fork
state and its chain context.

Validator registration follows monad-revm 0.8.0 compatibility rules: it does not
verify validator signatures and uses simplified public-key lookup identifiers.
This differs from validator admission in the C++ consensus client.

## Development

Requires Rust 1.96 or later. With [rustup](https://rustup.rs/) installed, Cargo
selects the repository's toolchain automatically. LLVM is not required.

```sh
cargo check --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo test --locked --all-features
cargo doc --locked --no-deps --all-features
```

To check `no_std` support:

```sh
rustup target add thumbv7em-none-eabi
cargo check --locked --lib --no-default-features --target thumbv7em-none-eabi
cargo check --locked --lib --no-default-features --features serde --target thumbv7em-none-eabi
```

## Features

| Feature | Purpose | Default |
| --- | --- | --- |
| `std` | Enable the standard library in evm2 and optional serialization support. | Yes |
| `serde` | Enable Serde support. | No |
| `c-kzg` | Enable evm2's C KZG backend. | Yes |
| `secp256k1` | Enable evm2's native secp256k1 backend. | Yes |
| `blst` | Enable evm2's BLST backend. | Yes |
| `portable` | Enable portable builds of the selected KZG and BLST backends. | Yes |
| `memory_limit` | Apply memory limits shared by active call frames, capped at 8 MiB for MonadNine and later. | Yes |

Disable default features for `no_std` builds. The `serde` feature supports
`no_std` and enables the local Serde dependency without enabling evm2's
engine-state serialization.

## License

MIT. See [LICENSE](LICENSE).
