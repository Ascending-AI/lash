# Every durable row names one owner and one reclaim trigger

## Status

Accepted.

## Context

A retention policy answers how long data remains. It does not identify whose
state it is, when its owner releases it, or what evidence permits deletion.
Without those answers a sweep can delete live data, retain ownerless data
forever, or present an enumeration failure as a healthy empty pass.

Session and process stores own storage reclamation. The effect host owns
execution-state retirement. Hosts choose explicit retention horizons and invoke
factory-wide maintenance. These authorities need one ownership vocabulary.

## Decision

### 1. Universal ownership axiom, with no exceptions

Every durable row class names exactly one owner and one reclaim trigger class.
The owner is the session, turn, process, effect group, factory or exact referrer
whose obligation the row records. Trigger classes are owner-delete cascade,
terminal-state vacuum and an explicitly armed reclaim pass. An unowned table
is a missing domain decision, even if it is small or bounded by age.

#### Reclaim is severance, not sweeping

Singly-owned rows are reclaimed by the operation that severs their ownership.
They need no reference counting or tracing collector. Receipts and other
retained evidence remain subject to
[ADR 0023](0023-retention-stays-a-parameterized-host-lever.md).

Multi-referenced data uses exact reference edges. The severing transaction
checks indexed edge absence before freeing shared data. Stored reference counts
cannot replace the edges: a count is another state that can drift from the data.
Artifacts use the referrer-edge contract of
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).

Verification is read-only. Repair is a separate, explicit operation that reports
its work and may free proven unreachable edges and blobs. `gc_unreachable` is
that catalog-wide repair lever. Correctness does not depend on a periodic repair
pass. SQLite's repair first removes unrooted checkpoint projection edges, then
deletes blobs unreachable from retained roots; PostgreSQL applies its storage predicate
transactionally. An enumeration failure cannot authorize repair.

#### Armed reclamation, defined

Reclamation is armed by recorded severance or an explicit host retention
operation. A factory pass is bounded to the evidence its own authority can
prove; a clock cannot stand in for owner death. Factory-scoped residue and
shared bytes can require work after the severing transaction commits.
Attachment condemnations have their own generation-fenced recovery protocol
under §6. They do not borrow a session-execution lease.

A dormant or rotated session is not proof that an active execution ended.
Storage residue still needs the owner frontier and enumeration evidence that
justify its reclamation.

| Reference-edge class | Owner | Reclaim trigger |
| --- | --- | --- |
| `checkpoint_blob_refs` / `lash_checkpoint_blob_refs` | The session-owned checkpoint root named by `checkpoint_ref` | Deleting an unreferenced root cascades its edges in the owning transaction. Explicit global repair also severs edges of roots held by no retained revision before deleting any component blob. |
| Artifact referrer edge | The exact frame, process, subscription, start, execution or host pin holding it | Severance by that referrer; artifact cleanup can free the descriptor after the last edge disappears. An id stored without an edge retains nothing. |

### 2. Terminal-before-reclaimable

Reclamation is armed by the owner's terminal transition or severance. A grace
period can defer that reclamation; it cannot establish terminality. The host's
retention bound and relevant projection acknowledgement constrain execution
of reclaim after eligibility exists. No automatic age rule may delete a live
owner's continuation.

Both SQL backends guard reclaim markers with named CHECK constraints.
A parent-end plan's `settled_at_ms` requires delivered cancellation work;
claimed or stalled work remains retained. Delivery settlement arms the marker
atomically and honors its claim token. A subscription change's `deleted_at_ms`
requires the tombstoned lifecycle in its retained JSON record. Missing lifecycle
tags cannot satisfy that guard under SQL NULL semantics.

A fired trigger occurrence belongs to its committed delivery fan-out. A
zero-match fan-out is complete at ingest; a matched fan-out waits until its
last delivery ends and the retained delivery rows can be removed. The absence
of surviving delivery rows authorizes the occurrence cascade.

A non-fired occurrence is factory-owned audit history, not a delivery-free fired
occurrence. Delivery-fan-out retention cannot select it. The explicit
`TriggerStore::prune_non_fired_occurrences(cutoff_epoch_ms)` operation removes
only non-fired rows recorded strictly before the host's cutoff. The factory has
no terminal frontier, so the host states that audit-retention decision each
time. `audit_retained_count` reports retained audit rows without making a
completed sweep incomplete.

### 3. Scope-split trigger topology

Owner-scoped reclamation runs in the owning transaction. Factory-wide work runs
only through explicit host-invoked maintenance. Session deletion reclaims its
eligible rows and unreferenced blobs while retaining protected receipts under
ADR 0023. A failed cascade fails the delete. A session transaction does not
silently become a whole-catalog blob sweep.

#### Trigger-store ownership map

