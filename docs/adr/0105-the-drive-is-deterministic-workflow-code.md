# 0105: The drive is deterministic workflow code

## Status

Accepted 2026-09-24 (FIG-3672). The adopted drive uses
`RuntimeEffectController` through `ScopedEffectController`. Restate records
command outcomes, cancel races, admission, and root claims. The controller is
the execution seam; the separate context trait proposal was removed in
FIG-3940 because production never implemented it. The decision below states
the adopted contract. The historical slice inventory and implementation notes
remain evidence of how it was reached.

Refines the execution seam of
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
§2: that ADR requires the seam, and this one states it. It is the prerequisite
of FIG-3600 S5, which freezes the drive API against it.

The evidence is the seam proof
(`/workspace/notes/lash/prospect-unreal-agent-lanes/lane-seam-proof.report.md`),
its contract review (`/workspace/notes/lash/fig3664-engine-contract/astra-seam-report.md`)
and the FIG-3672 inventory and slice plan
(`/workspace/notes/lash/prospect-unreal-agent-lanes/lane-3672-plan.report.md`,
baseline origin/main `dd80a5e7c`).

## Context

At the decision date, the turn machine and effect commands were replayable:
the machine's transitions were synchronous, and commands and outcomes were
serde values with explicit replay identities. The driver still had live
decision inputs. The inventory counted 238 decision-path sites in eight
categories: `select!` races against live cancel tokens, spawned forwarders and
cell tasks, channel arrival order, wall-clock reads and CPU-time budgets,
store I/O in drive code, lazy caches and mutable side channels, and async
hooks that decide outcomes without being recorded. Five findings reach past
the seam proof:

- the committed turn is assembled from the observation stream;
- five execution-side bodies leak state into the drive outside their
  recorded outcome;
- `PresentToolResult` hashes a wall-clock duration into its envelope;
- the Restate group-child dispatch and the process-segment workflow have
  their own unjournaled cancel races;
- the main Restate effect journal has no version gate.

These findings motivated the Restate replay cutovers. A replay that
re-derives a decision from a clock, a cache or a race it did not record can
issue different effects from the ones its journal holds.

A drive is deterministic when every decision input is either a recorded
outcome, a recorded immutable input, or a pure function of those. The only
place a drive may wait is an operation the engine records.

## Decision

### 1. The drive uses the recorded controller

A drive issues effects through its scoped `RuntimeEffectController`. The
Restate implementation records an envelope and outcome under the effect's
replay key, checks the canonical envelope on replay, and serves the recorded
outcome without dispatching the body again
(`crates/lash-restate/src/controller/journaled_effect.rs`). The local
`LocalTestCx` implements that same controller and compares fresh and replayed
command streams (`crates/lash-core-execution/src/engine/testing/controller.rs`).

The drive must make decisions from recorded outcomes, immutable admitted
inputs, and pure functions of them. Observation is keyed by replay identity
and ordinal and cannot decide a commit. Live store reads that protect an
already recorded decision may only stop stale work before its next effect;
those reads cannot choose different work. The determinism harness detects
unrecorded commands and wakes outside its recorded operations. The Restate
controller owns durable timers, keyed waits, and their cancellation races.

### 2. Unfenced admission, separate from fenced execution

`AdmitDrive` records the candidate root and its admission identity.
`DrawRootStart` records the root's nonce, and `SealDriveAdmission` advances the
drive epoch with a store compare-and-set. The admission verdicts and fence
remain engine-neutral values, while the controller records the operations.
A reset can preserve an admission result while invalidating its authority,
so the seal rechecks it in the same transaction. A child never mints an epoch.

