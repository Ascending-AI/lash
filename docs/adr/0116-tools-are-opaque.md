# 0116: Tools are opaque, batch is sugar, and a spawn is a declared start

## Status

Accepted 2026-09-29 (FIG-3562). It pins the end state and the four lanes
that build it (§10). The lanes land directly on `main`, and each keeps the
workspace compiling. There is no integration branch. It is consistent with
[ADR 0112](0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md),
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md) and
[ADR 0114](0114-a-stopped-turns-partial-output-is-sealed-durably-and-returned-to-the-host.md),
whose cutovers are in flight on `fig-1628/cutover`, `fig-4031/referrers` and
`fig-433/stopped-partials`. §10 carves this arc's files out of their lane
globs. Nothing below describes current behaviour unless it cites today's
code. Every citation was checked at `5436e99e02`.

The rulings on FIG-3562 (Sam, 2026-09-29) are binding:

1. **Tools are opaque.** A tool body takes arguments and returns a result.
   It may declare effects (intents, applied after the attempt commits) and
   may return Pending (the result arrives later). It never drives lash from
   inside its body.
2. **`batch` is protocol sugar, on by default.** The driver expands it into
   the top-level tool group alongside the response's native calls, and folds
   the results into one batch result. A batch inside a batch is refused per
   member. The per-batch maximum is configurable, defaults to 64, and 64 is
   the hard ceiling. Every batch member, native parallel call and
   `Promise.all` member starts before any finishes.
3. **`spawn_agent` stays in `lash-subagents` as an ordinary tool.** It returns
   Pending carrying exactly one declared `ToolIntent::StartProcess`
   (`ProcessInput::SessionTurn`), resolved by a new
   `PendingResolver::DeclaredStart`, under the launch rule of the spawn
   recheck. It returns the child's final value only. The child's identity is
   host-visible metadata. A timeout cancels the child. Handles stay RLM's
   `processes.start`/`processes.await`.
4. **The version freeze (FIG-3846).** Shapes change in place, with no version
   bumps, adapters or legacy replay. **D5:** hosts never drive.
5. **Whole-hog.** The arc deletes every trace of the replaced systems: the
   orchestrator machinery, the old batch handling, nested coordination, the
   one-at-a-time fallback and their vocabulary. Nothing stays behind as dead
   code, a compatibility shim, a deprecated alias, a stale test, fixture,
   lint, CI step, example, doc, comment or ticket-era note. §6's grep is the
   machine check.

The design inputs are in `/workspace/notes/lash/tasks/lanes/`:
`study-batch-sugar.report.md` (the expansion design; its cap of 25 is
superseded by 64), `wholehog-opaque-tools-astra.report.md` and
`wholehog-opaque-tools-fable.report.md` (the capability table, deletion
inventory and end-state model), and `study-spawn-recheck-astra.report.md`
(the launch rule). The reference evidence is `ref-batch-*.md` and
`ref-spawn-*.md`. None of the seven referenced agent runtimes has a nested
orchestrating batch tool; opencode tried one and deleted it. Every runtime
that ships a subagent treats one call as one child, keyed by the call, with
cancellation cascading. Where the inputs conflict, the rulings win, then the
spawn recheck. Astra's earlier proposal to delete `lash-subagents` is
withdrawn by its own recheck and overruled by ruling 3.

## Context

### Three execution classes, two of which drive lash

Today a tool reaches the runtime through one of three classes
([ADR 0051](0051-the-facade-is-the-host-api-core-is-integrator-seams.md) §3):

- **Leaf.** `ToolProvider::execute(ToolCall) -> ToolAttemptOutcome`
  (`crates/lash-core-execution/src/tool_provider.rs:1510-1541`). The body sees
  the sealed, controller-free `AttemptContext` (`:127-370`) and runs as one
  atomic recorded attempt ([ADR 0042](0042-tool-attempts-are-atomic.md)).
- **Orchestrating.** `OrchestratingToolDef` and `OrchestrationContext`
  (`crates/lash-core-execution/src/tool_provider/orchestration.rs`). The body
  is authored process-replay code with no `ToolAttempt` frame. It dispatches
  nested tool batches (`call_tool_batch`, `:140`), starts, awaits, cancels
  and signals processes, derives start keys (`start_key`, keyed by the
  `OrchestrationCall` namespace at
  `crates/lash-core-store/src/process_identity.rs:193-202`), and emits child
  traces. A doc-hidden `unsafe` constructor seals the class (`:612-626`). A
  lexical lint (`scripts/lint_orchestrating_tools.py`, wired at
  `.github/workflows/ci.yml:390-391,494`, `.pre-commit-config.yaml:78-81` and
  `scripts/push-gate.sh:380`) polices body determinism. Two bodies use it:
  standard `batch` (`crates/lash-protocol-standard/src/lib.rs:124-125,214-340`)
  and `spawn_agent` (`crates/lash-subagents/src/rlm.rs:39-97,177-226`).
- **Internal process tool.** `InternalProcessToolDef` and
  `InternalProcessContext` (`crates/lash-core-execution/src/tool_provider/process.rs`),
  executed directly by the `ProcessInput::ToolCall` runner
  (`crates/lash-core/src/runtime/session_manager/process_runners/tool.rs:110-121`).
  It has the same powers and no production registrant.

Beside them, `ToolContext` (`tool_provider.rs:402-442`) still exports
body-shaped capability clients: `ToolDispatchClient`
(`tool_provider/dispatch.rs`), `ToolSessionAdmin`
(`tool_provider/session.rs:66-107`), `ToolTriggerClient`
(`tool_provider/triggers.rs`) and `ToolProcessEventClient`
(`tool_provider/process_events.rs:89-145`), all re-exported through
`lash::tools` (`crates/lash/src/lib.rs:270-298`).

### The relay deadlock

FIG-3562 was filed because the tool-batch parallelism law deadlocks on live
Restate with 5 of 8 leaves started. The relay producer is an orchestrating
body that calls `call_tool_batch`. Its nested batch runs on the relay child's
own invocation journal, which Restate replays by position, so the controller
falls back to `drive_independent_effect_work`
(`crates/lash-core-execution/src/runtime/effect/executor/control.rs:407-421`).
That default awaits each unit in order. Restate never overrides it; its
controller only forwards (`crates/lash-restate/src/controller/scope_recording.rs:267-272`).
A nested batch on Restate is therefore serial, and the law's registration on
the live harness was parked (`samuel-fig-3397-a-restate-parallelism`). The
double and the RLM registrations carry `reaches_relay = false` to dodge it
(`crates/lash-restate/src/tests/tool_batch_parallelism_on_the_double.rs:24`,
`crates/lash-protocol-rlm/tests/tool_batch_parallelism.rs:168,179`).

A flat group has no such fallback. Direct batches already overlap as effect
groups of child invocations on every tier ([ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)).
Parallelism at every level follows from removing the levels: no body
dispatches, so every tool call in a turn step is a child of one top-level
group.

### Why spawn needs one new shape

`spawn_agent` cannot simply become a leaf. A Pending attempt cannot carry
intents: the type forbids it (`crates/lash-core-execution/src/tool_intent.rs:494-504`),
and a UI fixture pins it (`crates/lash/tests/ui/pending_attempt_cannot_carry_intents.rs`).
A process id is minted only at registration
(`crates/lash-sqlite-store/src/process_registry/registration.rs:23-55`), so a
body cannot know the child's id to name `PendingResolver::ProcessTerminal`
(`crates/lash-core-execution/src/tool_result.rs:94`). "Declare a start and
park on its terminal" is therefore inexpressible. The restriction also
protects real machinery. A Pending launch leaves the coordinator before
intent settlement (`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:398-412`),
while a completed attempt commits, takes drain admission and then realizes
(`:640-672`). A pending group child takes its final rank only when it
resolves (`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1392-1416`).
Simply allowing intents on Pending would either strand the start behind its
own result, or seal the call against cancellation too early. §3 adds one
shape, with its own durable launch phase.

## Decision

### 1. The end-state tool model

#### 1.1 One trait, one context

There is one executable tool trait, `ToolProvider`, with the signature it has
today (`tool_provider.rs:1510-1541`): manifests, lazily resolved contracts,
an optional `prepare_tool_call`, one required
`execute(ToolCall<'_>) -> ToolAttemptOutcome`, and `attempt_may_defer`.
`ToolCall::context` is `&AttemptContext` (`:1467-1471`), and it is the only
context any tool body sees. Every executable tool registers as a
`ToolProvider`. There is no second registration kind, no provenance seal and
no body lint. Ordinary, intent-declaring and deferred tools are result modes
of the one trait, not kinds.

`batch` is not a tool. It is protocol sugar (§2): catalog-free, never
executed and never registered as a provider.

#### 1.2 Result modes

```rust
// crates/lash-core-execution/src/tool_intent.rs (unchanged shape)
pub enum ToolAttemptOutcome {
    /// Terminal output plus ordered declarations, drained after the final
    /// attempt commits (ADR 0042).
    Done { result: ToolOutcomeDone, intents: ToolIntents },
    /// The result arrives later. Carries no `ToolIntents`, by type.
    Pending(PendingCompletion),
}

// crates/lash-core-execution/src/tool_result.rs
pub enum PendingResolver {
    /// A known process's terminal resolves the wait (today's shape).
    ProcessTerminal { process_id: ProcessId },
    /// The terminal of the one process this call declares resolves the
    /// wait. The runtime launches it under §3's rule.
    DeclaredStart(DeclaredStart),
}
```

A body answers in exactly one of three modes:

| Mode | Value | Who acts, and when |
|---|---|---|
| **Done + intents** | `ToolAttemptOutcome::Done { result, intents }` | Lash records the final attempt, then admits and drains the intents in source order (ADR 0042; ADR 0099 §5 for the cross-child order). Retries discard non-final declarations. |
| **Pending** | `Pending(PendingCompletion)` with `resolved_by` `None` or `ProcessTerminal` | An out-of-band actor holds the completion key, or the runtime arms the named terminal on the park and on every redrive (`crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs:24-37`). |
| **Pending + declared start** | `Pending(PendingCompletion)` with `resolved_by: Some(DeclaredStart(..))` | The runtime seals the declaration with a launch obligation, launches the one start, arms its terminal, and consumes `CancelHint` if the call is cancelled or times out (§3). |

