# 0109: Store→engine delivery is an outbox of obligations

## Status

Accepted.

## Context

A SQL transaction can accept work before its engine delivery succeeds. The
accepted row must retain what delivery owes, when another attempt is due,
and whether an operator must intervene. Recovery must make progress past
one failing row and avoid making every deployment rescan settled history.

## 1. Interface

### 1.1 Obligation columns

An owning ledger row carries its delivery obligation. The common family is:

| Column | Meaning |
|---|---|
| `obligation_id` | Stable id of the armed obligation. |
| `obligation_state` | `due`, `claimed`, `delivered` or `stalled`. |
| `obligation_attempts` | Claims taken since arming or re-arming. |
| `obligation_due_at_ms` | Retry time while due, claim expiry while claimed. |
| `obligation_claim_token` | The claimant's settlement fence. |
| `obligation_stall_reason` | `attempts_exhausted`, `refused` or `undecodable`. |
| `obligation_last_error` | Message of the failed attempt. |
| `obligation_last_error_code` | Its typed error code; set exactly when the message is. |
| `obligation_settled_at_ms` | Delivered or stalled settlement time. |

A row owing nothing has no obligation id or state. Constraints keep the
state's claim, due time and settlement fields consistent. Indexed due reads
order by due time and id, include lapsed claims, and take a bounded page.
PostgreSQL uses `FOR UPDATE SKIP LOCKED`. A claim increments attempts, sets
its token and holds the row until its claim expiry.

`processes` has independent families: `start_obligation_*` for start and
`obligation_*` for terminal publication. Arming or settling one does not
change the other. `ArtifactCleanup` uses `artifact_cleanup_obligations`,
keyed by the artifact referrer; its owner and guard rules are in
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).
Wake deliveries have their own equivalent ledger vocabulary.

Evidence: `crates/lash-store-sql/src/obligation.rs:15`,
`crates/lash-store-sql/src/process/processes.rs:151`, and
`crates/lash-core-store/src/store/obligation.rs:163`.

### 1.2 Kinds

`ObligationKind::ALL` orders nine kinds: `Ingress`, `ControlIntent`,
`ScopeClose`, `ParentEnd`, `SessionDelete`, `TriggerDelivery`, `ProcessStart`,
`ProcessTerminal`, and `ArtifactCleanup`. Their snake-case labels are the
metric and drain-status keys. `ObligationKey` names the owning row of each
kind, including the artifact referrer for cleanup.

Every arm path obtains its id from `ObligationKey::id()`: the kind label
followed by byte-length-prefixed key parts in key order. Delimiters and Unicode
in a session, root or other key cannot alias another row. Re-arming the same
row retains this identity; claim tokens remain separate fencing identities.

Unknown stored vocabulary returns `StoreError::Incompatible` with
`UnknownVocabulary`. A key that cannot decode remains addressable by its
obligation id and is returned as an undecodable claimed row.

Evidence: `crates/lash-core-store/src/store/obligation.rs:35`,
`crates/lash-core-store/src/store/obligation.rs:135`, and
`crates/lash-core-store/src/store/obligation.rs:527`.

### 1.3 Store half: the ledger

`ObligationLedger` exposes arming, due claiming, by-id claiming, settlement,
explicit re-arm, stalled listing and standing reads. Producers arm inside
the transaction accepting their work. Repair arming affects only a row that
owes nothing. Backends provide one ledger per kind through `StoreSet`.

Due passes mint a fresh `ClaimToken`. A journaled claimant can derive its
token from the step's stable identity and obligation id. A by-id claim takes
a due row, or refreshes a claim held by that same token without incrementing
its attempts. It cannot take a claim another claimant holds. Once a due
pass retakes a lapsed claim under a fresh token, the old claimant cannot
refresh or settle it.

Settlement compares the token and answers `Applied` or `ClaimLost`.
`Delivered` clears the error; `Retry` records the next due time and error;
`Stall` records its reason and error. `Defer` returns guarded cleanup to due
with attempts reset. Re-arm accepts only a stalled row and resets attempts.
Producers arm work due at once where delivery must not depend on a database
clock being ahead of the relay's host clock.

Evidence: `crates/lash-core-store/src/store/obligation.rs:401`,
`crates/lash-core-store/src/store/obligation.rs:555`, and
`crates/lash-core-store/src/store/obligation.rs:594`.

### 1.4 Engine half: the relay

Each `ObligationRelay` supplies its ledger, policy and idempotent delivery.
The core assembles every kind's relay in `ObligationKind::ALL` order and
refuses missing delivery dependencies as `ObligationRelayUnavailable`.

