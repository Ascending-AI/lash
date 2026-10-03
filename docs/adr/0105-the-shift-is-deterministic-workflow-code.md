# 0105: The shift replays recorded decisions through the controller

## Status

Accepted.

## Context

A durable shift must resume without choosing different effects from live
clocks, caches, cancellation tokens or observation arrival order. The turn
machine consumes recorded inputs; the controller records the operations whose
answers decide work. This is the execution contract of ADR 0104 §2.
Durability is the goal. Deterministic scheduling of the test harness is not.

## Decision

### 1. The shift uses the recorded controller

A shift issues effects through its scoped `RuntimeEffectController`. Restate
records the canonical envelope and outcome under the effect's replay key,
compares the envelope on replay, and serves the recorded outcome. Code cells
reconstruct interpreter state by re-execution under ADR 0103.

Shift decisions derive from recorded outcomes, immutable admitted inputs and
pure functions of them. Observation is addressed by replay identity and
ordinal; it cannot decide a commit. A live storage check can refuse stale or
inconsistent execution, but cannot replace recorded work with newly selected
work. Restate records durable timers, keyed waits and cancellation races.
Opaque tool bodies execute inside recorded attempts under ADR 0116; they are
not replayable orchestration drivers.

The rule holds on every durable path, not only in the shift. A durable path is
any code that can run more than once for one durable effect: a handler's
replay, a retried recorded step, a workflow redrive, a duplicate or redelivered
invocation carrying the same durable identity (a signal id, a trigger
occurrence key, a tool-intent identity, a session-command idempotency key), and
a facade call retried under its handler's journal. On a durable path every
decision that shapes what is recorded or returned comes from one of three
sources:

- a recorded outcome: a journal entry of this invocation (a step result, a
  promise, an awakeable, a call reply);
- a recorded admission: the retained row a durable identity's first admission
  wrote, returned by the admission itself when it coalesces (the retained
  signal event and its wait binding, a trigger delivery's bound process, a
  tool-intent submission's recorded outcome, a session command's persisted
  outcome);
- an immutable admitted input, or a pure function of the three.

A fresh read of mutable store state is allowed in exactly two places. It may
run inside the recorded step whose output it becomes: the step journals the
answer, and nothing after the step decides from the read except through that
output. It may serve explicitly non-durable observation: listings, snapshots
and cursors a host reads and nothing records, and the live revalidation above,
which can stop stale work before its next effect but never chooses different
work. Anything else is a violation, in one of two forms. A read before the
record consults today's registry before the journaled command issues or
replays, so after a prune the replay refuses or branches differently. A
duplicate that re-decides runs its step fresh without a journal and re-derives
the decision from mutable state instead of from the admission the store
coalesced. An idempotent admission therefore retains every fact its first
decision used (the target binding, the selected wait, the canonical request),
and a coalesced admission returns those facts for the caller to use without
further reads.

So the process commands (start, signal, cancel, await, attach) carry their
target and session checks inside their recorded admissions. A cancel, signal or
await of a pruned process refuses or answers `NoLongerRetained` from the
recorded guard or admission, and a replay after a prune returns what the first
run recorded. A host-wide cancel-all records its running-process selection as a
`ProcessCommand::List` before issuing cancels from those recorded rows.
`CompleteExternal` records the observer verdict, attachment acquisition and
external-owner terminal write together, and returns that completion outcome
on replay. `ValidateVisible` records the session-observer or process-starter
verdict before a handle command runs. Compaction and observer transfer cannot
change any of these recorded decisions.

A host start `Until` a session checks the session inside its
recorded start admission (`HostSessionNotLive`), so a replay after the session
is deleted returns the recorded start. A tool-intent redelivery claims its
submission-ledger row before it realizes anything, and a row that already
holds an outcome answers that outcome.