`PendingAnnouncement` (`tool_result.rs:21-70`) stays: it is a runtime-executed
append declared on the park, not a body-side door.

#### 1.3 What stays on `AttemptContext`

Everything the body needs as input, and the I/O whose result the body itself
consumes:

- identity reads: `session_id`, `execution_scope_id`, `agent_frame_id`,
  `tool_call_id`, `attempt_number`, `max_attempts`, `replay_key`,
  `logical_root`, `enclosing_process`;

  Amended 2026-09-29 (FIG-4073, [ADR 0117](0117-lash-names-every-tool-call.md)):
  the call's identity read is `call_id() -> &ToolCallId`, mandatory, beside
  `attempt_number` and `max_attempts`. The optional `tool_call_id` and
  `replay_key` reads are deleted, and there is no provider-id read.
  `intent_identity` and `completion_key` below derive from the `ToolCallId`,
  and `completion_key` errs only when the host lacks the capability.
- the sealed payload: `prepared_payload`, `decode_prepared_payload`,
  `tool_execution_binding`;
- controller-free reads: `sessions()` (`AttemptSessionReads`,
  `tool_provider.rs:39-85`), `processes()` (`AttemptProcessReads`,
  `:88-107`) and `tool_catalog()`, the catalog the attempt was dispatched
  with, which `processes.create` links its source against (FIG-3116);
- `cancellation_token`, the cooperative stop the attempt host supplies;
- `attachments()`, durable blob output;
- `direct_completions()`, a direct model call inside the atomic attempt with
  usage attribution and no nested journal entry (ADR 0042);
- the declaration inputs: `start_cx`, `process_spawn_provenance`,
  `process_execution_env_spec`, `intent_identity`, `completion_key`;
- `named_phase`, the attempt-attributed fault-probe phase.

Amended 2026-09-29 (FIG-4113,
[ADR 0122](0122-a-stopped-turns-uncommitted-tail-lives-only-on-the-live-stream.md)):
ADR 0114's `progress()` sink is deleted with the capture it wrote to. A tool's
output reaches the host only with its completed call.

`DeferredToolResolver` and `link_with_deferred_resolution`
(`crates/lash-lashlang-runtime/src/deferred.rs:1-15,82-111`) are link-time,
read-only call-path resolution. They are not deferred results and are
unaffected.

#### 1.4 What goes

| Capability today | Where | Fate |
|---|---|---|
| Nested dispatch: `OrchestrationContext::call_tool_batch`, `callable_tool_manifest`, `ToolDispatchClient::batch` | `orchestration.rs:46,140`; `tool_provider/dispatch.rs:15-47` | Deleted. `batch` is sugar (§2). |
| Body-driven process lifecycle: `start_process`, `await_process`, `cancel_process`, `signal_process`, `start_key(ordinal)` | `orchestration.rs:73-128` | Deleted. The declared forms stay: `ToolIntent::{StartProcess, SignalProcess, CancelProcess}` realized after the attempt commits, keyed by `StartKey::for_tool_intent` (`process_identity.rs:185`), plus `DeclaredStart` (§3). |
| The internal process-tool class: `InternalProcessContext`, `InternalProcessAdmin`, `InternalProcessToolDef`, `InternalProcessToolCall`, `InternalProcessToolImplementation`, `PluginSpec::with_internal_tool`, `ToolRegistrations::internal` | `tool_provider/process.rs:16-156,175-349` | Deleted. `ProcessEngine` is the extension point for process bodies. A `ProcessInput::ToolCall` runs only an ordinary recorded attempt. |
| Session administration from a body: `ToolSessionAdmin::snapshot(other)`, `set_tool_membership` | `tool_provider/session.rs:66,101` | Deleted. Hosts reconfigure through their own facade (`crates/lash/src/admin.rs:912`). The reads stay on `AttemptSessionReads`. |
| Body-time trigger emission: `ToolTriggerClient::emit` | `tool_provider/triggers.rs:11` | Deleted. `ToolIntent::EmitTrigger` is the declared form. |
| Body-time process events: `ToolProcessEventClient` | `tool_provider/process_events.rs:89-145` | Deleted. `ToolIntent::EmitProcessEvent` is the declared form. The engine helper `enqueue_wake_delivery` in the same file stays. |
| Body-time child traces: `emit_child_process_started` | `tool_provider.rs:885-900`; `orchestration.rs:130` | Deleted from bodies. Intent realization fires the same hook (`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:319`). |
| The orchestrating registration kind, its source key, its unsafe seal, `CrossLaneToolIdCollision` and the lane's registry, dispatch, child-driver and runner branches | §5 | Deleted. |
| The determinism lint and its wiring | §5 | Deleted. ADR 0105's determinism binds drivers and engines, not tool bodies. |
| The `OrchestrationCall` start-key namespace (`StartKey::for_orchestration_call`) | `process_identity.rs:102-133,193-202` | Deleted. Intent, trigger and host namespaces are unchanged. |
| `drive_independent_effect_work`, `IndependentEffectWork` and every forwarder | §5 | Deleted. No caller remains once no body dispatches. |
| `reaches_relay`, `ToolBatchRoute::Orchestrating` and the relay scenario | §5 | Deleted. Every producer is a flat group. |
| `ToolActivation` (`Always`, `Internal`) | `crates/lash-sansio/src/tool_contract.rs:115-119` | Deleted with the manifest's `activation` field. `Internal` was set only by the internal class (`tool_provider/process.rs:133`), so one variant would remain. Catalog filters on `Internal` go with it. |

#### 1.5 `ToolContext` and the facade

`ToolContext` becomes `pub(crate)` runtime dispatch state. Its `pub
runtime_dispatch` field (`tool_provider.rs:409`) becomes `pub(crate)`. It
leaves the export lists of `lash-core-execution`, `lash-core`
(`crates/lash-core/src/lib.rs:839`) and `lash::tools`
(`crates/lash/src/lib.rs:277`). Its field `orchestrating_sinks` (`:435`) and
the nested-call wait inheritance are deleted. `turn_cancel_wait` (`:441`)
stays only if the ordinary child driver still reads it, and its doc stops
naming nested calls. Test helpers that build a `ToolContext` for an
`AttemptContext` (`crates/lash-protocol-rlm/src/control_tools.rs:418,432`,
`crates/lash-plugin-process-controls/src/lib.rs:582-596`) switch to an
`AttemptContext` test builder in `lash_core::testing`. `lash::tools` exports
the one trait, `ToolCall`, `AttemptContext` and its read and client types,
the outcome types, `PendingCompletion`, `PendingResolver`, `DeclaredStart`
and the intent types. Nothing it exports reaches dispatch, process
administration or an effect controller.

### 2. `batch` is protocol sugar

#### 2.1 Where the expansion happens

The standard protocol driver expands `batch` inside the drive, in
`StandardProtocolDriver::handle_response`
(`crates/lash-protocol-standard/src/lib.rs:500-530`), where it already turns
each response tool call into a `PendingToolCall`. The parsing and folding are
pure functions in `crates/lash-protocol-standard/src/batch.rs`. The driver is
deterministic workflow code ([ADR 0105](0105-the-drive-is-deterministic-workflow-code.md)),
and its protocol configuration is pinned per turn at admission
([ADR 0103](0103-code-cells-replay-by-re-execution-on-every-host.md), FIG-3571
amendment). The expansion is therefore a pure function of the recorded
response and the turn's admitted configuration. It needs no journal record of
its own, and a replay recomputes the identical plan.

The expansion rides the sansio work item that hands the calls to the host:

```rust
// crates/lash-sansio/src/sansio/turn_protocol.rs
pub enum PendingWork<M: TurnProtocol = UnitTurnProtocol> {
    // ...
    Tools {
        /// The flat executable slots of the step's one tool group.
        calls: Vec<PendingToolCall>,
        /// How the slots fold back into the response's calls. Empty when the
        /// response held no sugar.
        expansion: ToolExpansionPlan,
    },
    // ...
}

pub struct ToolExpansionPlan {
    pub wrappers: Vec<ExpandedWrapper>,
}

pub struct ExpandedWrapper {
    /// Position of the wrapper call in the response.
    pub source_position: u32,
    /// Provider call id and replay metadata, kept for the transcript.
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub replay: Option<ProviderReplayMeta>,
    /// One row per member, in member order.
    pub rows: Vec<ExpandedRow>,
}

pub enum ExpandedRow {
    /// The member runs in flat slot `slot` of the group.
    Slot { member_index: u32, tool: String, slot: u32 },
    /// The member was refused before the group opened.
    Refused { member_index: u32, tool: String, error: serde_json::Value },
}
```

`Effect::ToolCalls` carries the same `expansion` to the host. The host path
(`crates/lash-core/src/runtime/turn_driver/tools.rs`) is unchanged in kind.
It prepares every slot in `calls` as it prepares native calls today
(`:52-80`), forms one group, and runs it through `execute_prepared_tool_group`
(`crates/lash-core-execution/src/session/tool_execution/group.rs:778`). It
answers `Response::ToolResults` with one `CompletedToolCall` per slot, in
slot order. The expansion never reaches the runtime's group code.

The fold runs in the machine, before anything is appended or emitted.
`TurnMachine::handle_tool_results`
(`crates/lash-sansio/src/sansio/turn_machine.rs:803-820`) takes the plan from
the pending work and calls a new trait method,
`ProtocolDriverPlugin::fold_tool_results(plan, completed) -> Vec<CompletedToolCall>`
(default: identity when the plan is empty). The standard driver implements it
with `batch.rs`'s fold. Only folded calls reach the stream events, the
transcript append and `handle_tool_results`. The model and the transcript see
one call and one result per wrapper, with the wrapper's original call id and
replay metadata (`crates/lash-protocol-standard/src/lib.rs:513-529`).

#### 2.2 Slot order and member identity

- **Slot order.** Slots are laid out by outer source position, then member
  position. A native call takes one slot. A wrapper's admitted members take
  consecutive slots at the wrapper's position. A refused member takes none.
  Slot order is fixed by the plan and never re-sorted.
- **Group identity.** Unchanged. The group is opened under
  `turn_tool_group_invocation` (`crates/lash-core-execution/src/runtime/causal.rs:137-150`),
  keyed by admitted scope, session, physical turn, turn index, protocol
  iteration and effect id.
