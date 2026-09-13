# Artifact lifetimes use exact owner edges

## Status

Accepted.

## Context

Lashlang modules and captured process execution environments are immutable,
content-addressed inputs to durable execution. Their stores previously exposed
ownerless writes, and the module store also exposed a generic mutable raw-byte
keyspace. SQLite kept artifact references as permanent blob roots while
PostgreSQL kept modules outside reclamation. Consequently a failed or abandoned
publisher could leak bytes forever, while deleting by content address would be
unsafe because several durable processes may share the same content.

Compilation made this boundary less clear by optionally persisting as part of
`compile_module`. A pure compiler cannot decide how long a host or replayable
execution needs the artifact it produced.

## Decision

Compilation parses, links, introspects, and returns a `ModuleArtifact`; it does
no I/O. Publication is a separate store operation and always names an
`ArtifactOwner`: an explicit host identity, an authoritative process record, or
a replayable execution scope.

Stores persist exact `(artifact, owner)` edges rather than reference counts.
Publishing the same verified content for another owner adds an edge. Releasing
one owner severs only that edge, and the store reclaims the immutable bytes when
no edge remains. Host edges are indefinite until the host explicitly releases
them.

Process start uses a deterministic execution owner derived from the process id.
Environment and engine artifacts are protected under that staging owner before
registration. After the process row commits, store operations atomically add
the process owner and sever the staging owner; replaying that transfer is
idempotent, including a replay after staging retirement. An authoritative
abandonment decision retires the staging owner; a scheduling error after
registration remains resumable and preserves its inputs. Retirement records a permanent execution-owner fence
before severing remaining edges, so a delayed writer cannot republish after
abandonment.

The generic raw artifact overwrite keyspace and all ownerless publication APIs
are deleted. Module publication verifies the module's content-derived identity;
environment publication verifies that its reference matches valid encoded
bytes. Existing process-environment family-version refusal remains
reject-and-recreate.

SQLite durable-core schema 57 adds owner edges and execution-owner retirement
fences beside `artifact_refs`; process schema 33 retains exact prune release
inputs, and effect schema 18 records artifact-cleanup completion on the existing
scope-retirement evidence. PostgreSQL component 87 brings module and
process-environment rows under the same owner-edge model, serializes every
artifact mutation at a stable advisory-lock identity, and retains both prune
release inputs and scope-retirement cleanup completion.
These are reject-and-recreate boundaries under ADR 0081; no migration or dual
path is provided.

## Consequences

- Two processes or hosts may safely share identical bytes without a mutable
  counter or last-operation marker on the content.
- Reclamation is an owner-severing transaction, not tracing garbage collection
  and not a lease-, time-, or receipt-based inference.
- Process pruning writes exact environment and engine release inputs before the
  authoritative rows are pruned, then acknowledges them only after every store
  has severed the process owner; tombstone compaction cannot outrun that evidence.
- Scope retirement remains the abandonment authority: artifact cleanup consumes
  its durable verdict until every configured store acknowledges the permanent
  fence and owner severance; it does not invent another lifecycle journal.
- A backend must make publication, transfer, release/reclaim, and execution-owner
  retirement atomic and retry-safe. Missing exact edges on release are an
  idempotent success; missing both sides of a transfer is an error.
