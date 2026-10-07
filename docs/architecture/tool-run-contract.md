# Tool-run contract

This is the contract every tool call runs under. A call's admission (A), its
attempts (X), its decision (D) and its presentation (V) run in memory, inside
the admitted execution that makes the call durable
([ADR 0132](../adr/0132-durability-is-state-first-over-the-lash-store.md) §5).
Nothing is a journal, and no code ever runs again against a recorded history.
Long, isolated or independently living work is a process admitted before its
body runs. [ADR 0099](../adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md)
records ownership and [ADR 0116](../adr/0116-tools-are-opaque.md) defines
authoring.

## Where a call is durable

A call has one durable home: the admitted execution that runs it.

- **A turn's tool round** (`runtime/actor/round`, the phase runner's
  `durable/tool_round.rs`). The round is admitted in `model.done`, with an
  `x_start` for every member. No body starts before that commit, so a model
  stream that never committed launches nothing. Each member's attempt runs
  the call's A, X, D and V in memory between its `x_start` and its
  `x_outcome`; the outcome's material is the answer the machine is given.
  A resume folds the round's run records: a started `Once` without an
  outcome is `Interrupted` and never starts again, and a started `Repeatable`
  reruns at its ordinal. A retryable failure is a known failure, and its
  retry due is a row. A turn cancel ends unfinished members as `Cancelled`.
  The round's `present` record rides the turn's next commit
  (`round.present+model.start` or `turn.commit`) and answers the members
  in declared order. A member that parks records an `x_wait` before its
  `x_outcome`; see Pending calls.
