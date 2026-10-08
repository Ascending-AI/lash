# 0105: The session actor decides from committed state

## Status

Accepted. The durable mechanics this decision relies on (actors, epochs, phase
rows, wait rows, snapshots and the format gate) are owned by
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md). This file
owns the decision rules the session actor's owner follows on top of them.

## Context

A durable session must resume without choosing different work from live
clocks, caches, cancellation tokens or observation arrival order. The turn
machine consumes committed inputs, and every effect whose outcome must survive
commits as its own phase. Durability is the goal. Deterministic scheduling of
the test harness is not.

## Decision

### 1. Decisions come from committed state

The owner of a session actor runs admitted runs under the actor's epoch fence
(ADR 0132 §3). Nothing re-runs to reach a recorded outcome: resume loads
committed rows and continues, and an uncommitted stretch is recomputed from the
last committed state (ADR 0132 §2).

The rule holds on every durable path, not only in the turn loop. A durable path
is any code that can run more than once for one durable identity: a resumed
actor activation, a retried transaction, and a duplicate or redelivered
submission carrying the same durable identity (a turn id, a host start key, a tool-intent identity, a session-command idempotency key). On
a durable path every decision that shapes what is committed or returned comes
from one of three sources:

- a committed phase: a row of this actor (an admission, a checkpoint, a tool
  outcome, a resolved wait);
- a recorded admission: the retained row a durable identity's first admission
  wrote, returned by the admission itself when it coalesces (a retained process registration or a
  tool-intent submission's recorded outcome, a session command's persisted
  outcome);
- an immutable admitted input, or a pure function of the three.

A fresh read of mutable store state is allowed in exactly two places. It may
run inside the transaction whose write it decides: the read and the write
commit together under the fence, and nothing after the transaction decides
from the read except through what it committed. It may serve explicitly
non-durable observation: listings, snapshots and cursors a host reads and
nothing records, and live revalidation, which can stop stale work before its
next effect but never chooses different work. Anything else is a violation, in
one of two forms. A read before the admission consults today's registry
outside the transaction that records the decision, so after a prune a
resumed path refuses or branches differently. A duplicate that re-decides
re-derives the decision from mutable state instead of from the admission the
store coalesced. An idempotent admission therefore retains every fact its
first decision used (the target binding, the selected wait, the canonical
request), and a coalesced admission returns those facts for the caller to use
without further reads.

So the process commands (start, cancel, await, attach) carry their
target and session checks inside their recorded admissions. A cancel or
await of a pruned process refuses or answers `NoLongerRetained` from the
recorded guard or admission, and a repeated command after a prune returns what
the first admission recorded. A host-wide cancel-all records its
running-process selection before writing cancels from those recorded rows.
`ValidateVisible` records the session-observer or process-starter verdict
before a handle command runs. Compaction and observer transfer cannot change
any of these recorded decisions.

A host start `Until` a session checks the session inside its recorded start
admission (`HostSessionNotLive`), so a repeated start after the session is
deleted returns the recorded start. A tool-intent redelivery claims its
submission-ledger row before it realizes anything, and a row that already
holds an outcome answers that outcome.