`deliver_now` claims and attempts a producer's immediate delivery.
`deliver_claimed` attempts a claim already taken in the producer's
transaction. `relay_due` claims a bounded page and attempts its rows
concurrently. Every delivery runs under its attempt budget.

A successful ordinary delivery settles `Delivered`. A consumer-settled
delivery, notably ingress, leaves its claim held when the engine accepts
the ask; the consumer's transaction delivers the row. A delivery that
settles its own row can leave the relay's settlement answering `ClaimLost`.

A refusal stalls as `refused`; an undecodable key stalls as `undecodable`.
A retryable failure backs off until the ceiling, then stalls as
`attempts_exhausted`. An exhausted consumer-settled claim stalls before
another ask. Guarded cleanup that is not yet owed defers rather than stalls.
One undecodable or slow row does not prevent other claimed rows from running.

Defaults are a 1-second initial backoff, 15-minute maximum backoff, 16
attempts, 60-second claim TTL and 30-second attempt budget. Backoff doubles
per attempt and starts at the attempt's start time. The host owns these
policy values; the attempt budget must remain below the claim TTL.

Evidence: `crates/lash-core/src/runtime/drive/relays.rs:148` and
`crates/lash-core-execution/src/runtime/drive/relay.rs:55`, `:255`, `:338`, `:388`.

### 1.5 Stalled surfacing

Ingress stalls appear as `TurnStatus::Stalled(StalledDelivery)`. Deployment
and generation drain status expose stalled-obligation counts and cannot
report drained while those counts are nonzero. Generation drain addresses
one build's retained work; deployment drain addresses the deployment.
Closing sessions also hold the drain while their delete remains owed.

`LashCore::stalled_obligations` lists stalled rows and
`LashCore::rearm_obligation` explicitly makes one due again. Nothing
implicitly re-arms a delivered process start or a stalled row. Operational
metrics record delivery outcomes, stalled counts and recovery-leader
standing.

Evidence: `crates/lash/src/core.rs:302`,
`crates/lash/src/core/drain.rs:38`, and
`crates/lash-core-execution/src/runtime/drive/relay.rs:243`.

### 1.6 Leader lease

The recovery leader row is scoped to storage and engine authority, named
`recovery:{turn_control_binding_id}`. It records holder, generation rank,
term, election time and expiry. Acquisition, renewal and resignation use
the database clock. A lapsed lease can be acquired; a higher rank can
preempt after minimum tenure. Changing holder increments the term.
Resignation expires the row rather than deleting it.

The host sets generation rank, default 0, and lease timings. Defaults are
15-second TTL, renewal every 5 seconds, 2.5-second request timeout,
2-second trust margin, 5-second follower retry plus up to 500 milliseconds
of jitter, and 30-second minimum tenure.

A deployment owns one holder and a separate election task. Host-clock trust
expires at renewal start plus TTL minus trust margin. Losing trust stops
leader duties. Drain and shutdown resign; dropping the deployment also
resigns a grant whose initiating caller lost its answer. The lease controls
load and is not an execution fence.

Evidence: `crates/lash-core-execution/src/engine/reconcile.rs:140`,
`crates/lash-core/src/runtime/recovery_lease.rs:151`,
`crates/lash/src/core/recovery.rs:76`, and
`crates/lash-store-sql/src/recovery_leader.rs`.

### 1.7 Duties

Every deployment serves its own handlers and immediate deliveries.
PostgreSQL permits due claims on every deployment through skip-locked reads;
SQLite restricts due claims to the leader. Parks, repair, drain handover,
park-feed compaction and opt-in evidence retention are leader duties.
All duties remain idempotent when leader activity overlaps.

Evidence: `crates/lash-core/src/runtime/recovery_lease.rs:151` and
`crates/lash-core/src/runtime/drive/reconcile.rs`.

### 1.8 Detection and delivery bounds

Recovery follows a 10-second fixed grid. Each kind's pass runs on its own
`RelayLanes` lane. The default tick waits at most 1 second on due passes
before running leader duties, and each delivery has a 30-second budget.
A busy kind is skipped until its pass finishes. Rows in one claimed page
run together, rather than waiting behind each other's delivery budgets.
Leader recovery arms run concurrently under an outer guard of `2W`, where
`W` is the host's tick wait (default 1 second). Each Restate paused-work,
lost-process and lost-root page shares one `W` deadline across its store
read, queries, outcomes and durable writes. A slow repair cannot hold drain
handover or the next interval tick beyond that guard.