Evidence: `crates/lash-restate/src/controller/journaled_effect.rs:314`,
`crates/lash-restate/src/controller/execution.rs:122`,
`crates/lash-core/src/runtime/observation_publisher.rs:1`,
`crates/lash-core/src/runtime/turn_boundary/recorded_assembly.rs:1`,
`crates/lash-core-execution/src/engine/testing/controller.rs:40`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs:1`,
`crates/lash-restate/src/controller/process_command.rs:1`,
`crates/lash/src/tool_intent_ingress.rs:1`.

### 2. Unfenced admission, separate from fenced execution

`AdmitShift` records the selected run and admission identity. Before execution,
`DrawRunStart` records a random nonce in the run's journal, and
`SealShiftAdmission` raises the shift epoch by store compare-and-set. The same
admission and nonce replay the stored fence. A different nonce for the same
sealed admission is `ExecutionLost`, reported as `SubstrateLost`, and runs no
run work. A child inherits authority rather than minting an epoch. L-S8 is
the missing-history law.

`AdmitRun`, keyed by the run, binds its rows and records the base
`SessionHeadRef`, turn index and executable generation in `RunAdmission`.
Replay reconstructs the admitted state rather than selecting from the live
head. A base the store cannot retain refuses `TurnBaseNotRetained` and parks.
An unfinished run keeps ownership of its admitted head until terminal evidence.

`InspectAdmittedHead` records one of these verdicts:

- `Ready` reconstructs the admission's base, including when the run already
  has a committed turn.
- `Advanced` reconstructs the head the unfinished run's fenced commits
  publish, including its own frame changes.
- `Overtaken` means another writer advances past the base. The run ends with
  `StoreCommitSuperseded` before turn effects.
- `Diverged` means a lower revision or the same revision with inconsistent
  leaf or checkpoint. The run parks.

Replay honors the recorded verdict at every journal position. A head that
moves after a recorded `Ready` is met by the fenced commit, rather than a live
re-check that changes the command stream.

A follow-on recovery run records `RecoverFollowOn`, keyed by the run
(`shift-follow-on:{run}`), between its seal and its turn. Its body reads the
fact the head owes, raises the recovery count in a fenced write, and records
`Run` with the raised fact, `Exhausted` past the recovery bound the fact froze
for its logical run, or `Ceded` when the head does not owe the follow-on. The
recorded fact carries that bound; the recovering host's bound never decides.
The run executes the recorded answer and never re-decides from the head a replay
finds. `Run` and `Exhausted` also record the head the follow-on's turn runs on
and that turn's index, and the body retains that head as the session's latest
admission's base before the raise, as `AdmitRun` retains its own. The run
adopts the recorded head and pins the recorded index before its turn, so a
replay past the follow-on's own commit runs the turn its journal holds on the
head it was recorded on, never on the head that commit moved (FIG-4380).

A live read outside the recorded steps never decides which steps a run
journals: not the resident-session refresh before `AdmitRun`, not the engine's
opening of the session. A sealed run whose shift holds no current head,
because the engine cannot open its session (its close or tombstone committed)
or because the refresh fails, runs headless. It still issues its recorded steps
under the envelopes a run with a head issues: `AdmitRun` and
`InspectAdmittedHead` for an input- or queued-headed run, the
`session-command-run:{n}` reads for a command run, and `RecoverFollowOn` for a
follow-on recovery run. A command run also reads on headless once its session
retires under a run it read, because every command but a compaction settles and
commits off the journal. The bodies of those steps admit, inspect, read and
decide nothing. A deleted session is the step's recorded outcome: the catalog's
tombstone, read inside the step, or a session the engine cannot open. The run
then ends with the typed `SessionDeleted` refusal of
[ADR 0049](0049-session-ids-are-used-once.md) where its journal holds nothing
more. A recorded `Ceded` cedes the run as it does with a head. Any other refresh
fault is the attempt's and is recorded nowhere. A journal holding work past
those steps (the turn after `InspectAdmittedHead` or after a recorded `Run` or
`Exhausted`, a compaction's apply) cannot be retraced without the session's
head. That attempt ends as a live fault that journals nothing, and the engine's
park reconcile releases a run whose session stays deleted as `TargetGone`.

A sealed execution carries `ShiftFence`. Head-changing writes and ingress
settlement check that fence in their transaction. Replay envelopes do not hash
the fence into effect identity, L-S12. A refusal no retry can change ends its
run through `RunTerminalCause::Refused` before the engine records the outcome.
That idempotent end presents the run's fence; a stale owner leaves the
run to its current owner. A park, unknown commit outcome or retryable live
fault ends no run. A later epoch adopts existing run terminal evidence,
L-S6, rather than replacing it with a new commit.

The session head has one owner at a time: the unfinished run, from its
admission until its terminal; an owed follow-on; or the command lane while a
session command is open, sealed command runs included. A head-changing write
that does not present the owner's current fence is refused in its own
transaction, on SQLite and PostgreSQL alike, with the typed, retryable
`StoreError::SessionHeadOwned`, which names the owner. A host's head write from
outside a turn is therefore a session command the shift applies at a turn
boundary (ADR 0101 §4), and a terminal callback writes under its ended run's
fence. A run commit can still meet a moved head when another writer presents
the same fence, such as a second execution of the run. That commit ends its
run with `StoreCommitSuperseded` and never wedges; the redrive that reloads
the head is a new run.

Evidence: `crates/lash-core/src/runtime/shift/admission.rs:94`,
`crates/lash-core/src/runtime/run_start.rs:1`,
`crates/lash-core-store/src/store/shift_fence.rs:185`,
`crates/lash-core-store/src/store/head_ownership.rs:1`,
`crates/lash-core/src/runtime/shift/run.rs:105`,
`crates/lash-core/src/runtime/shift/run.rs:457`,
`crates/lash-core/src/runtime/shift/run.rs:1125`,
`crates/lash-core/src/runtime/shift/run.rs:677`,
`crates/lash-core/src/runtime/shift/run.rs:828`,
`crates/lash-core/src/runtime/shift/run.rs:876`,
`crates/lash-core/src/runtime/shift/run.rs:929`,
`crates/lash-core/src/runtime/shift.rs:886`,
`crates/lash-core/src/runtime/shift.rs:922`.

### 3. Cancel races and losing work

The controller records the winner of a durable wait and its cancel gate.
Replay uses that winner. An `AfterStep` request remains deferred while the
step runs, and an `Immediate` escalation can stop the guarded wait. A recorded
step keeps a live cancel watch inside its body; what the body returns is the
recorded outcome. Replay does not re-run the watch.

A watch failure is not cancellation. The shared retry ladder has eight
attempts, starting at 25 ms and doubling to a 1 s cap. A step that can leave
a derivation unrecorded ends the attempt with `TransientCancelWatch` when the
watch gives up. A tool attempt whose engine records every returned outcome
runs to its own end instead; the shift observes cancellation at its next
recorded peek.

The shift advances its honored cancellation fact only through recorded peeks
and outcomes. A host-local stop is forwarded as a durable gate request.
Execution-side tokens stop bodies; they do not select shift work.

A code cell observes cancellation at instruction-count checkpoints. Its first
gap is 2^20 instructions; gaps double to a cap of 2^28. The accounting and
cell grammar belong to its executable generation. Process waits and body
checkpoints observe the process's durable cancellation fact. The process
handler journals registry steps for admission, resume, completion, boundary,
handover, cancel forwarding and handover retirement. A retryable store fault
is the attempt's fault rather than a durable decision.

Evidence: `crates/lash-restate/src/controller/context.rs:1`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:235`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:350`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:369`,
`crates/lashlang/src/runtime/mod.rs:185`,
`crates/lash-restate/src/process/workflow.rs:1`.

