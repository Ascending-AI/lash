# Attachments are three layers: dumb blob storage, lash-owned references, host lifecycle policy

The attachment subsystem had conflated three concerns into one `AttachmentStore`
trait: it stored bytes, tracked which session owned them (via `*_for_session`
methods that wrote into a physical `sessions/<sha256(session_id)>/` namespace),
and reclaimed orphans (a per-session `reclaim_orphaned_attachments` sweep). The
physical namespace was a blunt instrument: it copied identical bytes once per
session to keep sessions from reading each other's content, and it forced every
backend — the flat blob stores that hosts supply — to understand sessions. We
split the subsystem into three layers with sharp boundaries, and this ADR
records the model. It **supersedes the physical per-session namespace design**.

**Layer 1 — blob storage is host-supplied dumb infrastructure.**
`AttachmentStore` is now a flat, content-addressed blob trait: `put(bytes, meta)
-> AttachmentRef`, `get(&id) -> StoredAttachment`, `delete(&id)`, `list() ->
Vec<StoredBlobRef>`, plus a `persistence()` descriptor. It has no notion of
sessions — no `*_for_session` methods, no `unscoped_backend`/`bound_session_id`
hooks, no manifest-commit tracking. The physical layout is flat
(`sha256/<prefix>/<hash>`) in the file store, the S3 store, and the in-memory
store; the `sessions/<sha256(session_id)>/` namespacing and
`session_storage_namespace` are gone. Identical bytes written by any number of
sessions resolve to **one** physical blob, and that dedup is now natural and
intended. Puts and deletes stay idempotent, missing blobs map to a typed
`NotFound`, and `list` exists solely to feed mark-and-sweep GC. The file store
additionally hardened its `Durable` claim: it stages into a per-write unique
sibling (pid + counter, not a fixed `.tmp`) and fsyncs the parent directory
after `rename`, so the directory entry itself is crash-durable and a stale
staging file from a prior crash never blocks a later write.

**Layer 2 — reference tracking is lash-owned.** The `AttachmentReferrers`
port is the core boundary. A digest is held by referrer edges, the same
referrers that hold artifacts (ADR 0113, ADR 0124): the execution journal of
the turn that put it, the process record of a process runtime, a session that
committed or was sent it, or one upload of a session. There is no
`(session_id, attachment_id)` identity, no owner column and no commit stamp;
an attachment's URI is derived from its digest. Write state is separate from
the edges: `begin_attachment_write` records a pending write and its claim's
edge before the bytes land, and `complete_attachment_write` records upload
evidence. The runtime's one attachment surface is the concrete
`RuntimeAttachmentStore`: it binds a flat backend, the referrer port and the
runtime owner that holds its puts, and exposes inherent `put`/`get`/`delete`,
`backend()`, `holder()` and a forwarded `persistence()`. A session turn binds
the facade to its execution journal while it runs, so its puts are held by
that journal until it settles; a put outside any turn is held by a fresh
upload of its session until the upload expires; a process runtime's puts are
held by its record. The boundary commit acquires the session's edge on every
attachment the committed messages and tool outputs name
(`acquire_attachment_refs`), so a turn put nothing commits is released when
the turn's journal settles. FIG-2501 supersedes the original session-boundary
read guard: reads resolve content addresses directly, and hosts own
authorization. `delete(id)` forgets the holder's lasting edge only when
retained history no longer needs it; physical deletion remains GC's job.
Ephemeral facades record no durable roots.