- **Child replay keys.** Unchanged. A slot's child is `{group}:child:{slot}`
  (`group.rs:287-296,370`), and its canonical retained request must match on
  reopen. A changed expansion changes the canonical requests, and the
  existing drift refusal applies.
- **Member call ids.** A member's internal call id is
  `{wrapper call id}/batch/{member index}`. It is never shown to the model.
  It feeds tracing, the attempt and completion identities, and intent
  identity. No identity is derived from a call id alone. Observation keys are
  `{iteration}:{slot}:{call id}` (`turn_driver/tools.rs:59-62`), children are
  keyed by slot, and intent identity carries the minting final-attempt
  emission (`tool_intent.rs:361-374`). A model that repeats call ids,
  repeats identical arguments or reuses a frame aliases nothing.

Amended 2026-09-29 (FIG-4073, [ADR 0117](0117-lash-names-every-tool-call.md)):
a member's call id is `ToolCallId::child(member index)` of the wrapper's
`ToolCallId`, over the original member index counted before refusals. It
replaces the `{wrapper call id}/batch/{member index}` string. The wrapper's
own id is lash's, derived from its admission root and position, never the
provider's. Every identity above (attempt, completion, intent, child and
observation) is derived from the member's `ToolCallId`, so "no identity is
derived from a call id alone" now means no identity is derived from a
provider call id.

#### 2.3 Member admission

Each member is admitted exactly as a native call is. It gets its own schema
validation, permission check, approval and before-tool hooks
(`crates/lash-core-execution/src/tool_dispatch/preparation.rs:184,214,219`),
its own attachment policy and its own intents. The wrapper is not a tool
invocation. It has no hooks, no permission rule and no approval, and it
grants nothing to its members. A member resolves against the session's
callable catalog, not the request's listed tools. Discovery's rule that a
native call must name a listed tool (`lib.rs:707-720`) does not apply to
members, so `batch` keeps its reach to discovered tools. A member naming an
unavailable tool is refused by preparation, as a native call would be.

A member's `ToolControl` (`Finish`, `Fail`, `SwitchAgentFrame`) is not
promoted to a protocol control. It is presented in the member's row as its
value. This keeps today's observable behaviour.

#### 2.4 The folded presentation

The wrapper's result keeps today's shape (`batch.rs:39-62`):

```json
{ "results": [ { "index": 0, "tool": "read", "success": true, "result": ... },
               { "index": 1, "tool": "batch", "success": false, "error": "..." } ] }
```

Rows are in member order. Refused rows sit at their index. The wrapper
succeeds even when rows fail. A wrapper whose members were all refused folds
at once. If no slot remains in the whole step, the host opens no group and
answers an empty `ToolResults`. Member usage, possession and trace records
are counted once, by their own children. The fold adds no usage and executes
no intent. Attachment normalization happens per member before the fold.

#### 2.5 Limits and nested refusal

- **Per wrapper.** `tool_calls` must hold 1 to `max_members` entries. More
  than `max_members`, an empty list, or a structurally malformed argument
  refuses the whole wrapper with one failed result. No member starts. The
  body's old "first 25, refuse the rest" fallback (`lib.rs:273,327-333`) is
  deleted.
- **Nested batch.** A member naming `batch` becomes a `Refused` row: "`batch`
  cannot run inside `batch`". The expansion never recurses.
- **Per group.** The flattened group is admitted whole against the opener's
  retained-children bound, 1024 by default
  (`crates/lash-core-execution/src/session/opener_groups.rs:96-100`), under
  ADR 0099's retirement rule. It is never split into waves. If the bound
  refuses the group, the existing refusal applies to the whole step.

#### 2.6 Cancellation

Cancellation is the group's own (ADR 0099 §4). Closing or cancelling the
group follows the recorded consumed-prefix rules
(`group.rs:487-518`). Committed drains finish, undecided children are
cancelled, and late settlements are incorporated once, through the opener.
The fold renders a cancelled row for every slot left unobserved at the
cancellation boundary. Replay preserves that boundary, even if more finals
exist by then. Final-commit rank orders drains, and member order orders rows.
An infrastructure failure aborts the turn. It never becomes a member error
row.

#### 2.7 Configuration

```rust
// crates/lash-protocol-standard/src/lib.rs
pub const BATCH_MEMBER_CEILING: usize = 64;

pub enum BatchSugar {
    /// `batch` is offered, with at most `max_members` members per call.
    Enabled { max_members: std::num::NonZeroUsize },
    /// `batch` is not offered. A call named `batch` is an ordinary unknown tool.
    Disabled,
}

impl Default for BatchSugar { /* Enabled { max_members: 64 } */ }

impl StandardProtocolConfig {
    pub fn batch(self, sugar: BatchSugar) -> Self;
}
```

The standard plugin validates the setting when it builds. `max_members`
above `BATCH_MEMBER_CEILING` is refused with
`PluginError::InvalidBatchMaximum { requested, ceiling: 64 }`. When enabled,
the driver adds the `batch` definition to each request's tool list, with
`maxItems` and the description rendered from `max_members`. The prompt
section (`lib.rs:56`) renders the same number. `batch` is not a Tool Catalog
entry, so tool membership does not apply to it, RLM cells and processes
cannot call it, and discovery cannot list it. The explicit non-standard batch
installer in the agent-scenario harness
(`crates/lash/src/tests/agent_scenarios/harness.rs:343`) is deleted.

#### 2.8 Parallelism is structural

Every slot of the group is launched before any is awaited, on every tier.
There is no serial fallback, no wave and no ordinal-journal path. Native
parallel calls and `Promise.all` members were already group children. `batch`
members now are too, in the same group as their native siblings. §7's barrier
laws pin it on all three tiers, with a forced-serial negative control that
must fail.

### 3. `DeclaredStart`

#### 3.1 Shape

```rust
// crates/lash-core-execution/src/tool_result.rs
/// The one start a pending call declares, and whose terminal resolves it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeclaredStart {
    start: Box<StartProcessIntent>, // sealed
}

impl DeclaredStart {
    /// Validates the one start against the attempt that declares it.
    pub fn new(
        context: &AttemptContext<'_>,
        start: StartProcessIntent,
    ) -> Result<Self, DeclaredStartRefused>;
    pub fn start(&self) -> &StartProcessIntent;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeclaredStartRefused {
    #[error("a declared start must name the declaring session")]
    ForeignSession,
    #[error("a declared start needs a prepared tool call id")]
    MissingCallId,
    #[error("a declared start needs a completion key")]
    CompletionUnavailable,
}

impl PendingCompletion {
    pub fn resolved_by_declared_start(self, start: DeclaredStart) -> Self;
}
```

The one start is structural. `Pending` carries no `ToolIntents`, and
`DeclaredStart` holds exactly one `StartProcessIntent`. Its intent identity
is `AttemptContext::intent_identity(0)`, and its start key is
`StartKey::for_tool_intent` of that identity. A second start, a second
intent or a malformed index cannot be written.

#### 3.2 The launch rule

These are the rules of the spawn recheck (`study-spawn-recheck-astra.report.md`),
made normative:

1. **One same-session start.** A pending launch contains exactly one
   `StartProcess` of the declaring session, and nothing else.
   `DeclaredStart::new` checks the session; the types forbid the rest.
2. **The completion key is reserved before execution.** A provider that can
   return a declared start answers `attempt_may_defer` `true` for that tool,
   so the attempt's completion key is derived and taken before the body runs.
   A terminal that lands before the park resolves before the wait
   (`ResolveOutcome::AlreadyResolved`).
3. **Seal before launch.** The selected attempt's declaration and a
   recoverable launch obligation (§3.3) are sealed together, at the child's
   cancel linearization point (ADR 0099 §4), before any realization. Exactly
   one of the launch seal and a cancel disposition commits first.
4. **Discarded retries execute nothing.** Only the sealed attempt's
   declaration launches. A retried or superseded attempt's `DeclaredStart` is
   discarded with the attempt.
5. **Registration alone mints the id.** Recovery drains the obligation under
   its recorded identity. The registrar mints the `ProcessId` inside the
   registration transaction ([ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md) §1).
   Retained-key registration returns the existing process before minting.
6. **Receipt, then arm, on every redrive.** The realized id, or the typed
   refusal, is recorded as the launch receipt. Then the runtime arms the
   terminal wait (`attach_process_terminal(process_id, key)`). The wait is
   re-armed from the recorded receipt on every redrive, as today's resolvers
   are (`pending_resolver.rs:10-17`).
7. **Refusal settles; infrastructure failure retries.** A typed start refusal
   (for example `ParentEnded`, a reachability refusal, or a closed scope)
   settles the call as a failure. An infrastructure failure leaves the
   obligation in place for recovery.
8. **Cancel before launch prevents admission.** A cancel disposition that
   commits before the seal refuses the seal, typed and with no journal write.
   Nothing launches.
9. **Cancel after launch cancels the child.** A cancel disposition that
   commits after the seal keeps the launch obligation. The start still
   realizes, so its id is deterministic, and the cancel obligation (§3.4)
   durably cancels the resulting child.
10. **Release drain ordering before waiting.** The launch takes its place in
    the group's final-commit drain order (ADR 0099 §5) like an intent drain.
    It releases that order once the receipt is recorded, before parking. No
    sibling waits on a running child, so a batch of spawns overlaps.
11. **Rank at resolution.** The call's terminal settlement rank is allocated
    only when the result or the cancel resolves it
    (`tool_child_driver.rs:1392-1416`), never at launch.
12. **The child stays cancellable.** An unresolved child is always reachable
    by the cancel obligation, by its lifetime's scope close (ADR 0108 §5) and
    by the call's deadline (§3.5).

#### 3.3 The launch obligation

The launch obligation is a step in the tool child's own invocation journal:
`{child}:launch:seal`, carrying the declaration digest. The child invocation
is not finished until its receipt step, `process:start:{start key}`
(`crates/lash-core-execution/src/runtime/effect/envelope.rs:903`), is
recorded. Recovery is the engine's redelivery of the unfinished child
invocation ([ADR 0110](0110-the-engine-owns-process-recovery.md)). On replay
the seal is read back and the start is issued under its recorded key. No
store-side obligation kind is added. The same journal positions exist on
every tier, because every tool child is a group child invocation
(ADR 0099 §2).

#### 3.4 The cancel obligation consumes `CancelHint`

