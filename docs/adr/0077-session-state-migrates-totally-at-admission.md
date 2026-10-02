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

Shift admission reads the marker before admitting work. Recovery checks it
before guarded payload decoding. A version outside the fleet read window
returns `SessionStateVersionUnsupported` or `SessionStateVersionNewerThanRuntime`
without attempting to interpret the payload.

`admit_session_state` validates a sealed `ShiftFence` inside a backend
transaction before reading the marker. Its result carries the session id,
version, and shift epoch. This admission checks compatibility; it does not
execute a per-session converter chain or advance the marker.

A tool child of an effect group (ADR 0099) passes the same gate before its
tool body runs, but there the gate is defence in depth, not the compatibility
seam. A build's drain generation `G` hashes its `SessionAdmissionWindow` —
the supported range and every writer pin the recorded `F` could select
(FIG-4454) — so every build that serves the child's lane admits every marker
its opener's build admitted. A child or a successor sent on its opener's lane
therefore never meets a refusal its opener would not have met. A child whose
final already committed is past the gate: it drains whatever the marker says,
and a generation refusal of a committed child reports a violated
precondition, not a compatibility outcome.

The marker has no production writer. It moves only by fleet conversion under
a shift fence, and a shift fence excludes executes, not group children: a
committed child whose seat is still owed may drain after the fence is taken.
A future marker mover must therefore also exclude committed, undrained group
children on the sessions it moves, replay-safely, before it advances a marker
(ADR 0099 §5).

### Record counters remain codec discriminators

Head, checkpoint, component, wake, and protocol counters remain independent.
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
The production session marker is generation 3; the synthetic-next tier uses
generation 4 to prove adjacent-format support. Neither admits snapshot-era
session generations by guessing defaults.

### Executable state remains pinned

The marker does not translate a parked VM instruction pointer, heap, compiled
artifact, or engine invocation into another executable deployment. Executable
identity and drain rules remain separate under ADR 0043. Wire negotiation and
physical-store admission also retain their own contracts.

## Enforcement gates

The session-state admission law places malformed payload behind an unsupported
marker and asserts that marker refusal wins over decoding. It also proves
that a stale shift fence fails before the marker check. These laws run against
SQLite file, SQLite memory, and PostgreSQL. Upgrade proofs use the synthetic-next
tier; host laws cover the Restate server double, live Restate, and lash-sim's
in-process effect host where applicable.

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

- `crates/lash-core-store/src/store/state_version.rs:4-101` defines marker admission, the fleet window, and the `SessionAdmissionWindow` descriptor.
- `crates/lash/src/formats.rs:594-660` folds the session admission window into the drain generation `G`.
- `crates/lash-core/src/runtime/shift/admission.rs:108-113` gates shift admission.
- `crates/lash-sqlite-store/src/persistence/session_commit.rs:166-199` validates the shift fence.
- `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:220-239` implements the same transaction.
- `crates/lash-conformance/src/conformance/session_store_factory/state_version.rs:16-139` pins refusal ordering.
