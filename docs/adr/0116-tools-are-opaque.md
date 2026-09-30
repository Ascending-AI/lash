# 0116: Tools are opaque, batch is sugar, and a spawn is a declared start

## Status

Accepted.

## Context

A tool body runs as one recorded attempt. Durable composition belongs to the
runtime and the language, so a body needs inputs, ordinary I/O and declarations
whose execution Lash owns. Giving a body a controller would let it create
journal steps inside an attempt and make cancellation and recovery depend on
unrecorded body execution.

Parallel calls need one group of independently started children. A subagent
spawn also needs a process identity that the body cannot mint, a durable
terminal wait, cancellation and protection against pruning during recovery.
The executable boundaries are `ToolCall` and `ToolProvider`
(`crates/lash-core-execution/src/tool_provider.rs:1342`, `:1401`), and the
runtime-owned resolver is `arm_pending_resolver`
(`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:130`).

## Decision

### 1. The tool model

#### 1.1 One trait, one context

Every executable tool registers as a `ToolProvider`. The trait supplies
manifests, contract resolution, optional preparation, required
`execute(ToolCall) -> ToolAttemptOutcome`, and `attempt_may_defer`.
`ToolCall::context` is an `AttemptContext`; its constructor accepts that
context, rather than runtime dispatch state
(`crates/lash-core-execution/src/tool_provider.rs:1342`, `:1351`, `:1401`).

`batch` is protocol syntax. It has no provider registration and no body.
The standard driver expands it before returning tool work
(`crates/lash-protocol-standard/src/lib.rs:679`).

#### 1.2 Result modes

A body returns one of these modes:

| Mode | Runtime action |
|---|---|
| `Done { result, intents }` | Record the final attempt, then execute its ordered declarations. Declarations from discarded attempts execute nothing. |
| `Pending` with no resolver | Park on the reserved completion key for an external actor to resolve. |
| `Pending` with `ProcessTerminal` | Arm the named process terminal at the park and on every redrive. |
| `Pending` with `DeclaredStart` | Launch the recorded start, retain its receipt, then arm the resulting process terminal. |

The outcome type keeps `ToolIntents` on `Done` only
(`crates/lash-core-execution/src/tool_intent.rs:517`). `PendingResolver` names
`ProcessTerminal` or `DeclaredStart`
(`crates/lash-core-execution/src/tool_result.rs:88`). `PendingAnnouncement`
declares a replay-keyed event that the runtime appends when the call parks;
the append does not wake the session waiting on that call (`:36`, `:60`).

#### 1.3 What `AttemptContext` provides

The body can read its execution owner, session or process lineage, scope,
frame, mandatory Lash `call_id`, attempt number and retry bound. It can read
the prepared payload, execution binding, admitted catalog, sessions and
process handles. It has cooperative cancellation, attachment output and
direct model completions whose usage belongs to the attempt. It can obtain
start context, spawn provenance, execution environment digest, intent identity,
completion key and a named fault-probe phase
(`crates/lash-core-execution/src/tool_provider.rs:162`, `:215`, `:305`,
`:365`, `:383`, `:405`, `:431`, `:442`).

`call_id()` returns `&ToolCallId`. Intent and completion identities derive
from that identity. A completion key requires host completion support
(`crates/lash-core-execution/src/tool_provider.rs:383`, `:442`, `:447`).
Session and process accessors expose reads (`:34`, `:123`). Tool output reaches
the host with the completed call; the context has no incremental output sink.

`process_execution_env_ref` derives the live capture's digest or returns an
inherited reference. The runtime publishes or acquires that environment
before journaling the declared attempt outcome (ADR 0113 §3.7).

#### 1.4 Body capability boundaries

Bodies declare process starts, signals, cancellation, trigger emission and
process events through `ToolIntent`. The runtime executes them after the
attempt commits. A pending body can declare the one start of §3.
Bodies cannot recursively dispatch tools, administer sessions or processes,
append process events directly, emit child traces, or obtain an effect
controller. The public context and the compile-fail fixtures enforce that
boundary (`crates/lash-core-execution/src/tool_provider.rs:162`,
`crates/lash/tests/ui.rs:83`).

A second executable registration class or an unsafe constructor would bypass
that boundary. The provider registration and context types make the boundary
structural, without a body determinism lint
(`crates/lash-core-execution/src/plugin/registry.rs:119`,
`crates/lash/tests/ui/plugin_spec_registers_only_providers.rs:1`).

#### 1.5 Runtime dispatch state and the facade

`ToolContext` and its builder are crate-private runtime dispatch state
(`crates/lash-core-execution/src/tool_provider.rs:490`, `:606`).
`lash::tools` exports the provider, call, attempt context, read clients,
outcomes, pending resolvers and intent types
(`crates/lash/src/lib.rs:284`). Hosts configure and administer
sessions through their facade. Tool bodies express durable work through
results and declarations.

