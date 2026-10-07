# 0124: Attachments are kept alive only by their referrers

## Status

Accepted.

## Context

Attachment bytes live outside the store that records their users. Session
history, executions, process records and uploads have different
lifetimes. A receiver needs its own hold before recording a delivered value,
so the producer's lifetime cannot determine whether the receiver can read it.

The referrer, guarded claim, permanent fence and cleanup obligation of
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md) give
attachments the same lifecycle vocabulary as artifacts. The store holding
those claims owns the root predicate.

## Decision

### 1. A referrer holds attachments exactly as it holds artifacts

The shared `ArtifactReferrer` holds artifacts and attachments alike; it
keeps its name. Its claims, canonical codec, permanent fence
(`referrer_fences`) and cleanup obligation are ADR 0113's, cited here and
not repeated.

Six kinds may hold an attachment (`ArtifactReferrerKind::holds_attachments`):

| Kind | Acquired by | Guard | Ends when |
|---|---|---|---|
| `execution` (execution `j`) | a put in a session runtime bound to `j`; a delivery into any scope that is not a process | `AwaitJournal` | `j` settles (ADR 0113 §2.5). |
| `process_record` (`p`) | every put in `p`'s runtime; every delivery into `p`; `p`'s terminal output and its start input | none | Prune's `Ended` record. |
| `start_input` (`key`, `starter`) | the start input's stored ids, before registration | `AwaitStart { starter }` | Cleanup acquires the retained input under `ProcessRecord(p)` before ending this starter's staging. Without a retained row, the starter's settled execution ends staging. |
| `session` (`s`) | the boundary commit, on the committed ids; an enqueue into `s` | none | Session deletion arms `AwaitSessionGraphRetired`; the executor ends it once `s` is deleted and no untombstoned graph node of `s` remains, so a fork keeps what its retained history names. |
| `upload` (`s`, `u`) | a put in a session runtime with no execution bound | `AwaitUploadExpiry { expires_at_ms }` | `expires_at_ms` passes, or `s` is deleted or absent. Each put mints a fresh `u`, so one expiry never fences a later upload. |
| `source` (`AwaitEventKey`) | K4 process-terminal delivery, before publishing the immutable result seal on its wait row | source holder fence | Logical Run or scope retirement ends the source. |

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

Root enumeration returns `CompleteAttachmentRoots`, minted only after the
shared collector reads every edge kind and pending write and exhausts every
page. The sweep requires that witness before any physical delete, including
an adopted condemnation's completion (ADR 0067 §5).

### 3. The verbs and their refusals

The `AttachmentReferrers` port has eight verbs: `begin_attachment_write`,
`complete_attachment_write`, `abort_attachment_write`,
`acquire_attachment_refs`, `forget_attachment_ref`,
`end_attachment_referrer`, `session_referrer_state` and
`attachment_referrers`. Each is one transaction. A verb that opens a new
edge or write permit — `begin` or `acquire` — is checked against the
referrer's kind and its ended fence; `forget` and `end` check the kind.
These verbs refuse:

- a kind that does not hold attachments, with `ReferrerKindRefused`;
- a fenced referrer, with `ArtifactReferrerEnded` (`forget` and `end`
  open nothing, so a fenced referrer is a no-op for them);
- and, for `acquire`, a digest with no evidence, with `UnknownAttachment`.
  A refusal writes nothing for any id in the batch.

Completion and abort of an in-flight write are settled by the write
permit `begin` issued, not by a fresh referrer check: once the referrer
ends and its pending row is removed, completion refuses the typed
`StaleWritePermit`, and abort without a permit is an idempotent no-op.
The permit carries the referrer and kind `begin` checked, so SQLite's
completion and abort need no second kind check
(`crates/lash-sqlite-store/src/attachments.rs`, with
`abort_write_conn` at `crates/lash-sqlite-store/src/attachments.rs`);
PostgreSQL obtains one through referrer locking, which is equivalent
(`crates/lash-postgres-store/src/postgres/attachments.rs`).

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
`guarded(ReferrerGuard::Journal(execution))`. A process never holds through
its execution: prune settles the execution before it removes the row, and the
terminal output needs the row anyway.

- **Deferred and process-terminal delivery.** A `process_terminal` wait row
  names the receiver's source
  ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §6).
  Delivery acquires the receiver's attachment references before retaining the
  canonical result and sealing `Resolved(ref)` in the producer's terminal
  transaction. The Run records the terminal, then discharges the consumer
  hold. An ended receiver
  refuses delivery and cannot recreate a subscription. Permanent acquisition
  refusal stays typed; transient store faults retain retry ownership.
