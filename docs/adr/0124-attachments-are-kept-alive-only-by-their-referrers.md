# 0124: Attachments are kept alive only by their referrers

## Status

Accepted 2026-09-30 (FIG-4215). Amends ADR 0028 (the lifecycle layer of
attachments), ADR 0049 (runtime-internal session ids), ADR 0107 (process
identities) and ADR 0113 (the referrer vocabulary and the cleanup executor).

## Context

An attachment's bytes were kept alive by a manifest row keyed by
`(session_id, attachment_id)`, with an optional owner (a turn or a process)
and a `committed_at` stamp. Process runtimes had no session, so lash minted
synthetic ones (an environment session and a turn session per process), admitted a
catalog row for each, and deleted them again at prune. A process's
attachments were rows of that synthetic session. Liveness was a mix of
edges, age (an uncommitted intent older than a grace window was forgotten)
and owner death (a process-owned intent died with its process, which the
root set had to prove through a registry it was wired to).

That model had three faults. A process runtime pretended to be a session,
so every interface a process crossed carried a session id that named
nothing. Liveness had three sources, two of them clocks and proofs outside
the store that held the edge. And a value delivered from one runtime to
another carried stored attachments whose edges belonged to the producer, so
the consumer held the bytes only as long as the producer happened to.

ADR 0113 already gives lash one vocabulary for "who holds this": a
referrer, a guarded claim, a permanent fence, and a cleanup obligation the
executor resolves. This decision puts attachments on it.

## Decision

### 1. A referrer holds attachments exactly as it holds artifacts

The shared `ArtifactReferrer` holds artifacts and attachments alike; it
keeps its name. Its claims, canonical codec, permanent fence
(`referrer_fences`) and cleanup obligation are ADR 0113's, cited here and
not restated.

Four kinds may hold an attachment (`ArtifactReferrerKind::holds_attachments`):

| Kind | Acquired by | Guard | Ends when |
|---|---|---|---|
| `execution` (journal `j`) | a put in a session runtime bound to `j`; a delivery into any scope that is not a process | `AwaitJournal` | `j` settles (ADR 0113). |
| `process_record` (`p`) | every put in `p`'s runtime; every delivery into `p`; `p`'s terminal output and its start input | none | Prune's `Ended` record. |
| `session` (`s`) | the boundary commit, on the committed ids; an enqueue into `s` | none | Session deletion arms `AwaitSessionGraphRetired`; the executor ends it once `s` is deleted and no untombstoned graph node of `s` remains, so a fork keeps what its retained history names (FIG-653). |
| `upload` (`s`, `u`) | a put in a session runtime with no execution bound | `AwaitUploadExpiry { expires_at_ms }` | `expires_at_ms` passes, or `s` is deleted or absent. Each put mints a fresh `u`, so one expiry never fences a later upload. |

No other kind holds an attachment, and an attachment end carries nothing:
the receiver acquires before the source may end (§4), so `Ended.carries`
stays artifact-only.

### 2. The external-byte protocol

Bytes live outside the database, so the store keeps three facts per digest
beside the edges:

- **Pending writes.** `begin_attachment_write` records a write with a fresh
  write id and the claim's referrer, inserts the claim's edge, and arms the
  claim's guard, all before any byte is written. `complete` records upload
  evidence and deletes the pending row; it never inserts an edge, so a write
  that outlives its referrer's end recreates nothing. `abort` deletes the
  pending row and, when no evidence and no sibling write exists, the edge.
- **Upload evidence.** One `attachment_uploads` row per digest, earliest
  write wins. A digest with no evidence, or one being deleted, cannot be
  acquired: `acquire_attachment_refs` refuses the whole batch with
  `UnknownAttachment`.
- **Condemnation.** The sweep condemns a digest in the authority that holds
  its roots, and that same transaction deletes its evidence. A condemnation
  claim is a foreign key to the pending write that took the digest back
  (`ON DELETE SET NULL`): completing, aborting or ending the write releases
  the claim structurally.

The sweep runs over one root predicate, **I-root: a digest is live iff it
has an edge or a pending write.** There is no age predicate and no
owner-death predicate. `AttachmentReclamationPolicy::grace_period_ms` bounds
only the delete-time freshness re-check.

An attachment URI is derived from its digest; the store keeps no second
copy of it.

### 3. The verbs and their refusals

The `AttachmentReferrers` port has eight verbs: `begin_attachment_write`,
`complete_attachment_write`, `abort_attachment_write`,
`acquire_attachment_refs`, `forget_attachment_ref`,
`end_attachment_referrer`, `session_referrer_state` and
`attachment_referrers`. Each is one transaction. Every verb that names a
referrer refuses:

- a kind that does not hold attachments, with `ReferrerKindRefused`;
- a fenced referrer, with `ArtifactReferrerEnded` (except `forget` and
  `end`, for which a fenced referrer is a no-op);
- and, for `acquire`, a digest with no evidence, with `UnknownAttachment`.
  A refusal writes nothing for any id in the batch.

`end_attachment_referrer` fences the referrer, deletes its pending writes
(releasing their condemnation claims) and deletes its edges. It reclaims
nothing: bytes go only through the sweep.

On PostgreSQL every transaction takes its advisory locks in one global
order, skipping any class it does not touch: session-history locks by
session id; referrer locks (`lash-artifact-referrer:<kind>:<id>`, one key
space shared with artifacts, taken in one sorted batch); artifact locks;
attachment digest locks by digest; the sweep generation. SQLite serializes
on its one writer.

### 4. Delivery is acquire, record, release

