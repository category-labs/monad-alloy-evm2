# monad-evm2-fixtures

Replay signed blockchain JSON fixtures through `alloy-monad-evm2`, verify their
execution commitments, and compare individual operations with the published
`monad-revm = 0.8.0` release.

## Running

From the workspace root, supply files or directories:

```sh
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests --filter sstoreGasCosts
cargo run --locked -p monad-evm2-fixtures -- /path/to/blockchain_tests --json --fail-on-unsupported
```

Reports go to stdout; invocation errors go to stderr. Directories are searched
recursively without following symlinks. Paths are deduplicated and sorted;
`index.json` manifests are excluded. Malformed files remain failures even when
filtering by case name. Every selected case is reported.

## Verification

Each case starts from its `pre` allocation and executes blocks in order. The runner checks:

- Genesis state and RLP, block-header/RLP agreement, block linkage, transaction
  sender recovery, and transaction and withdrawal roots.
- Block gas used, receipts, logs bloom, and complete state roots.
- Final block hash, optional final account assertions, and `postStateHash`.
  Account checks include existence, nonce, balance, bytecode and storage.

Execution includes EIP-4788 and EIP-2935 system calls when their contracts are
deployed, then transactions and withdrawals. Reserve decisions receive the
current block's senders and recovered authorities, plus parent and grandparent
participants. Historical block hashes are supplied to both engines.

Before each transaction or system call, the reference receives the same complete
pre-state as the adapter. Comparison covers success/revert/halt status, consumed
gas, refunds, output, logs, created address and the complete post-state root.
The adapter's committed state supplies subsequent operations. A reference
difference therefore cannot change the inputs of later comparisons.

Monad receipts charge the full transaction gas limit. The runner checks that
independently against the adapter and fixtures. The 0.8.0 reference's receipt gas
is normalized to this rule, and each changed reference receipt is counted in
`corrected_reference_receipts`. Consumed execution gas and fee effects remain
checked separately; normalization does not alter either engine's execution.

The published reference tracks reserve balances without automatically rejecting
violations. The adapter enforces them. These differences remain visible in
`cases[].parity.differences`, including both observations and state roots.
Reference execution errors are also differences. No cases or fields are silently
exempted by the runner.

`status` describes the fixture assertions; `parity` describes the reference
comparison. A fixture can pass while parity differs. Comparisons are recorded up
to the first fixture assertion or execution failure.

| Exit code | Meaning |
| --- | --- |
| `0` | At least one fixture passed, no failures or parity differences, and no unsupported cases when strict mode is requested. |
| `1` | A fixture failed, parity differed, or strict mode encountered unsupported cases alongside executable cases. |
| `2` | Invocation/discovery error, empty selection, or entirely unsupported input. |

## Supported inputs

Blockchain fixtures support `MONAD_EIGHT`, `MONAD_NINE`, and timestamp transitions
such as `MONAD_EIGHTToMONAD_NINEAtTime30`. Fork selection uses fixture timestamps
and explicit chain IDs. Input parsing follows the
[monad-revm fixture runner](https://github.com/category-labs/monad-revm/tree/9082f9b850998df1b5322dec5d38d129e565d0ea/crates/monad-fixtures).

Other forks and fixture formats, invalid-block expectations, genesis above block
zero, nonzero excess blob gas, ommers, unsupported seal engines, and unknown
semantic fields are reported as unsupported. Blob transactions are rejected.
Hex quantities may omit leading zeros; decimal strings and JSON integers are
also accepted. Transactions and withdrawals come from block RLP, and the declared
header must match it.

Verification covers valid-block execution. It does not validate every consensus
header rule, epoch transition, or staking reward/snapshot lifecycle, and it does
not execute the C++ node. Focused library tests cover MonadTen and MonadNext.

## CI corpus

CI checks out the execution-spec fixtures at
[`12e2c05bcfa6daeef9c0f4c54c5c7a13fde39e89`](https://github.com/category-labs/monad-execution-specs/tree/12e2c05bcfa6daeef9c0f4c54c5c7a13fde39e89/test/execution/fixtures/blockchain_tests).
The expected result is 226 passes, one known `highGasUsage` fixture mismatch,
and zero unsupported cases. Its 2,155 comparisons contain 301 reserve-enforcement
differences across 18 cases and 702 normalized reference receipts.

The `highGasUsage` mismatch comes from the execution spec allowing reserve
spending when fees exceed the reserve; the adapter follows the C++ reserve rule.
The failed status and differing receipt roots remain in the report.

[The acceptance policy](../../.github/scripts/fixture_ci.py) checks the complete
fixture inventory, counters, exact known failure, and exact parity observations.
Known reserve differences must retain matching gas, refunds, receipt charges and
creation addresses, with empty adapter logs/output. The
[expectations file](../../.github/scripts/fixture_expectations.json) pins each
case's difference count and SHA-256 of its full observations, serialized as JSON
with sorted keys and compact separators. Changed outcomes, missing comparisons,
new differences and unexpected passes fail CI. The raw JSON report is uploaded
even when the check fails.

To reproduce with a checkout at that revision:

```sh
cargo build --locked -p monad-evm2-fixtures
python3 .github/scripts/fixture_ci.py \
  --runner target/debug/monad-evm2-fixtures \
  --fixtures ../monad-execution-specs/test/execution/fixtures/blockchain_tests \
  --report target/fixture-report.json
```

## Runner tests

```sh
cargo test --locked -p monad-evm2-fixtures
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
```

Tests construct signed fixtures with independently calculated expectations and
deliberately corrupt roots, receipts and reports. They require no external corpus.
The runtime library remains the default workspace member; this tool requires
`std` and does not change the library's `no_std` configuration.
