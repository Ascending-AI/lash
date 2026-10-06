# The lashlang VM is a heap substrate with dialect-lowered value semantics

## Status

Accepted.

## Context

A source dialect needs identity, closures, exceptions, bounds and durable
continuations. Duplicating the machine for each language would duplicate its
persistence and metering contracts. The shared IR and VM execute lowered
programs; source-language semantics belong to the front end.

## Decision

The VM stores heap objects by identity and preserves references. Binding two
names to one object and mutating through either name affects that object.
TypeScript is the shipped source dialect and lowers ECMA reference semantics
into this machine. The machine does not insert isolation copies at stores.
ADR 0061 defines the dialect/IR boundary; ADR 0091 owns expression lowering.

A dialect is a compiler front end with its own accepted semantics and evidence.
If its language requires copying, its lowering must express that behavior using
the shared machine's contracts. It does not require a second heap VM.

### Why the reverse design is not available

A machine that implicitly copies every value cannot directly represent aliasing
or identity comparisons. A reference dialect would need a guest object table
and indirect property operations. That table would act as a second heap,
separate from the machine's collector and memory meter. Native identity lets
collection, bounds and persistence describe the same objects the guest sees.

### What the substrate owns, and no dialect may tune

The shared machine owns:

- allocation-ordered object IDs that are not reused;
- non-moving mark-sweep collection every 1,024 allocations and at boundaries
  that need an exact live set;
- logical-memory charges computed from the live heap objects and the
  explicit execution bounds of ADR 0055;
- the allocation counter and heap objects carried in durable VM snapshots
  ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8); and
- the shared AST nesting cap of 64, checked before linking and compilation.

A front end must produce IR within the shared structural bound. TypeScript has
its own earlier source-nesting diagnostic; that diagnostic does not change the
machine's cap.

Function values, closures, stackless call frames, handler/finally stacks and
error routing also belong to the VM. A TypeScript `return` is a function return
that executes pending cleanups. `Expr::Finish` is an execution terminal and
cannot substitute for that return.

### Relationship to ADR 0076

Runtime roots hold the actual heap references. Host views are projections of
those roots and do not define durable ownership. ADR 0076 governs this
root/view boundary and validation of persistent heap state.

At capture the VM collects the reachable heap and first checks whether it is an
exclusively owned forest. If the forest check fails, it validates a shared
acyclic graph. Shared references and supported exotics persist as graph state;
cycles and dangling references fail validation. The reader checks the same
shape obligations. The wire's `reference_semantics` flag describes the stored
forest/graph form, not a source dialect.

Each object persists once under its heap ID. Fragments assign objects to the
first root that discovers them, so references across roots preserve identity.
A reader need not infer ownership from a language selector.

### Format versions

`lash::formats` exposes the current bytecode, continuation, ABI and snapshot
contracts. Their writers and readers use the registered fleet version windows. ADR 0115 owns compatibility, migration and drain rules.
During the pre-1.0 freeze, shapes change in place without version bumps or
upcasters.

## Consequences

- One VM and durable format family serve source front ends with independent
  semantics and evidence.
- Aliasing, collection and logical accounting refer to the same heap objects.
- Durable state can be a forest or a validated shared graph. A forest-only
  assumption is insufficient for a reader.
- A front end that needs an operation the IR cannot express must extend the
  shared machine contract. A second VM requires a separate decision.

## Code evidence

- [Dialect contract](../../crates/lash-protocol-rlm/src/dialect.rs#L33).
- [Heap identity and counters](../../crates/lashlang/src/runtime/heap.rs#L103).
- [AST bound](../../crates/lashlang/src/ast.rs#L140).
- [Forest or graph capture](../../crates/lashlang/src/runtime/state.rs#L699).
- [Shared-object partition](../../crates/lashlang/src/runtime/heap/partition.rs#L1).
- [Format manifest](../../crates/lash/src/formats.rs#L339).
