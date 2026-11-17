# Build and tests

Use the repository's Rust 1.96.0 toolchain, Node.js 22 or newer, and matching bindings:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.128 --locked
npm ci --ignore-scripts --prefix crates/monad-evm2js
crates/monad-evm2js/node_modules/.bin/playwright install chromium
cargo test --locked -p monad-evm2js --features monad-evm2/default
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
reserve history, staking, simulation, snapshots and debugging. Native/WASM
comparisons check complete results, traces and state. Protocol expectations remain
explicit in the fixtures and Rust tests.

Version-1 snapshots can be restored when the fork and chain ID match. The built
package exports the Engine interface and TypeScript request/result types. This
crate does not change or distribute Remix.

On Monad Eight, inactive CLZ fails before its instruction gas is charged. Local
staking helpers use a 30M execution allowance, consumed in full by failed calls;
this is a library allowance, separate from canonical protocol replay's zero-gas
results. Snapshot restoration preserves balances, nonces, code, storage and supplied
block hashes.