`CancelHint` (`tool_result.rs:14-18`) is declared today and consumed nowhere.
The child driver arms and awaits without cancelling the process
(`tool_child_driver.rs:1525-1601`). It becomes binding for both runtime-owned
resolvers:

- When a parked call whose resolver is `ProcessTerminal` or `DeclaredStart`
  is cancelled (group cancel, turn cancel, a race loser, or its deadline),
  and its `on_cancel` is `CancelExternalWork`, the cancel disposition's
  commit also records a cancel obligation, `{child}:cancel-work`.
- The obligation is drained before the child settles `Cancelled`. It issues
  one replay-keyed `ProcessCommand::Cancel` for the resolved or launched
  process id. If the launch was refused (rule 8), the obligation records
  "nothing launched" and completes.
- It is replay-keyed by the call's completion key, so a redrive re-issues the
  same command and the process registry answers the same outcome.
- `CancelHint::Ignore` drops only the wait. The process keeps running.
- A terminal and a cancel have one winner, at the linearization point. A
  terminal that wins settles the call normally, and a later cancel of the
  child is a no-op.

#### 3.5 A timeout cancels the child

A `deadline` on a declared-start call counts as a cancellation for
`CancelHint` purposes. When it elapses, the call resolves as the
`TimeoutBehavior` says (`tool_result.rs:1-9`), and the cancel obligation
cancels the child. `TimeoutBehavior::FailTurn` fails the turn, and the
turn's scope close cancels the child through its lifetime as well.

#### 3.6 Retention pinning

The registration of a declared start carries a consumer hold on the process
row, keyed by the parked call's completion key. The hold is written in the
registration transaction. The registry's prune refuses to prune a held row.
The child driver releases the hold with a replay-keyed registry command after
it incorporates the call's settlement or cancellation. The opener's scope
close releases every hold its calls still own, so an abandoned turn leaks
none. While the hold stands, a redrive always finds the process under its
key, and a start whose receipt was lost can never register a second child.
This closes ADR 0107's retention gap for declared starts.

#### 3.7 `SessionTurnResult` and the projection

The SessionTurn input's `output_contract` is carried but never consumed by
the runner (`crates/lash-core/src/runtime/session_manager/process_runners/runner.rs:57-75`).
The trigger router still fingerprints it
(`crates/lash-core-execution/src/triggers/router.rs:272-289`). It is replaced
in place:

```rust
// crates/lash-core-execution/src/runtime/process/model.rs
SessionTurn {
    definition_key: String,
    create_request: Box<crate::SessionCreateRequest>,
    turn_input: Box<crate::TurnInput>,
    result: SessionTurnResult,
},

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionTurnResult {
    /// The runner answers the child's `AssembledTurn` (today's value).
    Turn,
    /// The runner answers the child's final value, checked against `schema`.
    FinalValue { schema: Option<serde_json::Value> },
}
```

The router's definition fingerprint hashes `result` where it hashed
`output_contract`. The spawn result projection moves out of the tool and into
the SessionTurn runner, `output_from_process_turn`
(`crates/lash-core/src/runtime/session_manager/process_runners/session.rs:331-370`).
Under `FinalValue` the runner:

- extracts the final value as `task_result_value` does today
  (`crates/lash-subagents/src/rlm_support.rs:279-303`): a final value, a
  tool value, or the trimmed assistant text;
- refuses an agent-frame switch or a stopped child with a typed failure;
- validates the value against `schema` with `lash-sansio`'s schema validation
  (its `schema-validation` feature is already enabled for `lash-core`);
- keeps typed cancellation, and the child's `submit_error` reason as the
  failure, which already originates at `session.rs:258-295`.

`tool_output_from_completion_resolution` (`tool_result.rs:428-440`) unwraps a
`DeclaredStart` resolution exactly as it unwraps `ProcessTerminal`. The call
therefore answers the child's own value, failure or cancellation, never the
envelope. `child_task_result` (`crates/lash-subagents/src/rlm.rs:240-276`) is
deleted.

#### 3.8 What the host sees

The launch receipt is recorded as the call's intent outcome for index 0:
`ToolIntentExecutionOutcome::Executed { identity, kind: StartProcess, result }`
(`crates/lash-sansio/src/tool_output.rs:180-196`), on the `ToolCallRecord` and
the host `CompletedToolCall`. The realization fires the child-process trace
hook, `ToolChildProcessStarted` (`intent_executor.rs:319`), so the trace links
call and child. The child session derives from the minted id (ADR 0107), so
the record identifies the child session too. The model sees only the value.

### 4. `spawn_agent` on the new shape

`RlmSubagentToolsProvider` becomes the one `StaticToolProvider` for
`spawn_agent` and, for child sessions, `submit_error`
(`crates/lash-subagents/src/rlm.rs:279-311`). `SpawnAgentOrchestratingTool`,
`spawn_agent_orchestrating_tool` and `execute_orchestration` are deleted, and
the plugin registers only the provider (`crates/lash-subagents/src/lib.rs:116-181`).

- **Prepare** is unchanged (`rlm.rs:99-174`). It resolves the capability, the
  depth and the `ParentFork` plugin init, clears `session_id`, renders the
  task, and seals `PreparedSpawnAgent { create_request, turn_input }`. It
  additionally seals the parsed output schema, which today is re-parsed at
  execution (`rlm.rs:56-57`).
- **Execute** decodes the payload and builds one `StartProcessIntent` whose
  input is `ProcessInput::SessionTurn { definition_key:
  "lash-subagent-session-turn", create_request, turn_input, result:
  SessionTurnResult::FinalValue { schema } }`. Its lifetime is the host
  policy applied to `context.start_cx()` (`lifetime::starter` by default),
  its provenance is `context.process_spawn_provenance()`, its environment is
  `context.process_execution_env_spec()`, and its declared identity is
  labelled `subagent`/`spawn`. It returns
  `ToolAttemptOutcome::pending(PendingCompletion::new().resolved_by_declared_start(DeclaredStart::new(context, start)?))`,
  with the host's deadline when one is configured. `attempt_may_defer`
  answers `true` for `spawn_agent`. The definition key loses its `:v1`
  suffix under the freeze.
- **Timeout.** `SubagentsPluginFactory::with_timeout(Duration)` sets the
  pending deadline. There is none by default. A timeout returns
  `"subagent timed out after {d}"` as an error result and cancels the child
  (§3.5).
- **The tool record** carries the launch receipt (§3.8). The model's result
  is the child's final value, a failure carrying the child's reason, or a
  cancellation.
- **Idempotency.** The intent identity fixes the start key. The start key
  fixes the process under retention (§3.6). The process fixes the child
  session and its turn. Every redrive reaches the same child.
- **Descriptions.** The tool description stops telling the model that direct
  awaits are serial (`rlm.rs:365`). `Promise.all` over `agents.spawn`, and a
  `batch` of `spawn_agent` calls, overlap.

Depth and capability enforcement (`crates/lash-subagents/src/capability.rs:124-146`),
recursive-tool hiding and inheritance are unchanged. Handles stay RLM's
`processes.start`/`processes.await` (`crates/lash-plugin-process-controls/src/lib.rs:307-335`),
which already return declarations and deferred results without awaiting in a
body.

### 5. Deletion inventory

Paths are grouped by crate. "Delete" removes the file. "Remove" removes the
named items and every caller, test and doc line that names them. Anything
the §6 grep finds in a file not listed here is removed as well.

**`crates/lash-core-execution`**

- Delete `src/tool_provider/orchestration.rs`, `src/tool_provider/process.rs`,
  `src/tool_provider/dispatch.rs`, `src/tool_provider/triggers.rs`.
- `src/tool_provider/session.rs`: remove `ToolSessionAdmin` and its
  administration and membership writes. Keep `ToolSessionModel` only if a
  read still uses it; otherwise remove it.
- `src/tool_provider/process_events.rs`: remove `ToolProcessEventClient`;
  keep `enqueue_wake_delivery`.
- `src/tool_provider.rs`: make `ToolContext` and its builder `pub(crate)`;
  remove `orchestrating_sinks`, the public accessors `dispatch`, `triggers`,
  `process_events`, `sessions` (admin), `emit_child_process_started`
  (`:846-935`), and the test `an_orchestrating_context_without_its_lineage_is_refused`.
- `src/tool_registry/{state.rs,sources.rs,registry_types.rs,registry_impl.rs,restore_execute.rs,rebind.rs}`:
  remove the orchestrating and internal registrations, `ToolSourceKey::Internal`,
  `ToolSourceExecution::{Internal,Orchestrating}`, `CrossLaneToolIdCollision`
  and the lane collision checks. Remove their tests from `tool_registry/tests.rs`,
  `tool_registry/tests/restore_tests.rs` and `tool_registry/pinning_tests.rs`.
- `src/plugin/{registry.rs,registrar.rs,runtime_impl.rs}`: remove
  `with_orchestrating_tool`, `with_internal_tool`, `ToolRegistrations::orchestrating`
  and `ToolRegistrations::internal`, and their plumbing.
- `src/tool_dispatch.rs`, `src/tool_dispatch/context.rs` (`OrchestratingChildSinks`,
  `:61-109,162`), `src/tool_dispatch/execution.rs` (`execute_orchestrating_tool`,
  `execute_internal_process_tool`, the no-attempt route and
  `orchestrating_tool_returned_pending`, `:132-190`),
  `src/tool_dispatch/preparation.rs` (the orchestrating re-prepare exception,
  `:138-152`): remove.
- `src/runtime/effect/tool_child_driver.rs`: remove the orchestrating arm, its
  sinks, its possession drain and its nested-wait cancel trio
  (`:950-960,1108-1137,1195,1312-1365,1435-1441,1513`). Keep
  `admitted_tool_drift` (`:988`) and `rebuild_refusal`: they guard every
  catalog child. Remove the orchestrating cases from
  `tool_child_driver/tests.rs` and `tool_child_driver/rebuild_tests.rs`.
- `src/runtime/effect/tool_settlement.rs:177,433`, `src/runtime/effect/tool_child.rs`,
  `src/runtime/effect/envelope.rs:404`, `src/runtime/effect/executor/control/scope.rs:81`:
  remove the "no attempt frame" classification and its prose. Every tool
  child is an attempt.
