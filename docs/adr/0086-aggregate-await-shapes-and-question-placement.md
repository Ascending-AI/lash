# Aggregate await operates on handles

## Context

The IR and VM distinguish a pending operation from a resolved value. Re-awaiting
a resolved value as a process handle must produce a guest diagnostic rather than
a fabricated host-failure result. Compiled aggregate operations also need an
explicit shape and leaf-unwrapping rule.

## Decision

The internal `ResourceOperationBatch` and `ResourceOperationListBatch` paths
carry compiled aggregate shapes and per-leaf unwrap choices. Literal and
list-batch compilation evaluate operands and collect their leaf arguments before
handing the batch to the host. These are IR operations, not an additional
authored language or a prompt syntax.

The internal process-await path accepts a process handle or a tuple, list, or
record of handle leaves. An already-resolved leaf raises the catchable
`AwaitExpectsHandle` error, identifying its type and nested path. Actual handle
failures return the host failure or result record required by the operation's
unwrap mode.

TypeScript authoring uses direct awaited calls and Promise aggregates. Its
runtime-array contract is
[ADR 0087](0087-typescript-runtime-promise-arrays.md). Promise aggregates retain
plain array elements and await pending-operation leaves; that is a separate
path from recursively awaiting a container of process handles. Process controls
are explicit tools under ADR 0095.

## Alternatives considered

Returning an error-shaped value for an already-resolved operand confuses a
program error with a host operation failure. Implicitly treating every ordinary
container as authored aggregate syntax would conflate the IR shape with the
TypeScript Promise contract. Typed guest errors and distinct operations preserve
that distinction.

## Consequences

Internal aggregate compiler and runtime tests pin the IR contract. TypeScript
prompts teach Promise aggregates. Durable bytecode and continuation readers use
their own format guards; this decision does not authorize reinterpretation of
an incompatible compiled program.

## Code references

- `crates/lash-vm/src/runtime/compiler/effects.rs` compiles operation batches and list batches.
- `crates/lash-vm/src/runtime/vm/effects.rs:830-961` recursively awaits handle containers and diagnoses resolved leaves.
- `crates/lash-vm/src/runtime/error.rs:490,701` declares and classifies `AwaitExpectsHandle`.
- `crates/lash-vm/src/runtime/vm/pending_tools.rs:168-260` implements the separate Promise-array path.