- **A code cell** (`tool_dispatch::CellMembers`, `lash_vm_broker::members`).
  Every call a cell makes is its own admitted execution: the quiet point of
  the operation that issues it admits it in `cell.snapshot+admit`, under the
  policy and limit its tool declares and, for a tool that may defer, a
  pinned `tool_completion` wait. Its body runs through the same member
  lifecycle as a round's, so a started `Once` is `Interrupted` (injected as
  `cell.inject`), a `Repeatable` reruns at its ordinal, and a parked call
  waits on its row; a cell whose calls wait only on rows suspends its turn.
  The cell is answered from the members' committed outcomes alone, so a
  restore onto the operation answers it the same way. A member its cell
  answered before it settled (a race's loser) stays in the snapshot's ledger
  and settles on its own. A cell resumed from its snapshot never asks again
  for a value its heap holds.
- **A lashlang process**: its steps are admitted executions of the same
  lifecycle (`runtime/actor/process`).

`RuntimeExecutionContext::round_tools` gives a turn's drive its
`RoundTools`; `CellMembers::new` gives a cell the bodies of its calls.

## Seams

| Seam | Contract | Pinned in |
| --- | --- | --- |
| K1 | Admission: owner, call id, prepared request, three-capability declaration, callback binding, execution policy, before-check record, capacity | `lash_core_store::tool_run::admission`; `tool_dispatch::call_run::AdmittedToolCall::admit` |
| K1/K3/K10 hooks | Tool hook phases, occurrences, verdicts, reducer and selection | `lash_core_store::tool_run::tool_hooks` |
| K2 | Owner-qualified material references and typed retained-result refusals | `lash_core_store::tool_run::material` |
| K3 | A call's attempt outcomes and its one final-or-cancel decision | `lash_core_store::tool_run::run_event` |
| K4 | A parked call's completion source: its `tool_completion` wait and the process terminal it races | `lash_core_store::tool_run::run_event::CompletionSource`; `runtime/actor/waits` |
| K5 | Declared start obligation: stable `StartKey`, environment, consumer hold, cancel policy | `crates/lash-core-execution/src/runtime/process/declared_start.rs` |
| K8 | Operation Run input kind over the session-operation opener | `lash_core_store::tool_run::operation` |
| K10 | Callback slots, state authority, command batches, resolutions, applied frontier | `lash_core_store::tool_run::state_command` |

## One call

`AdmittedToolCall::admit` validates the declaration, checks the binding
against the plugin revisions this build executes, binds an isolated call to
its process, prepares the request once and asks every before-check of that
one prepared request. `attempt` runs the body once and captures it under the
admitted declaration, then decides it: the Run's cancel, then every
after-check. A final realizes its declared intents in place, launches and
discharges its declared start, and is presented; a withheld call emits its
stream and is observed. A failure its policy repeats ends the attempt, and
the member lifecycle records the retry's due as a row; a cancel during the
backoff ends the call `Cancelled` and starts no next attempt.

The declared intents of a final realize in place
(`SingletonToolHandlers::realize`, `execute_final_tool_intents`) before it is
presented. There is no separate realization invocation.

## Aggregates

A cell's aggregate is one operation: its calls are admitted together, and
its timers are pinned with them. A consumer (`race`, `any`, `all`,
`allSettled`, the list batch) answers from the order leaves settled
(`lash_lashlang_runtime::aggregate_answer`): leaves settled when the
aggregate formed (a refused call, a plain operand) come first, in source
order; members follow in the order their outcomes committed, then timers
that came due. `all` reports the first rejection; `any` the first
fulfilment, or every rejection. A loser keeps running as its own member.

A cell counts every tool call its quiet points admitted, in its snapshot
envelope. Past `max_tool_calls` the operation's calls are refused whole
before any is admitted.

## Binding rulings

**Declaration.** The author declares exactly `may_defer`, `intents` and
`isolated`, as the `ToolDeclaration` on the tool's `ToolManifest`. Admission
reads it from the manifest the call is admitted under: the catalog's, the
grant's, or a replayed cell's recorded binding. A round's calls are admitted
together before any prepares: an invalid declaration, or an isolated
declaration with no bound process implementation, refuses every member with
a typed `ToolAdmissionRefusal`. In a turn's round the refused members settle
at admission, in `model.done`, and no body runs. An outcome the declaration
does not admit (Deferred without `may_defer`, an undeclared intent kind)
fails the call with `ToolFailureCause::Declaration` before anything it
declared is realized. An isolated call is a process from its start, with no
inline body. There is no per-call timeout, duration, budget or idempotent
capability. Retry and cancel policy are recorded runtime policy; a stored
`Repeatable` with a current `Once` does not repeat, and a stored `Once` is
never upgraded. `ToolCallId` is the external idempotency key.

**Binding.** Admission binds the executable, preparation and presentation
callbacks as `PluginCallbackIdentity { owner: PluginRevision, key }`. A call
whose bound plugin revision is unavailable refuses with
`PluginExecutionRefusal` (`plugin_revision_unavailable`) before any body.
Presentation binds an optional singleton presenter followed by ordered steps;
an empty plan is explicit and adopts no callback installed later.

**Material.** A reference carries owner (Run, process or source), role
(prepared request, attempt output, presentation), location (journal-local or
retained artifact) and a digest under `lash-tool-material/v1`. Retention
moves bytes, never identity. A failed read is a typed `MaterialRefusal` and
never re-executes a body. A round member's outcome names journal-local
material held by its own run record.

**Operation.** A tool-bearing host operation is a Run with its own input kind
over the session-operation opener. Its call ids and start keys keep their
bytes.

**State and hook policy.** Only before-turn, after-turn, checkpoint and
after-tool (result check) callbacks on the Run's sequential path may return
state commands. A successful final's commands are reduced privately at its
decision and commit in the member's `x_outcome` record; they publish into the
resident namespace only once that record is committed, and a resume publishes
them from the same record before the round is presented. A failed or
cancelled candidate publishes none. One refusal publishes nothing. Two
members of one round that change one namespace apply in the order they
reduce: the second reduces only once the first's outcome committed and
published, against that committed value, so no member reads another's
uncommitted state.

The namespace a member's resolutions publish into is its owner's. A turn's
round members and a code cell's calls change the session's namespaces. An
engine process's tool steps change the process's own: they run on the
process's plugin session, which one activation of its actor holds, and the
step lifecycle publishes each committed `x_outcome`'s resolutions into it. A
new activation builds the session again and publishes every committed step
outcome's resolutions into it from the rows before any step runs on it. A
`SessionTurn` process has no steps; its child turn's tools change the child
session's namespaces. A code cell's quiet point prunes the records no
snapshot can reach again; the snapshot that prunes a settled call's
`x_outcome` carries its resolutions in its ledger, and a resumed cell
publishes them before it runs.

**Hook composition.** For one admitted call: argument transforms, provider
preparation, then every before-check on one immutable prepared call. Checks
reduce by AbortRun > Deny/Cancel > CachedSuccess > Allow, ties broken by
ascending UTF-8 plugin id and then callback key. A cached success is data
only and still passes the result transforms and after-checks. After-checks
return only Allow, Deny, Cancel or AbortRun and never replace a result.
A check's cancel ends only its call. AbortRun fails the call, and its
output carries the `ToolControl` that ends the cell or round that took it.

**Attempt stream.** The bounded stream a body emits belongs to its attempt's
capture (`AttemptStreamRecorder`, capped by `ATTEMPT_STREAM_BYTE_BUDGET`
under a typed `AttemptStreamTruncation`). It is emitted when the call is
presented or withheld.

**Declared starts (K5).** A final may declare one process start under its
stable start key, bound to the Run's environment and to a consumer hold that
carries the call's cancel policy. A keyless start, or one in a Run with no
environment, is the attempt's typed `StartRefused`. A withheld call never
reaches its launch. A final launches its start under the key, then
discharges it: a Run cancel that fired by then cancels the process when its
policy owes that, and the hold is released, all before it is presented.
A lost launch registers again under the same key and recovers the same
process.

**Pending calls.** A turn's round admission pins a `tool_completion` wait
(L5's `waits::pin`) for every member whose declaration may defer, with the
member's one limit as its deadline, and records the wait in the member's
admission. The body re-derives the wait's `wk1` key with `waits::host_key`,
so a rerun is handed the same key; the tool hands it to whoever completes
the call. A body that returns Pending records an `x_wait` carrying its
`CompletionSource`: the wait, the process terminal of its declared start,
if it launched one, and the parked call's material. The round runner
races the parked members' waits (`waits::race`): the host's
`resolve_host` of the key, or the declared start's terminal, settles the
call through `RoundTools::resolved` in its one `x_outcome`; the deadline
settles it `TimedOut { ExecutionTotal }`, and a turn cancel `Cancelled`.
Before that outcome is recorded, the park is discharged: a call that ends
cancelled under `CancelExternalWork` cancels the child its declared start
launched, and the call's hold on that child is released. Both are
idempotent, so a crash before the outcome repeats them harmlessly. A forged
or altered key verifies nothing and resolves nothing. A code cell's
admission pins the same wait for each of its calls that may defer, and the
cell races it through the same member lifecycle. A process pins no
completion wait, so a Pending body there is refused typed
(`pending_tool_missing_completion_key`).

## Identity preimages

The goldens in `crates/lash-core-store/src/tool_run/identity_tests.rs` pin
these preimages byte for byte:

- `ToolCallId`: `tc_` plus BLAKE3 under `lash-tool-call-id/v1`, rooted in the
  opener's admission (ADR 0117 §2).
- Opener encodings: `turn:`, `drain:` (session operation) and `process:`,
  each component length-prefixed.
- `StartKey`: `process-start-key:v1:<namespace>:blake3:<hex>` for the
  intent, trigger, host and keyless families; keyless keys take scope tags
  1 turn, 2 process, 3 session operation, 4 session delete and
  5 runtime operation.

A change that moves one of these is an identity change, never a shape change.

## Laws

| Law | Pinned by |
| --- | --- |
| No `Once` body starts twice; a started `Once` without an outcome is `Interrupted` | `lash-durable-test/tests/round_crash_matrix.rs`, `turn_round_crash_matrix.rs` |
| A model result that never committed launches no tool | `turn_round_crash_matrix.rs` |
| Members present in declared order whatever order their outcomes commit in | `round_crash_matrix.rs::outcomes_committed_out_of_order_present_in_declared_order` |
| A cancel during a retry backoff starts no next attempt | `round_crash_matrix.rs::a_cancel_during_a_retry_backoff_starts_no_next_attempt` |
| A stored `Once` is never upgraded and a current `Once` vetoes a stored repeat | `runtime/actor/round/fold_tests.rs` |
| A cell's `Repeatable` call reruns at its ordinal across a cut | `lash-durable-test/tests/cell_restore_laws.rs` |
| A cell's `processes.await` parks, its session releases, and it resumes with the process's outcome | `cell_restore_laws.rs` |
| A cell's race loser is admitted and settles on its own | `cell_restore_laws.rs` |
| A cell past `max_tool_calls` refuses the same call at every cut, and no refused call runs | `tool_crash_laws.rs` |
