# History is shared; branches are sessions

## Status

accepted. Shipped mechanics are stated in the present tense. Rulings whose
implementation remains pending are labelled as such and name their owning
ticket.

## Context

Session history used to mix two different kinds of state. Immutable transcript
nodes belonged to one session, while the same session row also carried mutable
execution state and replacement operations over those nodes. Retries could
therefore agree on semantic content while realizing different graph topology,
SQLite could only isolate sessions by putting them in separate files, and a
branch either copied history or weakened the session-level fences that make
execution replayable.

The concrete correctness failure was a phantom graph: a retried commit found a
successful receipt while performing no write, then the runtime adopted its new
proposal's node ids even though those rows had never been realized. The next
append targeted a leaf that did not exist.

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

The intent hash detects divergence of **intent, not observation**. Each store
derives that hash from the typed commit content it receives; callers supply
only the operation identity and cannot stamp a hash over a different proposal.
On a receipt hit, the store compares its newly derived hash with the persisted
hash before returning the stored result. On the write path it also re-derives
operation-derived node ids, then writes nodes, head, and receipt atomically.
The former caller-supplied hash and redundant `realization_digest` echo are
removed.

Clock-derived values remain outside intent. Stores realize those observations,
return them on the receipt, and the runtime rehydrates them so resident and
durable state converge.

The graph mutation algebra is append-only. `GraphCommitDelta::Append` is the
only graph-changing variant; `Unchanged` represents a commit whose history head
does not move. Appends are create-only and validate their leaf before writing.
Full replacement, session reset, fresh-open replacement, orphan healing, and
in-place rewind are removed rather than emulated. A host rewinds by retaining a
target, creating a session there, switching to it, and deleting the old session
when the host no longer wants that execution.

Agent Frames are immutable `FrameOpen` nodes. `parent_node_id` is a real indexed
edge, and the current frame is derived from the nearest `FrameOpen` ancestor
rather than from a mutable frame vector. A turn commits its graph exactly once.
The runtime durability tier on effect hosts, process engines, and cancellation
receipts is removed. `EffectReplayOwnership` replaces it with the mechanical
fact of whether the runtime or its controller owns replay; any end-to-end
durability claim belongs to the Host Application.

SQLite uses one factory-wide durable-core database so a new session head and
its references to shared history can change atomically. Because that topology
widens the blast radius of a writer lock, commits are rejected before opening a
transaction when they exceed the measured 512-node or 1 MiB logical-payload
budget. A realistic fully captured session checkpoint measures 2.3–4.4 KiB and
stays flat across 100 turns: it is a replacement snapshot at each boundary and
can shrink, not an accumulator. Reaching the 1 MiB cap requires roughly 1 MB of
live globals in one turn. The cap is a live-state capacity limit, not a
time-dependent failure.

Reachability is defined by stored edges. Session heads, child nodes, and retained
continuation anchors keep history alive. Forking adds a new session root and
shares the prefix. Ownership is therefore reachability, not producer-session
exclusivity. Processes remain independent durable objects and stay outside
stored history-node reachability.

Effect-journal identity and lifecycle retirement are implemented as recorded by
ADR 0025. The attachment/blob part remains pending: explicit attachment-edge
relations, bounded reclaim surfaces, and the `holds_ref` deletion belong to the
FIG-653 L7 retention work. Current attachment liveness still uses manifest rows
and commit-receipt predicates.

## ADR 0024 applies directly at deletion

History retirement is the plain agreement required by
`docs/adr/0024-drainage-reads-over-artifact-refcounts.md`, not an exception to
it. Parent edges, live session heads, and continuation anchors are the only
reachability truth. There is no `incoming_refs` cache, drift error, or scrub
API.

Every destructive decision derives live children, heads, and anchors in the
same transaction. Deleting a session removes its head, then reclaims only its
producer nodes that no child, head, or anchor can still reach; the ancestry
walk stops at a shared prefix. PostgreSQL locks affected node rows so commits,
forks, pin changes, and deletion serialize their root and edge mutations.

Process roots are deliberately excluded from stored history reachability. They
live in a different store family, so their liveness continues to be recomputed
from process truth on demand, exactly as ADR 0024 requires.

## Consequences

- Forking does not copy nodes, usage, claims, queues, waits, effect-journal
  entries, or mutable Agent Frame state. The new session gets an independent
  execution identity and ledger over a shared historical prefix.
- A receipt match is permission to adopt only the store-recorded result. The
  store derives the retry's intent hash from the received commit and rejects a
  different proposal under the same operation identity.
- A missing leaf, parent, or frame ancestor is corruption, not a repair
  invitation. Append conflicts are typed and never become upserts.
- Reclamation remains host-scheduled. Effect-journal retirement is shipped and
  lifecycle-gated. The remaining L7 ruling gives `vacuum`, receipt pruning, and
  attachment reclamation explicit bounds and replaces inferred attachment
  liveness with stored edges; FIG-653 owns that implementation.
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
  immutable session history, and processes remain outside stored history
  reachability.
