# 0116: Tools are opaque, batch is sugar, and a spawn is a declared start

## Status

Accepted. The [arc-baseline text](https://github.com/Ascending-AI/lash/blob/48f11c5fa761cabb4991e8497218bb870db83169/docs/adr/0116-tools-are-opaque.md)
records the child-specific dispatch and the reroute of slow bodies to
processes this decision rejects; it is not an operating contract.

## Context

A tool body runs as one recorded attempt. Durable composition belongs to the
runtime and the language, so a body needs inputs, ordinary I/O and declarations
whose execution Lash owns. Giving a body a controller would let it create
durable records inside an attempt and make cancellation and recovery depend on
unrecorded body execution. Parallel calls need independently started attempts
in one logical Run ([ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)).
A subagent spawn also needs a process identity the body cannot mint, a durable
terminal, cancellation and protection against pruning during recovery.

## Decision

### 1. The tool model

#### 1.1 One trait, one context

Every executable tool registers as a `ToolProvider`, supplying manifests,
contract resolution, optional preparation and `execute(ToolCall)`.
`ToolCall::context` is a sealed `AttemptContext`, not a controller. The body
can use ordinary I/O, read its admitted owner and environment, observe
cooperative cancellation, produce attachment output and make direct model
completions captured with the attempt. It cannot recursively dispatch tools,
administer sessions or processes, append process events, or create nested
durable records. Durable composition belongs in a process.

#### 1.2 Result modes

A body returns `Done` with output and ordered `ToolIntents`, or `Pending`.
The Run records Pending as a Deferred X: no rank, final decision, presentation
or `ToolCompletion` occurs until its source supplies a terminal. A Pending
result carries no general intents. It may name `PendingResolver::ProcessTerminal`
or one sealed `DeclaredStart`; a plain Pending call reserves a key for an
external completion. An idempotency-keyed `PendingAnnouncement` is progress metadata
and does not wake the session blocked on that call.

#### 1.3 Declaration and admission

The author-facing `ToolDeclaration` contains exactly `may_defer`, `intents` and
`isolated`. Whole-round K1 admission records it with the prepared input,
read-only plugin snapshot, callback revisions, runtime retry/cancel policy and
capacity. An unavailable admitted revision or changed request refuses typed
before a body, route or identity. No live declaration hook overrides admission.

#### 1.4 State commands and hooks

Bodies receive read-only plugin state and return bounded declared state
commands. The Run reduces successful commands privately, records resolutions
with the eligible decision, and publishes after durable acceptance. Failed or
cancelled candidates publish no success commands. Resume runs no completed
body, hook or reducer. The permitted hook writer phases and transform/check
composition are defined by [ADR 0128](0128-tool-hooks-compose-as-transforms-then-checks.md).

#### 1.5 Delivery follows the execution policy

Every attempt is an admitted execution with a started row and an outcome
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §5). Its
`ExecutionPolicy` is recorded at admission (ADR 0132 §7). A `Once` attempt
started without an outcome records `Interrupted` and never runs again. A
`Repeatable` attempt runs again at the same ordinal with the same
`ToolCallId`. A reported failure advances the ordinal only if the recorded
retry policy admits another attempt. External side effects deduplicate on
`AttemptContext::call_id()`. `ToolRetryPolicy::Never` prevents a reported
retry. Lash-owned effects use declared intents and their exactly-once fences.

#### 1.6 Duration and isolation

An inline attempt carries an `ExecutionLimit` recorded before its body
starts; a Pending call's wait carries a `WaitDeadline` (ADR 0132 §7). An
inline execution declared above `tool_ceiling` is refused at registration with
`RegistrationRefused::InlineBudgetExceedsCeiling`. Long work is a process
tool, an isolated tool on a host engine, or a Pending tool, each with a
bounded inline prefix and a wait deadline. A body also owns its transport
timeout. A slow body is never stopped and rerun as a process. An explicit
isolated declaration binds and admits a supported process implementation
before any ordinary body runs. Independent lifetime alone does not prove hard
physical isolation; the registered engine's termination contract supplies
that guarantee.

#### 1.7 The intent admission budget

Intent admission allows at most 32 declarations, 16 of one kind, and 64 KiB
of canonical intent JSON per completed attempt. The byte bound measures the
complete declaration, including its captured environment's digest. The
capture lives once in the content-addressed process environment store, and
durable referrers keep it available for realization
([ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md)). Tool
output values have no universal core byte cap; hosts apply tool and provider
output policy.

### 2. `batch` is protocol sugar

#### 2.1 Where the expansion happens

The standard driver expands response calls into ordinary member calls and a
`ToolExpansionPlan` before runtime admission. `PendingWork::WaitingForToolResults`
carries the plan beside the slots, and the turn machine folds completed slots
before transcript and stream processing. Expansion and folding are pure
functions of the recorded calls and admitted configuration
(`crates/lash-protocol-standard/src/batch.rs`). They need no wrapper execution
or recorded entry. The model and transcript receive one result per wrapper.

