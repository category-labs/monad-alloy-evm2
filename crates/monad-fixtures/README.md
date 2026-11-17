# monad-evm2-fixtures

CLI and library for replaying signed blockchain JSON fixtures with
[`monad-evm2`](../monad-evm2/README.md). Verifies execution commitments and
protocol outcomes against fixture expectations.
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

| Exit code | Meaning |
| --- | --- |
| `0` | Cases passed; unsupported cases are allowed unless strict mode is enabled. |
| `1` | Fixture failure or unsupported case with `--fail-on-unsupported`. |
| `2` | Invocation/discovery error, empty selection, or entirely unsupported input. |

## Coverage and reports

- Supports `MONAD_EIGHT`, `MONAD_NINE`, and timestamp transitions such as
  `MONAD_EIGHTToMONAD_NINEAtTime30`.
- Verifies block linkage, transaction recovery, gas charges, receipt and state
  roots, logs bloom, withdrawals, and final-state assertions.

The JSON report uses `schema_version: 3` and
`verification: "valid_block_execution"`. Each case includes its status and
diagnostic; the summary counts passes, failures, and unsupported cases.

`known_spec_model_divergence` labels only the exact `highGasUsage` failure as
`gas_fees_exceed_reserve`. Its second transaction charges 10.2 MON against a
10 MON reserve and cannot use the emptying exception. The
[OCaml model](https://github.com/category-labs/monad-execution-specs/blob/12e2c05bcfa6daeef9c0f4c54c5c7a13fde39e89/lib/reserve_balance.ml#L59-L64)
documents why it permits this consensus-excluded input; the runtime follows the
execution client's reserve rejection. This label is `null` for other outcomes.
`summary.known_spec_model_divergences` counts these cases within `failed`;
the original diagnostic and exit code `1` remain unchanged.

Unsupported formats and rules are reported explicitly. Verification covers
valid-block execution, not every consensus header rule or staking lifecycle.
MonadTen and MonadNext are covered by focused runtime tests.

## Local development

Run from the workspace root using the toolchain in
[rust-toolchain.toml](../../rust-toolchain.toml). Python 3 is needed for the CI
acceptance checks. This crate requires `std`.

```sh
cargo test --locked -p monad-evm2-fixtures
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
cargo build --locked -p monad-evm2-fixtures
python3 .github/scripts/fixture_ci.py \
  --runner target/debug/monad-evm2-fixtures \
  --fixtures ../monad-execution-specs/test/execution/fixtures/blockchain_tests \
  --report target/fixture-report.json
```

The [acceptance policy](../../.github/scripts/fixture_ci.py) checks the complete
corpus against [recorded expectations](../../.github/scripts/fixture_expectations.json),
including the exact `highGasUsage` spec-model classification and diagnostic.
CI reports known spec-model divergences separately from unexpected failures and
rejects any changed outcome.
Raw failures remain in the JSON report.

Licensed under [MIT](../../LICENSE).