Evidence: `crates/lash-core/src/runtime/observation_publisher.rs`,
`crates/lash-core/src/runtime/turn_boundary/recorded_assembly.rs`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs`,
`crates/lash/src/tool_intent_ingress.rs`.

### 2. Run admission, separate from execution

A run's admission is a committed row of the session actor. There is no seal
step and no run-start nonce: the owner's epoch, raised by its claim, fences
every write of the run (ADR 0132 §3). A child inherits authority rather than
minting an epoch.

Admission, keyed by the run, binds its rows and records the base
`SessionHeadRef`, turn index and executable binding in `RunAdmission`. Resume
reconstructs the admitted state rather than selecting from the live head. A
base the store cannot retain refuses `TurnBaseNotRetained` and parks. An
unfinished run keeps ownership of its admitted head until terminal evidence.

Admission also records the head inspection in that outcome:

- `Ready` reconstructs the admission's base, including when the run already
  has a committed turn.
- `Advanced` reconstructs the head the unfinished run's fenced commits
  publish, including its own frame changes.
- `Overtaken` means another writer advances past the base. The run ends with
  `StoreCommitSuperseded` before turn effects.
- `Diverged` means a lower revision or the same revision with inconsistent
  leaf or checkpoint. The run parks.

Resume honors the recorded verdict. A head that moves after a recorded `Ready`
is met by the fenced commit, rather than a live re-check that changes what the
run does.

A follow-on recovery run records `RecoverFollowOn`, keyed by the run
(`shift-follow-on:{run}`), before its turn. Its transaction reads the fact the
head owes, raises the recovery count and records `Run` with the raised fact,
`Exhausted` past the recovery bound the fact froze for its logical run, or
`Ceded` when the head does not owe the follow-on. The recorded fact carries
that bound; the recovering host's bound never decides. The run executes the
recorded answer and never re-decides from the head a resumed owner finds. `Run`
and `Exhausted` also record the head the follow-on's turn runs on and that
turn's index, and the transaction retains that head as the session's latest
admission's base, as admission retains its own. The run adopts the recorded
head and pins the recorded index before its turn, so a resume past the
follow-on's own commit runs the turn on the head it was recorded on, never on
the head that commit moved (FIG-4380).

A live read outside a deciding transaction never chooses what a run does. If
refresh fails or its session has retired, a headless resume loads the recorded
root composition and plugin transition, command-lane reads or follow-on
recovery facts, and performs no fresh admission or head mutation. A recorded
refusal cedes the run; a recorded session retirement ends it with the typed
`SessionDeleted` refusal of [ADR 0049](0049-session-ids-are-used-once.md). A
missing recorded root is `RuntimeStoreCorrupt`, never permission to select a
new one. Work beyond the recorded admission and transition cannot resume
without the session head. That attempt ends as a live fault rather than
committing a replacement answer; park reconciliation can release a deleted
target as `TargetGone`.

Head-changing writes and ingress settlement check the owner's epoch in their
transaction. A refusal no retry can change ends its run through
`RunTerminalCause::Refused`. That idempotent end presents the owner's epoch; a
stale owner fails the fence, drops the actor and leaves the run to its current
owner. A park, unknown commit outcome or retryable live fault ends no run. A
later owner adopts existing run terminal evidence rather than replacing it with
a new commit.

The session head has one owner at a time: the unfinished run, from its
admission until its terminal; an owed follow-on; or the command lane while a
session command is open, command runs included. A head-changing write that
does not present the actor's current epoch is refused in its own transaction,
on SQLite and PostgreSQL alike, with the typed, retryable
`StoreError::SessionHeadOwned`, which names the owner. A host's head write from
outside a turn is therefore a session command the owner applies at a turn
boundary (ADR 0101 §4), and a terminal callback writes under its ended run's
owner. A run commit can still meet a moved head when another writer of the
same epoch advanced it. That commit ends its run with `StoreCommitSuperseded`
and never wedges; the resume that reloads the head is a new run.

Evidence: `crates/lash-core/src/runtime/durable/session.rs`,
`crates/lash-core-store/src/store/head_ownership.rs`, and
`crates/lash-core/src/runtime/durable/head_commit.rs`.

### 3. Cancel races and losing work

A cancel request is a mailbox row of the session actor, and its race with
completion is decided by which owner transaction commits first (ADR 0132 §11
and [ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md)).

### 4. The Run owns concurrent calls and aggregate selection

`RunCoordinator` records whole-round admission, independent attempts, final or
cancel decisions, rank, protected drain, presentation and incorporation as the
opener's Run rows (ADR 0132 §5). Aggregate consumption preserves its recorded
prefix. An early winner leaves losers live; a later program effect can
progress beside them. Only the logical owner records Closing. A change of owner
carries pending sources and the entire Run forward in its rows.

Deferred completion is an immutable `Resolved(ref)` or `Cancelled` source seal
on a wait row. The Run accepts the sealed winner before protected
finalization; a descriptor never wins an aggregate. Cancellation observations
that choose committed work are recorded, so resume takes the same branch. A
Deferred call's wait carries its `WaitDeadline`; an inline attempt carries its
`ExecutionLimit` (ADR 0132 §7).

[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) owns
the lifecycle and drain rules. Evidence:
`crates/lash-core-execution/src/tool_dispatch/run_coordinator/`.

### 5. Keyed promises

Durable waits are wait rows resolved by their first winner under ADR 0132 §6
and [ADR 0003](0003-keyed-promise-is-scope-agnostic.md).

### 6. Protocol drivers and hooks use recorded inputs

`ProtocolDriverHandle` and `ContextProjector` are synchronous decision APIs
over committed inputs. Interior mutability that changes those decisions
violates their contract. Provider and tool I/O goes through admitted
executions. A plugin's live mutable memory is not the authority for a resumed
decision.

Hooks receive read views and return decisions; core owns the resulting writes.
The context-pressure hook runs before the Prompt View's policies with the
committed view, previous prompt usage and context window. `Record` nodes join
the turn's append draft. `OpenFrame` performs its own fenced, idempotent commit
before the turn's model call. The frame key and operation identity derive
from the turn and hook. Resume uses the admitted base and the committed
summarizer completion, and checks the frame commit's receipt. A later turn
failure leaves that committed frame in place. The write is a store operation
under §9. A hook whose phase did not commit runs again from committed state,
so hooks are repeat-safe (ADR 0132 §4).

The phases a model call issues are recorded with it. The model call's
committed phase holds the raw completion together with its ordered response
callback plan. Each entry names its callback key and owning plugin revision,
selected before the paid call. An empty plan serves the raw completion. Resume
follows the recorded order, and a completed derivation loads its committed
result without callbacks. An unfinished derivation resolves every recorded
callback before invoking any and parks if a key or revision is unavailable.
Stream end state pairs with one response callback identity, including its
revision.

Evidence: `crates/lash-sansio/src/sansio/turn_protocol.rs`,
`crates/lash-core/src/runtime/turn_loop/context_pressure.rs`,
`crates/lash-core-execution/src/runtime/effect/llm_outcome.rs`,
`crates/lash/src/tests/response_phase_replay.rs`.

### 7. Continue-as-new and version decisions

An actor has no execution history to bound, so there is no continue-as-new.
Activation progress and the `ActivationLoop` park follow ADR 0132 §3, and a
draining node releases actors at their next committed phase under
[ADR 0106 §1](0106-durable-formats-upgrade-by-migration-or-drain.md).

### 8. Send and !Send

The owner's futures are `Send`; the sans-io turn machine is plain data. ADR 0132
§3 owns the actor's activation loop.

### 9. Commit, park and settlement use fenced store writes

Core assembles the commit from committed results and calls
`commit_runtime_state_verified`. The store checks head, epoch, operation
identity and existing receipt, then writes the head, run terminal evidence,
and ingress settlement together in the turn commit's one transaction (ADR 0132
§4). Lost replies are checked against the stored commit, and the owner can
safely repeat the idempotent write. An exact repeat of a stored commit answers
from its receipt even when its epoch is stale, and writes nothing: a committed
run whose reply was lost answers from the receipt after a later owner's claim
or a session command's commit. The receipt is the whole boundary: who raised
the epoch does not matter. A stale epoch still refuses every commit the store
has no receipt for, including one that reuses a stored commit's operation with
other content.

A classified run park is an idempotent store write that cannot replace an
existing terminal. Reconciliation can write the same park. A non-retryable
run refusal writes its terminal before returning, as in §2. External
operations still need stable idempotency identities; a store fence cannot
retract a request already sent.

Evidence: `crates/lash-core/src/runtime/turn_boundary.rs`,
`crates/lash-core/src/runtime/durable/head_commit.rs`,
`crates/lash-core/src/runtime/durable/session.rs`,
`crates/lash-core-store/src/store/runtime_commit.rs`.

### 10. Commands and executors

Effects are admitted executions with phase rows (ADR 0132 §5). Environment sync
records prompt and catalog definitions, and the owner installs what it
commits. Run admission fixes the tool's prepared caller environment and
executable binding; resume uses that recorded material. Deterministic refusals
remain recorded answers. A live store or session fault in an uncommitted
derivation commits nothing and is recomputed from committed state.

### 11. Validation and laws

The no-replay laws NR-1 to NR-4 and the crash matrix of ADR 0132 §2 and §14 are
the validation of §1. The stale-epoch boundary of §9 has two turn-config laws:
`a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt` and
`an_older_admission_redriven_after_a_profile_change_is_fenced_out`.

### 12. Journal generations and the version freeze

Durable formats, the format gate and drain are owned by ADR 0106 and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md). The
pre-1.0 freeze changes shapes in place, without version bumps or upcasters.

## Rejected alternatives

Re-running orchestration code against a recorded history to rebuild its state
makes every kernel change a format change. It is rejected by ADR 0132 §2. An
epoch check alone cannot fence an external request already sent; stable
idempotency covers that boundary. Cancellation belongs to the actor's mailbox
and its fenced commits.

## Consequences

Resume loads committed decisions and repeats fenced, idempotent writes. Live
observation and cancellation transport do not replace committed facts. Core
owns commit and terminal evidence. The contract applies to drivers and
engines; opaque tool bodies are admitted executions. Changes under the pre-1.0
freeze carry no cross-build compatibility guarantee.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.

## Preparation and cancellation

The environment prelude retains the early recorded configuration, the
accumulated context-pressure decisions through the first frame open, the final
prepared messages and registered tool-provider identities, and the before-turn
callback record. Configuration is recorded before preparation: pressure and
context hooks may issue their own summarizer calls, each a `Repeatable` model
call whose result commits as its own phase. The prelude serves the prepared
context to downstream protocol work. Live provider handles are rebound from
their recorded registered identities.

An admission may retain a typed cancellation-intent snapshot in its run record.
For such a run, the final head transaction validates the admitted snapshot,
the current intent and the selected cancellation request. Its cancellation
decision is derived from durable intent and commits in the same transaction;
observer gates are notified after the commit and cannot change its decision.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