With tick interval `T`, budget `B`, page store latency `S` and a free lane,
a due row is claimed within `T` of its due time. A busy lane can add `B + S`.
A lapsed claim first becomes eligible at claim expiry, and then follows the
same bound. SQLite failover also adds the election delay. Retry eligibility
is attempt start plus its backoff, so time spent delivering counts toward
the wait. A stalled row waits for explicit re-arm.

These bounds require sufficient page capacity and store and leader-duty
latency within the tick budget. They are not a throughput guarantee under
an unbounded incoming queue. Simulation varies scheduling to exercise loss,
retry, stalled delivery and failover.

Evidence: `crates/lash-core/src/runtime/drive/interval.rs:19`,
`crates/lash-core/src/runtime/drive/lanes.rs`, and
`crates/lash-core-execution/src/engine/reconcile.rs:105`.

## 2. Rationale

A durable obligation closes the failure window between SQL acceptance and
engine delivery. A best-effort send alone leaves accepted work without a
retry owner. Repeated full-table recovery scans spend work on settled rows
and give a persistent failure no explicit ceiling. Due indexes, bounded
claims and typed stalls make those costs and operator decisions explicit.

A single due-time column represents both retry eligibility and claim expiry
because a row cannot be due and claimed simultaneously. A separate side
outbox is useful when work has no owning row; ordinary obligations live on
the row that accepts them. Artifact cleanup needs its referrer ledger because
cleanup can outlive the referrer's authoritative row.

SQL remains the acceptance authority. Restate executes accepted work and
records execution; it is not another ingress authority.

## 3. Per-ledger mapping

| Kind | Owning ledger | Armed by | Delivered when |
|---|---|---|---|
| `Ingress` | `pending_turn_inputs`, `queued_work_batches` | Accepted ingress | Root admission selects the row in its transaction |
| `ControlIntent` | `control_intents` | Recorded control verb | Its engine half and follow-on delivery complete |
| `ScopeClose` | `session_roots` | Root terminal transaction | Its scope-close transaction |
| `ParentEnd` | `parent_end_plans` | Recorded scope end | Each child's cancel is delivered or refused |
| `SessionDelete` | `session_meta` | Close acknowledgement | Physical storage deletion completes |
| `TriggerDelivery` | `trigger_deliveries` | Reservation insert | Binding records the process |
| `ProcessStart` | `processes`, `start_obligation_*` | Registration of an executed input | Engine accepts its current-segment submission |
| `ProcessTerminal` | `processes` | Terminal transaction | Engine terminal publication completes |
| `ArtifactCleanup` | `artifact_cleanup_obligations` | Referrer end or guarded staging | Referrer cleanup completes |

Ingress composes the two tables' oldest due rows. Its id is
`ingress:{item_id}` and its drive request is `{obligation_id}:{attempt}`.
The engine accepting the ask holds the claim; root admission settles it
regardless of the relay state. A child-session acceptance takes its row's
claim in the acceptance transaction, because its acceptor's inline drive is
the ask ([ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md) §6). That claim only delays the relay's ask: once a root's
admission is recorded, its executor decides who runs it, whoever holds the
claim. A waiter follows later claimed attempts if
the earlier drive ends without admitting the item. An ingress retry uses a
minted token and cannot send another ask while a prior claim remains held.

Process start uses `process_start:{process_id}`. The producer's immediate
attempt and the due fallback use the same ledger. Restate's journaled start
claims with a token derived from its claim step, sends with its execution
context, then settles. A cancelled step can rerun and recover its own claim.
Cancellation at the send await does not prove a failed start. Once delivered,
the obligation remains delivered; engine-owned lost-run recovery is
[ADR 0110](0110-the-engine-owns-process-recovery.md) §2.

Every process terminal arms publication. The storing segment can publish
and settle in its own journal; the relay publishes through the process root's
`complete_terminal`. A terminal process's paused segment can be killed after
publication is delivered.

Trigger emit's start and bind are its immediate attempt without a relay
claim. Binding settles the row in any state. A concurrent relay can then
observe `ClaimLost`. Recovery uses the recorded reservation and the same
router wiring as an emit.