| Row class and scope | Owner | Reclaim trigger |
| --- | --- | --- |
| Process definition artifact | Its exact referrers | Cleanup after the last referrer edge is severed. Host names and versions belong in host storage. |
| Session subscription | Registering session | The deleted-session frontier and zero remaining deliveries. A tombstone remains a `Revive` fence while that session can still speak. |
| Host or platform subscription tombstone | Host or platform namespace | Permanent name fence; the namespace has no deleted-session frontier or purge lever. |
| Session ingress tombstone | Its session | Host vacuum after its terminal transition, or session deletion. A command retains its first receipt and applying operation key until vacuum. |
| Session mutation receipt | Registering session's replay eligibility | Host-selected retention, including after session deletion. |
| Host or platform mutation receipt | Namespace replay eligibility | Explicit host retention through the trigger-store primitive. |
| Fired occurrence | Committed delivery fan-out | Transactional reconciliation after zero delivery rows remain. |
| Non-fired occurrence | Factory audit history | Explicit non-fired audit cutoff, never delivery reconciliation. |
| Occurrence tombstone | Factory redelivery fence for one reclaimed occurrence identity | Explicit host deletion through `forget_trigger_tombstones(written_before_epoch_ms)`, after the host vouches that its source stops redelivering the selected identities. Reclaim never deletes these tombstones (ADR 0021, FIG-4610). |
| Trigger delivery | Process run | Process-retention policy under ADR 0021. |
| Attachment condemnation | Factory condemnation protocol | Adoption and discharge under §6. |

Trigger reconciliation deletes exact terminal delivery candidates, then applies
occurrence and deleted-session subscription cascades in one transaction. A
failure rolls the transaction back. Receipts whose owner cannot be established
from their retained record remain protected; an irreversible hashed receipt key
is not evidence for assigning an owner.

### 4. Report contract: a three-way split

A pass reports completed work, incomplete work, or a stop. The concrete
maintenance vocabulary distinguishes all five reachable arms:

- `MaintenanceSweep::Swept` is a completed pass with reclaimed items and no
  failures or deferrals.
- `MaintenanceSweep::NothingToDo` is completed enumeration with nothing to
  reclaim.
- `MaintenanceSweep::Incomplete` is a completed pass with unfinished work,
  including per-item attachment failures, live-peer deferrals and typed stalls.
- `MaintenanceStop::Refused` is an unmet destructive precondition.
- `MaintenanceStop::Failed` is a backend failure that stops the pass.

The first three are `Ok(report)`. Both stop arms are
`Err(MaintenanceFailure { stop, partial })`, preserving the report accumulated
before the stop. A refused or failed pass cannot be rendered as a clean zero.
A transaction that rolls back reports zero committed deletions, not attempted
ones. A completed attachment pass can report failed deletes as incomplete
without pretending the entire backend enumeration failed.

The retained-evidence report names receipts, usage deltas, attachment roots
and retired runtime-operation scopes. Those counts let the host see which
ownership transitions occurred.

### 5. Witnessed emptiness

Every destructive boundary that uses an enumerated live set consumes a typed
complete-enumeration witness. An empty, complete scope differs from an error,
a partial scan or an unwired source. None of those incomplete results can
construct the witness or authorize a delete.

`AttachmentRootSet::live_attachment_refs` returns `CompleteAttachmentRoots`.
Only the shared collector constructs it. The collector visits every kind
allowed by `ArtifactReferrerKind::holds_attachments`, the remaining edge kinds,
and pending writes. For each source it reads ordered pages of 256 digests plus
one lookahead row and follows every continuation. A missing source or failed
continuation stops the pass before any physical delete, including completion
of a crashed predecessor's condemnation. The backend's `attachment_root_page`
returns unproven rows, not deletion authority.

The SQLite and PostgreSQL blob collectors consume `CompleteEnumeration` before
severing dead checkpoint projection edges or deleting unretained blobs. Their
closed inventories include every retained revision, complete checkpoint
component traversal, and SQLite's artifact pointers. PostgreSQL stores artifact
bytes separately. Artifact deletion consumes `CompleteArtifactReferrers`, which
covers every `ArtifactReferrerKind::ALL` entry after a complete edge read, and
retains the transactional `NOT EXISTS` predicate as its concurrency check.
An unfinished `ReclamationEnumeration` returns `IncompleteEnumeration` with the
unproven scope and source. It has no conversion to a deletion witness.

Ancestry retirement consumes a private `RetirableAncestryNode`, constructed
only after the complete child and retained-revision check under the same
writer transaction or node lock as the retirement.

Owner-scoped cascades and terminal-state vacuum do not decide liveness from a
caller-supplied root set. Their owner transition and exact SQL edge predicates
remain inside the transaction. Ending a referrer's edges consumes the resolved
cleanup fact; it does not infer the referrer's death from enumeration or age.

A refusal is `Err(MaintenanceFailure { stop: Refused(..), partial })`, never a
healthy report. Backend enumeration errors are `Failed` and retain their
structured cause. The attachment pass may report an empty backend with an
unavailable root source because no byte's fate depended on those roots; when
bytes exist it fails or refuses before destruction. A host's empty-root policy
is applied only after complete enumeration and cannot substitute for it.

