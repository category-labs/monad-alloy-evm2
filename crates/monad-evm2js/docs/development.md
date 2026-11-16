# Build and tests

Use the repository's Rust 1.96.0 toolchain, Node.js 22 or newer, and matching bindings:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.128 --locked
npm ci --ignore-scripts --prefix crates/monad-evm2js
crates/monad-evm2js/node_modules/.bin/playwright install chromium
cargo test --locked -p monad-evm2js --features monad-evm2/default,compare-monad-revmjs-main
node crates/monad-evm2js/scripts/build.mjs
node crates/monad-evm2js/scripts/test-package.mjs
```

The build writes `target/monad-evm2js/package`. The test packs it and installs the
archive into a temporary project with install scripts disabled. It also type-checks the declarations and runs Chromium page, synchronous-loader and
module-worker checks against the installed package. Nothing is published.
`build-info.json` records the source revision, dirty-state flag, toolchain and lockfile
checksum. Consumers need no Rust toolchain or runtime npm dependencies.

The WASM build uses pure-Rust crypto backends without the runtime's default native
features. Package tests compare it with a native build using secp256k1, c-kzg and blst.
Shared fixtures cover all four forks, precompiles, typed transactions, EIP-7702,
reserve history, staking, simulation, snapshots and debugging. The optional
`compare-monad-revmjs-main` feature also compares the JSON contract with the pinned
reference wrapper; it is excluded from the WASM package. Validation error messages
can name different backend types.

Version-1 snapshots can move between wrappers when the fork and chain ID match.
The JavaScript module name changes to `monad-evm2js`; the Engine interface and exported
TypeScript shapes stay the same. This crate does not change or distribute Remix.

Reference parity preserves the native runtime's behavior in three boundary cases:
empty touched beneficiaries may be omitted from snapshots; inactive CLZ on Monad
Eight is rejected before opcode gas is charged; failed staking system calls consume
the 30M Osaka budget, whereas revm's shared system-call constant includes an
Amsterdam reservoir (31,566,720 total). These differences are asserted explicitly.
All native/WASM comparisons use identical results, traces and state.
