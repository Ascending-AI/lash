# Checkpoint component identity is a backend contract

## Context

A resumable checkpoint must retain unchanged execution bytes even when the
runtime stops carrying a hydrated body. Backend-specific treatment of refs
can silently discard that state on cold open.

## Decision

A session checkpoint is one manifest plus a complete keyed set of independently
addressed components (ADR 0056). Tool state, plugin snapshots and execution
state are well-known keys, not a fixed cardinality limit. The manifest records
each component's content ref and encoding version.

Changed logical bytes have a content-derived `BlobRef`. A backend validates
and stores those bytes under that identity before publishing the checkpoint
root, and returns the realized manifest in `RuntimeCommitReceipt`. A later
ref-only entry means the component is unchanged. Cold hydration resolves the
stored bytes; a missing ref fails with `CheckpointComponentMissing`, not a
partial checkpoint. Supplied body/ref disagreement and unsupported encoding
are typed refusals.

SQLite file, SQLite memory and PostgreSQL implement the same identity and
hydration contract. Conformance covers arbitrary component keys, unchanged
refs, changed bytes, shared refs, cold hydration and unknown-ref rejection.

Each boundary replaces the complete resumable-state snapshot. Reusing a
component body is not an event-log append. Compression is a backend mechanism
that cannot change logical identity or hydration. Component refs identify
bytes; the single-use session id identifies their checkpoint owner (ADR 0049).

## Consequences

The runtime can omit clean hydrated bytes without losing resumable state.
Echoing a ref without providing its stored body is rejected because it makes
a ref-only checkpoint incomplete. Hard-coding three component slots is rejected
because independent runtime components need stable keys rather than another
manifest redesign.

## Implementation

[Keyed manifest, content identity and codec validation](../../crates/lash-core-store/src/store/checkpoint.rs)
and [component admission laws](../../crates/lash-conformance/src/conformance/runtime_persistence/checkpoint_admissions.rs)
define the shared backend obligation.
