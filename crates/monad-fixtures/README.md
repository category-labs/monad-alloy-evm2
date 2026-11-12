# monad-evm2-fixtures

CLI and library for replaying signed blockchain JSON fixtures with
[`monad-evm2`](../monad-evm2/README.md). Verifies execution commitments, with
optional comparisons against the published monad-revm 0.8.0 release.
This workspace tool is not published to crates.io.

## Usage

Run from the workspace root, supplying JSON files or directories:

```sh
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests --filter sstoreGasCosts
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests --json --fail-on-unsupported
```

Directories are searched recursively. `--filter` matches case-name substrings;
`--json` writes a structured report to stdout. Invocation errors go to stderr.
Rust callers can use [`monad_evm2_fixtures::run`](src/lib.rs) to obtain a `Report`.

Default builds verify fixtures without compiling or executing monad-revm.
Enable the `compare-monad-revm` Cargo feature to add reference comparisons:

```sh
cargo run --locked -p monad-evm2-fixtures --features compare-monad-revm -- /path/to/blockchain_tests
```

| Exit code | Meaning |
| --- | --- |
| `0` | Cases passed without enabled comparison differences; unsupported cases are allowed unless strict mode is enabled. |
| `1` | Fixture failure, enabled comparison difference, or unsupported case with `--fail-on-unsupported`. |
| `2` | Invocation/discovery error, empty selection, or entirely unsupported input. |

## Coverage and reports

- Supports `MONAD_EIGHT`, `MONAD_NINE`, and timestamp transitions such as
  `MONAD_EIGHTToMONAD_NINEAtTime30`.
- Verifies block linkage, transaction recovery, gas charges, receipt and state
  roots, logs bloom, withdrawals, and final-state assertions.
- With `compare-monad-revm`, compares each transaction or system call using the same
  pre-state: status, consumed gas, refunds, output, logs, and resulting state.

Fixture `status` and optional reference `parity` are reported separately.
The JSON `verification` field identifies the active mode; comparison counters
are zero when the feature is disabled. The reference's receipt gas is normalized
to Monad's full-limit charge; consumed gas and fee
effects remain checked separately. Automatic reserve rejection differs from
monad-revm 0.8.0 and remains visible in the parity report.

Unsupported formats and rules are reported explicitly. Verification covers
valid-block execution, not every consensus header rule or staking lifecycle.
MonadTen and MonadNext are covered by focused runtime tests.

## Local development

Run from the workspace root using the toolchain in
[rust-toolchain.toml](../../rust-toolchain.toml). Python 3 is needed for the CI
acceptance checks. This crate requires `std`. The `compare-monad-revm` feature
enables the optional reference engine and its comparison tests.

```sh
cargo test --locked -p monad-evm2-fixtures
cargo test --locked -p monad-evm2-fixtures --features compare-monad-revm
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
cargo clippy --locked -p monad-evm2-fixtures --all-targets -- -D warnings
cargo fmt --all -- --check
cargo doc --locked -p monad-evm2-fixtures --no-deps --open
```

The tests generate their own fixtures and require no external corpus.

To reproduce CI, use the
[pinned execution-spec fixtures](https://github.com/category-labs/monad-execution-specs/tree/12e2c05bcfa6daeef9c0f4c54c5c7a13fde39e89/test/execution/fixtures/blockchain_tests).
With that checkout at `../monad-execution-specs`:

```sh
cargo build --locked -p monad-evm2-fixtures --features compare-monad-revm
python3 .github/scripts/fixture_ci.py \
  --runner target/debug/monad-evm2-fixtures \
  --fixtures ../monad-execution-specs/test/execution/fixtures/blockchain_tests \
  --report target/fixture-report.json \
  --compare-monad-revm
```

The [acceptance policy](../../.github/scripts/fixture_ci.py) checks the complete
corpus against [recorded expectations](../../.github/scripts/fixture_expectations.json),
including the known `highGasUsage` fixture mismatch and reserve-enforcement
differences. Raw failures and differences remain in the JSON report.

Licensed under [MIT](../../LICENSE).
