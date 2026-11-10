# monad-evm2

Monad execution on [evm2](https://github.com/alloy-rs/evm2).

Import the library as `monad_evm2`. See the [workspace README](../../README.md)
for construction examples and build and test commands.

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

`MonadEvmFactory` constructs a native evm2 `Evm<MonadEvmTypes>` from a hardfork,
runtime version, block and database. It is stateless: the embedder owns its
inspectors, snapshots and accepted state. For each ordinary transaction, compose
the application's inspector with `reserve_balance::reserve_inspector()` and
pass it to native `transact_with_inspector()`. `MonadPrecompiles` includes root
reserve observation; custom providers can use `reserve_precompiles()`.
Ordinary execution without the reserve observer is rejected.

Native hosts use their configured hardfork. Embedders that execute new frames
on separately configured hosts need no shared frame selector. Protocol replay
works directly through native `transact()` and observes its installed inspector;
the transaction handler isolates and restores the enclosing Monad context.

`MonadEvmBuilder::new(chain_id)` constructs the standalone `MonadEvm` wrapper.
Its `limits` field accepts runtime overrides. The wrapper owns `MonadExecution`,
which composes inspectors, supports per-frame hardfork overrides and resolves
synthetic context together with state. `create_host()` exposes the same
`(host, execution)` pair for applications wanting that lifecycle explicitly.
Pass inspectors directly to `execution.transact()` when using this pair.

For external state changes, synchronize reserves before the change and rebase
afterward. `rebase_reserve_balance_with_originals()` retains transaction-start
fields for persistent accounts carried into another fork, including accounts
first debited after the switch. `replace_while_executing()` restores matching
extension state while retaining live frame, memory and enforcement bookkeeping.
Use the rebase helper after state replacement: resetting the journal cursor alone
does not preserve original balances. The `execution` module documents the
standalone lifecycle.

Standalone inspectors can select a call or creation's Monad hardfork in their
`call` or `create` hook:

```rust
message.ext.hardfork = Some(MonadHardfork::MonadNine);
```

Descendants inherit that hardfork unless overridden, and returning restores the
parent's rules. `frame_hardfork(host)` reports the selected Monad rules. evm2's
configuration remains fixed throughout execution; its `config_spec_id()` and
interpreter `spec()` describe that underlying configuration. Transaction
admission and fees use the configured fork. The default precompiles follow each
frame; custom providers manage their own selection. `reset_precompiles()`
reinstalls the default provider.

`monad_precompiles()` returns `MonadPrecompiles`. Its relocation API allows
cryptographic precompiles to move, but rejects nonidentity moves from staking
(`0x1000`) and reserve balance (`0x1001`), whose handlers depend on their protocol
addresses. `MonadEvm` applies the same restriction to installed custom providers.
Rejected batches leave all fork tables unchanged. Direct map customization is an
explicit override and remains the caller's responsibility.

`MonadEvm::system_call()` uses the installed inspector;
`MonadEvm::system_call_with_inspector()` borrows one for a single call. Staking
block-lifecycle helpers also use the installed inspector. These standalone
system calls bypass ordinary transaction validation, fees, nonce updates and
reserve enforcement, and preserve any enclosing synthetic call's reserve
context. Their state handles support commit, discard and detach.

For trusted historical block replay, set `evm.ext_mut().protocol_replay = true`
before passing recovered envelopes to `transact()`. Canonical legacy envelopes
from `SYSTEM_ADDRESS` execute reward, snapshot and epoch-change syscalls with
zero receipt gas. Replay validates the chain ID and nonce, increments the system
nonce, and mints reward value to staking. Failure rolls back the entire
transaction. Installed and borrowed inspectors observe these calls, and any
enclosing synthetic context is restored.

`protocol_system_call()` classifies envelopes without changing state. The caller
must authenticate the system sender and validate block ordering, epoch scheduling
and reward amounts. Replay is disabled by default and must remain disabled for
transaction-pool admission. Direct system-call helpers retain their separate
nonce and prefunding semantics.

Internal-call simulations may set `preserve_reserve_balance_tracker` after
initializing or copying the enclosing transaction's reserve context. These
synthetic calls retain its original balance baselines and defer enforcement to
the enclosing transaction. The standalone wrapper restores context when a
synthetic call is discarded and keeps tracking changes when committed. Native
embedders own this context resolution alongside their state lifecycle.
The enclosing caller must check reserves before accepting execution and restore
the flag to `false` for ordinary transactions. `MonadCheckpoint` and
`MonadStateSnapshot` pair state with Monad
bookkeeping; `rebase_reserve_balance` rebuilds tracking against replacement fork
state and its chain context.

Staking follows C++ calldata-consumption rules, accepts zero-value delegation as
a no-op, applies the delegation minimum to compounded rewards, and emits
delegation events before validator status changes.

Validator registration retains monad-revm 0.8.0's unverified signatures and
simplified public-key lookup identifiers. This differs from validator admission
in the C++ consensus client.

## Features

| Feature | Purpose | Default |
| --- | --- | --- |
| `std` | Enable the standard library in evm2 and optional serialization support. | Yes |
| `serde` | Enable Serde support. | No |
| `c-kzg` | Enable evm2's C KZG backend. | Yes |
| `secp256k1` | Enable evm2's native secp256k1 backend. | Yes |
| `blst` | Enable evm2's BLST backend. | Yes |
| `portable` | Enable portable builds of the selected KZG and BLST backends. | Yes |
| `memory_limit` | Compatibility feature; memory limits are always enforced. | No |

Memory limits are shared by active call frames and capped at 8 MiB for MonadNine
and later, including builds with default features disabled.

Disable default features for `no_std` builds. The `serde` feature supports
`no_std` and enables the local Serde dependency without enabling evm2's
engine-state serialization.

## License

MIT. See [LICENSE](../../LICENSE).