### 2. `batch` is protocol sugar

#### 2.1 Where the expansion happens

The standard driver expands response calls into a flat list and a
`ToolExpansionPlan`. `PendingWork::Tools` and the tool-call effect carry the
plan beside the slots. Runtime preparation and execution receive ordinary
calls. The turn machine folds completed slots before transcript and stream
processing (`crates/lash-protocol-standard/src/lib.rs:679`, `:728`,
`crates/lash-sansio/src/sansio/turn_protocol.rs:57`, `:487`,
`crates/lash-sansio/src/sansio/turn_machine.rs:832`).

Expansion and folding are pure functions of the recorded calls and admitted
configuration (`crates/lash-protocol-standard/src/batch.rs:95`, `:217`).
They need no wrapper invocation or journal entry. The model and transcript
receive one result per wrapper.

#### 2.2 Slot order and member identity

Native calls occupy one slot each. A wrapper's non-nested members occupy
consecutive slots at its position, in member order. A nested refusal occupies
no slot. A member has `wrapper.call_id.child(original_member_index)`, counted
before refusals, and no provider call id. The wrapper preserves its own Lash
id, optional provider id, arguments and replay metadata
(`crates/lash-protocol-standard/src/batch.rs:95`, `:117`, `:134`, `:142`).

The group and child identities use the admitted call identities. Reopening a
child also checks its retained request; a changed admitted request is drift
(`crates/lash-core-execution/src/session/tool_execution/group.rs:212`, `:563`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:980`).
Provider-supplied duplicate ids cannot name another member's attempt.

#### 2.3 Member admission

Each executable member passes ordinary tool preparation, schema validation
and the host's admission hooks. Members resolve against the callable catalog.
The wrapper performs no admission of its own and grants nothing to a member
(`crates/lash-core/src/runtime/turn_driver/tools.rs:33`, `:108`,
`crates/lash-core-execution/src/tool_dispatch/preparation.rs:81`, `:179`). Lash supplies
the hooks; hosts own authorization policy.

A member's finish, failure or frame-switch control contributes its projected
value to its row. The wrapper does not promote it to protocol control
(`crates/lash-protocol-standard/src/batch.rs:292`, `:316`).

#### 2.4 The folded presentation

The result is `{ "results": [{ "index": 0, "tool": "read",
"success": true, "result": ... }] }`, with `error` on failed rows.
Rows remain in member order and include nested refusals. A valid wrapper
returns success even when members fail. Folding retains member presentation,
attachments, attachment notices and intent outcomes, while restoring the
wrapper's identity and replay metadata
(`crates/lash-protocol-standard/src/batch.rs:257`, `:292`, `:308`).

A step with no executable calls opens no group. It still folds the refused
rows. Folding executes no tool or intent
(`crates/lash-core/src/runtime/turn_driver/tools.rs:33`, `:108`,
`crates/lash-protocol-standard/src/batch.rs:217`).

#### 2.5 Limits and nested refusal

A wrapper accepts 1 to its configured maximum members. An empty, oversized
or unparseable member list refuses the entire wrapper before any member
starts. A member named `batch` becomes one failed row; expansion never
recurses (`crates/lash-protocol-standard/src/batch.rs:104`, `:119`, `:167`).
The flat group also obeys its retained-child bound. It is admitted as one
group, without waves
(`crates/lash-core-execution/src/session/opener_groups.rs:293`).

#### 2.6 Cancellation

Members use the group's cancellation, settlement and intent-drain rules in
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md).
The group observes its recorded consumed prefix, completes protected drains,
and presents cancellation for calls outside that boundary. Folding preserves
the returned member order and does not execute a second drain
(`crates/lash-core-execution/src/session/tool_execution/group.rs:212`, `:563`,
`crates/lash-protocol-standard/src/batch.rs:257`).
Infrastructure faults remain runtime failures.

#### 2.7 Configuration

`BatchSugar::Enabled { max_members }` is the default, with maximum 64.
`BatchSugar::Disabled` offers no wrapper syntax. The hard ceiling is 64,
and plugin construction refuses a higher setting as `InvalidBatchMaximum`.
The request definition and prompt render the configured maximum
(`crates/lash-protocol-standard/src/lib.rs:60`, `:71`, `:81`, `:89`, `:144`,
`:267`, `crates/lash-protocol-standard/src/batch.rs:25`).

`batch` has no catalog entry. Tool membership and discovery operate on the
member tools. RLM composition uses language aggregates rather than calling a
wrapper provider (`crates/lash-protocol-standard/src/lib.rs:209`,
`crates/lash-protocol-rlm/src/tool_catalog.rs:11`).

#### 2.8 Parallelism is structural

Batch members and native siblings execute as children of one group.
Language aggregates use the runtime's group path as well. The host starts
independent children before awaiting their results; body execution cannot
create a nested dispatch group
(`crates/lash-core-execution/src/session/tool_execution/group.rs:212`, `:563`,
`crates/lash-conformance/src/conformance/tool_batch_parallelism.rs:1`).
Barrier laws observe actual body starts, and a forced-serial host must fail
the same law (§7.1).

### 3. `DeclaredStart`

#### 3.1 Shape

`DeclaredStart` privately contains one boxed `StartProcessIntent`, its boxed
index-0 `ToolIntentIdentity`, and an optional deadline. Its constructor
requires the start's owner to equal the attempt's runtime owner and requires
a completion key. The refusals are `ForeignOwner` and
`CompletionUnavailable`
(`crates/lash-core-execution/src/tool_result.rs:135`, `:160`, `:246`).
The owner can be a session or a process; the declaration does not require a
session-owned attempt.

Decoded declarations are checked again against the identity derived from
the admitted call. Owner mismatch is `OwnerMismatch`; any identity mismatch
is `DeclaredStartIdentityMismatch`. A mismatch becomes the call's typed
launch receipt, with no registration
(`crates/lash-core-execution/src/tool_result.rs:199`,
`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:141`, `:242`).

#### 3.2 The launch rule

A provider that can defer reserves the completion key before body execution.
The runtime records the selected pending attempt, binds its declaration to
that call and realizes the start under its recorded intent identity. The
start's own journaled admission is protected by the call's cancellation
fence. Cancellation before admission prevents launch; an admitted start
remains recoverable after cancellation
(`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:148`,
`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:21`, `:130`,
`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:132`).

Registration mints the process id. A retained start key returns its process.
The launch receipt records the realized handle or typed refusal. The runtime
then arms the terminal, and repeats that arming on redrive. A refusal settles
the call; infrastructure faults and replay refusals propagate for recovery
(`crates/lash-sqlite-store/src/process_registry/registration.rs:16`,
`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:141`, `:176`,
`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:193`).

The launch does not wait for the child terminal. A group of spawns can launch
all children before any result resolves. The pending child's final settlement
occurs after resolution
(`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1475`,
`:1493`, `:1528`).

#### 3.3 The launch obligation

The start uses its ordinary `process:start:{start key}` journaled admission
under the call's lineage. Engine redelivery recovers an unfinished call and
reuses the recorded start and receipt. Recovery needs no separate store-side
launch obligation kind
(`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:21`,
`:201`, `crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:146`).

#### 3.4 The cancel obligation consumes `CancelHint`

For `ProcessTerminal` and `DeclaredStart`, a cancelled or timed-out wait with
`CancelExternalWork` issues a replay-keyed process cancellation.
`Ignore` drops only the wait. A terminal that wins the resolution race issues
no cancel. If the invocation cannot journal cancellation, the consumer hold
keeps the obligation for the opener to discharge
(`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:334`,
`:351`, `:449`). The cancel command derives from the Lash call identity.

#### 3.5 A timeout cancels the child

A pending deadline resolves as the configured `TimeoutBehavior`.
`ErrorAsResult` returns a timeout failure; `FailTurn` fails the turn.
For runtime-owned resolvers with `CancelExternalWork`, the finish path
cancels the child before releasing its hold
(`crates/lash-core-execution/src/tool_result.rs:1`,
`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:351`, `:405`).

#### 3.6 Retention pinning

A declared start launched under a starter scope registers a consumer hold,
keyed by the completion key and owned by that scope. Pruning excludes held
rows. When the wait resolves, the runtime discharges cancellation and then
releases the hold before settling the call. A redrive can replay the
journaled terminal even if the row is pruned after release
(`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:207`,
`:263`, `:370`, `:405`,
`crates/lash-sqlite-store/src/process_registry/sql.rs:256`).
The opener handles abandoned holds; release failure leaves the hold available
for that recovery (`pending_resolver.rs:370`, `:449`).

#### 3.7 Session-turn result projection

`ProcessInput::SessionTurn` carries `result: SessionTurnOutcome`.
`Turn` returns process id, child session id and the assembled turn.
`FinalValue { schema }` returns the final value, terminal tool value or
trimmed assistant text, validated against the optional schema. A frame switch
or stopped child has a typed failure. Process cancellation and child failure
retain their typed outputs
(`crates/lash-core-execution/src/runtime/process/model.rs:99`, `:138`,
`crates/lash-core/src/runtime/session_manager/process_runners/session.rs:343`,
`:394`, `:430`). The trigger definition fingerprint includes the result
projection (`crates/lash-core-execution/src/triggers/router.rs:226`).

#### 3.8 What the host sees

The declared launch is intent outcome 0, carrying the realized child handle
or refusal. It accompanies the completed tool call. Intent realization fires
the child-process trace hook when supplied. The model receives the child's
projected value, failure or cancellation
(`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:71`,
`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:150`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1557`).

