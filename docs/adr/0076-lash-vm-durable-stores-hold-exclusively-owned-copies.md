# Durable VM state preserves shared references and owns its roots

## Context

TypeScript bindings can refer to the same mutable heap object. A durable
snapshot or continuation must preserve that identity so mutation through one
alias remains visible through another after restore. A detached host value
cannot represent every value that the VM can retain.

## Decision

The VM uses reference semantics. Name, slot, global, and container stores can
share heap references. They do not universally copy the stored graph.
Operation-specific copies do not change this language-wide aliasing rule.
TypeScript targets the shared IR and VM under ADR 0096.

### Validated durable graphs

A snapshot writer first tries the forest validator. A heap that satisfies it
uses the forest form. Otherwise the writer requires `validate_persisted_graph`
and sets the wire's `reference_semantics` flag. The flag records the heap's
representation, rather than an authoring dialect.

Shared-graph validation permits multiple roots or members to name one object.
It requires every reference to resolve, every object to be reachable, valid
object shapes, and an acyclic graph. Depth and memory bounds still apply.
Snapshot and continuation encode, decode, and resume validate the applicable
form. The forest form is an encoding choice for heaps that fit it, not a ban
on aliases in programs.

VM roots include slots, globals, operands, iterator state, and function frames.
The common root traversal supports collection and durable validation. Pending
operands can alias slot values, including when assignment runs inside another
expression. A suspension preserves those references.

### Runtime roots and the host view

Runtime roots own a heap-backed state's bindings. `host_view` derives the
host-facing globals from those roots. It omits the whole binding when export
encounters a function, an exotic without a detached host shape, or an exported
value containing a pending-tool handle. It never exposes a partial value.

An omitted binding can still exist in the roots. Existence checks and global
patch outcomes read the roots, not the host view. The wire stores roots and
heap objects; decoding derives the view through the same function as live
installation. A state with a hidden binding therefore round-trips without
losing either the binding or its host-view omission.

Closures are private to their compiled execution. A continuation can retain
them against that program. At execution completion the runtime drops bindings
that reach functions and records expired names for later diagnostics. Other
unprojectable bindings remain in the roots while the view omits them. Explicit
host-boundary uses of functions or unprojectable exotics return typed errors.

## Alternatives considered

Copying on every durable store changes TypeScript mutation and identity.
Requiring a forest at every boundary refuses valid aliasing programs. The graph
form preserves identity while the validator enforces durability bounds.

Using the host view as authoritative state loses bindings that have no detached
host shape. Serializing both roots and view creates two records to reconcile.
One derivation keeps the view consistent with the restored roots.

## Consequences

Aliases survive snapshots and continuations. Cycles and malformed durable graphs
are refused at the boundary. Host-visible globals are a projection, so an absent
entry alone does not prove that a name is absent from runtime state. Durable
consumers inspect the representation flag and use the matching validator.

## Code references

- `crates/lash-vm/src/runtime/state.rs:445-480,699-740` derives the view and selects the wire form.
- `crates/lash-vm/src/runtime/heap/validation.rs:163-255` validates shared graphs.
- `crates/lash-vm/src/runtime/state/expired.rs` removes completed-execution function bindings.
- `crates/lash-typescript/tests/dialect.rs:310-365,410-416` asserts alias preservation across suspension.
- `crates/lash-vm/src/runtime/state/tests.rs:1697-1714` asserts hidden-binding round trips.
