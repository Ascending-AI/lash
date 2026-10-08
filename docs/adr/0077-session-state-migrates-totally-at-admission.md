# Session state admits one compatible continuation generation

## Context

Individual record codecs identify envelopes, but recovery also needs a version
for the session's mutable continuation as a unit. The marker must be readable
before an incompatible head or checkpoint is decoded.

## Decision

Each session carries an independently readable `session_state_version` beside
its durable binding metadata. New sessions receive the version selected by the
store's recorded fleet format. The marker guards the mutable session recovery
unit: head and config, current checkpoint and Lash-owned components, pending
inputs, and queued work. It is separate from physical DDL and individual record
codec versions.

### Admission is the compatibility seam

Run admission reads the marker before admitting work. Recovery checks it
before guarded payload decoding. A version outside the fleet read window
returns `SessionStateVersionUnsupported` or `SessionStateVersionNewerThanRuntime`
without attempting to interpret the payload.

`admit_session_state` validates the session actor's epoch fence inside a
backend transaction before reading the marker
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3). Its
result carries the session id, version, and epoch. This admission checks compatibility; it does not
execute a per-session converter chain or advance the marker.

Run admission carries the session gate and the admitted executable binding.
A build's format set includes its `SessionAdmissionWindow`, including all
writer pins the recorded fleet format could select, so a node claims only
sessions whose marker it reads
([ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1).
Compatible admission is checked before new work; a committed final remains
protected through drain.
A marker refusal cannot replace a durable final or authorize fresh execution.

The marker moves only through the explicit fleet conversion contract. A future
marker mover must exclude every active logical Run and owed protected drain,
including transferred work, before changing it. An epoch fence alone is not
proof that all execution dependencies ended. The Run's ownership and frontiers
in [ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
supply those facts; there is no separate child compatibility gate.

### Record counters remain codec discriminators

Head, checkpoint, component and protocol counters remain independent.
The owning readers apply their format guards within the recorded fleet window.
A compatible session marker does not authorize ignoring a record's own guard.
`scripts/versioned-surfaces.toml` and `lash::formats` identify the formats the
binary supports.

### Conversion and source-shape enforcement

Durable format conversion belongs to the fleet finalize contract in
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) and the
registered lifts and migration guarantees of
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md). Admission
does not invent a converter for unsupported state or reset its contents.
`CURRENT_SESSION_STATE_VERSION` is 1 in the ordinary build and 2 with
`synthetic-next`, as declared in
`crates/lash-core-store/src/store/state_version.rs`. The marker's admitted
window, rather than historical cutover numbers, decides what a build reads.
The pre-1.0 freeze changes shapes in place; equal numbers do not establish
compatibility between development builds.

### Executable state remains pinned

The marker does not translate a parked VM instruction pointer, heap or
compiled artifact into another executable contract. A VM snapshot bound to an
executable identity whose bytecode contract changed finishes on a node that
reads it or is refused under ADR 0106 §1. VM worker IPC admission and
physical-store admission retain their own contracts. Host transport DTOs are
host-owned under ADR 0136; the engine supplies typed local reads.

## Enforcement gates

The session-state admission law places malformed payload behind an unsupported
marker and asserts that marker refusal wins over decoding. It also proves
that a stale epoch fence fails before the marker check. These laws run against
SQLite file, SQLite memory, and PostgreSQL. Upgrade proofs use the synthetic-next
tier. Laws run the production runtime over a fault-injecting store with labelled commits, a virtual clock and `SimNodes` ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §14).

## Alternatives considered

Checking only individual records can admit an incoherent recovery unit. Moving
the marker inside the guarded payload prevents refusal before decode. Lazy
reset or opportunistic conversion at an ordinary read makes recovery depend
on which row is visited first. Explicit fleet conversion and an early marker
gate avoid those ambiguities.

## Consequences

An incompatible session cannot begin recovery or new work. A compatible marker
and compatible record codecs are both required. Store transactions own fence
validation, while fleet conversion owns supported format transitions.

## Code references

- `crates/lash-core-store/src/store/state_version.rs` defines marker admission, the fleet window, and the `SessionAdmissionWindow` descriptor.
- `crates/lash/src/formats.rs` folds the session admission window into the build's format set.
- `crates/lash-core/src/runtime/durable/session.rs` gates run admission.
- `crates/lash-sqlite-store/src/persistence/session_commit.rs` validates the fence.
- `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs` implements the same transaction.
- `crates/lash-conformance/src/conformance/session_store_factory/state_version.rs` pins refusal ordering.