**Layer 3 — lifecycle is host policy, with lash levers and a bundled default.**
The per-session `reclaim_orphaned_attachments` sweep is deleted. In its place is
`reclaim_unreferenced_attachments(root_set, backend, policy)`:
mark-and-sweep GC that enumerates every blob via `list`, computes the live root
set, and deletes every blob no referrer holds.
The policy refuses an empty root set when any blob is
deletion-eligible unless the host explicitly authorizes deleting everything
unreferenced; emptiness cannot itself prove that destructive interpretation.
A digest is live iff it has an edge or a pending write: there is no age
predicate and no owner-death predicate, and a referrer's end is decided by
the cleanup executor (ADR 0113 §2.5, ADR 0124), never by the sweep. The grace
period's one use is a
*delete-time freshness re-check*: the `list` snapshot's modification time is
stale by the time the sweep reaches a candidate, so before deleting, the sweep
re-stats the blob via `AttachmentStore::head` and spares any blob touched inside
the window. That closes the race where a new intent plus a `put` of the same
content id lands after the root snapshot — every backend's `put` refreshes the
blob's modification time on a dedup hit (the file store rewrites/utimes, S3 PUTs
unconditionally, the in-memory store restamps) precisely so this re-check sees it.

**The delete window is fenced, not raced (FIG-1259).** A re-check is still a
read: between deciding a digest is unreferenced and physically deleting it, a
session can record an intent and write the same content, and no amount of
re-reading closes that. The sweep closes it with a clockless CAS state machine in
the lash-owned root authority — the same durable store the edges live in, so
the two sides meet inside one transaction rather than across two reads.

Per digest the state is `Free`, `Condemned`, or `Deleting`, and ownership
transitions are conditional mutations. A failed delete records a retry deadline
on the store clock; that deadline grants no ownership. The writer's `put`
goes through `AttachmentReferrers::begin_attachment_write`, which mints a fresh
`write_id` for the attempt, records the write-ahead intent under it, *and*
resolves the condemnation in one mutation: it claims a `Condemned` digest with
that id, and against a `Deleting` digest or a phase owned by another writer it
records nothing and retries, because bytes written into an in-flight delete are
lost bytes. The sweep condemns before deleting
(`AttachmentRootSet::condemn_attachment`, refused if any root or intent exists),
arms the delete (`arm_attachment_delete`, refused if a writer revoked), issues
the physical delete only for an armed digest, then retires the condemnation row
after success (`settle_attachment_condemnation` with `Deleted`). A fenced final
`HEAD` that finds the bytes already absent settles the same way without issuing
a redundant delete. Every sweep transition belongs to the sweep pass's
generation, and a sweep adopts and finishes a crashed predecessor's
condemnations before it condemns anything new (ADR 0067 §6).

**Adoption is gated on positive upload evidence, not on a tombstone
(FIG-2795).** A completed upload records the digest's upload evidence through
`complete_attachment_write`, which is fenced and matched on `write_id`: a
permit superseded by a newer attempt, or retired by its referrer's end,
records nothing and fails the put with `StoreError::StaleWritePermit`. A
digest is adoptable iff it has upload evidence and no `deleting`
condemnation exists; `acquire_attachment_refs` validates every digest in the
batch before writing anything, so evidence outlives the uploader's own
edge. Otherwise acquisition refuses with `StoreError::UnknownAttachment` and
writes nothing. Condemnation deletes the digest's evidence under the same
fence, so a swept digest is unadoptable until somebody puts the bytes again —
the byte-absence fact is the absence of evidence rather than a phase that has
to be preserved. GC liveness (the root set) and adoption eligibility (upload
evidence) stay distinct predicates: a pending write is a root the moment it
exists, and only a completed upload makes those bytes adoptable. A failed put
releases only its own attempt: `abort_attachment_write` deletes its pending
write, and its edge only when no evidence and no sibling write remains; a
stale permit settles nothing at all. A condemnation claim is a foreign key to
the pending write that holds it, so completing, aborting or ending the write
releases the claim structurally; when another root exists, abort retires the
old unarmed condemnation before the older sweep can arm it. A failed delete returns `Deleting` to `Condemned`, where a
writer may reclaim the digest and later sweeps retry it with capped backoff; a delete that
keeps failing stays listed with a typed stall until success (ADR 0067 §6).
Whoever loses a CAS
yields: a writer parks and retries, and a sweep that meets a peer's condemnation
defers the digest to the next sweep. Nothing waits on a
lease or a TTL, and no SQL/blob-store atomicity is needed, because the
authority's state machine — not the backend — decides whether bytes may die. A
parked writer does pace its re-acquires with a bounded backoff rather than
spinning, since a physical delete against remote storage takes real time; that
delay paces CAS attempts. Amended 2026-09-29 (FIG-4139): failed sweep
deletes also use a durable retry deadline on the store clock, with capped
backoff. Elapsed time makes a retry eligible; it never proves a sweep owner
dead, revokes a fence, or expires a row.
Amended 2026-09-29 (FIG-4100): clearing a condemnation left behind by a
sweeper that died mid-delete is no longer host policy, and the host lever
`AttachmentRootSet::release_attachment_condemnation` is deleted. Attachment GC
is lash's own protocol, and a host cannot judge fencing safely: the next sweep
adopts a dead sweeper's `Condemned` or `Deleting` row under a generation that
proves the predecessor dead, and finishes the delete (ADR 0067 §6). A restoring
writer's claim is never adopted. The separate
`recover_abandoned_attachment_write` lever deletes the pending write that
holds a digest's claim, but only after the host establishes that the writer
is no longer running. It applies abort's rules: retire the old condemnation to
`Free` when another root exists, otherwise leave it `Condemned` and
sweep-owned. A fresh re-put
then claims any retained phase normally. lash expires neither state on a
timer.