### 4. Group operations are complete

The controller opens durable groups, serves ranked settlements, reads a rank
without advancing its cursor, commits child final results under the cancel
fence, waits for protected drain and closes under the loser policy.
`EffectGroupHandle` owns the settlement cursor. ADR 0099 defines the child
attempt, committed and seated boundaries.

A wait child races its guarded timer or keyed wait against the child's durable
cancel wait in the journal. A tool child's driver peeks before attempts, and
the attempt body watches the decided cancel. The attempt's cancelled outcome
is recorded. A lost watch does not fabricate cancellation. Settlement,
retirement and cancel admission retain their group fences across replay.

Evidence: `crates/lash-core-execution/src/runtime/effect/executor/control.rs:369`,
`crates/lash-restate/src/effect_group/child_cancel.rs:1`,
`crates/lash-restate/src/controller/context/child_cancel.rs:1`,
`crates/lash-restate/src/effect_group/dispatch.rs:368`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:300`.

### 5. Keyed promises

A resolution persists before a waiter exists. The first writer wins.
`ResolveOutcome` distinguishes `Accepted`, `AlreadyResolved { terminal }` and
`UnknownOrRevoked`; a duplicate returns the retained terminal and cannot
overwrite it. Revoking a session tombstones its wait index and cancels its
waits; later resolves return `UnknownOrRevoked`.
An acknowledged resolution is durable. The engine's wait identities remain
private behind neutral keys under ADRs 0003 and 0012.

Evidence: `crates/lash-restate/src/durable_wait.rs:1057`,
`crates/lash-restate/src/durable_wait.rs:1414`,
`crates/lash-restate/src/durable_wait/run_retirement.rs:1`,
`crates/lash-core-effect/src/retirement.rs:200`.

### 6. Protocol drivers and hooks use recorded inputs

`ProtocolDriverHandle` and `ContextProjector` are synchronous decision APIs
over recorded inputs. Interior mutability that changes replay decisions
violates their contract. Provider and tool I/O goes through recorded effects.
A plugin's live mutable memory is not the authority for a replayed decision.

Hooks receive read views and return decisions; core owns the resulting writes.
The context-pressure hook runs before Prompt View transforms with the
committed view, previous prompt usage and context window. `Record` nodes join
the turn's append draft. `OpenFrame` performs its own fenced, idempotent commit
before the turn's model call. The frame key and operation identity derive
from the turn and hook. Replay uses the admitted base and recorded summarizer
completion, and checks the frame commit's receipt. A later turn failure leaves
that committed frame in place. The write is a store operation under §9, not
an extra controller command variant.

The phases a model call issues are recorded with it. Phase 1 journals the raw
completion together with its ordered response callback plan. Each entry names
its callback key and owning plugin revision, selected before the paid call.
An empty plan serves the raw completion. Replay follows the recorded order,
and a completed derivation serves its journaled result without callbacks.
An unfinished derivation resolves every recorded callback before invoking any
and parks if a key or revision is unavailable. Stream end state pairs with
one response callback identity, including its revision.

Evidence: `crates/lash-sansio/src/sansio/turn_protocol.rs:699`,
`crates/lash-sansio/src/sansio/turn_protocol.rs:779`,
`crates/lash-core/src/runtime/turn_loop/context_pressure.rs:1`,
`crates/lash-core/src/runtime/turn_loop/context_pressure.rs:114`,
`crates/lash-core-execution/src/runtime/effect/llm_outcome.rs:44`,
`crates/lash/src/tests/response_phase_replay.rs:1`.

### 7. Continue-as-new and version decisions

A session shift runs at most `MAX_RUNS_PER_SHIFT`, 64 runs, per invocation.
At a run boundary the `SessionShifts` sends a continuation with an idempotency
identity derived from the shift request. It transfers no live turn machine or
group cursor.

Restate counts a handler's failed attempts over the invocation's whole retry
loop, which only a suspension or a new invocation restarts. A shift that
awaits one run after another never suspends, so its eight-attempt budget
would be spent on the sum of every run's failures. The shift therefore
records two kinds of step. Its first command is `lash.shift.leg`, whose body
marks the attempt that runs it as the one that started the leg. The step
precedes admission 0 and the read of the installed driver, so a failed
attempt inside either is seen, and as the shift's first command it carries
the shift's generation (§12). After each run it goes on from, the shift
records `lash.shift.boundary`: whether the attempt that reached the boundary
was served the leg's start from the journal, which makes it a retry or a
resume. Such an attempt hands off there, before its run bound, and its stop
is `ShiftStop::HandedOff`. One retry loop then covers the runs up to the
first boundary a replaying attempt reaches live, the leg's first boundary
included, never the backlog. Under always-replay every attempt past a leg's
first await replays, and a shift runs in legs of one run. An attempt that
fails before the leg's start is stored journals nothing and is not seen: a
deployment that refuses the invocation, or a request the handler cannot
open.

A shift whose build is draining hands off sooner, before its next run
([ADR 0106 §1](0106-durable-formats-upgrade-by-migration-or-drain.md)): the
recorded admission answers `AdmitVerdict::Draining` and the shift sends its
continuation under the stable name, to the newest build, with the stop
`ShiftStop::Draining`. The decision is the journaled admission's, so a replay
makes it again.

The continuation's request carries what the kernel's stop rules (`ShiftLoop`)
remember of the runs the handing-off leg ran, and the next leg starts from
it. A run that admission names again right after it ran therefore stops the
shift in the leg that meets it, as it does inside one invocation, instead of
being called again by a leg that forgot it. Only the handing-off leg's own
runs travel, so the request stays bounded by one leg. A waiter's attach
under the continuation's identity carries none; it joins the invocation the
leg's journaled send started. Process execution uses bounded segments and
durable handovers.

`ShiftRequest.build_generation` names the build generation. Recorded routes
and generation sentinels bind journal replay under ADR 0106 §1. A process's
next segment can enter the latest compatible build; replay does not patch the
old segment's journal in place. Durable formats follow the current freeze
and compatibility rules of ADR 0106.

Evidence: `crates/lash-core-execution/src/engine/shift.rs:1`,
`crates/lash-restate/src/session_shifts.rs:1113`,
`crates/lash-restate/src/process/workflow.rs:1`,
`crates/lash-core-execution/src/engine/contracts.rs:37`.

### 8. Send and !Send

`RuntimeEffectController` is `Send + Sync` through its resolver contract and
returns `Send` async futures. The local replay-check harness accepts a
`!Send` shift future and polls it on one thread. Production Restate handlers
use the production controller. The local harness's execution model supplies
no shipping engine or scheduling guarantee.

Evidence: `crates/lash-core-execution/src/runtime/effect/executor/control.rs:369`,
`crates/lash-core-execution/src/engine/testing/check.rs:1`.

### 9. Commit, park and settlement use fenced store writes

Core assembles the commit from recorded results and calls
`commit_runtime_state_verified`. The store checks head, fence, operation
identity and existing receipt, then writes the head, run terminal evidence,
and ingress settlement together. Lost replies are checked against the
stored commit. The shift can safely repeat the idempotent write. An exact
replay of a stored commit answers from its receipt even when its fence is
stale, and writes nothing: a shift that runs several runs in one journal,
such as a `SessionTurn` process that executes a run queued ahead of its own,
replays the earlier run's commit after the later run's seal, and a committed
run whose reply was lost is redriven after a session command's seal, such as a
model change's. The receipt is the whole boundary: who raised the epoch does not
matter. A stale fence still refuses every commit the store has no receipt for,
including one that reuses a stored commit's operation with other content.

A classified run park is an idempotent store write that cannot replace an
existing terminal. Reconciliation can write the same park. A non-retryable
run refusal writes its terminal before returning its engine outcome, as in
§2. Commit, park and ingress settlement have no separate journaled command
variants. External operations still need stable idempotency identities; a
store fence cannot retract a request already sent.

Evidence: `crates/lash-core/src/runtime/turn_boundary.rs:1`,
`crates/lash-core/src/runtime/turn_loop/commit.rs:1`,
`crates/lash-core/src/runtime/shift/park.rs:1`,
`crates/lash-core/src/runtime/shift.rs:922`,
`crates/lash-core-store/src/store/runtime_commit.rs:1`.

### 10. Commands and executors

A serialized `RuntimeEffectEnvelope` produces a `RuntimeEffectOutcome` or a
typed controller error. The runtime executor supplies the recorded body; live
services do not become part of command bytes. Restate validates the canonical
envelope before returning a recorded result. An envelope mismatch is
`EffectReplayDivergence` and parks.

Environment sync records prompt and catalog definitions, and the shift
installs what it returns. A tool child loads its execution environment through
its own `LoadExecutionEnv` step. Deterministic refusals remain recorded answers.
A live store or session fault in an uncommitted derivation is retry authority:
the engine ends the attempt without recording that fault as the step's answer.
The same rule applies to sync and assistant-response hook derivations.
Recorded tool definitions and drift handling follow ADR 0103.

Evidence: `crates/lash-core-execution/src/runtime/effect/envelope.rs:1`,
`crates/lash-restate/src/controller/journaled_effect.rs:314`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1`,
`crates/lash-core-execution/src/runtime/effect/executor.rs:1`.

