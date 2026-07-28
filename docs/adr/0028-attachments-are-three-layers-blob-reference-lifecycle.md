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

**Layer 2 — reference tracking is lash-owned.** The `AttachmentManifest` layer
with `(session_id, attachment_id)` identity, a write-ahead intent recorded
before the bytes land, and a commit stamped inside the session-store transaction
remains the core boundary. Each intent also records the durable logical owner
that can finish it: a turn id, a process id, or no owner for direct host puts.
What changed is the facade: the trait-implementing
`SessionScopedAttachmentStore` decorator became a concrete `SessionAttachmentStore`
struct — the one and only attachment surface the runtime and its consumers see.
It binds a flat backend, a manifest, and a `session_id`, and exposes inherent
`put`/`get`/`delete`, `backend()`, `session_id()`, and a forwarded
`persistence()`. Turn and recovered-process execution scope the facade while
they run; owner identity is written in the same manifest mutation as the
intent. There is no process-local pending-id correctness state. At final commit,
the store stamps every row owned by the committing turn inside the graph and
checkpoint transaction. The explicit tool-output/message attachment-id union
remains as adoption for cross-turn references. The critical
invariant is the **session-boundary guard** that replaces physical isolation:
`get(id)` first asks the manifest whether this session holds a ref (intent or
commit) for `(session_id, id)`, via a new `AttachmentManifest::holds_ref`, and
returns `NotFound` for an unknown ref *before* it ever touches the backend. A
turn in session A therefore cannot resolve session B's blob by guessing a
content hash — prompt-injected tool calls and buggy plugins are semi-trusted, so
the guard, not a physical copy, is what keeps them apart. `delete(id)` now drops
only this session's manifest ref and leaves the backend bytes in place (a
semantic change); the bytes die later via GC once no session references them.
Ephemeral runtimes with no durable reference store wrap their backend in a
`SessionAttachmentStore` carrying a `NoopAttachmentManifest` (which imposes no
guard and records nothing), so every consumer sees exactly one facade type.

**Layer 3 — lifecycle is host policy, with lash levers and a bundled default.**
The per-session `reclaim_orphaned_attachments` sweep is deleted. In its place is
`reclaim_unreferenced_attachments(root_set, backend, grace_period_ms)`:
mark-and-sweep GC that enumerates every blob via `list`, computes the live root
set, and deletes every blob no session references. Every committed ref is live.
An uncommitted intent becomes eligible only when it is older than the retention
cutoff **and** its durable owner is proven dead: a different, later turn commit
supersedes a turn owner, and pruning the durable process row kills a process
owner. Direct host puts have no owner proof and retain the legacy age-only
fallback. Elapsed time is therefore policy after terminal proof, never the
correctness argument that a replayable execution is dead. The other use of the
window is a *delete-time freshness re-check*: the `list` snapshot's modification time is
stale by the time the sweep reaches a candidate, so before deleting, the sweep
re-stats the blob via `AttachmentStore::head` and spares any blob touched inside
the window. That closes the race where a new intent plus a `put` of the same
content id lands after the root snapshot — every backend's `put` refreshes the
blob's modification time on a dedup hit (the file store rewrites/utimes, S3 PUTs
unconditionally, the in-memory store restamps) precisely so this re-check sees it.