The freshness re-check survives as what it always was, a cheap pre-filter. It
now runs only after the sweep arms the digest as `Deleting`; writers arriving in
that window record no intent and retry after the sweep settles the phase.

Answering `Fenced` is a claim about ten methods across two traits —
`AttachmentReferrers::begin_attachment_write`, `complete_attachment_write`, and
`abort_attachment_write` plus the root set's `fence`,
`begin_attachment_sweep`, `adopt_attachment_condemnations`,
`condemn_attachment`, `arm_attachment_delete`,
`settle_attachment_condemnation`, and `recover_abandoned_attachment_write` —
and a partial implementation is worse than none,
because it silences the warning while keeping the loss. The sweep
downgrades its own report to `BestEffort` when a self-declared fenced authority
cannot condemn, but it cannot detect a missing writer half; that one is on the
implementer.

A root authority that implements neither half reports
`AttachmentGcFence::BestEffort` and runs the legacy path: a targeted root
re-check before the delete and the same probe after it, with any late root
recorded in the report's `deleted_while_referenced` field and logged at error
level. That is *detection telemetry* for a window it cannot prevent, not a
remedy — such deployments should point the backend at recoverable deletion
(object-store versioning or a quarantine prefix) so the reported coordinates lead
to bytes that can be restored. Under a fenced authority the same field is a fence
alarm: it must stay empty, and a non-empty list means the transitions are not
atomic with intent recording, or two authorities are pointed at one backend.

The grace period is a post-terminal retention choice and need not bound turn or
replay duration. `acquire_attachment_refs` writes an edge only for a digest
with upload evidence, so adoption cannot resurrect a ref whose bytes were
reclaimed: the condemnation that preceded the delete removed that evidence. The sweep **continues
past per-blob delete failures**, collecting failed ids into its report rather than
aborting on the first error.
The root set is a factory-level lever: every `SessionStoreFactory` must explicitly
implement `AttachmentRootSet`, so being accepted by the GC is a declaration that
the factory owns the complete root-set answer. An empty answer is not destructive
authority by itself: when the backend has a deletion-eligible blob, the policy
must explicitly authorize the delete-all interpretation. A factory that cannot
enumerate its roots must return an error from `live_attachment_refs`. The sweep
then lists the backend only to decide whether anything is past the grace window:
an eligible blob propagates the enumeration error before any delete, while a
no-candidate sweep returns a report carrying the failure so hosts can distinguish
it from a healthy empty sweep. Decorators that can answer must deliberately
delegate. The
implementation answers from the referrer edges and pending writes in one
transaction: on PostgreSQL over the deployment's tables, on SQLite over the
factory-wide durable-core catalog. The catalog is the sole SQLite root-set
authority; if it cannot be opened, the sweep aborts rather than treating
unreadable live references as absent. A deleted session's edges end through
the cleanup executor once its retained history is gone (ADR 0124), and a
pruned process's edges end with its record, so their blobs become
unreferenced and GC collects them — correct by construction on both backends.
The stated assumption is
that the backend instance is **exclusive to this lash deployment**: a blob with
no live ref is genuinely garbage only if every writer to that bucket/directory is
this deployment's sessions. A host wires the bundled sweeper as one post-startup
background pull with a generous grace period, logging its report; lash-core itself
gains no scheduling infrastructure — the lever plus one host pull is the end-state.