### 11. Validation and laws

Replay evidence includes a cold replay, a separate worker without live caches,
and perturbations of recorded-outcome delivery. The local `DeterminismCheck`
compares command transcripts and commit bytes across these modes; its name
does not impose deterministic scheduling on production or lash-sim.

L-S3 and L-S4 require one durable epoch transition per admission, regardless
of seal-body retries. L-S6 adopts the terminal of an already-committed run.
L-S8 refuses fresh execution after retained start history is lost. L-S12
keeps shift fencing out of replay-envelope identity.

Storage laws run on SQLite file, SQLite memory and PostgreSQL. Execution
hosts are the in-process Restate server double, live Restate and lash-sim's
in-process effect host. The server double supports crashes and always-replay
through its backend constructor. Restate cancellation and process crash
matrices exercise the production handlers. Upgrade proofs use synthetic-next.

The stale-fence boundary in §9 has two turn-config laws:
`a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt`
and `an_older_admission_redriven_after_a_profile_change_is_fenced_out`.
Together they replace
`a_committed_run_redriven_after_a_model_change_refuses_its_stale_epoch`,
whose refusal of an exact stored commit contradicted §9. The refusal law
still requires `StaleShiftFence` for changed or unrecorded commits and
`StoreCommitSuperseded` at the runtime boundary.