### 4. `spawn_agent`

`lash-subagents` registers an ordinary static provider for `spawn_agent` and,
in child sessions, `submit_error`. Preparation checks the capability and
output schema, captures the parent's policy and any `ParentFork` plugin
initialization, clears the requested session id and records the child input
(`crates/lash-subagents/src/rlm.rs:41`, `:55`, `:95`, `:117`).

A process-owned spawn parents the child under the session that originated
the process chain and records the process as its cause. A host-originated
chain has no parent session and refuses. A process cannot use `ParentFork`,
since it has no conversation to fork (`rlm.rs:98`, `:212`).

Execution declares one `SessionTurn` process with definition key
`lash-subagent-session-turn`, `SessionTurnOutcome::FinalValue`, the host's
lifetime policy, and inherited originator. It returns pending on that
`DeclaredStart`; `attempt_may_defer` is true. An optional host timeout becomes
the pending deadline (`crates/lash-subagents/src/rlm.rs:137`, `:170`, `:186`,
`:191`, `:270`). A timeout returns a failure and cancels the child.

The start identity retains one process, child session and turn across
redrives. Depth and capability checks remain in `lash-subagents`; handles and
durable composition use `processes.start` and `processes.await`
(`crates/lash-subagents/src/capability.rs:116`,
`crates/lash-plugin-process-controls/src/lib.rs:1`).