Whenever a value carrying stored attachments moves from a producer to a
receiver, the receiver acquires its own edge before the value is recorded
where the receiver can read it, and the producer's edges may end only after
that record exists.

The receiving claim is one function of the receiving scope,
`receiving_claim(scope)`: a process receives through
`unguarded(ProcessRecord(p))`, every other scope through
`guarded(Execution(journal), AwaitJournal)`. A process never holds through
its journal: prune retires the journal before it removes the row, and the
terminal output needs the row anyway.

- **Parked and group calls.** The terminal resolver acquires before it
  resolves the waiter's key: on Restate the journaled step
  `process-attach-acquire` in `LashProcessAttach`, in-process the
  `AttachTerminal` task. The waiter's journal records the value when the key
  resolves; `release_consumer_hold` runs after.
- **Direct awaits and process-to-process delivery.** A direct
  `ProcessCommand::Await` is re-expressed as `AttachTerminal` plus a durable
  wait on a derived key, so every terminal reaches its receiver through the
  one resolver above. The key is the invocation's `AwaitEventKey` with wait
  identity `Custom { key: "process-await:<process id>:<effect id>" }`: the
  effect id makes two awaits of the same child from one scope two waits.
  The wait races the turn cancel and the process cancel as before. A turn
  stop that wins releases that wait as cancelled, so the await then arms a
  second attach on `process-await:<process id>:<effect id>:after-turn-cancel`
  and reads the cancelled process's terminal from it.
  In-process, the `Await` arm acquires after the terminal returns and before
  the outcome is recorded.
- **Queued inputs.** The enqueue transaction acquires `Session(s)` on the
  batch's stored ids and records the row together; an `UnknownAttachment`
  refuses the enqueue. A pending input therefore resolves its bytes when its
  turn commits however long it waited, even past the upload expiry of the
  put that produced it. Process wakes carry text only, so a wake delivers
  no attachment.
- **Terminal publication.** Before a registry records an output, the caller
  acquires `ProcessRecord(p)` on the output's stored ids. An engine terminal
  whose source was already swept is recorded as the typed failure
  `process_result_attachment_unavailable`; an external or host completion is
  refused with `ProcessOutputAttachmentUnavailable` and nothing is recorded.
- **Start inputs.** Inside the journaled start step the registration
  commits first, because it mints the id, and then the step acquires
  `ProcessRecord(p)` on the input's stored ids. The starter's own edge
  cannot end before its journal settles, which is after the step; a replay
  of the step answers `Existing` and acquires again.
- **The boundary commit** acquires `Session(s)` on the committed ids, all or
  nothing. The committed ids are every stored attachment the committed
  history names: tool outputs, omitted calls, message parts, and each
  retained output (FIG-1643). A tool result's retained block is read from
  its message part. A code cell's retained prints and finish value live in
  protocol records the commit cannot read, so the turn driver notes them
  from the cell's recorded response, and a replay notes the same ones.

A delivery whose acquisition finds no evidence answers `SourceGone`: the
producer's edges were already ended and swept. That is the typed outcome of
a race against prune, never a silent loss.

**A turn put nobody references dies at journal settlement.** A put under a
turn is held by the turn's `Execution` referrer. If no committed message or
tool output names it, nothing else acquires it, and it is unreferenced once
the journal settles.

### 5. The upload expiry lever

An unbound put's upload edge expires at the store clock at the put plus
`upload_expiry_ms`, fixed when the put is made. Its guard is owed at that
instant: a relay visit before it defers the row to the expiry (or the
maximum backoff, if sooner), never past it. The host sets it with
`LashCoreBuilder::attachment_upload_expiry(Duration)` (default 24 hours);
every runtime the host builds, session or process, inherits it.

### 6. A process runtime is keyed by its id

A process is not a session. Its runtime, `ProcessRuntimeContext`, is built
from the host's ports, the environment its start captured and the
controller the worker admitted; it has no session state, no catalog row and
no plugin session of a fake session. The synthetic sessions, their
admissions and their prune-time deletion are gone.

Interfaces both kinds of runtime cross are keyed by `RuntimeOwner`
(`Session(s)` or `Process(p)`, displayed `session:<id>` and
`process:<id>`); a dispatch is keyed by `ExecutionOwner`
(`SessionFrame { session_id, agent_frame_id }` or `Process { process_id }`).
A session-only operation asked of a process refuses with
`NotASessionRuntime`; a process is never handed its originator's session as
a stand-in for its own id.

- **Visibility.** A process sees the processes whose recorded lineage names
  it as the immediate starter (`ProcessLineage::starter()`), plus run-local
  possession.
- **Tool intents** are keyed by `RuntimeOwner`. The identity encoding tags
  the owner, and the durable `tool_intent_submissions` column is `owner` on
  both backends.
- **A group tool child a process opened** runs, when its opener is not live,
  under a process runtime built from the child's recorded environment.
- **A subagent spawned inside a process** parents under the session that
  originated the process chain, read by name, and is caused by the process.
  A host-originated chain and a `ParentFork` capability refuse (ADR 0116).

## Consequences

- Liveness has one source per digest: its edges and pending writes. The root
  set needs no registry, no clock and no proof of owner death.
- Every delivery path acquires before it records; a lost race is a typed
  failure the receiver records, not bytes that vanish later.
- Process runtimes no longer mint, admit or delete sessions, and nothing a
  process holds is keyed by a session it does not have.
- A turn's scratch puts that nothing commits are reclaimed without an age
  window.
- The shapes changed in place: the tool-intent identity encoding, the
  submissions column and the attachment tables have no older reader to
  upcast for.