Both run in `//crates/lash-restate:lash-restate__unit_test`, under
`tests::shift_laws_on_the_double`, on SQLite memory and file, plain and
always-replay. CI's `buck2-tests` job selects that target through
`//:workspace_core_tests`. The ignored PostgreSQL legs run through
`scripts/ci/store-tests.sh pg-store`. Reproduction must select the current
law names and confirm executed cases; selecting the retired name proves
neither replay nor refusal.

The §1 durable-path rule has a repository gate and a law harness. The gate,
`replay_read_gate`, is a test over the workspace source (`crates/`,
`examples/`, `runbooks/`). It finds every replay path: a function whose
signature names a recorded controller or context, a method of a type holding
one, and every helper those call outside a recorded step. It then fails on
each read of the store's mutable surface (the process registry's query,
observer, event-log and lifecycle reads, the trigger store's listings, and the
facade's session and process lookups) that runs on a replay path outside
every recorded step. Its surface also holds the live host services a durable
path consults (the trigger route restorer): a call to one outside every
recorded step fails the same way. A service call is matched by its receiver's
declared trait, including typed closure parameters and bindings carried
through closure receivers or match scrutinees, not by the
method's bare name, so an unrelated method of the same name is no hit. It
parses the store and service traits, so a new trait method must be classified
as a read, as a write or admission, or as a live service before the gate
passes. A read the rule allows outside a step (a non-durable observation, a
stop-only revalidation, an exempt store-side fact) is pinned in the gate's
table with its class and reason, and a violation owned by an open ticket is
pinned with that ticket; a pin that matches nothing fails as stale. A live
host service has no pin class: it serves new work only. The route restorer is
asked inside the delivery start's recorded admission (`register_process_start`),
and only while no process holds the start's key, so its answer, an unavailable
or revoked route included, is that step's recorded outcome; a replay reads the
record and a redrive finds the started process, and neither asks again.
Hosts install this live service through `LashCoreBuilder::trigger_route_restorer`;
immediate facade and session emissions and delivery recovery share it. Emit
reports retain the recorded refusal code as an enum locally and remotely.
The
gate's self-tests plant a read ahead of a journaled command, including in the
real facade, and a restorer call ahead of the delivery start and of the
Restate registration step, and require the gate to fail.

