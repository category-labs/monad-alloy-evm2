# alloy-monad-evm2

Monad execution on [evm2](https://github.com/alloy-rs/evm2).

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
| `memory_limit` | Apply configured memory limits and the 8 MiB cap for MonadNine and later. | Yes |

Disable default features for `no_std` builds. The `serde` feature supports
`no_std` and enables the local Serde dependency without enabling evm2's
engine-state serialization.

## License

MIT. See [LICENSE](LICENSE).