Control-intent acknowledgements and refusals compare their claim token. An
intent's state records only what was decided about its engine half: pending,
acknowledged, superseded, or refused with the engine's typed cause. Failed
attempts and their exhaustion live on the obligation alone. The engine half
is owed exactly while the intent is pending and its obligation is due or
claimed; each store states that once, as the generated column
`engine_half_owed`, and admission reads it. A permanent refusal writes the
intent refused and stalls the obligation; attempts running out, a failing
ledger call included, stall the obligation and leave the intent pending.
Either way the stall unwedges admission, and the session is asked to drive
once it is durable. Explicit re-arm makes the intent owed again, returning a
refused one to pending. Root scope close and parent-end cancellation
retain their own retry ownership, so a failing child does not keep a cancel
or fork verb open. A refused child is recorded on its plan.

Paused drives require a park and an explicit redrive, except a drive waiting
for a redrive that then settles. A drive with no resumable root or deleted
session is killed. Accepted ingress retains its obligation for a fresh drive.
Engine-owned lost-run recovery also checks non-terminal session roots and
ends failed runs with durable loss or cancellation evidence, settling their
ingress and arming `ScopeClose` atomically. Lost-process and lost-root scans
each inspect at most one page per tick, including healthy and failed rows,
and retain separate cursors across ticks. Cursors advance before engine
requests, so failed or timed-out items cannot pin a page; an exhausted
catalog wraps and retries them.

Evidence: `crates/lash-core/src/runtime/drive/relays.rs:148`,
`crates/lash-core-execution/src/runtime/trigger_delivery.rs:50`,
`crates/lash-sqlite-store/src/process_registry/registration.rs:117`,
`crates/lash-restate/src/process/park_reconcile.rs:109`, and
`crates/lash-core-execution/src/runtime/vocabulary.rs:493`.

The same pass reads a root whose key Restate holds no run of on any
generation lane. An admission delivers its input's ingress obligation in the
same write, so no relay owns that input. The pass judges the root by the
execution its recorded admission names (`RootExecutor`), which the store
lists beside each open root. It never infers the executor from the root's
name. The first admission records the executor, and every later admission of
the root reads the record back unchanged.

- **Its own run.** Retention purged the run, or its journal store was lost.
  The root has started, its effects may have run, and a fresh execution
  would run them again under an empty journal. It ends `SubstrateLost` in
  the same transaction as a failed run's root.
- **A process's run.** A `SessionTurn` process's drive runs inline, in the
  process's own run, every root it admits: its child turn's root, and any
  root admitted ahead of that turn in a reused session. No lane ever holds a
  run of those keys. While the process's record is not terminal, the pass
  leaves the root and its admitted input, and the lost-process pass owns the
  process's run. A terminal process runs nothing more, so the root ends
  `SubstrateLost`. A failed registry read ends nothing.
- **Another execution's drive.** An in-process drive holds no engine run
  the pass can read, so absence proves nothing and the pass leaves the root.
- **No recorded admission.** The root has started nothing. Its ingress
  obligation still owes its input and drives it, so the pass leaves it.

An admin read that fails proves nothing about any run and ends nothing.

## 4. Two-phase session delete

Before accepting a close, delete checks the session's pending turn-cancel
closure authorizations. A pending authorization refuses it as
`StoreError::TurnCancelClosureLifecyclePinned`, with its session and count.
Nothing has closed. The turn's final commit consumes its exact authorization;
its answer does not prove that a successor or a replaying turn holds no pin.
`LashCore::await_turn_cancel_closures` observes consumption of these stored
pins before a host attempts a close again. It does not wait for effect-group
pins. Every close still checks its refusals, since new work can race readiness.

Delete records `CloseSession` and marks the session closing. New sends refuse
as `SessionClosing`. The engine half stops its roots and closes its scopes.
Acknowledgement arms `SessionDelete` in the same transaction. Once the close
commits, deletion bypasses pre-close pin checks and retains its recorded close.

An answered root's terminal commit arms `ScopeClose`. On Restate, the root's
`run` returns and sends its separate shared `close` handler. The answer can
therefore precede delivery of scope cleanup. That delivery records and applies
parent-end plans and calls `LashDurableWaitIndex/<session>/retire_root` to
retire the root's indexed waits. The session index serializes this call with
its other exclusive handlers. A busy index can delay cleanup; an attempt that
fails or exceeds its delivery budget leaves its obligation owed under §1.4.
Replay alone is not a reason to retain the scope-close obligation after its
consumer has acknowledged the close.

Physical delete waits retryably while the session's scope-close or parent-end
cleanup is undelivered. It removes process-session state and subscriptions,
revokes waits, retires the effect journal and deletes storage last. Storage
delete removes the owning obligation row. Every preceding step is idempotent;
a failed attempt leaves the obligation owed for another relay attempt.
The permanent `CloseSession` tombstone is retained, per ADR 0108 §5a.

