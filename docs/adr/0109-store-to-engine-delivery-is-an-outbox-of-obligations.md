# 0109: Work that outlives its transaction is an outbox of obligations

## Status

Accepted. Every cross-actor message is a mailbox write plus a wake in its
producer's transaction
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §12). This
decision owns the two kinds of work that cannot finish inside one transaction:
`SessionDelete` and `ArtifactCleanup`.

## Context

A transaction can accept work that cannot complete inside it: deleting a
session's storage after its close, or deleting attachment bytes that live
outside the database. The accepted row must retain what is owed, when another
attempt is due, and whether an operator must intervene. Recovery must make
progress past one failing row and avoid making every deployment rescan settled
history.

## 1. Interface

### 1.1 Obligation columns

An owning ledger row carries its obligation. The common family is:

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

`SessionDelete` lives on `session_meta`. `ArtifactCleanup` uses
`artifact_cleanup_obligations`, keyed by the artifact referrer; its owner and
guard rules are in
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).

Evidence: `crates/lash-store-sql/src/obligation.rs` and
`crates/lash-core-store/src/store/obligation.rs`.

### 1.2 Kinds

`ObligationKind::ALL` orders the two kinds, `SessionDelete` and
`ArtifactCleanup`. Their snake-case labels are the metric and drain-status
keys. `ObligationKey` names the owning row of each kind, including the
artifact referrer for cleanup.

Every arm path obtains its id from `ObligationKey::id()`: the kind label
followed by byte-length-prefixed key parts in key order. Delimiters and Unicode
in a session or other key cannot alias another row. Re-arming the same row
retains this identity; claim tokens remain separate fencing identities.

Unknown stored vocabulary returns `StoreError::Incompatible` with
`UnknownVocabulary`. A key that cannot decode remains addressable by its
obligation id and is returned as an undecodable claimed row.

Evidence: `crates/lash-core-store/src/store/obligation.rs`.

### 1.3 Store half: the ledger

`ObligationLedger` exposes arming, due claiming, by-id claiming, settlement,
explicit re-arm, stalled listing and standing reads. Producers arm inside
the transaction accepting their work. Repair arming affects only a row that
owes nothing. Backends provide one ledger per kind through `StoreSet`.

Due passes mint a fresh `ClaimToken`. A by-id claim takes a due row, or
refreshes a claim held by that same token without incrementing its attempts.
It cannot take a claim another claimant holds. Once a due pass retakes a
lapsed claim under a fresh token, the old claimant cannot refresh or settle it.

Settlement compares the token and answers `Applied` or `ClaimLost`.
`Delivered` clears the error; `Retry` records the next due time and error;
`Stall` records its reason and error. `Defer` returns guarded cleanup to due
with attempts reset. Re-arm accepts only a stalled row and resets attempts.
Producers arm work due at once where delivery must not depend on a database
clock being ahead of the worker's host clock.

Evidence: `crates/lash-core-store/src/store/obligation.rs`.

### 1.4 Delivery

Each kind supplies its ledger, policy and idempotent delivery. The core
assembles both kinds in `ObligationKind::ALL` order and refuses missing
delivery dependencies as `ObligationRelayUnavailable`.

`deliver_now` claims and attempts a producer's immediate delivery. `relay_due`
claims a bounded page and attempts its rows concurrently. Every delivery runs
under its attempt budget. A successful delivery settles `Delivered`.

A refusal stalls as `refused`; an undecodable key stalls as `undecodable`.
A retryable failure backs off until the ceiling, then stalls as
`attempts_exhausted`. Guarded cleanup that is not yet owed defers rather than
stalls. One undecodable or slow row does not prevent other claimed rows from
running.

Defaults are a 1-second initial backoff, 15-minute maximum backoff, 16
attempts, 60-second claim TTL and 30-second attempt budget. Backoff doubles
per attempt and starts at the attempt's start time. The host owns these
policy values; the attempt budget must remain below the claim TTL.

### 1.5 Stalled surfacing

Drain status exposes stalled-obligation counts. Closing sessions hold a drain
while their delete remains owed.

`LashCore::stalled_obligations` lists stalled rows and
`LashCore::rearm_obligation` explicitly makes one due again. Nothing
implicitly re-arms a stalled row. Operational metrics record delivery outcomes
and stalled counts.

Evidence: `crates/lash/src/core.rs`.

### 1.6 Leader lease

There is no leader. Every serving node keeps a heartbeat row, and a reaper
releases a dead node's actors (ADR 0132 §3). Due claims are fenced by claim
tokens, so any live node can run a due pass.

### 1.7 Duties

