# Session ids are used once

## Status

Accepted.

## Context

Lash previously allowed a host to delete a session and create another session
with the same id. `IncarnationId` distinguished those lifetimes throughout
runtime state, history-node preimages, effect-journal identity, turn addresses,
and lifecycle retirement. Every seam that carried the discriminator was also a
seam that could omit it. Session-keyed deletion and await-event revocation
evidence were consequently unsafe when an id recurred.

Lash cannot prove that an arbitrary host string is globally unique. It can
enforce a narrower and sufficient invariant inside each store: once an id has
been used in that store, the id is never reused there.

## Decision

A session id is host-provided, identifies exactly one session lifetime, and is
used at most once in a store. Deletion writes a permanent tombstone. Creating
or forking to a deleted id fails with `StoreError::SessionDeleted`, whose
message states that the id was used and deleted. Retention and vacuum never
remove this identity evidence.

Creating an already-live id remains idempotent. Opening an existing id remains
an explicit operation. `fork_at` already takes the new host-provided session id
and uses the same permanent-tombstone admission path.

`SessionLifetime`, `EphemeralRunId`, and `IncarnationId` are removed. The old
durable/ephemeral identity distinction did not describe two identities after
reuse was forbidden; the meaningful boundary is whether a runtime has been
bound to a store. `SessionCommitStore::ensure_session_bound` retains that
boundary and checks the store's permanent deletion fence.

Store-less sessions use their host-provided session id anywhere stable
derivation is required. Ordinary history nodes derive from session id,
operation id, and ordinal. Frame nodes derive from session id and frame key.
Effect-journal identities and turn addresses likewise use the session id
without a second discriminator.

SQLite schema 20, SQLite effect schema 6, and PostgreSQL schema 28 are
reject-and-recreate boundaries. No old shape is migrated or dual-read.

## Consequences

- Deleting a session is final for that id in the store. A host reset creates a
  new id; it never deletes and reopens the old one.
- Permanent session-keyed await-event revocation is correct by construction.
  The ledger needs no clearing rule, resolving FIG-748's reuse hazard.
- Session-owned effect rows retire by session id. Process-owner incarnation
  fencing and replay-stream incarnation ids are separate concepts and remain.
- Hosts and third-party stores must implement the binding seam and preserve
  tombstones permanently. Lash detects reuse at creation rather than relying on
  every downstream identity preimage to carry a lifetime discriminator.
