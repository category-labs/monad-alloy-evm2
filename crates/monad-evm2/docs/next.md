# Monad Next

`MONAD_NEXT` (`MonadHardfork::MonadNext`) inherits MonadTen's rules on Osaka,
including MIP-8 page-based storage warming and pricing. It has no scheduled
activation and must be selected explicitly.

## Implemented features

- **MIP-7:** the `0xAE` extension prefix decodes a one-byte instruction selector.
- **MIP-18:** `CALLSTACKDEPTH` and `CALLERN` expose the current depth and callers
  of active ancestor frames.

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
