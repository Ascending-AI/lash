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
  in declared order.
- **A code cell or a lashlang process** (`tool_dispatch::call_run::ToolRun`).
  The cell's program forms aggregates over the calls it issues. Every call
  runs to its own end beside the program, and the cell's snapshot holds
  what its heap took from them. A cell resumed from its snapshot never asks
  again for a value its heap holds.

`RuntimeExecutionContext::round_tools` gives a turn's drive its
`RoundTools`; `drive_tool_run` gives a cell or a process its in-memory
`ToolRun`.

## Seams

| Seam | Contract | Pinned in |
| --- | --- | --- |
| K1 | Admission: owner, call id, prepared request, three-capability declaration, callback binding, execution policy, before-check record, capacity | `lash_core_store::tool_run::admission`; `tool_dispatch::call_run::AdmittedToolCall::admit` |
| K1/K3/K10 hooks | Tool hook phases, occurrences, verdicts, reducer and selection | `lash_core_store::tool_run::tool_hooks` |
| K2 | Owner-qualified material references and typed retained-result refusals | `lash_core_store::tool_run::material` |
| K3 | A call's attempt outcomes and its one final-or-cancel decision | `lash_core_store::tool_run::run_event` |
| K4 | Immutable `Resolved(ref)`/`Cancelled` source seal for a Deferred call | `lash_core_store::tool_run::source_seal`, `retention` |
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
stream and is observed. `run_call` loops attempts under the sealed policy,
backing off between them on the Run's clock; a cancel during the backoff
ends the call `Cancelled` and starts no next attempt.

The declared intents of a final realize in place
(`SingletonToolHandlers::realize`, `execute_final_tool_intents`) before it is
presented. There is no separate realization invocation.

## Aggregates

`ToolRun::form` forms an aggregate over the calls the Run started, its timers
and the leaves the caller settled itself. A consumer (`race`, `any`, `all`,
`allSettled`, the list batch) answers from the order leaves settled: leaves
settled when the aggregate formed (a refused call, a plain operand) come
first, in source order; dispatched settlements follow in the order they
ended. `all` reports the first rejection; `any` the first fulfilment, or
every rejection. A call the Run's cancel or a check's AbortRun ended is host
control, raised on the caller's host channel and never an operand's
rejection. A loser keeps running; `close` cancels the Run, discharges every
unfinished call's external work and drives each to its end.

A process holds a round's calls whole until every member ended; a cell
counts every call it ever made. Past `max_tool_calls` the whole round is
refused before any member starts.

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
state commands. A successful final's commands are reduced privately and
published with its decision; a failed or cancelled candidate publishes none.
One refusal publishes nothing.

**Hook composition.** For one admitted call: argument transforms, provider
preparation, then every before-check on one immutable prepared call. Checks
reduce by AbortRun > Deny/Cancel > CachedSuccess > Allow, ties broken by
ascending UTF-8 plugin id and then callback key. A cached success is data
only and still passes the result transforms and after-checks. After-checks
return only Allow, Deny, Cancel or AbortRun and never replace a result.
A check's cancel is `CallDecision::CheckCancelled`: it rejects only its
operand. `CallDecision::Cancelled` is the Run's own cancel. AbortRun fails
the call and stops the Run: it starts no call and forms no aggregate after.

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

**Pending calls.** A body that returns Pending needs a completion key armed
before the attempt. Until tool completion keys are L5 waits
(fig-5174-pending), no key is armed and a Pending body fails typed
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
| A check's cancel is its operand's rejection; an AbortRun is Run control | `tool_dispatch/call_run/tests.rs` |
| An aborted Run admits nothing more | `tool_dispatch/call_run/tests.rs` |
| A process holds a round's capacity whole until every member ended | `tool_dispatch/call_run/tests.rs` |