Physical delete is only ever this obligation's delivery. Deleting an id that
never materialized a session closes nothing, arms nothing and cleans up
nothing: it answers `SessionDeletion::Absent` and the id stays creatable
(ADR 0049).

`SessionDeletion::Closing` means accepted deletion remains owed, with a typed
reason: an unacknowledged close, cleanup counts, a failed delivery or the
obligation's standing. Hosts await `LashCore::await_session_deletion` instead
of reissuing deletion. It makes no engine request and starts no delivery.
`SessionDeleteCompletion::Deleted` requires the permanent storage tombstone;
`Absent` and `NotClosing` distinguish an unknown id and a live id whose close
has not committed. `Stalled` carries the retained close, scope-close,
parent-end or physical-delete obligation for explicit operator re-arm.
Neither a missing obligation nor an elapsed timeout proves deletion. Dropping
the observer leaves accepted work owed to recovery. A Restate host journals
the returned observation in its own step.

Delete does not wait for arbitrary engine replay. Journaled admission and
seal steps record either their pre-delete answer or retirement. Closing
session pins can be retired with storage because the session cannot activate
again. Pre-close checks run before the close's durable step, so a repeated
delete honors the recorded close.

A stalled close arms no physical delete. Its unfinished roots remain
accounted for as `held_by_stalled_close` until re-arm or deletion settles the
work. Cleanup stalls use the same operator listing and re-arm as other kinds.

Evidence: `crates/lash-core/src/runtime/session_close.rs`,
`crates/lash-core/src/runtime/session_delete.rs`,
`crates/lash/src/core/session_deletion.rs`,
`crates/lash-restate/src/session_driver.rs`,
`crates/lash-core-execution/src/runtime/process/scope_close.rs`, and
`crates/lash-restate-test/tests/host_send_wait/session_delete.rs`.

## 5. Due time belongs to delivery

Delayed delivery uses `obligation_due_at_ms`. Retry backoff is the relay's
policy. Queued work carries no separate `available_at_ms` scheduling field.

Evidence: `crates/lash-store-sql/src/turn_ingress/queued_batches.rs` and
`crates/lash-core-execution/src/runtime/drive/relay.rs:215`.

## 6. Scope-close recovery

A crash between terminal commit and scope close leaves durable terminal
evidence and an owed root-row obligation. The recorded close step or the due
relay completes it. The close transaction delivers the row; replay finds
it settled. A delivered close does not become new work on the next tick.
ADR 0108 §5 owns the lifetime meaning of the close.

Evidence: `crates/lash-sqlite-store/src/session_roots.rs:169` and
`crates/lash-core/src/runtime/drive/scope_close.rs:85`.

## 7. Commands and adjacent writers present the drive fence

The drive applies leading commands in a recorded step under its drive fence.
The command lane takes no session binding. Applying the command run settles
its rows in the commit; withdrawal of a selected command refuses the commit.
A settlement waiter reads the outcome rather than draining commands.

A writer beside the drive reads `current_drive_fence` and presents it on its
commit. A later sealed admission refuses that write as `StaleDriveFence`.
The writer does not raise the epoch to gain authority. [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md)
owns command admission and ordering.

Evidence: `crates/lash-core-store/src/store/drive_fence.rs:275` and
`crates/lash-core/src/runtime/session_api.rs:1161`, `:1478`.

## 8. Executable evidence

Relay and leader-lease laws live in store conformance and backend tests.
The store matrix is SQLite file, SQLite memory and PostgreSQL. Host laws use
the in-process Restate server double, live Restate and Lash-sim's in-process
effect host. Upgrade proofs use the synthetic-next tier.

`crates/lash-restate/src/tests/obligation_relay_on_the_double.rs` exercises
engine delivery. `crates/lash-restate/src/tests/process_terminal_obligation_on_the_double.rs`
checks terminal publication. `crates/lash/src/tests/obligation_relays.rs`
checks core relay assembly. The session-delete finalizer is covered by
`crates/lash/src/tests/core_session_builder/session_delete_finalizer.rs` and
`crates/lash-sim/tests/session_delete_bounds.rs`.

## Model usage accounting

`SessionDelete`'s delivery (§4) first drains the session's accounting
(`EffectHost::drain_usage_accounting`), before any session state is deleted;
a drain failure is retryable like every other step. The engine-to-store
direction of that drain is the accounting continuation of [ADR 0125](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md), not an
`ObligationKind`: there is no usage obligation kind and no SQL relay.