#### 2.2 Slot order and member identity

Native siblings occupy their own slots; a wrapper's non-nested members occupy
consecutive slots at its position, in written order. A nested refusal occupies
no executable slot. A member derives its identity from
`wrapper.call_id.child(original_member_index)`, counted before refusals, and
carries no provider call id. The wrapper preserves its own identity, provider
correlation and idempotency metadata. Provider-supplied duplicate ids cannot name
another member's attempt.

#### 2.3 Member admission

Every executable member passes preparation, schema validation and admission
checks against the callable catalog. The wrapper grants no authority. The Run
admits the entire round and registers independent attempts before awaiting any
result. One invalid admitted member starts none. A member's finish, failure or
frame switch contributes data to its row rather than promoting its control to
the wrapper.

#### 2.4 The folded presentation

Folding returns `{ "results": [{ "index": 0, "tool": "read", "success": true,
"result": ... }] }`, with `error` on failed rows. Rows preserve written order,
nested refusals, presentation, attachments, notices and intent outcomes. A valid
wrapper succeeds even if members fail. A step with only refused wrapper rows
executes no body, but still folds those rows.

#### 2.5 Limits and nested refusal

A wrapper accepts 1 to its configured maximum members. Empty, oversized or
unparseable lists refuse the wrapper before any member starts. A member named
`batch` becomes one failed row; there is no recursive expansion. Run capacity,
including the session's `max_tool_calls`, is reserved for the entire admitted
round, without dispatch waves (ADR 0099 §9).

#### 2.6 Cancellation

Cancellation uses the Run's immutable final-or-cancel decisions. A final
accepted before cancellation drains declarations and presents its recorded
result; a cancellation that wins declares nothing. A row never contradicts its
member's durable final, however a wait and a cancel raced. Folding never
substitutes a cancelled wait for a protected final or performs a second drain.
Infrastructure faults remain runtime failures.

#### 2.7 Configuration

`BatchSugar::Enabled { max_members }` defaults to 64. `BatchSugar::Disabled`
offers no syntax. The hard ceiling is 64; plugin construction rejects a larger
setting as `InvalidBatchMaximum`. The request definition and prompt render the
configured maximum. `batch` has no catalog entry; RLM composition uses language
aggregates rather than a wrapper provider.

#### 2.8 Parallelism is structural

Batch members, native siblings and language aggregates are independent
attempts of one Run, with distinct consumer rules in
[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md).
The Run starts them before awaiting any result; a body cannot dispatch nested
tools. Barrier laws observe actual body starts, and a forced-serial host must
fail the same law (§7.1).

### 3. `DeclaredStart`

#### 3.1 Shape

`DeclaredStart` privately seals one `StartProcessIntent` and its index-0
`ToolIntentIdentity`. Construction requires the start's owner to equal the
attempt's runtime owner and requires a completion key; it refuses
`ForeignOwner` or `CompletionUnavailable`. The owner can be a session or a
process. Decoded starts are checked against the admitted identity again before
registration: an owner mismatch is `OwnerMismatch`, any other identity mismatch
is `DeclaredStartIdentityMismatch`, and either becomes the call's typed launch
receipt with no registration.

#### 3.2 The launch rule

A provider that can defer has its completion key reserved before body
execution. The Run records the start obligation under a stable `StartKey`, bound
process implementation, captured environment and consumer hold. The start's own
recorded admission is protected by the call's cancel fence: cancellation before
admission prevents launch, and an admitted start stays recoverable after
cancellation. Registration mints the process id; a retained start key returns
its process. A discarded retry does not launch its declaration. The launch does
not wait for the child terminal, so a round of spawns launches every child
before any result resolves.

#### 3.3 The launch obligation

Registration inserts the process's runnable actor in the start's transaction,
so nothing is owed after it commits; a repeated start registers the same key,
returns the same process and reuses the recorded receipt. There is no second
store-side tool launch log.

#### 3.4 The cancel obligation consumes `CancelHint`

`CancelExternalWork` records cancellation of the owned process before releasing
its consumer hold. `Ignore` releases only observer interest. A terminal that
wins the decision issues no cancel. The cancel command derives from the Lash
call identity. Session-lifetime subagents keep their independent lifetime when
the observing turn cancels; `processes.await` uses `Ignore`. Process scope-end
lifetime rules still apply.

#### 3.5 The process terminal source

A process-backed Deferred start arms a `process_terminal` wait row as its
source after the process id is minted (ADR 0132 §6 and §11). Short registrations retain the producer terminal and
receiver dependency. Delivery acquires attachment ownership and canonical
result material before sealing the source. The Run accepts the immutable winner
before finalization, cancellation discharge and presentation. A source seal is
`Resolved(ref)` or `Cancelled`; a descriptor never wins `race` or `any`. A new owner
of the Run waits on the pending source without dispatching the tool again
(ADR 0099 §12).