- **Implemented (FIG-3815): the root's start marker.** Before its seal, an
  admitted root's execution records a `DrawRootStart` step in its own journal
  (`drive-root-start:{admission}` under the root's scope), whose body draws a
  random nonce. A retry of that execution replays the nonce; an execution that
  cannot read the journal (purged, or past retention) draws another. The seal
  stores the nonce with the admission on the session's `session_meta` row
  (`drive_root_start`). A later seal of the same admission under the same
  nonce is a retry and answers the stored fence; under another nonce it is a
  fresh execution of a root that already started, and the store answers
  `ExecutionLost`, which the seal records as `SubstrateLost` without running
  anything (L-S8). A seal stored before markers existed is answered as a
  retry. `admit` does not read the marker yet: a fresh execution is refused
  at its seal, not at admission.

- **Implemented (FIG-3682, FIG-3600 S5a): the root's admission records its
  base.** `Admitted` records no head. The root's recorded admission step
  (`AdmitRoot`, keyed by the root as `drive-admit:{root}` under the FIG-3927
  amendment) binds the rows and records, in its `RootAdmission` outcome,
  the head the root runs on
  (`base: SessionHeadRef`: generation, revision, leaf and checkpoint) and its
  `turn_index`. The admission is the one source of truth for the base: a
  replay reads that outcome back, rebuilds the turn from
  `SessionCommitStore::load_session_at(base)` and addresses it under the
  recorded index, never re-reading the live head, which the turn's own commit
  may have advanced. The store keeps the base checkpoint as a GC root until
  the session's next admission; a base it no longer holds refuses
  `TurnBaseNotRetained`, which parks. The admission's identity is the root,
  not the admission nonce: a later admission of the same root replays the
  same admission step, and the store records the admission on the root so a
  re-execution reads it back rather than selecting again (FIG-3927).

- **Implemented (FIG-3824): the admission and head inspection record drive
  decisions.** The `AdmitRoot` body runs no orphan repair: the repair died
  with the claims under FIG-3927, because a root's terminal write releases
  its rows, so there is nothing to repair. A replay serves its admission
  outcome. After the admission,
  `InspectAdmittedHead` records `Ready` or `Diverged` from the refreshed
  resident head, committed-root evidence and pending input rows: `Ceded` is
  no longer reachable after admission (FIG-3927), because
  an admitted head row is bound to the root and only the root's own commit or
  terminal settles it. Replay serves that verdict.
  The inspection's body is the drive's one live head check: a head that moved
  from the admission's base with no commit of this root behind it is
  `Diverged`, and the root parks before any turn effect. The body runs only
  when `drive-head` is the attempt's live frontier, so it protects an attempt
  that reaches `drive-head` after another writer moved the head, including
  one whose earlier attempt recorded the admission and died. The resident-head
  refresh may run again on replay, but its values only feed recorded steps.
  Loading the retained base may be re-evaluated safely: success reconstructs
  the same immutable head, while a missing base parks before a turn effect.
  It never selects different work.
- **Implemented (FIG-4058): replay honours the recorded verdict.** A replay
  serves `drive-head` and honours its verdict at every recorded position; the
  live head is never re-read against it. A retry whose journal runs past
  `drive-head` retraces what the first attempt journaled after it, so a live
  re-check that turned a recorded `Ready` into `Diverged` would park (or meet
  `RT0016`) at a position the journal already holds. A head that moved after
  `drive-head` was recorded is instead met by the turn's fenced commit as
  `StoreCommitSuperseded`, which ends the root (FIG-4010, FIG-4018): no turn
  commits on a head it was not admitted on either way. The re-check this
  replaced was safe across attempts only when the journal ended at
  `drive-head`.
- **Implemented (FIG-4010): a superseded commit ends its root.** A drive
  fence does not stop a host service from moving the session head, so a
  root's commit can be refused as `StoreCommitSuperseded`. The engine's retry
  would replay the admission base and fence the journal recorded and meet
  the same refusal on every attempt (and, before FIG-4058, its replay met the
  moved head in a live re-check at a position where the refused attempt
  already journaled the turn's commands: Restate `RT0016`, then a pause). The
  root therefore ends in the attempt that met the refusal, with the
  superseded commit as its typed refusal; the redrive that reloads the head
  is a new root.
- **Implemented (FIG-4018): a refused root ends in the store.** A root
  attempt that ends with a refusal no retry changes (a superseded commit, a
  finalize refusal, any refusal the engine records as `Released`) writes the
  root's terminal, `RootTerminalCause::Refused` with the refusal, before the
  engine records the run's outcome. It is an idempotent store write like the
  commit and the park (§9): the root's own inputs are answered with the
  refusal, the rows it held are released, and a root that already has
  terminal evidence is left as it is. The refused run is the one writer of
  that terminal; the engine's lost-root recovery ends only runs that
  recorded no outcome. A replay after a crash or store fault on either side
  of the write honours the recorded `Ready` (FIG-4058) and retraces its
  journal to the same refusal, writing the end if the first attempt did
  not.

A sealed verdict carries a `DriveFence` that the store checks on
head-changing writes and ingress settlement. `DriveFence` and `AdmissionId`
live in `lash-core-store` (`store::drive_fence`). The drive epoch and the
admission that last raised it live in `session_meta`. Effect envelopes carry
replay identity; their hash does not contain the fence (L-S12).

### 3. Cancel races and losing work

Restate records which durable wait or cancel gate completed first. A replay
uses that recorded winner. For a deferred `AfterStep` stop, the wait remains
pending and the controller registers an escalation gate; an `Immediate`
escalation stops it. The controller retires a gate when its guarded wait wins
(`crates/lash-restate/src/controller/context.rs`). A recorded step runs to its
recorded outcome, with cooperative cancellation inside the step body. The
controller's `close_effect_group` and child-final admission own child cancel
and protected drain. The tests in `lash-restate/src/tests/turn_cancel_modes.rs`,
`process_cancel_race_sdk.rs`, and `effect_group_child_cancel.rs` exercise the
production controller's race, loser, and escalation behavior.

**Implemented (P9): cancellation is an engine event.**

- **A recorded step keeps its loser inside its own body.** Restate cannot
  select a running `ctx.run` away: the SDK requires a run to be awaited as
  soon as it is issued, and a run is not a future its select can race. So a
  race whose first arm is a recorded step is carried out by the engine's
  recorded body. The body runs under a cooperative cancel that fires when the
  gate pair asks the turn to stop now (an `Immediate` request, or an
  escalation of an `AfterStep` one). The body watches the gate pair over the
  deployment resolver for its own lifetime, with a token it never fires, and
  drops the watch when it ends. On the Restate ingress every watch of one gate
  attaches to one server-side waiter by idempotency key, so a turn holds one
  waiter per gate however many model calls it makes. Whatever the body
  returns, finished or stopped, is the step's recorded outcome, so it also
  records which arm won. A replay serves that outcome and never runs the
  watch.
- **A tool attempt the turn runs in process is such a step too.** Its body
  watches the gate the same way and hands the tool the stop as its
  cancellation token, so a routed `Immediate` request stops a tool that waits
  on its token. A tool attempt's engine records every outcome, so a watch that
  gives up leaves the tool running to its own end; the turn honours the
  request at its next journaled peek.
- **A watch fault is never a cancellation.** Every gate watch (a step body's,
  and the SQL and ingress-host race arms) retries a fault on one ladder: 8
  attempts, 25ms doubling to 1s. A watch that exhausts the ladder ends the
  attempt with the typed live fault `TransientCancelWatch`. No engine records
  it (the SQL claim is released unsealed; on Restate the run ends retryably),
  so the step runs again. The body is dropped, never stopped, so no
  provider-cancelled evidence is minted (`ActiveTurnControl::run_step_body`,
  `TurnCancelGatePair::await_stop_retrying`).
- **Waits race the gate in the engine.** On Restate, a turn-observing timer,
  await-event, process await and effect-group rank wait race the turn's gate
  in the journal (the gate's awakeable, then its registration). On the SQL
  tier, a turn-observing timer, await-event and process await race it inside
  their recorded execution. No engine writes a verdict back into a token the
  drive reads. Two races are **not recorded yet**, and P14 (FIG-3672 inventory
  AC13) records them: an effect-group rank wait on the SQL tier and on the
  Restate ingress host races the gate live, so a replay that finds the rank
  already journaled can take a different settlement prefix than the live run.
- **A lost process await's cancel is a recorded answer (FIG-3752).** The
  journal already records which arm of a Restate race won: the select
  resolves on the first completion in journal order, on every execution. What
  follows the gate's win is `dispose(child, AwaitCancelled)`: the turn asks
  the store to record the stop's cancellation, then calls the process
  workflow's `cancel`. The store answers differently once that cancel has
  ended the process (a terminal process refuses a new request), so the ask is
  a recorded step (`process-await-turn-cancel-admission`). It records the
  cancellation the store holds, or that the process had already ended, in
  which case no cancel call is made. A replay reads that answer and makes the
  same calls. A store fault inside the step is not recorded; the attempt
  retries. Effect-journal generation 8.
- **A process start and a sleep journal a frontier marker (FIG-3779).** Each
  journals a `ctx.run` at `lash:{replay_key}:frontier` before it acts, on
  every execution: whether the effect is served only (a drifted binding's)
  depends on the live registry, so a marker only drift adds would change the
  journal between the live pass and its redrive. The marker is where a
  served-only start or sleep learns whether it was recorded (ADR 0103).
  Effect-journal generation 9.
- **Process-scope waits race the process's cancellation (P16).** A wait
  that observes no turn is a process body's (`waitSignal`, a group rank wait,
  a process await, a sleep). It races the process segment's durable cancel
  fact, as described under "Implemented (P16)" below.
- **The drive keeps one recorded fact.** It is the cancellation the turn
  honours, advanced only by journaled gate peeks (start gate, after the model
  call, the step boundary, after a code cell that stopped on the host, and
  after an abort a recorded outcome typed as the turn's cancellation) and by
  recorded outcomes. A cell that aborted on a session retirement asks no
  peek: the deleted session's gate is revoked with it, and the typed
  retirement refusal reaches the turn. There is no live watcher, token,
  evidence mutex or origin
  hint on the drive path. What remains live is outside the drive: the
  commit's `lease.is_lost()` filter (S5), and host glue that reads a
  `LocalTurnStop`'s token before the drive starts (queue drain and lane
  acquisition).
- **A code cell stops at recorded checkpoints.** The VM hands its host cancel
  checkpoint `n` when its executed-instruction count reaches the `n`th
  position of a fixed schedule: the first gap is 2^20 instructions, each gap
  doubles, and gaps stop growing at 2^28. A run of `I` instructions reaches at
  most `9 + (I - 511 * 2^20) / 2^28` checkpoints. Each checkpoint journals one
  gate peek, or two while an `AfterStep` request is pending (the second reads
  its escalation). The schedule is a function of lashlang's instruction
  accounting (`INSTRUCTION_ACCOUNTING_VERSION`: compiler emission, builtin cost
  charges, yield granularity and the schedule itself). That accounting is
  pinned into the cell journal grammar (`LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION`
  4), so a journal written under other accounting is refused before its cell
  runs. The checkpoint is also the cell's only wait during a long stretch of
  pure compute, so a single-threaded executor cancels a runaway cell without a
  scheduler yield.
- **A host-local stop is a durable request that a host request can adopt.**
  The facade's `cancel` token and `cancel_running_turns`, and a process
  stopping its child turn, are forwarded as a request on the turn's gate with
  lash's internal evidence. The turn honours that request where it honours any
  request. Forwarding retries until the turn ends; it never gives up while the
  turn can still honour the stop. Internal evidence chose no undelivered-input
  policy. So the first routed host request that would stop the turn now adopts
  it through the escalation promise, and the turn settles under that request's
  identity and policy (a later `Drop` is kept). Over a host-accepted base, the
  escalation still carries no policy (FIG-2874). One exception: two local
  stops (`AfterStep`, then `Immediate`) already hold the escalation promise, so
  a later routed request cannot adopt them.
- **Public API changes (pre-release).**
  - `TurnCancelOriginHint` and `facade_support::configure_local_turn_token`
    are removed; a host stops a turn through `LocalTurnStop`.
  - `RuntimeEffectController::await_next_settlement` and
    `EffectLayer::await_next_settlement` take a `TurnCancelWait` instead of a
    `CancellationToken`.
  - `lashlang::ExecutionHost::yield_now` is replaced by
    `cancel_checkpoint(u64)`.
  - `TURN_CANCEL_WATCH_MAX_ATTEMPTS` is removed.
  - `ActiveTurnControl::watch_immediate` no longer takes a token.
  - A host `TurnCancelRequest` id may not start with `internal:`.
- Effect-journal generation 7.

**Implemented (P16): a process's cancellation is an engine event.**

- **The fact.** A process segment's cancellation is a durable first-writer
  fact. The drive observes it three ways, each recorded:
  - a race on every durable wait the drive records that observes no turn;
  - a peek at each cancel checkpoint of the process body (the lashlang
    process host answers the VM's checkpoints with
    `RuntimeEffectController::observe_process_cancel`);
  - for a `SessionTurn` process, a peek before the child session is created
    and one before its turn is admitted, which decide whether the runner
    settles cancelled;
  - one peek after a `SessionTurn` runner settles successfully, so that a
    committed cancellation outranks the settled child (PR #897).

  A recorded cancelled outcome of a wait or a step advances the drive's fact.
  Nothing else does.
- **The stop is execution-side only.** A process execution's token is lent to
  the step bodies it runs (tool attempts, model calls, the tool children its
  live opener lends). The token is never a drive input:
  `RuntimeExecutionContext::with_lent_process_stop` keeps it out of
  `is_cancelled`, and the lashlang process fact is never its child. A body
  that observes the stop records a cancelled outcome, and that recorded
  outcome is what reaches the drive.
- **Where the obligation is met.**
  - On Restate the fact is the segment workflow's `process_cancel_requested`
    promise. Each process wait is guarded, then races a `GetPromise` of that
    promise through the VM's first-completed await (journal order: the
    guarded command, then the promise's). A lost event wait is released
    `Cancelled`; a lost process await's call is cancelled. The peeks are
    journaled `PeekPromise` commands.
  - The execution-side watch that fires the lent stop (`process_stop.rs`)
    fails closed. It retries transport faults on the shared gate ladder (8
    attempts, 25ms doubling to 1s). When the ladder is exhausted it ends the
    attempt with an unrecorded retryable error. When the workflow is unbound
    it ends the attempt with the engine's 404 terminal (FIG-1579). A segment
    never runs a step body that its committed cancel cannot reach.
  - The `cancel` handler also asks a `SessionTurn` process's child turn to
    stop, as a durable request on the turn's gate
    (`lash.process.cancel.child-turn`). The turn honours it whether or not
    the process runs anywhere.
  - Controllers that record no process cancellation fact (the SQL driver and
    the test controllers, which FIG-3668 deletes) answer the peek from the
    lent stop they are handed. It is their only cancellation input, and
    their journals are keyed. Their waits still race the lent stop inside
    their own recorded executions.
- **Every registry read and write of the segment handler is a named step.**
  - `lash.process.complete` stamps the terminal evidence clock inside the
    step and journals the stored outcome that the terminal promise
    publishes.
  - `lash.segment.boundary` records whether a boundary is declined.
  - `lash.segment.resume` journals the handover a later segment resumes
    from, checked against the digest its admission recorded. A redrive
    replays the runner from the journaled handover, never from the store.
  - `lash.segment.handover` records the successor reference and the
    handover. The stored handover names its writer (the admission's
    recorded nonce). The engine state carries measured wall-clock time, so a
    redrive that re-runs this step re-derives different bytes. A put by the
    same writer keeps the bytes already stored; another writer's put is a
    conflict (FIG-3809).
  - `lash.segment.cancel-forward` records whether a cancel is forwarded to
    the successor.
  - `lash.segment.retire` retires the segment's own handover (and older
    ones). It runs after the segment has sent its successor and recorded
    the cancel it forwards. A put never retires an older handover. A
    redrive before or after retire replays from the journaled resume
    handover. A retire the store refuses is logged and skipped: pruning the
    process deletes what it left.
  - In the shared `cancel` and `deliver_cancel` handlers, the steps are
    `lash.process.cancel.record`, `lash.process.cancel.child-turn` and
    `lash.process.cancel.route`.

  Each step records a non-retryable refusal as its answer. A retryable
  store fault ends the attempt unrecorded, so the step runs again.

  A segment never ends while its process stays `Running` (FIG-3789). Each
  failure class has one fixed path:
  - A missing or mismatched resume handover, a controller that cannot be
    minted, a failed boundary read and a failed handover write each end the
    process `Failed` through `lash.process.complete`. The failure is typed
    `process_segment_handover_missing`, `_handover_mismatch`, `_controller`,
    `_boundary` or `_handover_write`, and the terminal is published to
    awaiters.
  - A runner error that no retry can fix (`is_terminal`) ends the process
    `Failed` under its own code.
  - An opaque runner error is the host's infrastructure and fails the
    attempt retryably, so the process stays recoverable.
  - A lost lease or an unknown process ends only the invocation, because
    another owner carries the process or no process is left.
  - Once the successor is sent, the successor carries the process. A
    failure after the send therefore ends only the predecessor's
    invocation.

  The process body's wait-state writes are steps too, through
  `RuntimeEffectController::record_process_drive_step`: entering and
  clearing a signal wait (`lash.process.wait.enter:<key>`,
  `lash.process.wait.clear:<key>`). A write the registry refuses because the
  process is already terminal counts as settled. A redrive after the
  completion step therefore reissues the wait the journal holds.

  The one exception is the retired-generation refusal. It runs before the
  handler's first command, because a step there would mismatch the retired
  journal it refuses. It stores the typed `Abandoned` terminal and publishes
  that terminal through the root's shared `complete_terminal`, a separate
  invocation, so awaiters are released even when the refused invocation's
  own journal can never replay. `cancel` and `deliver_cancel` carry the
  generation their sender built them for and refuse any other generation
  before journaling anything. In-flight invocations of a retired generation
  meet the refusal as a journal mismatch, which the engine's retry policy
  bounds. Drain or kill them at deploy.
- **Admission records the segment's inputs.** The `lash.segment.admit`
  verdict journals four things:
  - that the segment is superseded;
  - that the segment's handover is missing;
  - the digest of the handover the segment resumes from, which the handler
    checks against the retained handover it reads after admission;
  - the boundary policy (`SegmentPolicy { effect_budget }`), which it
    computes from the host selector inside the step.

  `lash.segment.start` journals the process incarnation it started, so no
  live read follows admission. A redeploy that changes the selector cannot
  move a replayed cut.
- **A substrate-admitted segment is not admitted again.** When a durable
  substrate recorded the segment's start in its own journal, the process
  worker reads that start and never runs its start CAS again. The CAS is a
  live write, and it refuses a terminal process, which every redrive after
  the completion step reaches.
- Restate process journal generation 3. The effect journal is unchanged.
- **The process crash matrix (FIG-3809).** `lash-restate-test`'s
  `process_crash_replay` runs a real Lashlang process across three
  segments. The process sleeps, calls a tool, waits for a signal and calls
  the tool again. The matrix:
  - crashes every segment journal point once and redrives it, three ways:
    plain, under always-replay, and on a fresh process worker;
  - runs 16 serially scheduled seeds under always-replay, each with a cancel
    injected at a seeded point.

  Every run reaches the reference terminal, or `Cancelled` when the cancel
  landed first. There is no journal mismatch, and each tool call runs once
  unless the crash lost its result. The matrix found the retire and handover
  replay faults above. The resume step changes the process journal's shape
  without a generation bump.
- **The process-await guard (FIG-3808).** A process `Await`'s existence
  guard is one recorded step (`process-await-guard`). It records `Ok` or
  the typed refusal. A redrive after the child finished and was pruned
  serves the recorded answer. The effect journal generation is not bumped
  (frozen pre-1.0, FIG-3846).
- **The pre-run record read (W1) is replay-invariant.** The read decides
  nothing a later mutation could change. A terminal record carries no park:
  the terminal fold clears the wait. So clearing a park on a terminal record
  cannot happen. A law moves the cancel request, the wait and the external
  reference between attempts and replays the run unchanged.

### 4. Group operations are complete

`RuntimeEffectController` opens a recorded group, serves ranked settlements,
reads a rank without moving the cursor, commits a child's final result under
the cancel fence, waits for protected drain, and closes the group under its
loser policy. `EffectGroupHandle` is the cursor of record. The Restate
controller and group index assign dense ranks at their serialization point,
seat undecided children on cancel, and keep committed children in the drain
before seating them. Group-child cancellation is a durable fact observed at
wait and attempt boundaries, with the race recorded by the controller.

**Implemented (D20, FIG-3904): effect-group dispatch records its cancel
races.**

- **The fact.** It is the child's `Cancel` group wait. The index resolves it
  `Cancel` or `Retired` when it decides, and `Settled` once the child's
  settlement is seated. Every terminal except `Settled` is a cancel.
- **A wait child** races its timer or durable wait against a call on that
  wait. The race is journaled: the wait's command first, then the cancel
  call. A replay takes the arm its live run took. A lost wait is the typed
  `RuntimeEffectGroupChildCancelled`, which the dispatch settles `Cancelled`.
- **A tool child** has no handler-level race. Its drive reads the fact with
  a journaled peek before each attempt and before an orchestrating body.
  Each `ToolAttempt` body races a live watch of the fact on the execution
  side. The watch fires the attempt's stop and drops the body, and the typed
  cancel is then the attempt's recorded outcome. A watch fault retries on the
  shared cancel-watch ladder, and a watch that gives up leaves the body to
  run to its own end. The engine's cancellation of the child's invocation
  (`409`) is the same decided cancel wherever it surfaces.
- **An atomic child** keeps its two races around its recorded `ctx.run` body,
  pinned `RECORDED` in the drive-determinism allowlist. Its watch retries on
  the same ladder, so a transient fault never drops the body.
- **Journal shape.** A group child's journal records the peeks, the
  cancel-wait call and the attempt's cancelled outcome. Nothing races outside
  a recorded arm or a recorded body.

### 5. Keyed promises

- **Resolve before wait.** A resolution persists whether or not a waiter
  exists, and `await_key` returns it.
- **Dedupe.** The first writer wins. `ResolveAck` is `First`,
  `Duplicate { same }` or `Revoked`, and a differing duplicate never
  overwrites.
- **Revocation.** Revoking a session resolves every waiter `Revoked`, and a
  later resolve is acknowledged `Revoked`.
- **Acknowledgement.** An ack is returned only once the resolution is durable.

An engine whose signals are dropped when no handler is registered (Temporal)
implements keyed promises as signal-with-start wait executions per key digest,
never as bare signals.

### 6. Protocol drivers and hooks are pure

- **Protocol drivers.** `ProtocolDriverHandle` and `ContextProjector` methods
  are synchronous, take `&self` and have no side effects. A replay calls them
  again over the same recorded inputs and must reach the same decision.
  Interior mutability in an implementor is a contract violation, and the P5
  lint checks the protocol crates for it.
- **Plugin hooks are pure and synchronous over recorded inputs.** Their I/O
  runs in declared steps.
- **Hooks get read-only services.** A hook reads recorded state through
  read-only views. Every write, including a graph append, a frame switch, a
  session create and a tool-state apply, is a recorded command whose outcome
  is folded. Standard compaction's frame switch is the recorded outcome of its
  checkpoint step.
- **A group child's graph append is its own recorded command.** Its outcome
  rides the child's settlement, and the opener incorporates it at its rank. A
  child that settles after the opener has released has its append refused
  typed, never silently dropped.

### 7. Continue-as-new and version decisions

**Session continuation.** The drive runs at most 64 roots per invocation
(`MAX_ROOTS_PER_DRIVE`). At a turn boundary, the Restate session driver sends
an idempotent continuation request derived from the current drive request.
The next invocation admits work under its own recorded steps. It does not
transfer a live group cursor or an in-memory turn machine. A root that needs
replay remains bound to its recorded build generation.

**Version decisions.** `DriveRequest.build_generation` pins the deployment,
so the engine routes a replay to a compatible build. In-place code patches are
not supported: a changed drive runs on the next segment on the latest build
([ADR 0043](0043-hosts-register-immutable-deployments.md), ADR 0106).

Durable format changes keep using the generation gates (§12).

### 8. Send and !Send

The adopted controller is `Send + Sync` through `AwaitEventResolver`, and its
async methods return `Send` futures. The local determinism harness accepts a
`!Send` drive future and polls it on one thread; this permits a drive to hold
local state without giving the execution side access to it. The production
Restate handler uses the `Send` controller and can run on its executor. The
former compile tests of two unused engine types proved neither production
path and were removed. Laws for the production futures belong with the
controller and harness tests.

### 9. Commit, park and settlement use fenced store writes

`turn_loop/commit.rs` assembles a local commit request and
`turn_boundary.rs` calls `commit_runtime_state_verified`. The store performs
a fenced, idempotent compare-and-set for the head, root terminal evidence,
usage, and ingress settlement. A lost reply is checked by the commit identity
and the stored turn before the drive proceeds.

The root park is an idempotent store write after a classified abort. The store
refuses it if terminal evidence already exists. Park reconciliation can make
the same write after workflow-task divergence. The drive replays its recorded
effect receipts and re-executes these fenced store writes. There are no
separate journaled commit, park, or ingress-settlement command variants.
External calls carry a stable operation identity because a fence cannot
retract one already sent.

### 10. Commands and executors

The controller takes a serialized `RuntimeEffectEnvelope` and returns a
`RuntimeEffectOutcome` or a typed controller error. It resolves each body
through the production runtime executor and records the result. The proposed
separate executor registries and step-context types had no production
implementor and were removed. The controller rehydrates the services needed
by a recorded body; the drive never passes a live store or mutable session
through its command bytes.

- **Canonical-envelope validation on replay.** Every step result is recorded
  with its canonical envelope, and the adapter compares it before returning a
  replayed result. An engine whose replay matcher compares only ids and types
  (Temporal's activity matcher) returns `{envelope_hash, outcome}` as the
  activity result and compares it in workflow code. A mismatch is
  `EffectReplayDivergence`, which parks.
- **Memory projection refs are refused on durable paths.** A `memory` ref is
  worker-local by definition, so a replay on another worker cannot resolve
  it. Every durable path (envelope, global, seed) refuses it typed; embedders
  register durable, content-derived kinds. Nothing minted them outside tests,
  so the lane is deleted: the facade's registry export, the `Ref` seed kind
  and its transport are gone, and a legacy `kind:"ref"` seed payload fails
  decoding (P4).
- **Implemented (P7, first part).** The turn-effect state update is deleted: a
  step body runs on a copy of the driver and hands back nothing but its
  outcome.
  - The checkpoint's claims reach the driver only through its recorded claim
    set, folded in before its result is read, so a failed checkpoint hands
    its claims to the failure path on replay as on the live pass.
  - A model call runs on a copy of the provider the turn was admitted with;
    nothing the provider object learns during a call reaches the next one.
    The `LlmCall` command names the policy's provider beside the request's
    model.
  - The `LlmCall` outcome carries what the provider stream left for later
    steps: the reasoning blocks it already published, and each plugin's
    stream-hook end state. The `AssistantResponseHooks` command carries those
    states, and a response hook reads them, never its plugin's memory, so
    phase 2 derives the same response on any worker. The stream-finished hook
    returns the state.
  - Trace ids for model calls are named by their effect, not by a counter
    carried between steps.
  - Effect-journal generation 2.
- **Implemented (P7b): the tool surface is recorded and judged per tool.**
  - Every execution-environment sync records the tool surface it built, as
    the definitions of its catalog. The sync body installs nothing; the drive
    installs the recorded surface after each sync, on the live pass and on
    every replay. The recorded definitions are the catalog: membership,
    manifests and contracts are what the pass that wrote the journal saw, so
    availability and the manifests pinned into tool-group children never read
    the live registry. The live registry supplies only executors.
  - Each recorded tool is judged against the live registry on its own, on
    what decides how a call links and dispatches (identity, binding,
    activation, argument projection, retry policy, schemas, output contract):
    the FIG-3587 rule. A reworded description is not drift. A call on a
    drifted tool is prepared under its recorded definition, so the group
    child it forms is the recorded one. There is no whole-catalog digest.
  - A group tool child judges its own tool where it runs, against the
    registry serving it (FIG-3725): a drifted child is served its recorded
    result, and one the engine would run live refuses, records nothing and
    parks the turn (ADR 0103, FIG-3725 amendment). The opener refuses nothing
    before the group opens.
  - A code cell's journaled binding set is judged against the live catalog as
    before, and links against the recorded one.
  - The turn machine starts with no environment and always opens with its
    protocol-start sync; the drive builds no prompt and pins no surface of its
    own.
  - Effect-journal generation 3.
- **Implemented (FIG-3683, the first store read as a recorded step).** A tool
  child reads its recorded execution environment through its own
  `LoadExecutionEnv` step, so a replay executes under the recorded spec and
  never reads the store again. The step records only deterministic answers:
  the spec, or the refusal of an environment this build cannot use. A store
  that did not answer is an engine fault, never the step's outcome: the
  executor marks it with derivation retry authority, and Restate ends the
  attempt without journaling it, so the invocation retry runs the step again.
  A refusal the step recorded is its outcome on every replay, so a recorded
  park settles the child rather than ending the handler. Effect-journal
  generation 4.
- **Implemented (FIG-3726, the same rule for the turn's own store-backed
  derivations).** The environment sync and the assistant-response hooks carry
  the same seam as the tool child's environment read: a live store or session
  fault — the sync's catalog read, the hooks' derivation over the journaled
  completion — is the attempt's fault, never the step's recorded outcome, so
  the executor marks it with derivation retry authority and Restate ends the
  attempt without journaling it; the invocation retry runs the step again.
  What the step decides stays recorded: the synced environment, a refusal the
  sync recorded, a deterministic hook failure. Effect-journal generation 5.

### 11. Validation and laws

Every slice owes three tests where they apply:

- **cold replay:** drop and replay, with always-replay mode where available;
- **separate worker:** a fresh runtime with no live openers, caches or warm
  RLM state replays the recorded journal;
- **perturbed scheduling:** a single-threaded executor with seeded yields and
  poll-order shuffles between ready ops, comparing the command stream and the
  canonical bytes of the recorded command stream and committed store state across seeds.

They run on the in-tree Endpoint double until FIG-3665 lands, then on the
`lash-restate-test` runtime through its one constructor,
`lash_restate_test::backend(seed, cfg)`, shaped for the B2 construction
(`Backend::new(engine, stores)`, ADR 0104 §2).

The seam proof's per-session laws stand, amended:

- **L-S3 and L-S4 assert one durable seal:** exactly one drive-epoch
  transition in the store's epoch history, whatever the number of
  seal-body invocations. Lost replies cause retries, so counting body
  invocations proves nothing.
  L-S3's claim half is keyed by the root, not by an admission-derived
  identity: a redrive of the root, under this admission or a later one,
  replays the one recorded claim and runs on the base it recorded (§2).
- **L-S6: a later epoch adopts an already-committed root.** After a lost
  commit reply and a takeover, the committed root is preserved, never replaced
  with newer content. `admit` reads the root-addressed terminal evidence
  (ADR 0101, FIG-3607 contract 2) and answers `RootTerminal`.
- **L-X1 is widened.** Beyond failing on any wake not caused by a recorded controller operation,
  it compares replays under perturbed scheduling, because a wake tracker alone
  does not catch clock reads, RNG, mutable globals or immediately ready I/O.

### 12. The Restate journal generation cutover

- **Implemented (P1).** Every entry a recorded effect journals in its
  `lash:{replay_key}` slot carries `effect_journal_version`, stamped with
  `EFFECT_JOURNAL_VERSION` (`lash-restate`, registered in
  `scripts/versioned-surfaces.toml` and `lash::formats` as
  `RestateEffectJournal`). An entry stamped with another generation, or with
  none, still decodes, so the SDK never loops on a decode failure, and the
  controller refuses it as the engine-neutral `effect_replay_divergence`
  before the replay acts on it. The refusal parks the turn: the attempt fails
  retryably, the invocation keeps its journal, and only a build of the
  generation that wrote it replays it.
- The gate sits at each recorded effect's entry. A durable process command
  runs its work before its entry is journaled, but in a turn it always follows
  the recorded model call that asked for it, and a process segment's journal
  is refused first at admission by its own `RESTATE_PROCESS_JOURNAL_VERSION`.
  So an old journal is refused before a replay runs any effect.
- **Every later slice that changes the bytes a recorded effect journals, or
  the position of a recorded effect in the journal, bumps
  `EFFECT_JOURNAL_VERSION`.** The surface's guards fail CI on an unbumped
  shape change to the entry, the envelope and outcome vocabulary, or the
  controller error an outcome carries.
- S5 adds the input stamp for the session and turn handlers and the
  session-state generation bump that covers new admission entries.
- The process journal keeps its own version, which P16 bumps.
- The admission marker's nonce seal and its missing-history refusal are
  preserved as two separate steps, and S5 copies them for drives.

Old histories are refused typed and fail closed; there is no migration or
drain.

### 13. Order and ownership

The original plan placed P0, the ADR and proposed types, first. P1 to P7
could start immediately; P8 to P10, P14 and P16 waited for FIG-3585 so they
would not convert code it deleted. P14 followed the group-child fixes, P15
followed FIG-3659 NOW-A, and P18 needed FIG-3665. P16 belonged to the lane
that owned `lash-restate`'s process workflow. This order is historical; the
adopted execution seam is the controller described above.

## Rulings

The plan's open questions, as ruled on 2026-09-24:

| Question | Ruling |
|---|---|
| QB1 Race API | The controller records the winner of each wait and cancel gate; it retires the losing gate after a guarded win (§3). |
| QB2 An oversized turn under continue-as-new | The proposed checkpoint handover was not implemented. The adopted session continuation happens at a turn boundary (§7). |
| QB3 Memory projection refs | Refused on every durable path. The facade break is allowed (§10). |
| QB4 Hooks that receive services | Hooks get read-only services; writes are recorded commands (§6). |
| QB5 Group-child graph appends | A recorded command of the child, incorporated at its rank (§6). |
| QB6 Who owns P16 | The lane that owns the Restate process workflow (§13). |
| QB7 Send and !Send | The controller is `Send + Sync`; the local harness also accepts a `!Send` drive future (§8). |
| QB8 Versions and deployment pinning | `build_generation` pinning (§7). The recorded `version()` op this ruling also kept was dropped when ADR 0106 Q6 ruled lash takes no in-place code patches. |
| QB9 L-S6 | A later epoch adopts an already-committed root (§11). |

## What is deliberately not adopted

- **A pure poll interface for the whole drive.** The turn machine already is
  one. The lashlang VM awaits effects inline, and a poll seam would force a
  suspendable VM at every aggregate. Async code over recorded ops is
  replayable once it awaits nothing else.
- **Twin `Send` and `!Send` traits, or a chain duplicated by macro.** They
  double the surface. Running `!Send` under a `LocalSet` bridged by channels
  is the pattern this ADR removes.
- **Forbidding oversized turns.** It is not viable for long agent runs.
- **Fencing external effects by epoch check.** A check cannot retract a
  request already sent; idempotent operation ids cover that.

## Consequences

- **Replay on Restate becomes correct by construction** once the slices
  land: a replay cannot re-derive a decision from anything it did not record.
- **A second engine is a crate.** It implements the controller and engine
  ports, and passes the laws.
- **Every byte-changing slice is a journal cutover.** Histories written before
  it are refused typed, never migrated.
- **Hook authors lose write access.** Hooks that wrote the store or the graph
  directly declare commands instead.
- **Cost: boxed controller futures.** The controller uses async trait futures;
  the Restate implementation records each operation at its own durable boundary.

## Amendment (FIG-3562, 2026-09-29): determinism binds drives, not tool bodies

[ADR 0116](0116-tools-are-opaque.md) deletes orchestrating bodies, so the D20 peek "before an orchestrating
body" has no subject. A tool child's drive peeks before each attempt only.
The determinism of this ADR binds drivers and engines. It never bound opaque
tool bodies, and the body lint that policed orchestrating bodies is deleted.
The `batch` expansion runs in the standard protocol driver, as a pure
function of the recorded response and the turn's admitted configuration
([ADR 0116](0116-tools-are-opaque.md) §2.1).

## Amendment (FIG-4125, 2026-09-29)

Item 4: The old version-bump and refusal prescription in §12 is historical
during the pre-1.0 freeze. Durable shapes change in place until
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) establishes
the 1.0 baseline. FIG-4110 owns §6.