**The delete window and its residual.** The freshness re-check keys off the blob's
mtime, which a clock-skewed or coarse-timestamp backend can under-report, so after
it the sweep does a second, authoritative guard: a *targeted root re-check* for the
single candidate id (`AttachmentRootSet::has_live_attachment_ref` — Postgres and
SQLite each use one indexed `SELECT`, while in-memory uses a
map lookup), skipping any blob a session re-referenced since the snapshot. This
probe is reliable precisely because of the layer-2 write-ahead ordering: the facade
records the manifest intent *before* the backend `put`, so a root exists no later
than the bytes it protects. A residual window remains between that probe and the
physical `delete` — bounded to the single-digit milliseconds of one root-set query
plus one backend delete, and it cannot be closed further without a cross-store
transaction the blob backend and the reference store do not share. When a ref lands
in exactly that window the bytes are already unrecoverable, but the property that
saves correctness is *put-always-writes*: every backend `put` physically rewrites
absent content (verified per backend), so the referencing session's next `put`
self-heals the blob. The sweep does one more single-id root check immediately
*after* the delete; if a ref appeared, it records the id in the reclamation report's
`deleted_while_referenced` field and logs at error level, so this rare, self-healing
event is surfaced to the operator rather than lost silently.
The grace period is a post-terminal retention choice and need not bound turn or
replay duration. `commit_refs` only updates existing intent rows and never
re-inserts them, so adoption cannot resurrect a ref whose proven-dead owner was
already reconciled. The sweep **continues
past per-blob delete failures**, collecting failed ids into its report rather than
aborting on the first error.
The root set is a factory-level lever: `SessionStoreFactory::live_attachment_refs`
(surfaced to the GC through a blanket `AttachmentRootSet` impl) answers in one
transaction on the global manifest table for Postgres (conditionally delete
aged, owner-dead intents, then read the survivors), and in one transaction on
SQLite's factory-wide durable-core catalog. Process-owner death proof is an
explicit host-supplied capability: SQLite hosts call
`SqliteSessionStoreFactory::new_with_process_registry`, while Postgres hosts
call `PostgresStorage::session_store_factory_with_shared_process_registry` only
when the registry shares that database. Unwired factories warn and conservatively keep
process-owned intents forever; they never infer a sibling path or query a table
that merely happens to have the expected name. SQLite validates the configured
database's process-registry schema when GC attaches it (not during ordinary
session open), then evaluates process-row absence in the same
conditional delete rather than through a read-then-forget race. The catalog is
the sole SQLite root-set authority; if it cannot be opened, the sweep aborts
rather than treating unreadable live references as absent. Because
`delete_session` releases a session's manifest rows, and those rows are exactly
what the root set enumerates, a deleted session's blobs become unreferenced and
GC collects them — correct by construction, verified for both SQLite and
Postgres through manifest row deletion. The stated assumption is
that the backend instance is **exclusive to this lash deployment**: a blob with
no live ref is genuinely garbage only if every writer to that bucket/directory is
this deployment's sessions. The reference host (lash-cli) wires the bundled
sweeper as one post-startup background pull with a generous grace period, logging
its report; lash-core itself gains no scheduling infrastructure — the lever plus
one host pull is the end-state.

Terminal process retention owns the two internal session stores created while a
process runs: `process-env:<id>` and `process-session-turn:<id>`. Pruning first
releases their attachment-intent rows and deletes those stores, then removes the
terminal process row. PostgreSQL performs the sequence in one transaction;
SQLite deletes the explicitly wired session files before a transaction
revalidates and removes the terminal row. Failures therefore retain a terminal
process leak instead of deleting state that a live process could still need or
making a process-owned intent appear dead too early.

Why the global root set makes shared bytes safe where physical copies were the
blunt fix: once every session's refs are visible in one root set, GC can prove a
blob is unreferenced *everywhere* before deleting it, so two sessions can share
one physical blob and neither loses its content when the other is swept or
deleted. The physical per-session namespace bought isolation by never sharing at
all — paying a full byte copy per session and taxing every backend with session
awareness — to solve a problem the manifest already models precisely. Reads are
now gated by the reference layer, storage is dumb and deduplicated, and lifecycle
is the host's to schedule.

## Shared-history ruling (implementation pending)

ADR 0047 supersedes two parts of the Layer 2 account above.
The ruling deletes `AttachmentManifest::holds_ref` and the per-session read
guard. Reference tracking establishes liveness, not authorization: Lash has no
principal with which to decide whether a caller may read a content-addressed
blob. After that deletion, a Host Application that exposes attachment reads to
untrusted callers must enforce its own scope. Without that host check, the
factory-wide blob store is an existence oracle for a caller that can obtain or
compute a content hash.

The deletion and its replacement are not shipped. `holds_ref` currently gates
attachment reads, and the attachment-edge relations below do not exist. The
FIG-653 L7 retention work owns both changes.

The ruling makes attachment liveness an explicit stored edge. History-node
attachments, checkpoint attachments, process/artifact roots, and host roots
will be named relations in the durable core. A commit will insert its attachment
edges and retire the corresponding write-ahead intents in the same transaction;
reclaiming a node will remove its edges in that transaction. A branch will share
the historical prefix and therefore its attachment edges without copying
manifest rows.

Once implemented, this replaces the inferred predicate over commit receipts
described above. Receipt retention will no longer silently change the GC oracle:
edges will be truth, and attachment/blob counts only a cache over those edges.
Layer 1 remains a dumb content-addressed blob store, while Layer 3 remains
host-scheduled lifecycle policy with explicit bounds.

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
