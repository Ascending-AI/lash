# 0061: RLM dialects share one kernel and durable runtime

## Status

Accepted.

## Decision

Each dialect owns its syntax, semantics, lowering and source presentation.
TypeScript and Python both lower to the same kernel Document. One checker,
library, heap machine, checkpoint family and durable broker execute that code;
a dialect does not emulate another source language. ADR 0139 owns the kernel.

The host explicitly selects a `CellDialect` for new sessions and installs its
kernel dialect package in the worker. The session records the name at creation
and uses that name on later runs. A deployment missing the recorded dialect
refuses rather than substituting another. ADR 0138 owns session pinning and
ADR 0140 owns build features and worker installation.

`CellDialect` carries its name, number policy, `DialectPrompts` and tool
presenter. Prompt vocabulary and source spellings stay with that dialect;
shared execution services own stores, routes, worker transport and bounds.
A new front end supplies its own semantic and prompt evidence. Permanent
parity with another language is not a requirement of the shared machine.

## Consequences

Dialect selection remains coherent with saved state and prompt teaching.
Machine identity and durable recovery stay independent of source syntax.
Helpers are immutable build-library functions; ordinary package code retains
its defining Link (ADR 0139). Shapes change in place before 1.0.

## Code evidence

- [Dialect selection and prompts](../../crates/lash-protocol-rlm/src/dialect.rs).
- [Worker embedding](../../crates/lash-vm-worker/src/embedding.rs).
- [Kernel dialect contract](../../crates/lash-kernel-dialect/src/lib.rs).
- [TypeScript](../../crates/lash-dialect-typescript/src/lib.rs) and
  [Python](../../crates/lash-dialect-python/src/lib.rs) front ends.