The harness, `crates/lash/tests/replay_after_advance.rs`, records one durable
operation per effect family (process start, trigger emit, signal, cancel,
process await, durable wait), moves the store on (the target ended, pruned
and compacted, the session deleted, the wait resolved again, the delivery's
provider route revoked or restored), then loses the handler attempt so the
engine replays its journal. The replay must answer the recorded outcome, write
no registry row and send no new invocation. It runs on the Restate server
double over SQLite memory, SQLite file and PostgreSQL, and on live Restate
(the `replay-after-advance` suite in `scripts/restate-suites.toml`). A fresh
command against a pruned target still refuses there, since the refusal is the
recorded admission's. The tool-intent family's redelivery laws (start, signal,
cancel and trigger emit, after a prune and compaction or a session delete) run
in the ingress unit tests over `KeyJournalController`, because the ingress on
Restate runs only inside a handler scope: each redelivery arrives on a fresh
invocation with an empty journal and must answer the submission ledger's
recorded outcome and register nothing.

Evidence: `crates/lash-core-execution/src/engine/testing/check.rs:25`,
`crates/lash-conformance/src/conformance/shift_admission.rs:1`,
`crates/lash-conformance/src/conformance/run_start_marker.rs:1`,
`crates/lash-restate/src/tests/shift_laws_on_the_double.rs:1`,
`crates/lash-restate-test/tests/process_crash_replay.rs:1`,
`crates/lash-upgrade-harness/tests/phase_a/main.rs:1`,
`crates/lash-core-execution/src/replay_read_gate.rs:1`,
`crates/lash/tests/replay_after_advance.rs:1`,
`crates/lash/src/tests/tool_intent_ingress/replay_after_advance.rs:1`.