- `src/runtime/effect/executor/control.rs` (`:407-421,797-802`),
  `src/runtime/effect/layered_host.rs:660-668`, `src/runtime/effect/executor.rs:34`,
  `src/runtime/effect/mod.rs:62`, `src/runtime/mod.rs:59`, `src/lib.rs:750`:
  remove `drive_independent_effect_work` and `IndependentEffectWork`.
- `src/testing/attempt_sentinel.rs:278-283`: remove the forwarder.
  `src/testing/kernel_internals.rs`, `src/testing/test_protocol_fakes.rs`
  (its batch fake with the 25 wording, `:185-345`),
  `src/testing/execution_context_builder.rs`: remove the orchestrating and
  internal helpers.
- `src/session/process_handles.rs:64-110`: remove `start_tool_process`, the
  body-side start and its orchestration start-key derivation (`:91`).
- `src/session/tool_execution.rs`: remove the orchestrating and internal
  branches and the internal-activation filter (`:647`).
  `src/session/tool_execution/batch.rs`: remove prose that names body
  callers. `RuntimeExecutionContext::call_tool_batch` stays: it is the
  language runtime's aggregate path (RLM `Promise.all`, Lashlang process
  aggregates), not a body capability.
- `src/lib.rs`: remove the exports of everything above.
- Tests: delete `tests/store_backed/kernel/tool_dispatch/tests/orchestrating.rs`
  and `tests/store_backed/kernel/tool_dispatch/tests/internal_activation.rs`.
  Remove the orchestrating and internal cases from
  `tests/store_backed/kernel/tool_dispatch/tests.rs`,
  `tests/store_backed/kernel/tool_dispatch/tests/attachment_normalization.rs`,
  `tests/store_backed/kernel/session/tool_execution/batch.rs`,
  `tests/store_backed/kernel/session/tool_execution/presentation_redrive.rs`,
  `tests/store_backed/kernel/tool_provider/process.rs` and
  `tests/store_backed/kernel/runtime/effect/executor/process_local.rs`.

**`crates/lash-core-store`**: remove `ToolRegistrationKind` (with one variant
left, the persisted field is dropped in place; `src/tool_state.rs:155-162`),
`ReconfigureError::CrossLaneToolIdCollision` (`:234-240`),
`StartKeyNamespace::OrchestrationCall` and `StartKey::for_orchestration_call`
(`src/process_identity.rs:102-133,193-202`). The family stays
`lash.process-start-key` v1, changed in place.

**`crates/lash-core`**: `src/runtime/session_manager/process_runners/tool.rs:110-129`
(the internal and orchestrating branches), `process_runners/runner.rs:35`
(prose), `src/lib.rs:693,839` and `src/runtime/mod.rs:200` (exports),
`src/testing/runtime_helpers.rs`: remove. Remove the orchestrating cases from
`tests/runtime/tests/child_sessions.rs` and `tests/runtime/tests/effect.rs`.

**`crates/lash-sansio`**: remove `ToolActivation` and the manifest
`activation` field (`src/tool_contract.rs:115-123,767`), the `Internal`
filters (`src/tool_catalog.rs:204,239,410`) and the export
(`src/lib.rs:149`).

**`crates/lash-protocol-standard`**: remove `standard_batch_orchestrating_tool`,
`StandardBatchOrchestratingTool`, `execute_orchestration`, `BatchCallSpec`
parsing into invocations, `BATCH_MAX_TOOL_CALLS` and the 25 wording
(`src/lib.rs:56-58,124-125,214-381`); the `Internal` discovery filter
(`:142`); the orchestrating contract cases in `src/driver_contract_tests.rs:181`
and `src/tests.rs`. `src/batch.rs` becomes the definition renderer plus the
pure expansion and fold.

**`crates/lash-subagents`**: remove `execute_orchestration`,
`SpawnAgentOrchestratingTool`, `spawn_agent_orchestrating_tool` and
`child_task_result` (`src/rlm.rs:39-97,177-276`); the orchestrating
registration (`src/lib.rs:140-162`). `rlm_support::task_result_value` and
`validate_task_result` move to the SessionTurn runner (§3.7).

**`crates/lash-protocol-rlm`**: `src/tool_catalog.rs` (`Internal` filters,
`:42,302,427,658,1207`), `src/driver.rs:150`, `src/executor/state/tests.rs`,
`src/control_tools.rs:418,432` (the `ToolContext` test constructors):
remove. `tests/tool_batch_parallelism.rs`: remove `reaches_relay` and the
relay comments.

**`crates/lash-lashlang-runtime`**: `src/replay_run.rs:195-198,319-326,584`,
`src/replay_commands.rs:128`, `src/replay_run_tests.rs:369`: remove the
orchestrating row shape (a `process:await` row under a tool-call ordinal, and
the `ToolCall`/`AwaitHandle` merge arm) and its prose. A declared start's rows
are `process:start:{key}` and `process:attach-terminal:{id}:{key}` under the
tool command, which `CommandShape::ToolCall` already reads (`:225-227`).
`src/cell_bindings.rs:171`, `src/lib.rs`, `src/lib_tests.rs`: remove the
`Internal` filter.

**`crates/lash-remote-protocol`**: `src/tools.rs` and
`src/core_conversions/tools.rs:182`: remove the activation field, in place.

**`crates/lash-restate`**: `src/controller/scope_recording.rs:267-272` (the
forwarder); `src/effect_group/dispatch.rs:382` (prose);
`src/tests/tool_batch_parallelism_on_the_double.rs` (`reaches_relay`);
`src/tests/journal_cut_runner.rs:183-190` (the empty-orchestration law);
`src/tests/conformance_and_poison.rs:317-339` (the orchestration factory);
`src/tests/effect_group_conformance.rs:49,553,748`,
`src/tests/process_start_replay_on_the_double.rs`,
`src/tests/turn_laws_on_the_double.rs`: remove the orchestrating cases and
prose. `crates/lash-restate-test/tests/tool_child_drift.rs`: remove the
orchestrating cases.

**`crates/lash-conformance`**: delete `src/conformance/cell_orchestration_redrive.rs`
and its macro. Remove the orchestrating leaf (`LEAF_ORCHESTRATING`) and every
orchestrating route from `src/conformance/tool_child_invocation/{mod.rs,driver.rs,batch_group.rs,admission_fence.rs,opener_end.rs,incarnation.rs}`
and `src/macros/tool_child.rs`. Remove the orchestration factories from
`src/conformance/migrated_tools_redrive.rs`, the `IndependentEffectWork` cases
from `src/conformance/segment_redrive.rs:600-640` and the forwarder from
`src/conformance/turn_crash_matrix/seam_controllers.rs:415-420`. Retarget
`src/conformance/served_process_start.rs:392-401` and
`src/conformance/process_registry/registration.rs:33` to
`StartKey::for_tool_intent`. Rework `src/conformance/tool_batch_parallelism.rs`
per §7.

**`crates/lash`**: `src/lib.rs:270-298` (exports), `src/tests/agent_scenarios/harness.rs:343`,
`src/tests/turn_streaming.rs:553`, `tests/integration/integrator_facade.rs:56`,
`tests/triggers_evidence.rs:35-38`, `tests/ui/facade_boundary_types_are_public.rs`:
remove. Delete `tests/ui/orchestrating_tool_def_requires_unsafe.{rs,stderr}`,
`tests/ui/orchestrating_tool_def_unsafe_is_auditable.{rs,stderr}` and
`tests/ui/tool_call_context_is_journal_incapable.{rs,stderr}`, their lines in
`tests/ui.rs:78-85` and `BUILD.bazel`'s `ui_fixtures` list.

**`crates/lash-sim`**: `src/request_snapshot.rs:111-131` (the 25 wording),
`tests/possession_conservation.rs:22`: update.

**Scripts, hooks and CI**: delete `scripts/lint_orchestrating_tools.py` and
`scripts/test_lint_orchestrating_tools.py`. Remove their wiring in
`.github/workflows/ci.yml:390-391,494`, `.pre-commit-config.yaml:78-81`,
`scripts/push-gate.sh:380-386` and `scripts/test_confidence_gate_ci_contract.py:2546`.

**Docs**: `runbooks/process-operations/runbook.md:133` (the spawn row) and
`examples/agent-workbench/README.md:226` (`ToolContext::completion_key`)
are rewritten to the new model. The ADRs are amended in this commit (§8).

**Kept on purpose.** The group and child driver, rebinding, deployment
reconstruction, drift refusals, cancellation fencing, final-commit ranks,
protected intent drains and late-settlement incorporation all protect
ordinary children. `SessionServicesRefusal`
(`tool_child_driver/deployment_context.rs:201-217`) protects live session
reads. `RuntimeExecutionContext::call_tool_batch`, `ToolInvocation` and
`ToolInvocationReply` serve language aggregates. `ToolChildProcessStarted`
serves intent realization. `PendingAnnouncement` and `DeferredToolResolver`
are unrelated to body driving.

### 6. The deletion grep

This command must print nothing on `main` when the arc closes:

```sh
rg -n --pcre2 \
  -e 'Orchestrat|(?i:orchestrating|_orchestrat|orchestration_)' \
  -e '(?i:\borchestration\b\s*[:,]|\borchestration (body|bodies|lane|context|call|calls|tool|tools|relay|route|start|starts|surface|factory|factories|redrive))' \
  -e 'InternalProcess|with_internal_tool|internal_process_tool|resolve_internal_manifest|ToolRegistrations::internal|ToolActivation' \
  -e 'drive_independent_effect_work|IndependentEffectWork|coordinate_nested_tool_batch|CrossLaneToolIdCollision' \
  -e 'ToolDispatchClient|ToolSessionAdmin|ToolTriggerClient|ToolProcessEventClient' \
  -e 'reaches_relay|relay_(tool|args|replies|name|definition|factory)\b|RendezvousRelay|EmptyRelay|ToolBatchEntry::Relay|tools\.relay\b|relay-call' \
  -e 'BATCH_MAX_TOOL_CALLS|\b1\s*[-–]\s*25\b|(?i:(up to|maximum of|max) 25\b)|"maxItems":\s*25\b|\b25[- ](tool calls|calls|members?|leaves)\b' \
  -g '!docs/adr/**' -g '!crates/lash-typescript/tests/test262/{test,outcomes,skip-register}/**' \
  crates scripts examples runbooks docs .github .pre-commit-config.yaml
```

What it covers:

- `Orchestrat` (case-sensitive) and `orchestrating` (any case) cover
  `OrchestrationContext`, `OrchestratingToolDef`,
  `OrchestratingToolImplementation`, `OrchestratingChildSinks`,
  `ToolRegistrationKind::Orchestrating`, `ToolBatchRoute::Orchestrating`,
  `OrchestrationCall`, `orchestrating_sinks`, `execute_orchestrating_tool`,
  `is_orchestrating_tool`, `with_orchestrating_tool`,
  `orchestrating_tool_returned_pending`, `LEAF_ORCHESTRATING`,
  `lint_orchestrating_tools` and the `orchestrating-tool-determinism` hook.
  The identifier forms cover `execute_orchestration`,
  `standard_batch_orchestrating_tool`, `StandardBatchOrchestratingTool`,
  `for_orchestration_call` and `cell_orchestration_redrive`. The phrase
  forms cover prose such as "orchestration body". The word stays legal where
  it means something else: "orchestrator" for the humans and agents that run
  lanes, ADR 0014's drain orchestrator, and plain "orchestration" as a noun.
  The test262 harness's "orchestrating thread"
  (`crates/lash-typescript/tests/test262/support/runner.rs:39,851`, its
  `README.md:143`) is reworded to "coordinating thread".
- The internal class, `ToolActivation`, the independent-work fallback, the
  cross-lane collision and the body capability clients by name.
  `call_tool_batch` as a body capability is covered by `OrchestrationContext`
  and `ToolDispatchClient`. The runtime's aggregate method of that name stays
  (§5).
- The relay scenario's identifiers. The word "relay" alone stays legal: the
  obligation relays (ADR 0109) own it.
- Every spelling of the 25-member cap. The three excluded test262 folders
  are the vendored conformance corpus, whose file names contain `1-25`.

`docs/adr/**` is excluded because ADRs are the historical record. Every ADR
that describes the replaced system carries a dated
"Amendment (FIG-3562, 2026-09-29)" note after that text. This check must
print nothing:

```sh
grep -L 'Amendment (FIG-3562, ' docs/adr/0042-*.md docs/adr/0051-*.md \
  docs/adr/0059-*.md docs/adr/0065-*.md docs/adr/0074-*.md docs/adr/0099-*.md \
  docs/adr/0120-tool-presentation-*.md docs/adr/0103-*.md docs/adr/0105-*.md \
  docs/adr/0107-*.md docs/adr/0108-*.md docs/adr/0114-*.md
```

Each lane's done-when includes the grep restricted to its own paths, so
traces do not pile up for the sweep. The restricted form is the same command
with the trailing path list replaced by the lane's owned paths from §10.

### 7. Acceptance tests

"All three tiers" means: in-process (native and SQLite effect hosts, through
a group-capable `HostTurnRunner`; `crates/lash-conformance/src/conformance/turn_runner.rs:297-305`),
the Restate server double (`lash-restate-test`), and live Restate (the
isolated server of `just effect-group-conformance-e2e`). The in-process
engine-testing controller refuses groups
(`crates/lash-core-execution/src/engine/testing/controller.rs:58-63`) and is
not a tier here. The double counts once. Live positive cases run five times.
Every body in a barrier law records `Started` and then waits on the barrier
for all distinct member starts before answering. Observer dispatch events
alone never satisfy a barrier.

#### 7.1 Barrier laws

Home: `crates/lash-conformance/src/conformance/tool_batch_parallelism.rs`
(reworked; the relay route, `ToolBatchEntry::Relay`, `ToolBatchRoute::Orchestrating`
and `reaches_relay` are deleted). Registrations:
`crates/lash-protocol-rlm/tests/tool_batch_parallelism.rs` (in-process and
double), `crates/lash-restate/src/tests/tool_batch_parallelism_on_the_double.rs`
(double) and `crates/lash-restate/src/tests/conformance_and_poison.rs` (live,
un-parking `samuel-fig-3397-a-restate-parallelism`).

| Law | Proves |
|---|---|
| `tool_group_members_start_before_any_finishes` | At widths 2, 8 and 64, every member starts before any finishes, for each producer: one `batch` wrapper; two wrappers plus native siblings sharing one barrier; native parallel calls; RLM `Promise.all`; RLM `Promise.allSettled`; a Lashlang process aggregate. Routes: leaf, granted, deferred. Peak in-flight equals the width, and reply order is exact. |
| `tool_group_reverse_dependency` | The input-first member answers only after the input-last member started. |
| `forced_serial_host_fails_the_barrier` | The negative control. A test-only `SerialGroupHost` decorator awaits each child's settlement before it starts the next. The law must fail on every tier, naming the members that never started. The deadlock budget only bounds the run; the named-members message is the assertion. |

#### 7.2 Batch sugar laws

Home: new `crates/lash-conformance/src/conformance/batch_sugar.rs`,
registered on all three tiers beside §7.1. Unit tests for the pure
expansion and fold sit in `crates/lash-protocol-standard/src/batch.rs`.

| Law | Proves |
|---|---|
| `batch_admission_and_identity_contract` | 64 members run; 65 refuse the whole wrapper and start nothing; an empty or malformed list refuses the wrapper; a nested `batch` is a refused row; unavailable tool, schema failure and denied approval are refused rows; a wrapper approval grants nothing; repeated model call ids, identical arguments and a reused frame alias no member; a disabled `batch` is an unknown tool. |
| `batch_config_ceiling_is_refused_at_build` (unit) | `max_members` 65 fails the plugin build with `InvalidBatchMaximum`; 1 to 64 build. |
| `batch_replay_preserves_fold_and_ranks` | A cold replay and a perturbed-schedule replay produce identical child keys, rows, presentation and ranks, and incorporate each settlement once. |
| `batch_redrive_reuses_children` | A crash after one final and before the fold: the redrive reuses the settled child, the unfinished members rendezvous under fresh attempt epochs, and a following barrier batch still overlaps, so cached replies cannot mask serialization. |
| `batch_cancel_preserves_committed_drains` | A cancel after all starts, with one committed drain blocked: that drain finishes, undecided members settle `Cancelled`, their rows say so, late settlements change no row and no accounting doubles. |
| `batch_all_refused_opens_no_group` | A response whose only call is a fully refused wrapper opens no group and answers the folded rows. |
| `batch_folds_to_one_transcript_call` | The transcript and stream events show one call and one result per wrapper, with the provider's call id and replay metadata, and no member calls. |

#### 7.3 Declared-start and spawn laws

Home: new `crates/lash-conformance/src/conformance/declared_start.rs`. The
laws take the `lash-subagents` plugin factory from the registering crate, as
the RLM registrations hand in theirs. Registrations: new
`crates/lash-subagents/tests/declared_start.rs` (in-process and double;
`lash-conformance` joins its dev-dependencies) and
`crates/lash-restate/src/tests/conformance_and_poison.rs` (live). Projection
unit tests sit in
`crates/lash-core/src/runtime/session_manager/process_runners/session.rs`'s
test module.

| Law | Proves |
|---|---|
| `declared_start_crash_at_every_launch_boundary` | A crash at each boundary: (B1) after the seal and before realization; (B2) after registration and before the receipt; (B3) after the receipt and before arming; (B4) after arming and before the child's terminal; (B5) after the terminal and before delivery and incorporation. Every redrive ends with one process, one child session, one child turn and one result, unchanged configuration, and no duplicated possession or usage. |
| `declared_start_discarded_retry_launches_nothing` | A retryable failure followed by a declared start launches exactly one child, from the final attempt. |
| `declared_start_refusal_settles_the_call` | A start refused by a closed scope settles the call as a typed failure and leaves no obligation. |
| `declared_start_early_terminal_resolves_before_wait` | A child that finishes before the park still resolves the call. |
| `declared_start_rearm_is_idempotent` | Repeated redrives re-arm the same wait with no second resolution. |
| `declared_start_cancel_at_each_point` | A cancel (C1) before the seal launches nothing; (C2) after the seal and before realization launches and then cancels the child; (C3) after the receipt and before arming cancels the child; (C4) while parked cancels the child; (C5) after the terminal loses to the terminal. Each child is cancelled at most once, and the cancel obligation survives a crash. `CancelHint::Ignore` leaves the child running. |
| `declared_start_timeout_cancels_the_child` | A deadline resolves the call as a timeout error and cancels the child. |
| `declared_start_scope_close_cancels_until_children` | Closing the starter scope cancels an unresolved child through its lifetime. |
| `declared_start_retention_hold_blocks_prune_until_consumed` | Prune leaves a held child, and a redrive with a lost receipt finds it. After incorporation the hold is released and prune may take it. |
| `spawn_agent_projects_final_value` (unit) | Final value, tool value and text extraction; schema pass and fail; `submit_error`; frame-switch and stopped-child refusals; typed cancellation. |
| `spawn_agent_record_carries_child_identity` | The tool record's intent outcome and the trace name the child process, and the model's result is the value only. |
| `batch_of_spawns_overlaps` | At widths 2, 8 and 64, each child's first model step waits on a barrier for every sibling child to start, so the children demonstrably run at once and no drain waits serially. It mixes in ordinary and deferred siblings, then cancels mid-flight (every child cancelled once) and crash-redrives (every child reused). Width 64 uses one `batch`, width 8 uses native parallel calls, and width 2 uses RLM `Promise.all` over `agents.spawn`. |

#### 7.4 Compile-fail fixtures

Home: `crates/lash/tests/ui/`, listed in `crates/lash/tests/ui.rs` and the
`//crates/lash:ui_fixtures` list. They prove that no context a body can hold
reaches dispatch, process administration or a controller, and that no
constructor, safe or unsafe, mints another path. They name only surviving
types, so the §6 grep stays empty.

| Fixture | Proves |
|---|---|
| `tool_context_is_not_nameable.rs` | `lash::tools::ToolContext` does not resolve. |
| `attempt_context_has_no_process_administration.rs` | `call.context.processes()` has no `start`, `cancel`, `signal` or `await_terminal`; only reads resolve. |
| `attempt_context_has_no_controller.rs` | No method or field of `AttemptContext` yields an effect controller or a dispatch context. |
| `pending_start_cannot_carry_intents.rs` | A `DeclaredStart` cannot be built from `ToolIntents` or hold a second start. |
| `declared_start_is_sealed.rs` | A `DeclaredStart { .. }` literal does not compile. |
| `plugin_spec_registers_only_providers.rs` | `PluginSpec`'s tool registration accepts only `Arc<dyn ToolProvider>`. |