Every node claims due obligations. PostgreSQL permits due claims on every node
through skip-locked reads. A SQLite deployment is one database file, and its
nodes, in every process that opens it, serialize their claims on its one
writer. Park-feed compaction and opt-in evidence
retention are idempotent duties any node may run; overlapping runs are safe.

### 1.8 Detection and delivery bounds

Due passes follow a 10-second fixed grid. Each kind's pass runs on its own
lane. The default tick waits at most 1 second on due passes, and each delivery
has a 30-second budget. A busy kind is skipped until its pass finishes. Rows
in one claimed page run together, rather than waiting behind each other's
delivery budgets.

With tick interval `T`, budget `B`, page store latency `S` and a free lane,
a due row is claimed within `T` of its due time. A busy lane can add `B + S`.
A lapsed claim first becomes eligible at claim expiry, and then follows the
same bound. Retry eligibility is attempt start plus its backoff, so time spent
delivering counts toward the wait. A stalled row waits for explicit re-arm.

These bounds require sufficient page capacity and store latency within the
tick budget. They are not a throughput guarantee under an unbounded incoming
queue. Simulation varies scheduling to exercise loss, retry and stalled
delivery.

Evidence: `crates/lash-core/src/runtime/shift/interval.rs` and
`crates/lash-core/src/runtime/shift/lanes.rs`.

## 2. Rationale

A durable obligation closes the window between a committed acceptance and
work that cannot run inside that transaction. A best-effort call alone leaves
accepted work without a retry owner. Repeated full-table recovery scans spend
work on settled rows and give a persistent failure no explicit ceiling. Due
indexes, bounded claims and typed stalls make those costs and operator
decisions explicit.

A single due-time column represents both retry eligibility and claim expiry
because a row cannot be due and claimed simultaneously. Artifact cleanup needs
its referrer ledger because cleanup can outlive the referrer's authoritative
row. Work between actors needs no obligation: the producer's transaction
writes the mailbox row and the wake, so nothing is owed after it commits.

## 3. Per-ledger mapping

| Work | Carried by | Written in | Done when |
|---|---|---|---|
| Ingress | `pending_turn_inputs`, `queued_work_batches` | The accepting transaction, which wakes the session actor | Run admission selects the row in its transaction |
| Control intent | `control_intents`, a mailbox row | The recording transaction, which wakes the actor | The session actor's owner commits its decision |
| Scope close | The run or process terminal | The terminal transaction itself | Committed with the terminal |
| Parent end | `parent_end_plans` with a cascade cursor | The terminal transaction | Each batch commits; the cursor reaches the last child (ADR 0132 §11) |
| `SessionDelete` | `session_meta` obligation | The close acknowledgement | Physical storage deletion completes (§4) |
| Process start | `processes`, a runnable actor row | The start's transaction | Committed with the registration |
| Process terminal | `processes`, `process_terminal` wait rows | The terminal transaction, which resolves the waits | Committed with the terminal |
| `ArtifactCleanup` | `artifact_cleanup_obligations` | Referrer end or guarded staging | Referrer cleanup completes |

An ingress row binds to the run that admits it in the admission transaction,
fenced by the session actor's epoch. A child-session acceptance inserts the
child's input and wakes the child actor in the parent's transaction
([ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md) §6). A waiter
follows the run that admits its item.

A control intent's state records only what was decided: pending, acknowledged,
overtaken by a later intent, or refused with its typed cause. The session
actor's owner reads the intent with its mail and commits the decision under
its epoch. Run scope close and parent-end cancellation retain their own
progress, so a failing child does not keep a cancel or fork verb open. A
refused child is recorded on its plan.

A run or process whose owner died is not lost: the reaper releases its actor
and another node claims it and resumes from committed state (ADR 0132 §3).
There is no lost-run or lost-process scan. An actor whose claims make no
progress parks with `ActivationLoop`.

Evidence: `crates/lash-sqlite-store/src/process_registry/registration.rs` and
`crates/lash-core-execution/src/runtime/vocabulary.rs`.

## 4. Two-phase session delete

Delete records `CloseSession` and marks the session closing. New sends refuse
as `SessionClosing`. The closing session actor's owner stops its runs and
closes its scopes in its own transactions. Acknowledgement arms
`SessionDelete` in the same transaction. Once the close commits, deletion
retains its recorded close. Run protected-drain and material dependencies
retain their own eligibility checks. Every close still checks its refusals,
since new work can race readiness.

A run's terminal transaction closes its scope, revokes its pending wait rows
and writes its parent-end plan (ADR 0132 §4 and §11). The answer and the
scope close commit together; the parent-end cascade proceeds in batches after
them.

