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
enforce a narrower and sufficient invariant inside each store: once a
host-facing id has durably materialized session metadata in that store, the id
is never reused there. Deleting an id that never materialized is a no-op.

## Decision

A host-facing session id is host-provided, non-empty, identifies exactly one
session lifetime, and is used at most once in a store. Lash otherwise treats it
as an opaque UTF-8 string; host transports may impose narrower syntax and
length rules. Deleting a materialized host-facing id writes a permanent
tombstone. Creating or forking to a deleted id fails with
`StoreError::SessionDeleted`, whose message states that the id was used and
deleted. Retention and vacuum never remove this identity evidence.

Runtime-internal process session ids are lash-minted and hosts cannot address
them, but they are used once on the same terms: process pruning deletes them,
and a delete writes the same permanent tombstone. Pruning them without one was
tried and does not hold. The deleted set is not only a reuse fence, it is the
frontier a delete reads to reclaim tombstoned history rows whose owner is
already gone — a node can be tombstoned after its owner's delete, and no
session-scoped vacuum can reach it, because the owning id is unbindable either
way. An untombstoned process id therefore strands rows in the store forever.
Two rows of identity evidence per pruned process is the price of a store that
drains; the alternative was unbounded leaked history.

Creating an already-live id remains idempotent. Opening an existing id remains
an explicit operation. `fork_at` already takes the new host-provided session id
and uses the same permanent-tombstone admission path.

`SessionLifetime`, `EphemeralRunId`, and `IncarnationId` are removed. The old
durable/ephemeral identity distinction did not describe two identities after
reuse was forbidden; the meaningful boundary is whether a runtime has been
bound to a store. `SessionCommitStore::admit_and_bind_session` takes the
complete `SessionBinding` (id and relation), returns
`SessionAdmission::{Created, Rebound}`, materializes metadata, and checks both
the handle binding and permanent deletion fence atomically. The runtime reads
the materialized identity back before committing, so a loose third-party store
cannot silently alias another session.

Store-less sessions use their host-provided session id anywhere stable
derivation is required. They therefore require a distinct id for every session
within one process; `LashCore` rejects store-less id reuse for its process
lifetime. A durable effect host shared beyond that process requires the host to
provide uniqueness across that wider domain. Ordinary history nodes derive
from session id, operation id, and ordinal. Frame nodes derive from session id
and frame key. Effect-journal identities and turn addresses likewise use the
session id without a second discriminator.

SQLite schema 20, SQLite effect schema 6, and PostgreSQL schema 28 are
reject-and-recreate boundaries. No old shape is migrated or dual-read.
Await-event promise keys carry an explicit `v2` epoch. Old in-flight Restate
invocations cannot resume across this cutover: operators must drain them or
purge the Restate state before upgrading, otherwise their old promises are
orphaned under the prior key.

## Consequences

- Deleting a session is final for that id in the store. A host reset creates a
  new id; it never deletes and reopens the old one.
- Permanent session-keyed await-event revocation is correct when the deletion
  tombstone, revocation ledger, effect journal, and Restate state share one
  lifecycle. They are one trust domain and must reset together. In particular,
  SQLite's catalog and effect database must not be wiped independently.
- Session-owned effect rows retire by session id. Process-owner incarnation
  fencing and replay-stream incarnation ids are separate concepts and remain.
- Hosts and third-party stores must implement the admission seam and preserve
  every deletion tombstone permanently — host-facing ids and lash-minted
  runtime-internal process session ids alike, whichever delete path wrote it.
  A store that keeps only the host-facing half satisfies the reuse fence but
  breaks reclaim: the delete arm reads the same set to decide which owners are
  gone. Lash detects reuse at creation rather than relying on every downstream
  identity preimage to carry a lifetime discriminator.
- A pruned process id is permanently unbindable as a session owner, and stays
  so after `compact_process_tombstones` removes the process tombstone that
  fenced its re-registration. Compaction frees registry rows, never ids:
  re-registering a compacted process id starts a process whose derived session
  stores cannot be created, failing with `StoreError::SessionDeleted` naming an
  internal id the host never chose (`process-env:<id>` or
  `process-session-turn:<id>`). Process ids are single-use for the store's life.
- Fork materialization followed by observer publication spans transaction
  domains. The fork relation retains the selected process ids as durable apply
  intent until every idempotent observer event commits. A crash burns no
  visibility choice: opening the single-use fork id replays the pending intent,
  and that id can never alias a later session lifetime. Replay reasserts the
  resolved selector wholesale: an observer removed before the intent is
  cleared can be added again, because clearing the durable host decision is
  the commit point.
