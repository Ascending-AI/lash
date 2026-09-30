# 0061: RLM dialects share one IR and VM

## Status

Accepted.

## Retained architecture

A dialect owns its syntax and semantics and lowers into the shared Lashlang IR.
One linker, compiler, heap VM, continuation family and durable runtime execute
that IR. A dialect does not emulate another language.

TypeScript is the shipped dialect. Its language ID is `typescript`, used in
prompt vocabulary, execution reporting and persisted RLM execution state.
`rlm_dialect()` selects that front end directly. There is no host dialect
selector or first-commit language pin.

The internal `Dialect` contract owns parsing, cell parsing, diagnostics, prompt
vocabulary, cell tags, tool signatures and addressable tool paths. Shared
execution-session services own artifact storage, resolvers, trace configuration,
bounds and transport.

A production dialect needs a front end and its own semantic evidence, selected
consistently with prompt teaching, tool bindings and restored state. This does
not require another dialect's examples or a source-language field on compiled
IR artifacts. ADR 0096 owns shipped-dialect selection and extension work;
ADR 0060 owns the machine/language separation.

The workflow-graph lens has a TypeScript canonical printer. Its round-trip laws
and typed refusals describe IR the printer can express. A printer for another
language needs evidence for its own accepted representations.

## Why

Sharing the machine keeps durable state, metering and runtime operations under
one contract. Independent front ends can define different source semantics
without duplicating that execution machinery. Permanent parity between source
languages is not required by the shared IR.

## Consequences

- A front end is selected at the source boundary, with coherent session state
  and prompt vocabulary.
- Each dialect proves its own accepted semantics.
- Registered format windows and ADR 0115 govern upgrades. The pre-1.0 freeze
  changes shapes in place without version bumps or upcasters.

## Code evidence

- [Dialect and direct selection](../../crates/lash-protocol-rlm/src/dialect.rs#L33).
- [Shared services and session](../../crates/lash-protocol-rlm/src/dialect.rs#L82).
- [TypeScript parse/lower/link path](../../crates/lash-typescript/src/lib.rs#L70).
- [RLM engine identity in durable root](../../crates/lash-protocol-rlm/src/executor/state.rs#L891).
- [Workflow-graph lens](../../crates/lash-typescript/src/workflow_graph/mod.rs).