The existing `attempt_context_has_no_journal_capability`,
`attempt_context_has_no_recursive_dispatch`, `attempt_context_has_no_session_mutations`,
`attempt_context_has_no_trigger_commands`,
`attempt_context_has_no_process_event_commands`,
`attempt_context_has_no_child_trace_emission` and
`pending_attempt_cannot_carry_intents` stay. So do the atomicity sentinels
(`sentinel_allows_no_undeclared_crossing_from_inside_an_attempt`).

### 8. Amendments

This commit adds a dated "Amendment (FIG-3562, 2026-09-29)" note to
[ADR 0042](0042-tool-attempts-are-atomic.md),
[ADR 0051](0051-the-facade-is-the-host-api-core-is-integrator-seams.md),
[ADR 0065](0065-concurrent-settlement-is-a-durable-group-at-the-effect-host-seam.md),
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md),
[ADR 0105](0105-the-drive-is-deterministic-workflow-code.md),
[ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md),
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md) and
[ADR 0114](0114-a-stopped-turns-partial-output-is-sealed-durably-and-returned-to-the-host.md).
Four more ADRs describe the replaced system in passing and get the same note:
[ADR 0059](0059-before-tool-call-directives-compose-monotonically.md),
[ADR 0074](0074-generation-intent-is-session-policy-and-its-fate-is-reported.md),
[ADR 0120](0120-tool-presentation-is-a-recorded-composable-step.md) and
[ADR 0103](0103-code-cells-replay-by-re-execution-on-every-host.md).

### 9. What is not changed

- `ProcessInput::SessionTurn` stays a process-engine input with ADR 0108
  ancestry and lifetimes. `ProcessInput::ToolCall` stays and runs an ordinary
  recorded attempt.
- RLM owns durable composition in the language: `processes.start`,
  `processes.await`, `Promise.all`, `Promise.race`.
- Intent realization, its identities and its host ingress
  (`ToolIntentIngress`) are unchanged.
- Hosts never drive (D5). A host that wants its own subagent tool writes an
  ordinary provider that returns a declared start.

### 10. Lanes and file ownership

#### 10.1 Shape

Four lanes, each staffed by one Opus worker. Each lands directly on `main`
in commits that keep the workspace compiling and `kiln clippy` green. There
is no integration branch.

```text
B (batch sugar) ──┐
                  ├──> D (deletion cutover) ──> W (final sweep)
S (declared start ┘
   and spawn)
```

There is no C0. B and S write disjoint files. They share only append-only
export lines (§10.4), which rebase trivially. Each migrates one consumer off
`OrchestrationContext` (B the standard batch, S `spawn_agent`) and deletes
that consumer's orchestrating body in the same commit. Between the later of
B and S and the end of D, the orchestrating machinery on `main` has no
production consumer, only its own tests. D removes it, so the window is one
lane long.

Edge kinds: **HARD** means the lane lands nothing until the other lane has
landed. **SOFT** means the lanes run in parallel, and the named part
rebases or waits.

#### 10.2 Lanes

**B — batch sugar.** Owns: `crates/lash-protocol-standard/**`;
`crates/lash-sansio/src/sansio/{turn_protocol.rs,turn_machine.rs}` and the
sansio tests of the machine's fold; `crates/lash-core/src/runtime/turn_driver/tools.rs`;
`crates/lash-conformance/src/conformance/tool_batch_parallelism.rs`; new
`crates/lash-conformance/src/conformance/batch_sugar.rs`;
`crates/lash-protocol-rlm/tests/tool_batch_parallelism.rs`;
`crates/lash-restate/src/tests/tool_batch_parallelism_on_the_double.rs`;
`crates/lash-sim/src/request_snapshot.rs`. Regions: §10.4.

- Scope: §2 in full, §7.1 and §7.2. Delete the standard orchestrating batch,
  the relay producer and `reaches_relay`. Register the barrier laws live.
- Done-when:
  1. `kiln test //crates/lash-protocol-standard:all //crates/lash-protocol-rlm:tool_batch_parallelism__test //crates/lash-restate:lash-restate__unit_test //crates/lash-sansio:all`
     passes, including every §7.1 and §7.2 law on the in-process and double
     tiers, and `forced_serial_host_fails_the_barrier` fails its inner law as
     required.
  2. `just effect-group-conformance-e2e` passes five consecutive runs with
     the §7.1 and §7.2 live registrations.
  3. `kiln clippy` is green.
  4. The §6 grep restricted to B's owned paths prints nothing.
- Edges: S SOFT (disjoint; export lines rebase). `fig-1628/cutover` SOFT: its
  runtime lane globs `crates/lash-protocol-standard/**`, `crates/lash-core/src/**`
  and `crates/lash-protocol-rlm/**`, and B's files are carved out; the branch
  rebases over B. `fig-433/stopped-partials` SOFT: B does not touch
  `turn_driver/{streaming.rs,effects.rs,local_effects.rs}` or
  `session/tool_execution/**`. `fig-4031/referrers` SOFT: B does not edit
  `group.rs`.