### 6. In-flight destruction: adoption-first, generation-fenced

After complete root enumeration, a fenced attachment sweep adopts incomplete
condemnations from proven-dead predecessors. The factory condemnation protocol owns those rows.
The pass completes each adopted row before listing and condemning new candidates.

`begin_attachment_sweep` mints a monotonically increasing durable generation.
Creation, adoption, arming and settlement compare the condemnation's recorded
`sweep_generation` with the pass's own generation. SQLite holds liveness in a
process registry; PostgreSQL holds a session advisory lock on a dedicated
connection keyed by catalog and generation. A predecessor is dead only when
its liveness authority permits adoption. A timed-out lock probe defers; it
does not prove death.

Adoption requires an older, dead generation, no restoring writer token and a
due retry. Competing passes claim the row through CAS. A physical-delete
failure preserves the condemnation, its attempt count and error. Bytes already
absent permit successful completion; a backend error does not. A successful
delete retires the condemnation and its stall together.

A refused delete stalls immediately; a retryable failure stalls after
`MAX_ATTACHMENT_DELETE_ATTEMPTS`, which is five. Later sweeps retry stalled
rows under the same liveness and generation fence. Each failure schedules a
store-clock deadline with exponential backoff from one second to a fifteen-minute
cap. Attempt counts saturate at the signed 32-bit storage bound. A deadline
paces retries; it never expires a row or proves an owner dead.

`list_condemnations` exposes phase, provenance, attempt count, last error and
typed stall. `stalled_ids` keeps the report incomplete. There is no operator
re-arm or condemnation-release lever. A restoring writer claims a surviving
condemnation with its opaque token and clears it only after restoring the bytes.

Effect-group retirement likewise severs owned state rather than expiring it.
The engine retains an identity fence and discharges group cleanup as a whole
under ADR 0065 and ADR 0099.

## Alternatives considered

Per-class age policies without owners cannot decide whether live data is
reachable. Retention stays subordinate to ownership and explicit host bounds.

Commit-triggered factory sweeps introduce catalog-sized work into a bounded
owner transaction. Explicit levers let the host schedule and observe it.

Stored reference counts can disagree with the real edges. Exact edges make
severance and remaining ownership one transactional predicate.

Empty reports for enumeration failure erase the difference between nothing
present and nothing known. Typed stops preserve the failure and completed work.

A timer or host judgement of sweeper death cannot fence a late physical delete.
Generation CAS and backend liveness prove which pass owns the condemnation.

## Consequences

Every durable table has a reviewable owner and reclaim trigger. Destructive
maintenance reports its stops and partial work. Host cutoffs bound retained
evidence, while physical deletion and interrupted deletion have separate
ownership protocols. Complete typed witnesses enforce complete source and page
coverage at the reclamation boundary.

## Executable evidence

- [Storage maintenance contract](../../crates/lash-core-store/src/store/mod.rs#L1695),
  [SQLite repair](../../crates/lash-sqlite-store/src/graph.rs#L131) and
  [PostgreSQL repair](../../crates/lash-postgres-store/src/postgres/runtime_persistence/maintenance.rs#L69)
  define explicit reclamation and its transactional predicates.
- [Maintenance outcomes](../../crates/lash-core-store/src/store/maintenance.rs#L1)
  and [retention counts](../../crates/lash-core-store/src/store/retention.rs#L15)
  implement §4. [Outcome laws](../../crates/lash-conformance/src/conformance/store_maintenance_outcome.rs#L1)
  cover failures and witnessed emptiness.
- [Trigger retention contract](../../crates/lash-core-execution/src/triggers.rs#L1693)
  and [audit cutoff](../../crates/lash-core-execution/src/triggers.rs#L1940)
  separate fired fan-out and non-fired history.
- [Sweep](../../crates/lash-core-store/src/attachments.rs#L819),
  [enumeration outcomes](../../crates/lash-core-store/src/attachments.rs#L883),
  [SQLite liveness](../../crates/lash-sqlite-store/src/attachments.rs#L299),
  [PostgreSQL liveness](../../crates/lash-postgres-store/src/postgres/attachments.rs#L263)
  and [condemnation SQL](../../crates/lash-store-sql/src/attachment/condemnation.rs#L1)
  implement §6. [Cold-reopen adoption laws](../../crates/lash-conformance/src/conformance/attachment_condemnation_recovery.rs#L1)
  cover interrupted passes and competing sweepers.
- Store laws run on SQLite file, SQLite memory and PostgreSQL. Effect-host laws
  run on the in-process Restate server double, live Restate and lash-sim's
  in-process effect host. Upgrade proofs use the synthetic-next tier.

- [Complete attachment root collector](../../crates/lash-core-store/src/attachments/root_enumeration.rs).
- [Reclamation enumeration witnesses and partial-scan laws](../../crates/lash-core-store/src/store/enumeration.rs).
- [Skipped-kind and truncated-page laws](../../crates/lash-conformance/src/conformance/attachment_referrers.rs).