Why the global root set makes shared bytes safe where physical copies were the
blunt fix: once every session's refs are visible in one root set, GC can prove a
blob is unreferenced *everywhere* before deleting it, so two sessions can share
one physical blob and neither loses its content when the other is swept or
deleted. The physical per-session namespace bought isolation by never sharing at
all — paying a full byte copy per session and taxing every backend with session
awareness — to solve a problem the referrer edges already model precisely. Reads resolve content addresses, storage is deduplicated, and lifecycle
is the host's to schedule.

## Shared-history implementation (FIG-2501 / FIG-653)

Holding a referrer edge establishes liveness, not authorization. The
attachment read guard and its membership capability are removed. History point
reads still enforce graph membership, and process waits enforce observer
subscription relationships. Hosts own authorization at their edge.

A boundary commit adopting a stored content address also acquires the
receiving session's edge, even if that session never put the bytes. Root acquisition and graph publication share one transaction; failure
leaves no new root. Adoption uses the existing digest fence, revoking an unarmed
condemnation and refusing a physical delete already in flight. After FIG-2795 it
also requires positive upload evidence for the digest, or the boundary
refuses with
`StoreError::UnknownAttachment`, because nothing proves the host blob store ever
held these bytes. The caller re-reads or re-puts through its own facade and
retries; the failed boundary publishes no edge or graph state. The receiver's
edge then follows the same session retention rule as any other.

FIG-2795 replaces the terminal `reclaimed` phase with upload evidence, and
the condemnation phase vocabulary narrows to `condemned` and `deleting`. Both changes move durable
values, so PostgreSQL component 92 and SQLite session schema 61 are
reject-and-recreate boundaries. Condemnation rows remain bounded to one row per
distinct condemned digest, and are deleted outright on a completed delete. A
restoring put's claim is its pending write's token, cleared when that write
completes, aborts or ends; no host blob access enters a store transaction.

A deleted session's edges are retained while any of its graph nodes remain
retained by a head, child, or pin: session deletion arms
`AwaitSessionGraphRetired`, and the executor ends the session's edges only
once the final prefix retirement leaves no untombstoned node (ADR 0124).
`forget_attachment_ref` respects the same graph-retention precondition.

This conservatively retains suffix attachments too, until the last retained
prefix is gone. Exact node-to-attachment edges would provide finer
reclamation, but are not required for safety.

## Cross-version consequences

This cutover changes the durable attachment format, so — per lash's
reject-and-recreate doctrine (there is no migration chain) — durable state from
before this release is **rejected loudly and recreated**, not migrated. The
attachment manifest is gated by a store schema-version bump: SQLite session
databases originally moved to `user_version = 10` and the single Postgres schema
component to version 11. Durable owner binding subsequently bumps them to 12 and
14 respectively. A pre-cutover database is rejected at open with a "delete and start
fresh" error, because its committed manifest rows carry canonical URIs and blob
references that named the old physical per-session layout, which the flat
content-addressed store cannot resolve.

Consequently, the **old `sessions/` blob trees are unreachable garbage** once the
manifest is recreated: nothing references them and no code path can read them.
Operators delete them manually. The exact patterns:

- File backend: the `sessions/<session-hash>/...` subtrees under the attachments
  root (the flat store now writes only under `sha256/<first2>/<hash>`).
- S3 backend: the `<prefix>/sessions/...` key prefix (the flat store now writes
  only under `<prefix>/sha256/<first2>/<hash>`).

No lash lever touches these paths; they are outside the flat store's `sha256/`
keyspace, so GC never enumerates them.