Physical delete waits retryably while the session's parent-end cascade is
unfinished. It removes process-session state, revokes
waits, prunes the session's phase rows and deletes storage last. Storage
delete removes the owning obligation row. Every preceding step is idempotent;
a failed attempt leaves the obligation owed for another attempt. The permanent
`CloseSession` tombstone is retained, per ADR 0108 §5a.

Physical delete is only ever this obligation's delivery. Deleting an id that
never materialized a session closes nothing, arms nothing and cleans up
nothing: it answers `SessionDeletion::Absent` and the id stays creatable
(ADR 0049).

`SessionDeletion::Closing` means accepted deletion remains owed, with a typed
reason: an unacknowledged close, cleanup counts, a failed delivery or the
obligation's standing. Hosts await `LashCore::await_session_deletion` instead
of reissuing deletion. It starts no delivery.
`SessionDeleteCompletion::Deleted` requires the permanent storage tombstone;
`Absent` and `NotClosing` distinguish an unknown id and a live id whose close
has not committed. `Stalled` carries the retained physical-delete obligation
for explicit operator re-arm. Neither a missing obligation nor an elapsed
timeout proves deletion. Dropping the observer leaves accepted work owed to
recovery.

Closing session state can be retired with storage because the session cannot
activate again. Pre-close checks run before the close commits, so a repeated
delete honors the recorded close.

A stalled delete keeps its session closing until re-arm or deletion settles
the work. Cleanup stalls use the same operator listing and re-arm as the other
kind.

Evidence: `crates/lash-core/src/runtime/session_close.rs`,
`crates/lash-core/src/runtime/session_delete.rs`,
`crates/lash/src/core/session_deletion.rs` and
`crates/lash-core-execution/src/runtime/process/scope_close.rs`.

## 5. Due time belongs to delivery

Delayed delivery uses `obligation_due_at_ms`. Retry backoff is the kind's
policy. Queued work carries no separate `available_at_ms` scheduling field; a
timer is a wait row with a due time (ADR 0132 §6).

## 6. Scope-close recovery

A scope closes in the terminal transaction that ends its owner, so no crash
separates the terminal from the close. ADR 0108 §5 owns the lifetime meaning
of the close. A close that meets corrupt stored data is refused rather than
retried (§9).

## 7. Commands and adjacent writers present the epoch fence

The session actor's owner applies leading commands in an `ActorTx` under its
epoch. The command lane takes no session binding. Applying the command run
settles its rows in the commit; withdrawal of a selected command refuses the
commit. A settlement waiter reads the outcome rather than draining commands.

Every other writer gets a `MailTx`: it appends to the actor's mailbox and wakes
it, and has no owner-state writers (ADR 0132 §3). It never raises the epoch to
gain authority. [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md)
owns command admission and ordering.

## 8. Executable evidence

Obligation laws live in store conformance and backend tests. The store matrix
is SQLite file, SQLite memory and PostgreSQL. Laws run the production runtime
over a fault-injecting store with labelled commits, a virtual clock and
`SimNodes` (ADR 0132 §14). Upgrade proofs use the synthetic-next tier. The
session-delete finalizer is covered by
`crates/lash/src/tests/core_session_builder/session_delete_finalizer.rs` and
`crates/lash-sim/tests/session_delete_bounds.rs`.

## 9. Corruption after a published answer is the session's fault

A run's answer is published with its terminal and before the session's next
admission reads the session. Corrupt stored data met by a later step cannot
fail that run, and no retry repairs it. The published answer stands and is
never rewritten or retracted.

The session owns the fault. Its `session_meta` row records one
`SessionFault`: the typed code (`runtime_store_corrupt`), the message, the
typed cause with its fields, what met it (`scope_close` of a run, or
`shift_admission`) and when. A session already faulted keeps its first
fault. The row lives and dies with the session.

- **Scope close.** The close records the fault and refuses, under the same
  code. A fault that could not be recorded leaves the transaction uncommitted,
  and the owner recomputes it from committed state. Every other store error
  stays retryable.
- **Run admission.** The admission transaction records the fault before it
  answers the corruption as its terminal outcome. A fault that could not be
  recorded commits nothing, and the next activation recomputes the admission.

While the fault stands the session admits nothing. The stored epoch carries
it, admission refuses every run with the fault's code and cause, and a send
whose input is still open is answered with the same typed error from the
store. Accepted inputs stay accepted.

`LashCore::session_faults` lists standing faults in session-id order.
`LashCore::clear_session_fault` is the only thing that clears one, after the
operator repaired the data, and wakes the session actor.

Evidence: `crates/lash-core-store/src/store/session_fault.rs`,
`crates/lash-core/src/runtime/shift/scope_close.rs`,
`crates/lash-core/src/runtime/shift/admission.rs`,
`crates/lash/src/send/resolve.rs` and
`crates/lash/src/tests/store_faults.rs`.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
