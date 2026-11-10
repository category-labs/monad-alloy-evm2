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

Inspectors can select a call or creation's Monad hardfork in their `call` or
`create` hook:

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

`system_call()` uses the installed inspector; `system_call_with_inspector()`
borrows one for a single call. Staking block-lifecycle helpers also use the
installed inspector. System calls bypass ordinary transaction validation, fees,
nonce updates and reserve enforcement, and preserve any enclosing synthetic
call's reserve context. Their state handles support commit, discard and detach.

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
the enclosing transaction. Discarding a synthetic call restores its context;
committing keeps its tracking changes. The enclosing caller must check reserves
before accepting execution and restore the flag to `false` for ordinary
transactions. `MonadCheckpoint` and `MonadStateSnapshot` pair state with Monad
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
| `memory_limit` | Apply memory limits shared by active call frames, capped at 8 MiB for MonadNine and later. | Yes |

Disable default features for `no_std` builds. The `serde` feature supports
`no_std` and enables the local Serde dependency without enabling evm2's
engine-state serialization.

## License

MIT. See [LICENSE](../../LICENSE).