### 7. Executable laws

The store matrix is SQLite file, SQLite memory and PostgreSQL. Effect hosts
are the in-process Restate server double, live Restate and lash-sim's
in-process effect host. Each registration supplies its supported combination;
two modes of the same double are one host family.

#### 7.1 Barrier laws

`tool_batch_parallelism.rs` owns body-start barriers, reverse-dependency
checks and the forced-serial negative control. Producers include batch
wrappers with native siblings, native calls, RLM `Promise.all` and
`Promise.allSettled`, and process aggregates
(`crates/lash-conformance/src/conformance/tool_batch_parallelism.rs`).
The double registers them in
`crates/lash-restate/src/tests/tool_batch_parallelism_on_the_double.rs`;
`crates/lash-protocol-rlm/tests/tool_batch_parallelism.rs` and
`crates/lash-restate/src/tests/conformance_and_poison.rs` supply their
registrations. A started observer event alone does not prove overlap.

#### 7.2 Batch sugar laws

Expansion and fold unit tests live beside their pure functions in
`crates/lash-protocol-standard/src/batch.rs`. Shared laws in
`crates/lash-conformance/src/conformance/batch_sugar.rs` cover admission,
identity, replay, redrive, cancellation, all-refused groups and transcript
folding. The standard plugin tests the configuration ceiling.

#### 7.3 Declared-start and spawn laws

`crates/lash-conformance/src/conformance/declared_start.rs` covers launch
crashes, discarded retries, refusal, early terminal, rearming, cancellation,
timeout, scope close, retention, decoded identity refusal, child metadata
and overlapping spawns. `crates/lash-subagents/tests/declared_start.rs`
registers normal and always-replay modes of the same Restate double.
The SessionTurn runner owns final-value projection checks.

#### 7.4 Compile-fail fixtures

`crates/lash/tests/ui.rs:83` registers fixtures for the attempt's missing
journal, recursive dispatch, session mutations, trigger commands, process
events and child traces. It also registers the unnameable runtime context,
missing process administration and controller, provider-only registration,
sealed declared start, and pending outcome without ordinary intents.
These fixtures test the public boundary.

### 9. Durable composition and host integration

Process engines own executable process bodies. Language aggregates compose
recorded calls and process operations. Hosts never drive a durable process;
a custom host subagent tool is an ordinary provider returning a
`DeclaredStart`. Process ancestry and lifetimes follow
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md)
(`crates/lash-core-execution/src/runtime/process/model.rs:99`, `:138`,
`crates/lash-plugin-process-controls/src/lib.rs:1`).

## Consequences

The tool API gives bodies one attempt context and explicit result modes.
The runtime owns launch, terminal waits, cancellation, retention and recovery.
A flat group supplies parallel execution without nested body dispatch.
`batch` costs no executable wrapper invocation, while its model presentation
remains one call and result. A host-defined subagent uses the same declared
start path as `spawn_agent`.