**S — declared start and spawn_agent.** Owns: `crates/lash-subagents/**`;
`crates/lash-core-execution/src/tool_result.rs`;
`crates/lash-core-execution/src/tool_dispatch/{pending_resolver.rs,attempt_coordinator.rs}`;
`crates/lash-core/src/runtime/session_manager/process_runners/session.rs` and
`process_runners/session/**`; new `crates/lash-conformance/src/conformance/declared_start.rs`.
Regions: §10.4 (the child driver's pending arm, `model.rs`'s `SessionTurn`,
the router's fingerprint arm, `intent_executor.rs`'s launch entry, `group.rs`'s
completion support, the registry's hold and release in the SQLite and
PostgreSQL process registries, `envelope.rs`'s seal and cancel-work keys).

- Scope: §3, §4, §7.3 and the §7.4 fixtures `pending_start_cannot_carry_intents`
  and `declared_start_is_sealed`. Delete `spawn_agent`'s orchestrating body
  and `child_task_result`. Replace `output_contract` with `result` at every
  construction site.
- Done-when:
  1. `kiln test //crates/lash-subagents:all //crates/lash-core:all //crates/lash-core-execution:all //crates/lash-sqlite-store:all //crates/lash-restate:lash-restate__unit_test //crates/lash:ui_fixtures`
     passes, including every §7.3 law on the in-process and double tiers.
  2. `just effect-group-conformance-e2e` passes five consecutive runs with
     the §7.3 live registrations.
  3. `kiln clippy` is green.
  4. The §6 grep restricted to S's owned paths prints nothing.
- Edges: B SOFT; `batch_of_spawns_overlaps`'s width-64 case lands after B.
  `fig-4031/referrers` SOFT: ADR 0113's lane R owns `process/model.rs` and
  its lane X owns `triggers/**`, `intent_executor.rs`, `group.rs` and the
  process registries. S writes only its named regions there, and the branch
  rebases over them. `fig-1628/cutover` SOFT: the branch rebases.
  `fig-433/stopped-partials` SOFT.

**D — deletion cutover.** Owns: `crates/lash-core-execution/src/tool_provider/**`
(except ADR 0114's `progress` region of `tool_provider.rs`, §10.4, since
deleted by ADR 0122, and the `enqueue_wake_delivery` region of
`process_events.rs`);
`crates/lash-core-execution/src/tool_registry/**`;
`crates/lash-core-execution/src/plugin/{registry.rs,registrar.rs,runtime_impl.rs}`;
`crates/lash-core-execution/src/tool_dispatch.rs`;
`crates/lash-core-execution/src/tool_dispatch/{context.rs,execution.rs,preparation.rs}`;
`crates/lash-core-execution/src/runtime/effect/{tool_child_driver.rs,tool_child_driver/rebuild_tests.rs,tool_child.rs,tool_settlement.rs}`;
`crates/lash-core-execution/src/session/process_handles.rs`;
`crates/lash-core-execution/src/testing/{attempt_sentinel.rs,kernel_internals.rs,test_protocol_fakes.rs,execution_context_builder.rs}`;
`crates/lash-core-execution/tests/store_backed/kernel/{tool_dispatch,tool_provider}/**`;
`crates/lash-core-execution/tests/store_backed/kernel/session/tool_execution/{batch.rs,presentation_redrive.rs}`;
`crates/lash-core-execution/tests/store_backed/kernel/runtime/effect/executor/process_local.rs`;
`crates/lash-core-store/src/{process_identity.rs,tool_state.rs}`;
`crates/lash-core/src/runtime/session_manager/process_runners/{tool.rs,tool/**,runner.rs}`;
`crates/lash-core/tests/runtime/tests/{child_sessions.rs,effect.rs}`;
`crates/lash-sansio/src/{tool_contract.rs,tool_catalog.rs}`;
`crates/lash-protocol-rlm/src/tool_catalog.rs`;
`crates/lash-conformance/src/conformance/tool_child_invocation/**`;
`crates/lash-conformance/src/conformance/{cell_orchestration_redrive.rs,migrated_tools_redrive.rs,served_process_start.rs}`;
`crates/lash-conformance/src/macros/tool_child.rs`;
`crates/lash-restate-test/tests/tool_child_drift.rs`;
`crates/lash/tests/ui.rs` and `crates/lash/tests/ui/**`;
`scripts/{lint_orchestrating_tools.py,test_lint_orchestrating_tools.py}`.
Regions: §10.4.

- Scope: §1.4, §1.5, §5 (everything B and S did not already delete), and the
  §7.4 fixtures `tool_context_is_not_nameable`,
  `attempt_context_has_no_process_administration`,
  `attempt_context_has_no_controller` and `plugin_spec_registers_only_providers`.
  It may land as several commits, each compiling, for example registry and
  dispatch, then the effect fallback, then facade, fixtures, lint and CI.
- Done-when:
  1. `kiln test //crates/lash-core-execution:all //crates/lash-core:all //crates/lash-core-store:all //crates/lash-conformance:all //crates/lash-restate:lash-restate__unit_test //crates/lash-restate-test:all //crates/lash-protocol-rlm:all //crates/lash-lashlang-runtime:all //crates/lash-remote-protocol:all //crates/lash:ui_fixtures //crates/lash:integration__test`
     passes.
  2. `kiln clippy` is green, with no `allow(dead_code)` added.
  3. The §6 grep restricted to D's owned paths and D's §10.4 regions prints
     nothing.
- Edges: B HARD (the standard batch must be off the orchestrating lane). S
  HARD (`spawn_agent` must be off it). `fig-433/stopped-partials` SOFT: ADR
  0114's lane R owns `tool_provider.rs` and puts `progress()` on
  `AttemptContext`. D makes `ToolContext` crate-private and keeps its
  `progress_reporter` field and `ToolProgressSink`; whichever lands second
  rebases. (ADR 0122 has since deleted both.) `fig-4031/referrers` SOFT: ADR 0113's lane R owns `control.rs` and
  `layered_host.rs`; D removes only the independent-work region. Its
  `tool_child_driver/tests.rs` edits are carried as a region.
  `fig-1628/cutover` SOFT: its branch touches `tool_provider.rs`,
  `tool_provider/process_events.rs` and five `crates/lash/tests/ui/` files,
  and rebases over D.

**W — final sweep.** Owns no file outright. It edits any file the §6 grep
still reaches, in the region the grep names, and the benign rewording in
§6 (the test262 harness).

- Scope: run the §6 grep over the whole tree and remove every remaining
  trace. Remove dead code, unused dependencies (`cargo machete` or the
  repo's equivalent over the touched crates) and orphaned BUILD targets and
  fixture list entries. Regenerate the BUILD files of the crates it touches.
  Confirm the amendment check in §6 still prints nothing.
- Done-when:
  1. The §6 grep and the §6 amendment check print nothing.
  2. `kiln clippy` is green, with no `allow(dead_code)` or
     `expect(dead_code)` added by this arc anywhere.
  3. No crate touched by this arc declares a dependency it does not use, and
     no BUILD target or fixture list entry names a deleted file.
- Edges: B, S and D HARD.

#### 10.3 Carve-outs from the in-flight cutovers

ADR 0112's lanes glob whole crates (its §15). ADR 0113 §8 and ADR 0114 §8
name files and regions. The files §10.2 gives B, S and D outright are carved
out of those globs: the ADR 0112, 0113 and 0114 lanes do not edit them. The
integrations of `fig-1628/cutover`, `fig-4031/referrers` and
`fig-433/stopped-partials` rebase over `main` and resolve against this arc's
changes there. Files an in-flight cutover owns by name stay with it, and this
arc writes only the regions in §10.4.

#### 10.4 Shared files and regions

| Shared file | Owner | FIG-3562 lane and region |
|---|---|---|
| `crates/lash-core-execution/src/tool_provider.rs` | ADR 0114 lane R | D: everything except the `progress` accessor, `progress_reporter`, `ToolProgressReporter`, `ToolProgressSink` and `ProgressRefused`, which were 0114's and are deleted by ADR 0122 (FIG-4113) |
| `crates/lash-core-execution/src/tool_provider/process_events.rs` | ADR 0112 runtime | D: remove `ToolProcessEventClient`; `enqueue_wake_delivery` is untouched |
| `crates/lash-core-execution/src/session/tool_execution.rs` | ADR 0114 lane R | D: the orchestrating and internal branches and the activation filter |
| `crates/lash-core-execution/src/session/tool_execution/batch.rs` | ADR 0114 lane R | D: prose naming body callers |
| `crates/lash-core-execution/src/session/tool_execution/group.rs` | ADR 0113 lane X | S: pending completion support for `DeclaredStart` (`:398-431`) |
| `crates/lash-core-execution/src/runtime/process/model.rs` | ADR 0113 lane R | S: `SessionTurn`'s `output_contract` becomes `result`, and `SessionTurnResult` |
| `crates/lash-core-execution/src/triggers/router.rs` and `triggers/router/tests.rs` | ADR 0113 lane X | S: the `SessionTurn` fingerprint arm |
| `crates/lash-core-execution/src/runtime/process/testing/registration_refusals.rs`, `src/session.rs:158-175` | ADR 0112 runtime | S: the `output_contract` construction sites |
| `crates/lash-core-execution/src/tool_dispatch/intent_executor.rs` | ADR 0113 lane X | S: the declared-start launch entry beside `execute_final_tool_intents` |
| `crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs` | this arc (D) | S: `await_child_completion`'s arming and cancel paths (`:1510-1601`); D: the rest |
| `crates/lash-core-execution/src/runtime/effect/tool_child_driver/tests.rs` | ADR 0113 lane R edits it | D: remove the forwarder (`:1228-1233`) and the orchestrating cases |
| `crates/lash-core-execution/src/runtime/effect/envelope.rs` | ADR 0114 lane R (C0 region) | S: the `launch:seal` and `cancel-work` replay keys; D: prose at `:404` |
| `crates/lash-core-execution/src/runtime/effect/executor/control.rs`, `layered_host.rs` | ADR 0113 lane R | D: remove `drive_independent_effect_work` and `IndependentEffectWork` |
| `crates/lash-core-execution/src/runtime/effect/executor/control/scope.rs`, `executor.rs`, `mod.rs`, `runtime/mod.rs` | ADR 0112 runtime | D: prose and export lines |
| `crates/lash-core-execution/src/lib.rs`, `crates/lash-core/src/lib.rs`, `crates/lash-core/src/runtime/mod.rs` | ADR 0112 runtime | B, S, D: their export lines |
| `crates/lash-core/src/testing/runtime_helpers.rs` | ADR 0112 readers | D: orchestrating helpers |
| `crates/lash-sansio/src/lib.rs` | ADR 0114 lane R (C0 region) | B: plan type exports; D: the `ToolActivation` export |
| `crates/lash-sqlite-store/src/process_registry/**`, `crates/lash-postgres-store/src/postgres/process_registry/**`, their schemas | ADR 0113 lanes S and P; ADR 0112 SQLite and PostgreSQL | S: the consumer hold column, its prune check and its release statement |
| `crates/lash-lashlang-runtime/src/{replay_run.rs,replay_commands.rs,replay_run_tests.rs,cell_bindings.rs,lib.rs,lib_tests.rs}` | ADR 0113 lane X | D: the orchestrating row shape and prose; the `Internal` filter |
| `crates/lash-protocol-rlm/src/{driver.rs,control_tools.rs}`, `src/executor/state/tests.rs` | ADR 0112 runtime; ADR 0113 lane F (`executor/**`) | D: activation lines and `ToolContext` test constructors |
| `crates/lash-remote-protocol/src/{tools.rs,core_conversions/tools.rs}` | ADR 0112 runtime | D: the activation field |
| `crates/lash-plugin-process-controls/src/lib.rs` | none | D: the test helper at `:582-596` |
| `crates/lash-restate/src/controller/scope_recording.rs` | ADR 0112 runtime | D: remove the forwarder |
| `crates/lash-restate/src/tests/conformance_and_poison.rs` | ADR 0112 runtime | B and S: live registrations; D: the orchestration factory at `:317-339` |
| `crates/lash-restate/src/tests/{journal_cut_runner.rs,effect_group_conformance.rs,process_start_replay_on_the_double.rs,turn_laws_on_the_double.rs}`, `src/effect_group/dispatch.rs`, `src/tests.rs` | ADR 0112 runtime | D: orchestrating cases and prose; B and S: `mod` lines |
| `crates/lash-conformance/src/conformance/{mod.rs,registration_macro_support.rs,turn_runner.rs,segment_redrive.rs,turn_crash_matrix/seam_controllers.rs,process_registry/registration.rs}`, `src/macros.rs` | ADR 0112 readers (ADR 0113 lane K and ADR 0114 lane S hold macro blocks) | B and S: their `mod` lines and macro blocks; B: the group-capable runner; D: the orchestrating and independent-work cases and the start-key retarget |
| `crates/lash-sim/tests/possession_conservation.rs` | ADR 0112 readers | D |
| `crates/lash/src/lib.rs` | ADR 0112 runtime (ADR 0113 R and ADR 0114 H hold export regions) | S: `DeclaredStart` exports; D: the `lash::tools` removals |
| `crates/lash/src/tests/agent_scenarios/harness.rs`, `src/tests/turn_streaming.rs` | ADR 0112 runtime | B: the non-standard batch installer; D: orchestrating cases |
| `crates/lash/tests/integration/integrator_facade.rs`, `tests/triggers_evidence.rs` | ADR 0112 readers | D |
| `crates/lash/BUILD.bazel` | ADR 0112 runtime (ADR 0113 K and ADR 0114 H hold targets) | S and D: `ui_fixtures` entries |
| `.github/workflows/ci.yml`, `.pre-commit-config.yaml`, `scripts/push-gate.sh`, `scripts/test_confidence_gate_ci_contract.py` | none | D: the lint wiring |
| `runbooks/process-operations/runbook.md`, `examples/agent-workbench/README.md` | ADR 0112 readers | S: the spawn row; D: the `ToolContext` passage |

The final astra review checks the end state against this record, including
the §6 grep and a read for semantic traces the grep cannot see.

## Consequences

- A tool body is opaque host code with one context and three result modes.
  The unsafe seal, the body lint and the second and third execution classes
  are gone, and so is the class of bugs where a body's journal commands
  diverge on replay.
- Every tool call in a turn step, native or batched, is a child of one flat
  group, so the barrier law holds on live Restate with no serial fallback.
  FIG-3562's relay deadlock has no code path left to occur on.
- `batch` costs no invocation of its own. It gives up nested durable fan-out
  from host code, which no reference runtime offers, and batch calls from
  RLM cells and processes, which use `Promise.all`.
- `spawn_agent` keeps engine-owned durability, lifetimes, depth and
  cancellation, and gains overlap in a batch, a timeout and retention
  safety. Lash takes on the launch seal, the cancel obligation and the
  consumer hold. Hosts write none of it.
- `CancelHint::CancelExternalWork` now means what it says for runtime-owned
  resolvers. A cancelled or timed-out wait on a process cancels that
  process.
- The durable shapes that change in place are `PendingResolver`,
  `ProcessInput::SessionTurn`, the manifest (no `activation`), the persisted
  tool registration (no kind), the start-key namespaces, the process
  registration (a consumer hold) and the remote protocol's tool shape.
  In-flight pre-cutover executions are drained or reset, and fixtures are
  regenerated.
- A host that wants a subagent of its own writes an ordinary provider that
  returns a `DeclaredStart`. That is the whole extension surface.
