# Monad Next extensions

`MonadNext` inherits MonadTen on Osaka and enables MIP-7 and MIP-18. It has no
scheduled activation. Amsterdam and MIP-12 are outside this implementation.

| Bytes | Instruction | Stack | Gas |
| --- | --- | --- | --- |
| `AE 00` | `CALLSTACKDEPTH` | `[] → [depth]` | 2 |
| `AE 01` | `CALLERN` | `[n] → [caller]` | 2 |

Depth starts at zero. `CALLERN(0)` returns the current frame's `CALLER`;
`CALLERN(depth)` returns the root caller. Indices greater than the current depth
return zero, including full-width 256-bit indices. Calls, delegated calls, and
constructors retain their actual message callers.

The prefix and selector execute as one instruction, with no additional prefix
charge. Missing or unassigned selectors halt as `InvalidFEOpcode`. Earlier forks
reject the prefix as `OpcodeNotFound`. Legacy jump-destination analysis stays
unchanged, including its treatment of `JUMPDEST` and PUSH data after the prefix.

Both the standalone builder and native factory track callers without requiring a
user inspector. Ancestor callers survive frame reverts and context replacement;
new ordinary transactions clear them. Synthetic hosts starting at a nonzero depth
can retain ancestors through preserved extension context; absent ancestors return
zero. The current frame always uses its message's caller.

## References and verification

The behavioral references are C++ proposals
[MIP-7, PR #2662](https://github.com/category-labs/monad/pull/2662)
at `25c6473346d24c7b6eb3038bff42d4c9f4032cdd` and
[MIP-18, PR #2621](https://github.com/category-labs/monad/pull/2621)
at `a352e5c35e5bb7a68b67cd6cbbcafd07b7cd10e8`. These proposals are not
part of the C++ main baseline used for this port.

CI compares protocol cases with
[monad-revm main at `6403f504`](https://github.com/category-labs/monad-revm/commit/6403f504c2bdab6875bd9cb34ad2d7aebb618309):
status, output, created address, execution gas, receipt charge, and pre-instruction
traces. Coverage includes selectors, terminal instructions, stack/gas boundaries,
jump analysis, nested calls, EIP-7702 delegation, CREATE/CREATE2, and failed calls.
Separate tests cover host reuse, frame overrides, and context restoration.

```sh
cargo test --locked -p monad-evm2 --features compare-monad-revm-main --test extensions
```

Default tests assert protocol behavior without compiling a reference engine. The
older `compare-monad-revm` feature retains its separate monad-revm 0.8.0 audit.
