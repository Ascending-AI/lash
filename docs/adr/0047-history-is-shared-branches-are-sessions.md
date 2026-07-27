# History is shared; branches are sessions

## Status

accepted

## Context

Session history used to mix two different kinds of state. Immutable transcript
nodes belonged to one session, while the same session row also carried mutable
execution state and replacement operations over those nodes. Retries could
therefore agree on semantic content while realizing different graph topology,
SQLite could only isolate sessions by putting them in separate files, and a
branch either copied history or weakened the session-level fences that make
execution replayable.

The durable-core cutover adopts one model:

> A node is history. A session is execution. History is shared; execution never
> is. A branch is another session over the same history.

This is one decision, not independent storage refinements. Shared history
requires globally sound node identity and reachability. Keeping execution
private to a session preserves the head-revision fence, claims, queues, effect
replay, waits, and usage accounting when a branch is created.

## Decision

History is a shared immutable object graph. A session is a mutable execution
head over that graph, and a branch is a new session whose head points at an
existing retained node. Nodes are never copied for a branch, and execution
state is never shared between sessions.

Every retryable history mutation carries an operation identity. Ordinary
history-node ids derive from the session id, operation id, and append ordinal;
structural `FrameOpen` nodes use their deterministic session-and-frame-key
identity because process provenance must be able to name a frame before its
surrounding commit is realized. The commit intent is hashed through the typed,
allowlisted `lash-intent/v1` projection. Topology and semantic payload enter the
projection. Transport authority, fencing tokens, store-assigned facts, snapshot
bytes, and clock observations do not.

The intent hash detects divergence of **intent, not observation**. Excluding a
clock-derived value is insufficient by itself: a retry could otherwise keep its
new in-memory timestamp while adopting an older durable receipt. Excluded clock
values are therefore store-realized, returned on the receipt, and rehydrated
into the runtime so resident and durable state converge.

Two different guards close the retry boundary. On the write path the store
re-derives operation-derived node ids before mutation. On every path the runtime
compares its proposed graph realization with the receipt's
`realization_digest`. That digest is **store-recorded, not store-computed from
rows**: before writing, the store records a digest of attempt A's proposal in
the same transaction as the rows and receipt. Transaction atomicity proves that
the rows exist. The digest proves, on a later receipt hit where no write-time
validation runs, that attempt B proposed the same topology attempt A durably
claimed.

The graph mutation algebra is append-only. `GraphCommitDelta::Append` is the
only graph-changing variant; `Unchanged` represents a commit whose history head
does not move. Appends are create-only and validate their leaf before writing.
Full replacement, session reset, fresh-open replacement, orphan healing, and
in-place rewind are removed rather than emulated. A host rewinds by retaining a
target, creating a session there, switching to it, and deleting the old session
when appropriate.

Agent Frames are immutable `FrameOpen` nodes. `parent_node_id` is a real indexed
edge, and the current frame is derived from the nearest `FrameOpen` ancestor
rather than from a mutable frame vector. A turn commits its graph exactly once.
The former durability tier is removed: replay ownership remains a mechanical
property of the configured effect controller, while any end-to-end durability
claim belongs to the Host Application.

SQLite uses one factory-wide durable-core database so a new session head and
its references to shared history can change atomically. Because that topology
widens the blast radius of a writer lock, commits are rejected before opening a
transaction when they exceed the measured 512-node or 1 MiB logical-payload
budget.

Reachability is defined by stored edges. Session heads and child nodes keep
history alive; retained continuation anchors and attachment/blob relations are
explicit edges rather than inferred predicates. Forking adds a new session root
and shares the prefix. Ownership is therefore reachability, not
producer-session exclusivity. Processes remain independent durable objects and
stay outside stored history-node counts.

## ADR-0024 is re-applied at deletion

_This contradicts ADR-0024 (drainage reads over artifact refcounts), worth
reopening because history now uses store-maintained node reference counts._
The resolution is to re-apply ADR-0024's safety rule at the destructive decision
point, not to reverse it.

ADR-0024 had two objections. Its coupling objection does not transfer: artifact
truth lived in a physically separate store, so a counter and its truth could
not share a transaction. The factory-wide durable-core database keeps history
edges and their cached counts together, and every mutation is
co-transactional. Its drift objection does transfer: atomicity cannot prevent a
missed mutation site.

Edges are truth; counts are a cache. A count that is too high leaks storage and
is recoverable. A count that is too low can delete a reachable shared prefix,
and `vacuum` can make that loss permanent. Consequently every decrement to zero
must re-derive the node's incoming references from indexed parent and
session-root rows in the same transaction. A mismatch aborts with typed
`NodeRefcountDrift`; a catalog-wide `verify_node_refcounts` scrub detects the
non-destructive high-count direction. The cost is paid only when a node is
about to become reclaimable, where ADR-0024's concern is load-bearing and the
query is bounded to that node's indexed incoming edges.

Process roots are deliberately excluded from stored node counts. They live in a
different store family, so their liveness continues to be recomputed from
process truth on demand, exactly as ADR-0024 originally required.

## Consequences

- Forking does not copy nodes, usage, claims, queues, waits, effect-journal
  entries, or mutable Agent Frame state. The new session gets an independent
  execution identity and ledger over a shared historical prefix.
- A receipt match is not permission to trust the retry's resident proposal.
  The runtime adopts store-realized observations only after the realization
  digest matches.
- A missing leaf, parent, or frame ancestor is corruption, not a repair
  invitation. Append conflicts are typed and never become upserts.
- Reclamation is host-scheduled and bounded. Every reclaim primitive takes a
  watermark; receipt and journal cleanup is terminal-gated; attachment
  liveness is an explicit stored edge.
- Lash owns the effect-journal contract while the configured substrate owns the
  journal. The session commit and effect journal remain separate transactions
  joined by stable operation identity.
- [ADR-0026](0026-model-capability-is-host-supplied-data.md) needs no amendment.
  Removing component-declared durability is its host-supplied-capability
  doctrine reaching the last runtime exception.
- [ADR-0029](0029-claims-are-generation-fenced-under-the-session-lease.md)
  needs no amendment. Its ruling remains correct; FIG-641 tracks the missing
  store enforcement.
- [ADR-0046](0046-process-transitions-are-events-record-is-a-fold.md) needs no
  amendment. Process event folding and weak observation are orthogonal to
  immutable session history, and processes remain outside stored history-node
  refcounts.