- **Direct awaits and process-to-process delivery.** A process await uses the
  same wait row and acquire-before-seal rule. An already-terminal process
  records its outcome after acquisition. Cancellation and revocation
  observations are recorded. No long attach execution is needed. A cancelled observer does not cancel the process unless
  its recorded cancel policy requires that separate obligation.

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
- **Start inputs.** Before registration, the recorded start acquires
  `guarded(ReferrerGuard::StartInput { start_key: key, starter })` on the input's
  stored ids.
  Unavailable input refuses the start before any process row is published.
  Registration mints the id, and the start acquires `ProcessRecord(p)`.
  Cleanup also acquires the retained record's input before ending
  `StartInput(key, starter)`, including when the caller abandons the start
  or an explicit `Ended` record ends staging. The upload expires independently.
  The starter's execution distinguishes staging claims across later uses of a
  pruned host key. A fenced attempt can adopt a retained row after acquiring
  its record; it cannot publish a new row with unstaged input. A repeated start
  answers `Existing` and acquires again. An acquisition's
  `Contended` or `StorageFailure` retains its typed retryable classification:
  the start records no refusal, and its retry acquires under the same process
  id and start key.
- **The boundary commit** acquires `Session(s)` on the committed ids, all or
  nothing. The committed ids are every stored attachment the committed
  history names: tool outputs, omitted calls, message parts, and each
  retained output. A tool result's retained block is read from
  its message part. A code cell's aggregate print archive and retained finish value live in
  protocol records the commit cannot read, so the turn driver notes them
  from the cell's recorded response, and a resumed turn notes the same ones.

A delivery whose acquisition finds no evidence answers `SourceGone`: the
producer's edges were already ended and swept. That is the typed outcome of
a race against prune, never a silent loss.

A delivery into a fenced receiver answers `ReceiverEnded`, distinct from
`SourceGone`: the producer can still hold all its bytes. K4 delivery to an
ended source completes without resolving or recreating its wait. The
receiver gains no edge. Genuine storage
faults retry the unrecorded acquisition; permanent compatibility refusals
are recorded once with their typed error.

**A turn put nobody references dies at execution settlement.** A put under a
turn is held by the turn's `Execution` referrer. If no committed message or
tool output names it, nothing else acquires it, and it is unreferenced once
the execution settles.

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
ownership of the node that claimed its actor; it has no session state, no catalog row and
no plugin session of a fake session. Process construction and prune operate
on the process id without admitting or deleting a synthetic session.

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
- **A process-owned tool call** runs under its admitted process runtime and
  recorded environment, including after another node claims the process.
- **A subagent spawned inside a process** parents under the session that
  originated the process chain, read by name, and is caused by the process.
  A host-originated chain and a `ParentFork` capability refuse (ADR 0116).

## Consequences

- Liveness has one source per digest: its edges and pending writes. The root
  set needs no registry, no clock and no proof of owner death.
- Every delivery path acquires before it records; a lost race is a typed
  failure the receiver records, not bytes that vanish later.
- Process runtimes hold attachments under their own ids, and nothing a
  process holds is keyed by a session it does not have.
- A turn's scratch puts that nothing commits are reclaimed without an age
  window.
- The pre-1.0 version freeze changes shapes in place; ADR 0115 governs
  upgrade read contracts.

## Implementation

- `crates/lash-core-store/src/artifact_referrer.rs` selects the six
  attachment-holding kinds; `crates/lash-core-store/src/attachments.rs`
  chooses the claim before a put.
- `crates/lash-core-store/src/store/attachment_referrers.rs` defines
  the eight verbs. `crates/lash-sqlite-store/src/attachments.rs` and
  `crates/lash-postgres-store/src/postgres/attachments.rs` implement them.
- `crates/lash-core-execution/src/runtime/attachment_delivery.rs` chooses
  the receiver's claim and acquires terminal and start input references.
- `crates/lash-core-execution/src/runtime/process/start_staging.rs`
  stages input before registration and acquires the process record afterward.
  `crates/lash-core/src/runtime/artifact_cleanup.rs` completes that
  acquisition before staging ends during recovery.
- `crates/lash-sqlite-store/src/persistence/turn_input.rs` and
  `crates/lash-postgres-store/src/postgres/runtime_persistence/turn_input.rs`
  acquire queued input in the acceptance transaction.
- `crates/lash-sqlite-store/src/persistence/session_commit.rs` and
  `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs`
  acquire committed ids in the boundary transaction.
- `crates/lash-core/src/runtime/process_runtime.rs` builds a process-owned
  runtime and attachment store.

Using age as the root predicate would forget a slow but live producer.
Consulting a process registry would add an independent owner-death proof to
attachment collection. Holding delivered bytes only through the producer
would let prune remove a value the receiver has recorded. Exact edges and
pending writes avoid those lifetime dependencies.