#### 3.6 Retention pinning

A declared start registers a consumer hold keyed by its completion key and owned
by the starting scope. Pruning excludes held rows. When the source settles, the
Run discharges cancellation and then releases the hold before settling the
call; release failure leaves the hold for the opener's recovery. Missing,
retired or corrupt retained material gives a typed refusal and cannot relaunch
the body or process.

#### 3.7 Session-turn result projection

`ProcessInput::SessionTurn` carries `result: SessionTurnOutcome`. `Turn` returns
process id, child session id and the assembled turn. `FinalValue { schema }`
returns the final value, terminal tool value or trimmed assistant text,
validated against the optional schema. A frame switch or stopped child has a
typed failure; process cancellation and child failure retain their typed
outputs. The trigger definition fingerprint includes the result projection.

#### 3.8 What the host sees

The declared launch is intent outcome 0, carrying the realized child handle or
refusal, beside the completed tool call. Intent realization fires the
child-process trace hook when supplied. The model receives the child's
projected value, failure or cancellation.

### 4. `spawn_agent`

`lash-subagents` registers ordinary providers for `spawn_agent` and, in child
sessions, `submit_error`. Preparation checks depth, capability and output schema,
captures parent policy and any `ParentFork` initialization, and records the child
input. Execution declares one `SessionTurn` process with definition key
`lash-subagent-session-turn`, stable start identity and final-value projection,
and returns Pending on its terminal source.

A process-owned spawn uses the session that originated its process chain as
parent and records the process as cause. A host-originated chain cannot fabricate
a parent session; `ParentFork` also refuses without a parent conversation.
Handles and durable composition use `processes.start` and `processes.await`.

### 5. Host operations

A host task that uses tools is an operation Run. The host starts it through
`plugin_operations().start_task`, follows its Run handle, and explicitly
cancels or reads its result. The session actor owns the operation Run and
all local handles through Closing and Settled. Hosts never drive
a turn, and tool-free administration retains its command scope.

### 7. Executable laws

The store matrix is SQLite file, SQLite memory and PostgreSQL. Laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). L07/L08/L12/L16/L17/L19/L22 in the
[tool-run contract](../architecture/tool-run-contract.md) name the rules.

#### 7.1 Barrier laws

`crates/lash-conformance/src/conformance/tool_batch_parallelism.rs` owns
body-start barriers, reverse-dependency checks and the forced-serial negative
control, for batch wrappers with native siblings, native calls, RLM
`Promise.all` and `Promise.allSettled`, and process aggregates.
`crates/lash-protocol-rlm/tests/tool_batch_parallelism.rs` registers them. A
started observer event alone does not prove overlap.
`crates/lash/src/tests/aggregate_oracle.rs` proves independent receipts,
retries, protected drain, losing-call progress and recorded timer/aggregate
selection.

#### 7.2 Batch sugar laws

Expansion and fold unit tests live beside their pure functions in
`crates/lash-protocol-standard/src/batch.rs`. Shared laws in
`crates/lash-conformance/src/conformance/batch_sugar.rs` cover admission,
identity, resume, redrive, cancellation, all-refused rounds and transcript
folding. The standard plugin tests the configuration ceiling.

#### 7.3 Declared-start and spawn laws

`crates/lash-conformance/src/conformance/declared_start.rs` covers launch
crashes, discarded retries, refusal, early terminal, cancellation, scope close,
retention, decoded identity refusal, child metadata and overlapping spawns.
`crates/lash-subagents/tests/declared_start.rs` registers them on the
production runtime, including stable launch and cancel and immutable
completion and authority.

#### 7.4 Compile-fail fixtures

Facade compile-fail fixtures in `crates/lash/tests/ui/` pin the attempt's missing
controller, recursive dispatch, session mutations, trigger commands, process events,
the unnameable runtime context, provider-only registration, the sealed declared
start and a pending outcome without ordinary intents.

### 9. Durable composition and host integration

Process engines own executable process bodies. Language aggregates compose
recorded calls and process operations. Hosts never execute a durable process;
a custom host subagent tool is an ordinary provider returning a
`DeclaredStart`. Process ancestry and lifetimes follow
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md).
`crates/lash-core-execution/src/tool_provider.rs`, `tool_intent.rs` and
`tool_result.rs` define the provider, result and declared-start boundaries;
`crates/lash-core-execution/src/tool_dispatch/production/` routes prepared calls
through `run_coordinator/`.

## Consequences

The tool API gives bodies one attempt context and explicit result modes. The
Run owns admission, launch, terminal sources, cancellation, retention and
recovery. Independent attempts supply parallel execution without nested body
dispatch. `batch` costs no executable wrapper execution, while its model
presentation remains one call and result. A host-defined subagent uses the same
declared start path as `spawn_agent`.