### 12. Journal generations and the version freeze

Recorded effects carry `effect_journal_version`. A foreign or absent stamp
decodes as a retained value, then refuses through the controller as replay
divergence rather than looping on a deserialization failure. The first session
or turn step also carries its build-generation sentinel. Process and group
journals have their own registered drain surfaces.

The pre-1.0 freeze changes shapes in place, without version bumps or upcasters.
The existence of a stamp does not promise compatibility across those edits.
Release compatibility, migration and drain are governed by ADR 0106 and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

Evidence: `crates/lash-restate/src/controller/effect_journal.rs:182`,
`crates/lash-restate/src/controller/effect_journal.rs:226`,
`crates/lash-restate/src/sentinel.rs:89`,
`scripts/versioned-surfaces.toml:1`.

## Rejected alternatives

A poll interface for the entire shift adds suspension machinery around a VM
that already awaits controller effects inline. Separate `Send` and `!Send`
controller traits duplicate the contract. An epoch check alone cannot fence
an external request already sent; stable idempotency covers that boundary.
A recorded step cannot select a live body away outside the body and still
replay its outcome. Cancellation therefore belongs to the recorded wait or
step body.

## Consequences

Durable replay uses recorded decisions and repeats fenced, idempotent writes.
Live observation and cancellation transport do not replace recorded facts.
Controller adapters own journal validation and durable wait races, while core
owns commit and terminal evidence. The contract applies to drivers and engines;
opaque tool bodies are recorded execution. Changes under the pre-1.0 freeze
carry no cross-build compatibility guarantee.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.
