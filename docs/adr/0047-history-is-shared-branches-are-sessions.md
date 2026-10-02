# History is shared; branches are sessions

## Status

Accepted.

## Context

Immutable history and mutable execution state need different ownership.
A branch must share retained transcript nodes without sharing another
session's admission, usage or continuation. A retry must adopt the topology
that storage actually realizes, not a fresh proposal beside a matching receipt.

## Decision

History is a shared immutable object graph. A session is a mutable execution
head over it. A branch is another session whose head points at a retained
node. Forking copies no history nodes and shares no mutable execution state.

Retryable history mutations carry `OperationId`. Ordinary node ids derive
from session id, operation id and append ordinal. Structural `FrameOpen` ids
derive from session id and frame key so process provenance can name a frame
before its surrounding commit. Intent hashing uses the typed
`lash-intent/v2` projection. Semantic content and topology participate;
transport authority, fencing and clock observations do not.

Stores derive commit identity from received content rather than accepting a
caller-supplied intent hash. Receipt replay validates the appropriate commit
and append identity and returns stored realization. Graph rows, head and
receipt publish atomically. The runtime adopts that realization, including
store-observed values, rather than its retry proposal.

`GraphAppend::Extend` appends create-only nodes; `PreserveHead` leaves graph
position unchanged. Agent Frames are immutable `FrameOpen` nodes. Parent ids
are stored edges, and the current frame is the nearest frame ancestor. A host
rewinds by retaining a target, creating another session there and switching
to it, rather than mutating immutable history.

SQLite uses one factory-wide durable-core database for atomic shared-history
head changes. Commit budgets are explicit host policy under ADR 0058, not a
backend's unconditional fixed cap. Checkpoints replace resumable state rather
than accumulate observation history (ADR 0048).

Reachability comes from parent edges and each session's retained revisions.
A fork adds a root over the shared prefix. A process registry row does not
implicitly root stored history; pins remain explicit. Effect journals
are owned by the configured engine, with stable identities joining their outcomes to
session commits across transaction domains.

A session id is host-provided and single-use in its store (ADR 0049).
History and frame identity therefore use that id directly. Ingress admission
and settlement use run bindings under the sealed shift fence (ADR 0101).

## Store leaf validation versus caller branch liveness

Store commits validate the expected head revision and parent the first append
on the stored leaf. Runtime preparation reloads that head before constructing
its append.

A caller's `requires_ancestor_node_id` asks whether the base it read is still
on the active path. It is not a head compare-and-swap. Concurrent content can
arrive after the base without invalidating work derived from that prefix.
The caller records any derivation claim in its payload rather than inferring
it from the new node's position.

## ADR 0024 applies directly at deletion

Destructive decisions derive live children and retained revisions in the
same transaction. Removing a head reclaims only ancestry that has no remaining
root or child. The walk stops at a shared prefix. There is no cached incoming
reference count or scrub authority. PostgreSQL serializes affected graph rows
and root changes with row locks.

Processes belong to a different store family. Their liveness comes from
process truth rather than an implicit stored-history root.

## Consequences

A fork has its own session id, ledger, ingress and mutable continuation over
shared history. A receipt match permits adoption only of the recorded result.
Missing leaves or frame ancestors are typed corruption or conflicts, not an
upsert invitation. Caller branch-liveness checks permit concurrent appends and
do not grant exclusivity. Full graph replacement and in-place rewind are
rejected because they weaken immutable identity and branch fencing.

Reclamation is host-scheduled and lifecycle-gated. SQL session commits and
engine journals remain separate transactions with explicit stable identities.

## Attachment prefix retention

Attachment reads address bytes directly; the host owns authorization.
Publishing committed attachment references acquires durable referrer edges in
the boundary transaction. Acquisition requires positive upload evidence and
no physical deletion in flight; absence fails atomically with
`UnknownAttachment`. The transaction does not invoke host blob code.

Session attachment edges remain live while retained history from that session
survives. This is conservative at session granularity rather than an exact
node-to-attachment map. ADR 0124 governs the explicit referrer-edge and byte
reclamation contract, including other referrer kinds. Graph retirement does
not make a still-live attachment referrer disappear.

## Implementation

- [Commit and node identity](../../crates/lash-core-store/src/store/commit_identity.rs) and [graph append algebra](../../crates/lash-core-store/src/store/mod.rs).
- [SQLite commit](../../crates/lash-sqlite-store/src/persistence/session_commit.rs) and [PostgreSQL commit](../../crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs).
- [Reachability retirement](../../crates/lash-sqlite-store/src/persistence/mod.rs) and [attachment edge acquisition](../../crates/lash-sqlite-store/src/attachments.rs).
