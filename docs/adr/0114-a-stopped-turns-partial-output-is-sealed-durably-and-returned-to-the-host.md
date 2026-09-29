# 0114: A stopped turn's partial output is sealed durably and returned to the host

## Status

Accepted 2026-09-29 (FIG-433). It pins the contract that four implementation
lanes build in parallel (§8). It stays consistent with the two contracts
landed just before it: [ADR 0112](0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md)
(the store cutover) and [ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md)
(artifact referrers). It honours the shared-file regions ADR 0113 §8 gives
FIG-433. Nothing below describes current behaviour
unless it cites today's code. The design it ratifies is
`/workspace/notes/lash/tasks/lanes/design-433.report.md`. The evidence behind
that design is the six-reference prospect in
`/workspace/notes/lash/tasks/prospect-433/`, as corrected by `verified-a.md`
and `verified-b.md`. Where today's code shows the design to be wrong, the
section says so.

Sam's rulings of 2026-09-29 on FIG-433 are binding here:

- A stopped turn's partial output is saved durably when the turn aborts.
- Turning it into new input is up to the host. Lash never feeds it back
  itself.
- Backtrack to the last checkpoint stays the default (FIG-408, D1). Doing
  nothing with the partial reproduces today's behaviour.
- Nothing truncated enters the conversation graph, `AssistantOutput` or
  `SessionHistoryRecord`.
- Hosts never drive a turn (D5).
- Under the version freeze (FIG-3846), shapes change in place.
- The change goes straight to the end state, with no shims.

This record supersedes the display-only `StoppedAssistantTailEvidence`
design of FIG-415. It keeps that design's two capture rules: capture happens
at the emitted-prose boundary, not in `LlmStreamAccumulator`, and it composes
with FIG-423's `AttemptReset` retraction. It places its store segment inside
[ADR 0112](0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md)'s
store shape (§3), and it refines
[ADR 0105](0105-the-drive-is-deterministic-workflow-code.md) §9 for one more
fenced store write, the capture seal.

## Context

Every citation below was read at fork HEAD `ba4c39a09a`.

**A stop discards what the host watched stream in.** An `Immediate` cancel
fires the cooperative token, and the uncommitted tail backtracks to the last
checkpoint (`docs/adr/0039-turn-cancellation-is-a-first-party-work-driver-primitive.md:131-135`).
An `AfterStep` stop waits for the iteration's checkpoint, and nothing
backtracks (`:140-149`). The cancellation finisher
(`crates/lash-core/src/runtime/turn_loop/commit.rs:892`) takes lash's own
evidence when no request reached the turn (`:922`). It then emits the
terminal sequence (`:923`) and commits through `finish_turn` (`:944`). The
admitted commit runs `final_commit` (`:183`), and adoption follows (`:268`).
Nothing on that path keeps the streamed prose.

**The terminal is published before the commit.** `emit_terminal_sequence`
(`crates/lash-core/src/runtime/turn_loop.rs:457`) records each terminal event
on the turn's recorded assembly and also publishes it to the observer. That
happens at `commit.rs:923`, before `finish_turn` at `:944`. The observer's
hand-off never waits (`crates/lash-core/src/runtime/turn_observer.rs:1-8`,
`:150`). Only the waiter's "finished" signal waits for the commit
(`:27-32`, `:198`). A published cancellation also discards queued deltas
beyond the lag budget (`:22-26`, `:185`, `:480`). A lagging host therefore
never sees some of the prose the turn produced.

**Hosts see only a live, bounded stream.** `SessionStreamEvent`
(`crates/lash-sansio/src/session_model/mod.rs:439`) carries `text_delta`,
`reasoning_delta`, `stream_block_started` and `stream_block_completed`, plus
`tool_call_start` and `tool_call`, both with parsed arguments. Nothing in it
marks a cut inside tool arguments. The live replay buffer trims by count and
age (`crates/lash-core/src/runtime/observation/replay.rs:891`), and a cursor
behind it reads `Trimmed` (`:923`). That buffer cannot hold a durable
partial.

**The semantic accumulator is the wrong capture point.**
`fold_llm_stream_event` (`crates/lash-core/src/runtime/assembly.rs:616`)
folds provider events into `LlmStreamAccumulator` (`:20`). Block completion
overwrites the block's text (`:504`), reasoning items are consolidated
(`:543`), and `AttemptReset` clears everything (`:622-625`). What hosts see
is what the forwarder sends after plugin transforms. Deltas pass through
`transform_assistant_stream_chunk`
(`crates/lash-core/src/runtime/turn_driver/streaming.rs:975`), and a block
end seals with the post-transform text (`:1116-1181`). Everything goes out
through `ProviderHostForwarder`
(`crates/lash-core/src/runtime/turn_driver/streaming/host_forwarder.rs:43`).
`AttemptReset` retracts the attempt's correlations before the host sees
`ModelAttemptReset` (`streaming.rs:1067-1085`). The provider handle emits
that reset on every retry (`crates/lash-core-llm/src/provider/handle.rs:779`).

**No adapter surfaces argument deltas.** `LlmStreamEvent`
(`crates/lash-sansio/src/llm/types.rs:1256`) has no tool-input variant. A
tool call arrives only whole, as `Part(LlmOutputPart::ToolCall)` (`:307`),
which the stream loop folds silently (`streaming.rs:1291`). Each adapter
buffers arguments privately:

- Anthropic in `input_buffer` (`crates/lash-provider-anthropic/src/stream.rs:419`, `:56`);
- OpenAI Responses in `push_tool_call_delta` and `set_tool_call_arguments`
  (`crates/lash-provider-openai/src/responses_shared/sse.rs:140`, `:149`),
  which the Codex websocket path shares
  (`crates/lash-provider-openai/src/codex/streaming.rs:422`);
- OpenAI Chat in `update_tool_call_delta`
  (`crates/lash-provider-openai/src/chat.rs:739`, `:1001`).

Google receives function calls whole (`crates/lash-provider-google/src/stream.rs:150`,
`:550`), so it cannot be cut mid-argument. Today, completeness is a JSON
parse: the standard protocol parses `input_json` (`crates/lash-protocol-standard/src/lib.rs:521`).
It refuses unparseable arguments as `invalid_tool_call_json` (`:685`).

**Tools have no progress channel.** `ToolContext`
(`crates/lash-core-execution/src/tool_provider.rs:402`) has no sink for
partial output.

**Where the step bodies run.** Restate journals every LLM call as a recorded
run (`crates/lash-restate/src/controller/execution.rs:214`). The body runs
under `run_step_body`
(`crates/lash-core/src/runtime/turn_driver/local_effects.rs:73`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:350`). That
body watches the gate through the deployment resolver and fires the token
when a stop arrives (`local_stop.rs:424`). On Restate the watch attaches
through ingress (`crates/lash-restate/src/effect_host.rs:636`). The stream
loop aborts the provider task on cancel (`streaming.rs:338-341`). The
recorded LLM outcome is `RuntimeLlmCallOutcome`
(`crates/lash-core-execution/src/runtime/effect/envelope.rs:1073`). It
carries an `LlmStreamRecord` (`:1085`), so replay never reads worker memory.

**Checkpoints are recorded steps, not store commits.** A checkpoint runs as
an effect (`crates/lash-core/src/runtime/turn_driver/handlers.rs:266`,
`crates/lash-core/src/runtime/turn_driver/effects.rs:334`). Its body is
`run_checkpoint` (`effects.rs:481`). The turn writes the store once, at its
final commit, and that write is a fenced idempotent store write that is
re-executed on replay (ADR 0105 §9).

**A lost root already records cancel-then-crash.** The lost-root sweep
(`crates/lash-restate/src/session_control.rs:48`) calls `end_lost_root`. That
call writes `RootTerminalCause::SubstrateLost { cancelled_by }`
(`crates/lash-core-store/src/store/root.rs:128`) from any recorded cancel
request, in one transaction (`crates/lash-postgres-store/src/postgres/session_roots.rs:160`,
`crates/lash-sqlite-store/src/session_roots.rs:161`, `:201`). Operator
cancels and forks of a parked root end it as `OperatorCancelled` or `Forked`
(`root.rs:116-123`).

**The host surfaces.** `TurnReport` (`crates/lash/src/turn.rs:152`) projects
`AssembledTurn` (`crates/lash-core-execution/src/runtime/vocabulary.rs:83`)
field by field (`turn.rs:224`), and `to_remote` projects it again (`:261`).
`RemoteTurnReport` (`crates/lash-remote-protocol/src/turn_result.rs:20`) is
built by `from_core`
(`crates/lash-remote-protocol/src/core_conversions/turn_result.rs:5`). The
send handle answers `SendOutcome` (`crates/lash/src/send.rs:415`, `:656`), and
dropping the handle stops nothing (`:618`). A turn that ran elsewhere gets a
durable report (`crates/lash/src/send/follow.rs:748-756`, `:768`). That report
rebuilds only the outcome and an empty `AssistantOutput`. `DurableSession`
has settled reads (`crates/lash/src/durable_session.rs:429`). `TurnInput`
carries only text and attachments
(`crates/lash-core-store/src/turn_input_vocabulary.rs:1083`, `:1098`). The
workbench sends through `session.send` (`examples/agent-workbench/src/restate/turn_follow.rs:125`)
and stops a turn through `/api/turn/cancel`
(`examples/agent-workbench/src/main_sections/bootstrap.rs:514`,
`examples/agent-workbench/src/main_sections/turn_cancel.rs:30`).

**Retention.** Receipt reclamation requires a durably deleted session and a
host-chosen horizon (`crates/lash-core-store/src/store/retention.rs:1-16`). The
sweep is `reclaim_retained_evidence`
(`crates/lash-core-execution/src/runtime/vocabulary.rs:716`), implemented
at `crates/lash-sqlite-store/src/retention.rs:10` and
`crates/lash-postgres-store/src/postgres/evidence_retention.rs:14`. Session
deletion is `delete_session` (`vocabulary.rs:691`,
`crates/lash-sqlite-store/src/session_store_factory.rs:887`,
`crates/lash-postgres-store/src/postgres/session_factory.rs:132`).

### Where the design is corrected

- **Store binding.** The design names a partial by "store binding, session,
  physical turn, checkpoint boundary, and sealed capture sequence". ADR 0112
  deletes handle binding (§1.1: "Nothing is bound"), and a session id is
  unique in its catalog. The identity here drops the binding (§1.1).
- **Publication order.** The design says "Publish the terminal only after
  acceptance". Today the terminal is published before the commit
  (`commit.rs:923` before `:944`). This record changes that order for every
  `Stopped` terminal (§4.3), and says how the delta discard is kept.
- **Completeness.** "Successful argument validation" is the protocol's JSON
  parse (`crates/lash-protocol-standard/src/lib.rs:521`), not a schema check.
  Schema checks stay at dispatch.
- **ProcessLoss.** "Recovery settles an interrupted turn" has exactly one
  home: `end_lost_root`, whose `cancelled_by` already separates
  cancel-then-crash from process loss (§4.4).
- **Google** cannot be cut mid-argument (above). Its adapter emits the three
  tool-input events together.

## Decision

### 1. The returned value

#### 1.1 Types

The vocabulary lives in `lash-sansio`, which every surface below depends on.
`lash-sansio` already has `blake3`.

```rust
// crates/lash-sansio/src/stopped_partial.rs

/// A stopped physical turn's uncommitted tail, sealed and committed with the
/// turn. It is data returned to the caller. It never enters the graph,
/// `AssistantOutput` or `SessionHistoryRecord`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct StoppedPartial {
    pub id: StoppedPartialId,
    /// BLAKE3, domain `lash-stopped-partial/v1`, over the canonical JSON of
    /// every other field of this value.
    pub digest: StoppedPartialDigest,
    pub reason: StopReason,
    /// Set independently of `reason` (§1.3).
    pub recovered_after_process_loss: bool,
    pub coverage: CaptureCoverage,
    /// In order of each item's first capture frame.
    pub items: Vec<PartialItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct StoppedPartialId {
    pub session_id: SessionId,
    /// The logical root the stopped physical turn ran under.
    pub root: TurnId,
    /// The physical turn that stopped.
    pub turn_id: TurnId,
    /// Checkpoints the turn recorded before the tail began (§3.1).
    pub base: CaptureBase,
    /// The last store-assigned capture sequence the seal included.
    pub sealed_through: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CaptureBase(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct StoppedPartialDigest(pub [u8; 32]);

/// `"{invocation}/{attempt_epoch}/{kind}/{key}"`. `key` is the provider's
/// block id or call id, or `#{ordinal}` when the provider minted none.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct PartialItemId(pub String);

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PartialItem {
    /// Post-plugin assistant text: exactly what the forwarder sent.
    Text { id: PartialItemId, state: CutState, text: String },
    /// The visible reasoning summary. Opaque replay material (signatures,
    /// encrypted content, provider item state) is never captured.
    Reasoning { id: PartialItemId, state: CutState, summary: String },
    /// A complete call: the provider closed its arguments and they parsed.
    ToolCall { id: PartialItemId, call: CompleteToolCall, execution: ToolExecutionState },
    /// Arguments the provider never closed, or closed but unparseable.
    /// Never prefix-parsed into a call.
    ArgumentFragment {
        id: PartialItemId,
        call_id: Option<String>,
        tool_name: Option<String>,
        /// The exact argument text the provider streamed.
        raw_arguments: String,
        state: FragmentState,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CutState { Complete, Interrupted }

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FragmentState {
    /// The stop landed before the provider closed the arguments.
    Interrupted,
    /// The provider closed them, and the protocol's parse refused them.
    Invalid { parse_error: String },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CompleteToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// "Complete call" never means "tool succeeded".
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolExecutionState {
    /// No attempt started before the cutoff.
    NotStarted,
    /// An attempt started and did not settle before the cutoff.
    Running(RunningTool),
    /// The attempt settled before the cutoff. Its result is kept.
    Settled { output: crate::ToolCallOutput },
}

/// A tool the stop interrupted. Whatever it did outside lash is unknown, and
/// lash never re-runs it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RunningTool {
    pub output: ToolOutputCapture,
    pub outcome: InterruptedToolOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterruptedToolOutcome { OutcomeUnknown }

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "capture", rename_all = "snake_case")]
pub enum ToolOutputCapture {
    /// The tool has no progress channel. This is not empty output.
    Unavailable,
    /// The chunks it reported before the cutoff, in order. Past the per-call
    /// cap (1 MiB) chunks are counted in `omitted_bytes`, never silently lost.
    Captured { chunks: Vec<ToolOutputChunk>, omitted_bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ToolOutputChunk { pub text: String }

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    UserCancel,
    ProcessLoss,
    Other { cause: OtherStopCause },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OtherStopCause {
    ProviderFailure,
    PluginAbort,
    RuntimeFailure,
    OperatorCancellation,
    /// The turn's protocol ended it: `Incomplete`, `InvalidInput`,
    /// `MaxTurns`, `ToolFailure`, `ContextOverflow`, `SubmittedError`,
    /// `ToolError`. The exact stop stays on the report's `TurnOutcome`.
    ProtocolStop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureCoverage {
    /// Every frame the turn emitted before the seal is included.
    Complete,
    /// Recovery rebuilt the prefix a lost worker acknowledged. Output that
    /// worker produced after its last acknowledged batch is not promised.
    AcknowledgedPrefix,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "eligibility", rename_all = "snake_case")]
pub enum ResubmissionEligibility {
    /// Every item has a structurally valid default in `build_resubmission`.
    Ready,
    /// The host must choose, item by item, before resubmitting.
    NeedsSelection { reasons: Vec<SelectionReason> },
    /// No items.
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SelectionReason {
    ArgumentFragment { item: PartialItemId },
    Reasoning { item: PartialItemId },
    AcknowledgedPrefixOnly,
}

impl StoppedPartial {
    /// A fragment exists, or a call is `Running`.
    pub fn cut_mid_tool_call(&self) -> bool;
    /// A call is `Running`: its outcome is `OutcomeUnknown`.
    pub fn tool_outcome_unknown(&self) -> bool;
    pub fn eligibility(&self) -> ResubmissionEligibility;
    /// `true` only for `Ready`.
    pub fn safe_to_resubmit(&self) -> bool;
    pub fn summary(&self) -> StoppedPartialSummary;
    /// Recomputes the digest and compares it.
    pub fn verify_digest(&self) -> Result<(), StoppedPartialDigestMismatch>;
}

/// What the observation carries: identity and facts, never payload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct StoppedPartialSummary {
    pub id: StoppedPartialId,
    pub digest: StoppedPartialDigest,
    pub reason: StopReason,
    pub recovered_after_process_loss: bool,
    pub coverage: CaptureCoverage,
    pub eligibility: ResubmissionEligibility,
    pub cut_mid_tool_call: bool,
    pub tool_outcome_unknown: bool,
    pub item_count: u32,
}
```

`lash-sansio/src/lib.rs` re-exports these, and so does the facade. Nothing
in the value is truncated content passing as complete: interruption is typed
per item, and omission is counted.

#### 1.2 Semantics

**Completeness.** Two facts together make a call complete: the provider
closed the arguments (`ToolInputEnd`, §2.1), and the arguments parse as JSON
exactly as the protocol parses them (`crates/lash-protocol-standard/src/lib.rs:521`).
Parsing alone does not. An unclosed call is an `ArgumentFragment` with
`FragmentState::Interrupted`. A closed call that fails the parse is a
fragment with `Invalid`. A call the protocol refused before dispatch
(`:685`) is a `ToolCall` whose execution is `Settled` with the refusal
output.

**Identity.** Items carry the provider's block or call id. When the
provider minted none, the adapter's attempt-local ordinal takes its place
(§2.1). Invocation and attempt epoch are part of every id, so a retried
attempt cannot collide with the one it replaced.

**Safety.** "Safe" means the resubmitted input is structurally valid
ordinary input that keeps interruption and uncertainty visible. It promises
neither factual correctness nor side-effect rollback.

- Text, even mid-sentence, is safe as a labeled quotation, never as a
  completed assistant answer.
- Reasoning is never safe for native replay, even when its block completed:
  signatures, opaque state and dependent items may be missing. Quoting its
  visible summary as ordinary text is allowed, but only when the host
  selects it.
- A complete unanswered call, or a running tool, is safe only through the
  helper's explicit aborted-result data (§5.3).

**Eligibility** is a pure function of the items and the coverage:

- `Empty` when there are no items.
- Otherwise `NeedsSelection` when any of these holds, with one reason each:
  - a fragment exists (`ArgumentFragment`);
  - a reasoning item exists (`Reasoning`);
  - the coverage is `AcknowledgedPrefix` (`AcknowledgedPrefixOnly`).
- Otherwise `Ready`.

**Presence.** A partial exists for every admitted physical turn whose
terminal is `TurnOutcome::Stopped(_)`, and for every root that ends without
a turn commit while a physical turn is active (§4.4). It may be empty. An
`AfterStep` stop's partial is always empty, because the iteration's
checkpoint advanced the base first (§3.1). A turn that finishes, or that
switches frame, has none. A capture failure never passes for an empty
partial: a failed write stops the turn as an infrastructure fault (§4.1).

#### 1.3 `StopReason` and recovery evidence

| Terminal | `reason` | `recovered_after_process_loss` |
|---|---|---|
| `Stopped(Cancelled { .. })` committed by the turn (`crates/lash-sansio/src/session_model/mod.rs:619`) | `UserCancel` | `true` iff a successor fenced an earlier writer epoch of the turn (§3.2) |
| `Stopped(ProviderError)` | `Other { ProviderFailure }` | as above |
| `Stopped(PluginAbort)` | `Other { PluginAbort }` | as above |
| `Stopped(RuntimeError)` | `Other { RuntimeFailure }` | as above |
| every other `Stopped(_)` (`:622-641`) | `Other { ProtocolStop }` | as above |
| `SubstrateLost { cancelled_by: Some(_) }` (`crates/lash-core-store/src/store/root.rs:128`) | `UserCancel` | `true` |
| `SubstrateLost { cancelled_by: None }` | `ProcessLoss` | `true` |
| `OperatorCancelled` or `Forked` (`root.rs:116-123`) | `Other { OperatorCancellation }` | as the first row |

A recorded cancel request wins: cancel-then-crash stays `UserCancel`, with
recovery evidence beside it. Cancellation evidence stays where it is, on
`TurnOutcome::Stopped(Cancelled { evidence })` (`mod.rs:590-595`). The
partial does not copy it. A host disconnect is neither a cancellation nor a
process loss, because dropping a handle stops nothing
(`crates/lash/src/send.rs:618`).

#### 1.4 Where the value sits

- `AssembledTurn` (`crates/lash-core-execution/src/runtime/vocabulary.rs:83`)
  gains `pub stopped_partial: Option<StoppedPartial>`.
- `TurnReport` (`crates/lash/src/turn.rs:152`) gains the same field, beside
  `assistant_output`. `from_assembled` (`:224`) and `to_remote` (`:261`) carry
  it.
- `RemoteTurnReport` (`crates/lash-remote-protocol/src/turn_result.rs:20`)
  gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub
  stopped_partial: Option<lash_sansio::StoppedPartial>`. `from_core`
  (`core_conversions/turn_result.rs:5`) carries it, and `validate` checks the
  digest. The remote protocol reuses the `lash-sansio` type, which derives
  `JsonSchema`, rather than mirroring it.
- **Never inside `TurnOutcome`.** `TurnOutcome` is recorded into the committed
  turn (`crates/lash-core/src/runtime/turn_loop.rs:448-455`) and persisted in
  `RootTerminalCause::Committed { stop }` (`root.rs:111-115`). A field there
  would carry the partial into history.

The field is `Some` exactly when §1.2 says a partial exists. `AssistantOutput`
(`crates/lash-core-llm/src/turn_vocabulary.rs:69`) and `SessionHistoryRecord`
(`crates/lash-sansio/src/session_model/mod.rs:205`) do not change.

### 2. Provider stream events and the tool progress sink

#### 2.1 Tool-input events

```rust
// crates/lash-sansio/src/llm/types.rs, added to `LlmStreamEvent`
/// A tool call's arguments started streaming in this attempt.
ToolInputStart { call: ToolInputIdentity },
/// A suffix of the call's raw argument text, never cumulative.
ToolInputDelta { call: ToolInputIdentity, text: String },
/// The provider closed the arguments. `raw_arguments` is authoritative, like
/// `TextBlockEnd::text`.
ToolInputEnd { call: ToolInputIdentity, raw_arguments: String },

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolInputIdentity {
    /// Attempt-local, dense from 0, in order of first appearance. It is the
    /// identity when the provider has not yet sent a call id.
    pub ordinal: u64,
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
    pub item_id: Option<String>,
}
```

**The adapter law.** For each call in an attempt, an adapter emits exactly
one `ToolInputStart`, then zero or more `ToolInputDelta`, then one
`ToolInputEnd`, and then the existing `Part(LlmOutputPart::ToolCall)`. Every
event carries the same `ordinal`. `call_id` and `tool_name` are filled from
the first event that knows them. `Part(ToolCall).input_json` equals
`ToolInputEnd::raw_arguments`. A call cut by a stop or an error ends without
`ToolInputEnd`. `AttemptReset` restarts the ordinals.

| Adapter | Change |
|---|---|
| Anthropic (`crates/lash-provider-anthropic/src/stream.rs:332`, `:419`) | `Start` at `tool_use` block start, `Delta` per `input_json_delta`, `End` at block stop |
| OpenAI Responses and Codex (`crates/lash-provider-openai/src/responses_shared/sse.rs:140`, `:149`) | `Start` at `output_item.added` for a function call, `Delta` per `function_call_arguments.delta`, `End` at `.done` with its `arguments` |
| OpenAI Chat (`crates/lash-provider-openai/src/chat.rs:739`, `:1001`) | `Start` at an index's first delta, `Delta` per argument delta, `End` when the choice finishes |
| Google (`crates/lash-provider-google/src/stream.rs:550`) | `Start`, one `Delta` holding the whole arguments, and `End`, together, before the part |

The scripted providers of `lash-sim` (`crates/lash-sim/src/provider/transport.rs`,
`crates/lash-sim/src/provider_variation_matrix.rs`) follow the same law. The
transport conformance suite (`crates/lash-llm-transport/src/conformance.rs`)
gains the law as a case that every adapter runs.

**The accumulator ignores them.** `fold_llm_stream_event` (`assembly.rs:616`)
takes the three events as no-ops, so the semantic response is unchanged. The
stream loop (`streaming.rs:1060`) turns them into capture frames (§3.1) and
publishes nothing for them.

#### 2.2 The tool-output progress sink

```rust
// crates/lash-core-execution/src/tool_provider.rs
impl<'run> ToolContext<'run> {
    /// This call's progress sink. A tool that never calls `report` is
    /// captured as `ToolOutputCapture::Unavailable`.
    pub fn progress(&self) -> ToolProgressSink;
}

#[derive(Clone)]
pub struct ToolProgressSink { /* sealed */ }

impl ToolProgressSink {
    /// Persist one chunk before it is published (§4.1). The first call
    /// flips the capture from `Unavailable` to `Captured`. It waits under
    /// backpressure. After the fence it returns `ProgressRefused::Fenced`, and
    /// the tool should stop.
    pub async fn report(&self, chunk: ToolOutputChunk) -> Result<(), ProgressRefused>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProgressRefused {
    #[error("the turn's capture is fenced")]
    Fenced,
    #[error("capture persistence failed: {0}")]
    Store(String),
}
```

The runtime publishes each persisted chunk as a new activity,
`TurnEvent::ToolOutputProgress { call_id: String, chunk: ToolOutputChunk }`,
added to `TurnEvent` (`crates/lash-core-execution/src/runtime/vocabulary.rs:226`).
No shipped tool reports progress yet. Test tools in the acceptance suite do.

### 3. The capture store

#### 3.1 Frames and keys

One reducer consumes three kinds of fact: emitted prose, tool lifecycle facts
and argument fragments. Frames live in staging tables of their own, outside
graph nodes and history records.

```rust
// crates/lash-core-store/src/store/capture.rs

/// The capture key: which physical turn, which checkpoint base, which effect
/// invocation, which attempt epoch, which position.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureFrameKey {
    pub turn: TurnAddress,
    pub base: CaptureBase,
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    /// Store-assigned, dense and monotone per physical turn.
    pub sequence: u64,
}

/// The invocation's replay key (`RuntimeEffectInvocation::replay_key`).
/// Opaque to the store.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct CaptureInvocationKey(pub String);

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum CaptureFrame {
    TextStart { block: StreamBlockIdentity },
    TextDelta { block: StreamBlockIdentity, text: String },
    TextEnd { block: StreamBlockIdentity, text: String },
    ReasoningStart { block: StreamBlockIdentity },
    ReasoningDelta { block: StreamBlockIdentity, text: String },
    ReasoningEnd { block: StreamBlockIdentity, text: String },
    ToolInputStart { call: ToolInputIdentity },
    ToolInputDelta { call: ToolInputIdentity, text: String },
    ToolInputEnd { call: ToolInputIdentity, raw_arguments: String },
    /// Written by the stream loop after `ToolInputEnd`, from the protocol's
    /// parse.
    ToolCallParsed { call: ToolInputIdentity, call_id: String, tool_name: String, arguments: serde_json::Value },
    ToolCallUnparseable { call: ToolInputIdentity, parse_error: String },
    ToolExecutionStarted { call_id: String },
    ToolOutputProgress { call_id: String, chunk: ToolOutputChunk },
    ToolSettled { call_id: String, output: ToolCallOutput },
}
```

Each frame's content is exactly what the host was sent, or will be sent once
the frame is acknowledged. `TextDelta` and `TextEnd` are captured inside
`ProviderHostForwarder` (`crates/lash-core/src/runtime/turn_driver/streaming/host_forwarder.rs:43`)
from the post-transform chunk and the sealed text (`streaming.rs:975-1011`,
`:1146-1181`). Plugin reasoning blocks are captured the same way.

**The base.** `CaptureBase(n)` is the number of checkpoints the physical
turn has recorded. `run_checkpoint` (`effects.rs:481`) advances the base
inside its step body, before it returns. A recorded checkpoint outcome
therefore implies a durable advance. A replay that re-runs the body repeats
the advance idempotently. The advance deletes the turn's frames under the
old base, so committed content is never part of the tail, and staging stays
bounded by one iteration.

**The reducer** is a pure function in `lash-core-store`. Stores and the
runtime share it:

```rust
// crates/lash-core-store/src/capture/reducer.rs
pub fn reduce_capture(
    id: StoppedPartialId,
    reason: StopReason,
    recovered_after_process_loss: bool,
    coverage: CaptureCoverage,
    frames: &[(CaptureFrameKey, CaptureFrame)],
    retracted: &BTreeSet<(CaptureInvocationKey, u32)>,
) -> Result<StoppedPartial, CaptureReduceViolation>;
```

It keeps frames whose base equals `id.base`, whose sequence is at most
`id.sealed_through`, and whose `(invocation, attempt_epoch)` is not
retracted. It folds them in sequence order. An end without a start, or a
settle before a start, is a `CaptureReduceViolation`, never a smaller
partial. Tool frames attach to their call by `call_id`. A call id with no
provider call item in the tail belongs to a call committed at an earlier
checkpoint and is dropped. Such a call cannot be running, because a
checkpoint follows its tools.

#### 3.2 The segment

Under ADR 0112, `TurnCaptureStore` is the eleventh segment of the
`RuntimeStore` alias (§1 there). It is placed there, not on `DeploymentStore`,
because every operation is session-scoped. Every operation takes a request
that carries its `TurnAddress`
(`crates/lash-core-store/src/turn_control_vocabulary.rs:19`), and so its
session. None has a default. Every operation goes on the decorator operation
list (`crates/lash-core-store/src/store/runtime_persistence_decorator.rs:30`),
so the `SessionStore` view gets forwarders that compare session ids
(ADR 0112 §3).

**Consistency with ADR 0113.** ADR 0113 adds no `RuntimeStore` segment. Its
artifact ports stay separate traits (§2.1 there). Capture frames and sealed
partials are session rows, not artifacts: they are stored inline in the
tables of §3.5, and no artifact referrer edge or fence names them. Their
lifetime is the turn's, and they are cleaned up as §5.1 says, never by
ADR 0113's cleanup executor. A partial that quotes a tool output holding an
artifact reference holds only the reference text. It acquires no edge, so it
keeps no bytes alive.

```rust
// crates/lash-core-store/src/store/capture.rs
#[async_trait::async_trait]
pub trait TurnCaptureStore: Send + Sync {
    /// Starts, or restarts after a worker loss, the writer of one invocation.
    /// Mints `attempt_epoch = previous + 1` and fences every earlier epoch of
    /// that invocation. The lease names the base current at open. When an
    /// earlier epoch had acknowledged frames and was never retracted, the
    /// turn is marked recovered, and the lease reports the prefix it inherits.
    async fn open_capture_writer(
        &self,
        request: &OpenCaptureWriter,
    ) -> Result<CaptureWriterLease, StoreError>;

    /// Appends one bounded batch under the writer's lease, in one
    /// transaction. It assigns sequences. It is idempotent by
    /// `(turn, invocation, attempt_epoch, batch_ordinal)`: an identical retry
    /// returns the first ack, and a different body under the same ordinal is
    /// `CaptureBatchConflict`.
    async fn append_capture_batch(&self, batch: &CaptureBatch) -> Result<CaptureAck, StoreError>;

    /// Retracts the lease's current epoch and mints the next one. The reducer
    /// then excludes every frame of the retracted epoch. Idempotent by
    /// `(turn, invocation, retracted_epoch)`.
    async fn persist_attempt_reset(
        &self,
        reset: &CaptureAttemptReset,
    ) -> Result<CaptureWriterLease, StoreError>;

    /// Moves the turn's base to `to` and deletes its frames under `to`.
    /// Idempotent at the current base. Any other value is
    /// `CaptureBaseStale`.
    async fn advance_capture_base(&self, advance: &CaptureBaseAdvance) -> Result<(), StoreError>;

    /// In one transaction: fence every writer of the turn, seal the cutoff at
    /// the highest acknowledged sequence, and materialize the partial with
    /// `reduce_capture`. First writer wins: a second call returns the
    /// existing seal whatever its request says, so a crash after sealing
    /// reuses it. Returns the committed partial once the turn has committed.
    async fn seal_turn_capture(&self, request: &SealTurnCapture) -> Result<SealedCapture, StoreError>;

    /// The authorized read (§5.2). It answers only for turns the session
    /// itself owns. A fork's ancestor turns are `Unknown`.
    async fn read_stopped_partial(
        &self,
        request: &StoppedPartialReadRequest,
    ) -> Result<StoppedPartialRead, StoreError>;
}

pub struct OpenCaptureWriter {
    pub turn: TurnAddress,
    pub root: TurnId,
    pub invocation: CaptureInvocationKey,
}

pub struct CaptureWriterLease {
    pub turn: TurnAddress,
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    pub base: CaptureBase,
    /// Frames acknowledged under earlier epochs of this invocation and not
    /// yet retracted: the prefix a successor rebuilds.
    pub inherited: Vec<(CaptureFrameKey, CaptureFrame)>,
}

pub struct CaptureBatch {
    pub lease: CaptureWriterLeaseRef,
    pub batch_ordinal: u64,
    /// At most `CAPTURE_BATCH_MAX_FRAMES` frames and
    /// `CAPTURE_BATCH_MAX_BYTES` of encoded frames.
    pub frames: Vec<CaptureFrame>,
}

pub const CAPTURE_BATCH_MAX_FRAMES: usize = 256;
pub const CAPTURE_BATCH_MAX_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureWriterLeaseRef {
    pub turn: TurnAddress,
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    pub base: CaptureBase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureAck {
    pub first_sequence: u64,
    pub last_sequence: u64,
}

pub struct CaptureAttemptReset {
    pub lease: CaptureWriterLeaseRef,
}

pub struct CaptureBaseAdvance {
    pub turn: TurnAddress,
    pub to: CaptureBase,
}

pub struct SealTurnCapture {
    pub turn: TurnAddress,
    pub root: TurnId,
    pub reason: StopReason,
    /// The highest `CaptureAck::last_sequence` the drive's recorded outcomes
    /// reference (§4.2). The seal must cover it, or the call fails
    /// `CaptureSealBelowWatermark`.
    pub recorded_watermark: Option<u64>,
}

pub enum SealedCapture {
    Sealed(StoppedPartial),
    Committed(StoppedPartial),
}

pub struct StoppedPartialReadRequest {
    pub session_id: SessionId,
    /// The physical turn id or the root id. Both are unique in a session,
    /// and a root has at most one stopped physical turn: only its last turn
    /// can stop.
    pub turn: TurnId,
}

pub enum StoppedPartialRead {
    Available(StoppedPartial),
    /// The turn is still running, or it is sealed but not yet committed.
    Pending,
    /// The turn settled without a stop.
    NotStopped,
    /// The session owns no such turn.
    Unknown,
}
```

#### 3.3 Materialize and commit

The commit is not a separate trait method, because it must share the
transaction of the receipt it belongs to.

```rust
// crates/lash-core-store/src/store/runtime_commit.rs, added to `RuntimeCommit`
/// The sealed partial this commit publishes. The backend checks that its
/// seal row exists with this id and digest, marks it committed, and deletes
/// the turn's staging frames and writer rows, all in the commit's
/// transaction. A commit of a turn that did not stop carries `None`. Its
/// transaction still deletes the turn's staging, because the commit
/// witnesses settlement.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub stopped_partial: Option<StoppedPartialCommit>,

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoppedPartialCommit {
    pub id: StoppedPartialId,
    pub digest: StoppedPartialDigest,
}
```

`RuntimeCommit` also gains ADR 0113's `frame_transition` (§8 there). The two
blocks are independent. In the commit transaction, the partial-commit block
runs after the frame-transition block and before the receipt insert.

The commit identity covers `StoppedPartialCommit`, which holds a reference
and no payload. A replay after a lost reply is adjudicated by that identity
(ADR 0105 §9). When a committed partial row already has the same id and
digest, the commit answers the existing receipt. A different digest fails
`StoppedPartialConflict`. `end_lost_root` and the operator root-terminal
writes run the same fence, seal, materialize and commit inside their own
transactions (§4.4).

#### 3.4 Typed errors

`StoreError` (`crates/lash-core-store/src/store/error.rs:6`) gains these
variants in place.

```rust
CaptureWriterFenced { session_id: SessionId, turn_id: TurnId, invocation: String, attempt_epoch: u32, current_epoch: u32 },
CaptureSealed { session_id: SessionId, turn_id: TurnId, sealed_through: u64 },
CaptureBatchTooLarge { frames: usize, bytes: u64, max_frames: usize, max_bytes: u64 },
CaptureBatchConflict { session_id: SessionId, turn_id: TurnId, invocation: String, attempt_epoch: u32, batch_ordinal: u64 },
CaptureBaseStale { session_id: SessionId, turn_id: TurnId, offered: u32, current: u32 },
CaptureSealBelowWatermark { session_id: SessionId, turn_id: TurnId, sealed_through: u64, recorded: u64 },
CaptureCorrupt { session_id: SessionId, turn_id: TurnId, violation: CaptureReduceViolation },
StoppedPartialNotSealed { session_id: SessionId, turn_id: TurnId },
StoppedPartialConflict { session_id: SessionId, turn_id: TurnId, existing: StoppedPartialDigest, offered: StoppedPartialDigest },
```

```rust
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaptureReduceViolation {
    DeltaWithoutStart { sequence: u64 },
    EndWithoutStart { sequence: u64 },
    DuplicateEnd { sequence: u64 },
    ToolFrameWithoutStart { sequence: u64, call_id: String },
    SequenceGap { after: u64, found: u64 },
}
```

Every capture operation on a deleted session answers `SessionDeleted`. A
request whose session differs from the view's answers
`ForeignSessionRequest` (ADR 0112 §3).

#### 3.5 Stored shapes

Per [ADR 0098](0098-one-owner-per-sql-table-across-both-stores.md), the
column lists and statements live in `lash-store-sql`. Each table is new, on
both backends:

- `turn_capture_turns (session_id, turn_id, root, base, next_sequence,
  recovered)`, with primary key `(session_id, turn_id)`.
- `turn_capture_writers (session_id, turn_id, invocation, attempt_epoch,
  state)`, with primary key `(session_id, turn_id, invocation,
  attempt_epoch)`. `state` is `live`, `retracted` or `fenced`.
- `turn_capture_frames (session_id, turn_id, sequence, base, invocation,
  attempt_epoch, batch_ordinal, frame_json)`, with primary key
  `(session_id, turn_id, sequence)`, and unique on `(session_id, turn_id,
  invocation, attempt_epoch, batch_ordinal, sequence)`.
- `stopped_partials (session_id, turn_id, root, base, sealed_through,
  reason, recovered, digest, partial_json, body_bytes, sealed_at_ms,
  committed_at_ms)`, with primary key `(session_id, turn_id)` and unique on
  `(session_id, root)`. `committed_at_ms` is null while the partial is sealed
  but uncommitted.

Under the freeze, a catalog without these tables fails the PostgreSQL
open-time shape check, or SQLite's first capture query, and is recreated.
Nothing is migrated.

### 4. Ordering

#### 4.1 A batch persists before its observations publish

The capture writer sits in `ProviderHostForwarder` and in the tool step body.
Every host observation that has a capture frame is held until the batch
holding that frame is acknowledged, and then released in order.

- At most one append is in flight per writer. Frames that arrive during it
  form the next batch. No timer is involved.
- The writer holds at most 4 MiB of unacknowledged frames. When it is full,
  the stream loop stops polling the provider task until an ack arrives, and
  a tool's `report` waits. Nothing is evicted.
- A failed append stops publication. It is retried as an infrastructure
  fault: the step ends retryably, the same way as ADR 0105's
  `TransientCancelWatch`. It never passes as a stop or an empty partial.
- On a normal stop, the stream loop flushes the frames it holds before it
  returns (`streaming.rs:338-341`). The partial therefore covers everything
  published, and may cover prose that a lagging host never received.

Publication now waits for a store flush. That is the price of a partial that
survives abrupt worker death. Saving in the abort handler cannot survive it.

#### 4.2 A reset persists before its retraction publishes

On `LlmStreamEvent::AttemptReset` (`streaming.rs:1067`), the writer first
flushes its pending batch, then calls `persist_attempt_reset`, and only then
sends `TurnEvent::ModelAttemptReset`. A recovered reducer can never include
the retracted prose. The LLM step's recorded outcome gains a reference, never
payload:

```rust
// crates/lash-core-execution/src/runtime/effect/envelope.rs:1073, added
#[serde(default, skip_serializing_if = "Option::is_none")]
pub capture: Option<CaptureWatermark>,

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureWatermark {
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    /// The last acknowledged sequence when the step body returned.
    pub acknowledged_through: u64,
}
```

The tool attempt's recorded outcome gains the same field. The Restate journal
holds these references and never a frame or a partial.

#### 4.3 A stop runs fence, seal, materialize, commit, publish

For every `Stopped` terminal, the finisher (`commit.rs:892`, and the failure
finisher that emits at `:1019`) runs these steps in order.

1. **Fence.** Call `seal_turn_capture` with `recorded_watermark` set to the
   maximum `acknowledged_through` over the turn's recorded step outcomes. In
   one transaction the store fences every writer, seals, and materializes
   the partial. The seal is a fenced idempotent store write, re-executed on
   replay like the commit (ADR 0105 §9), and it is first-writer-wins, so
   replay is deterministic. The finisher also calls
   `observer.discard_lagging_deltas()` (`turn_observer.rs:185`) at this
   point. That keeps today's "a cancelled turn stops streaming", since the
   `Stopped` event, whose publication used to trigger the discard, now
   publishes later.
2. **Seal and materialize** happen in that same call. Later writes fail
   `CaptureWriterFenced` or `CaptureSealed`. A tool that settles after the
   cutoff cannot rewrite the partial or claim a rollback.
3. **Assemble.** The partial is set on `AssembledTurn`. The terminal events
   are recorded on the recorded assembly, but not yet published.
4. **Commit.** `RuntimeCommit.stopped_partial` names the seal. The partial,
   the turn receipt (with its cancellation evidence) and the deletion of
   staging commit in one transaction (§3.3).
5. **Publish.** Only after the commit is accepted does the finisher publish,
   in this order: the optional `Error`, then `TurnOutcome { Stopped }`, then
   `StoppedPartialAvailable`, then `Done`. `emit_terminal_sequence`
   (`turn_loop.rs:457`) splits into a recording half, called before the
   commit, and a publishing half, called after it.
   `StoppedPartialAvailable` is never recorded on the recorded assembly, so
   it cannot reach history.

**Idempotence.** Finalization is keyed by `StoppedPartialId` and checked by
digest at every step. A second seal returns the first. A commit replay
compares the digest. A duplicate publication after a replay carries the same
id and digest, and hosts dedupe on the id.

#### 4.4 Worker loss

- **A redrive that continues the turn.** When a worker dies inside a step
  body the journal did not record, Restate re-runs the body on a successor.
  The successor's `open_capture_writer` fences the old epoch and returns the
  inherited prefix. The provider call restarts, so the successor calls
  `persist_attempt_reset` on the inherited epoch before it forwards anything
  new. That is today's recovery (FIG-423) made durable. No cancelled turn is
  fabricated, and the turn is marked recovered.
- **Cancel-then-crash through a redrive.** The replayed drive finds the
  recorded cancel. It finishes as §4.3 with `UserCancel` and
  `recovered_after_process_loss = true`.
- **A lost root.** The engine proves the run failed, and `end_lost_root`
  (`session_roots.rs:160` and `:161` on the two backends) writes the
  terminal. In that same transaction it fences, seals, materializes and
  commits the partial of the root's active physical turn. The reason follows
  §1.3: `ProcessLoss`, or `UserCancel` when `cancelled_by` is `Some`. The
  coverage is `AcknowledgedPrefix`. This is the only place `ProcessLoss` is
  sealed. `RootTerminal` gains `stopped_partial:
  Option<StoppedPartialSummary>`. The lost-root pass
  (`crates/lash-restate/src/session_control.rs:48`) publishes
  `StoppedPartialAvailable` through the core's Live Replay publisher after
  the write. That publication is best-effort, and the read of §5.2 is the
  fallback.
- **Seal reuse.** A crash after the seal and before the commit leaves a
  sealed, uncommitted row. The redriven finisher's seal returns it
  unchanged.
- **Operator cancel or fork** of a parked root writes `OperatorCancelled`
  or `Forked`, which run the same sequence with `Other {
  OperatorCancellation }`.
- **Hosts drive none of this.** The engine's successor, the lost-root pass
  and the operator's control intents are the only actors.

### 5. Retention and the host surface

#### 5.1 Retention and cleanup

- **Sealed partials live as long as their turn receipt.** The retention sweep
  (`crates/lash-sqlite-store/src/retention.rs:10`,
  `crates/lash-postgres-store/src/postgres/evidence_retention.rs:14`) removes
  a `stopped_partials` row under the receipt predicate
  (`crates/lash-core-store/src/store/retention.rs:5-8`): its session is
  durably deleted, and `sealed_at_ms` is before the host's horizon. The row
  goes in the same transaction as the receipts. `RetentionReport` (`:16`)
  gains `removed_stopped_partial_count`, which `reclaimed_count` includes.
  Ordinary vacuum never touches these rows.
- **Staging survives** disconnects, lease loss and parks. Four events
  delete it: a checkpoint advance (frames under the new base only), a commit
  of the turn (§3.3), a root-terminal write, and session deletion.
  Session deletion (`crates/lash-sqlite-store/src/session_deletion.rs`,
  `delete_session_tx` at
  `crates/lash-postgres-store/src/postgres/session_factory.rs:1063`) deletes
  the session's staging rows in its transaction, beside ADR 0113's frame
  fence and cleanup. `SessionBlobReclaimReport`
  (`crates/lash-core-store/src/store/maintenance.rs:69`) gains
  `removed_capture_frame_count`. A deleted session's sealed partials stay
  until the retention sweep. Every read of them answers `SessionDeleted`.
- **Forks do not inherit partials.** `fork_session` (today's `fork_at`,
  `crates/lash-sqlite-store/src/session_store_factory.rs:934`,
  `crates/lash-postgres-store/src/postgres/session_factory.rs:282`) copies no
  capture or partial row. `read_stopped_partial` matches the owning
  `session_id` only, never the ancestry ceilings of ADR 0112 §5.

#### 5.2 The host contract

**The send handle's report.** `SendOutcome.output.result.stopped_partial`
carries the hydrated value. The live path gets it from `AssembledTurn`. The
durable report (`crates/lash/src/send/follow.rs:768`) reads it with
`read_stopped_partial(root)`, and `ReportSource::Durable` now has a partial
where one exists. A root that ended through `end_lost_root` has no
`TurnReport`, so its host uses the read.

**The observation.**

```rust
// crates/lash-sansio/src/session_model/mod.rs, added to `SessionStreamEvent`
/// Published after the commit that made the partial durable. It holds
/// identity and facts, never payload.
#[serde(rename = "stopped_partial_available")]
StoppedPartialAvailable { summary: StoppedPartialSummary },
```

**The authorized read.** After a reconnect, or a `Trimmed` gap, the host
reads the partial:

```rust
// crates/lash/src/durable_session.rs
impl DurableSession {
    /// `turn` is the physical turn id or the root id. Authorization is the
    /// handle's session: the store answers only for turns that session owns.
    pub async fn stopped_partial(&self, turn: &TurnId) -> Result<StoppedPartialRead>;
}
// crates/lash/src/session.rs: `LashSession::stopped_partial` delegates to
// `self.durable()`.
```

**Doing nothing.** A host that ignores the partial gets today's behaviour
exactly. The graph, `AssistantOutput`, `SessionHistoryRecord` and the next
turn's context are byte-identical to a run without capture. Observation
changes in two places: the terminal publishes after the commit, and the new
event appears.

#### 5.3 `build_resubmission`

The helper is pure. It makes no model request and no store call.

```rust
// crates/lash/src/stopped_partial.rs (re-exported from the facade root)
pub fn build_resubmission(
    selection: &ResubmissionSelection<'_>,
    follow_up: TurnInput,
) -> Result<Resubmission, ResubmissionError>;

pub struct ResubmissionSelection<'p> {
    partial: &'p StoppedPartial,
    choices: BTreeMap<PartialItemId, ItemChoice>,
}

impl<'p> ResubmissionSelection<'p> {
    /// `Include` for text, calls and running tools. No choice for reasoning
    /// or fragments, which must be chosen explicitly.
    pub fn defaults(partial: &'p StoppedPartial) -> Self;
    pub fn choose(&mut self, item: &PartialItemId, choice: ItemChoice) -> &mut Self;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemChoice {
    /// Text, a complete call or a running tool.
    Include,
    /// A reasoning summary or a fragment's raw text, quoted and labeled.
    Quote,
    Omit,
}

pub struct Resubmission {
    /// Ordinary input: one labeled text item, then `follow_up`'s items.
    /// `trace_turn_id` and `turn_context` come from `follow_up`.
    pub input: TurnInput,
    pub omissions: OmissionReport,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OmissionReport {
    pub omitted: Vec<OmittedItem>,
    pub coverage: CaptureCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OmittedItem {
    pub item: PartialItemId,
    pub kind: PartialItemKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialItemKind { Text, Reasoning, ToolCall, ArgumentFragment }

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResubmissionError {
    #[error("items need an explicit choice: {items:?}")]
    SelectionIncomplete { items: Vec<PartialItemId> },
    #[error("{choice:?} is not allowed for {item:?}")]
    ChoiceNotAllowed { item: PartialItemId, choice: ItemChoice },
    #[error("no such item: {item:?}")]
    UnknownItem { item: PartialItemId },
    #[error("nothing to resubmit")]
    Empty,
}
```

**Allowed choices.** Text allows `Include` and `Omit`. A reasoning item
allows `Quote` and `Omit`. A fragment allows `Quote` and `Omit`, and `Quote`
renders it as invalid raw text. A `ToolCall` allows `Include` and `Omit`.
`Empty` means everything was omitted and `follow_up` has no items.

**The excerpt is fixed.** It is one `InputItem::Text`
(`turn_input_vocabulary.rs:1083`): the preamble below, a blank line, and
`serde_json::to_string_pretty` of the excerpt. The preamble is:

> The previous assistant turn was stopped before it finished. The JSON below
> quotes what it produced. It is not a completed answer: items marked
> interrupted were cut off, tool calls marked not_started never ran, and tool
> calls marked outcome_unknown may have partly run.

```rust
#[derive(serde::Serialize)]
struct ResubmittedExcerpt<'a> {
    stopped_turn: &'a TurnId,
    reason: &'a StopReason,
    coverage: CaptureCoverage,
    items: Vec<ExcerptItem<'a>>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExcerptItem<'a> {
    AssistantText { state: CutState, text: &'a str },
    QuotedReasoning { state: CutState, summary: &'a str },
    ToolCall { call_id: &'a str, tool_name: &'a str, arguments: &'a serde_json::Value, result: AbortedToolResult<'a> },
    QuotedInvalidArguments { call_id: Option<&'a str>, tool_name: Option<&'a str>, raw_arguments: &'a str },
}

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum AbortedToolResult<'a> {
    NotStarted,
    OutcomeUnknown { captured_output: &'a ToolOutputCapture },
    Settled { output: &'a ToolCallOutput },
}
```

Every complete call is paired with typed aborted-result data, and captured
output is kept. The helper never emits a tool-use message, a tool result
message or an assistant message. The host sends the result with
`session.send(resubmission.input)`, like any other input (D5).

#### 5.4 Agent-workbench flows

- `GET /api/turns/{turn_id}/stopped-partial` answers the read of §5.2. It
  also returns the summary, the default selection's rendered excerpt as a
  preview, and the omissions. It drives the panel both live, after
  `StoppedPartialAvailable`, and after a reconnect.
- **Discard** makes no request to lash and records nothing. The panel closes,
  and the next message is an ordinary send. That is the backtrack default.
- **Continue in context** posts the host's choices and a follow-up to `POST
  /api/turns/{turn_id}/stopped-partial/continue`. The route calls
  `build_resubmission` and sends through the path `/api/turn` uses
  (`examples/agent-workbench/src/restate/turn_follow.rs:125`). It refuses
  with the helper's typed error when choices are missing.
- The panel shows every omission and the eligibility reasons before the host
  confirms.

### 6. Acceptance tests

Each test names its target. In every test, the assertions show that the
stopped payload is absent from `AssistantOutput`, from every
`SessionHistoryRecord` and from the graph. They also show that a
resubmission produces only ordinary user input. Conformance cases live in
the new `crates/lash-conformance/src/conformance/turn_capture.rs`, and every backend
runs them: `//crates/lash-sqlite-store:conformance__test`,
`//crates/lash-sqlite-store:conformance_memory__test` and
`//crates/lash-postgres-store:conformance__test`. Runtime cases live in a new
module `crates/lash-core/tests/runtime/stopped_partial.rs`, run by
`//crates/lash-core:runtime_turns__test`.

1. **Empty cancellation** (`runtime_turns__test`). An `Immediate` cancel
   before any output, and an `AfterStep` cancel at a boundary, each yield
   `Some` with no items and `Empty`. The next turn's context is identical to
   a run with no capture.
2. **Text-only cut** (reducer in `//crates/lash-core-store:lash-core-store__unit_test`,
   runtime in `runtime_turns__test`, helper in `//crates/lash:lash__unit_test`).
   The cut keeps the exact post-plugin bytes as `Interrupted`, the partial is
   `Ready`, and the resubmission is the pinned excerpt golden.
3. **Reasoning-only cut** (same three targets). The partial is
   `NeedsSelection { Reasoning }`. `defaults` fails `SelectionIncomplete`, and
   `Quote` succeeds as `QuotedReasoning`. No opaque replay field appears
   anywhere in the value.
4. **Complete call plus text** (`runtime_turns__test`, `lash__unit_test`). An
   unanswered call is `NotStarted`, and the excerpt pairs it with `status:
   not_started`.
5. **Mid-arguments cut** on the fixture `{"path":"READ`: each adapter's unit
   target (`//crates/lash-provider-anthropic:lash-provider-anthropic__unit_test`,
   `//crates/lash-provider-openai:lash-provider-openai__unit_test` for
   Responses, Codex and Chat, and `//crates/lash-provider-google:lash-provider-google__unit_test`
   for the one-shot shape), the reducer unit test and `runtime_turns__test`.
   The value is an `ArgumentFragment` with exactly that raw text, `Interrupted`,
   and `cut_mid_tool_call`. `Include` fails `ChoiceNotAllowed`. A closed but
   unparseable call is `Invalid`.
6. **Running tool** (`runtime_turns__test`, with a counting tool that reports
   two chunks and then blocks). The partial holds `Running` with both chunks,
   `OutcomeUnknown` and `tool_outcome_unknown`. The tool's execution count
   stays 1 across the stop, the next turn and a replay. A tool with no
   progress yields `Unavailable`.
7. **Worker loss** (new `//crates/lash-restate-test:stopped_partial_recovery__test`,
   on SQLite and PostgreSQL).
   - A redrive after a mid-stream crash retracts the lost attempt and then
     finishes normally, with no partial.
   - A lost root without a cancel seals `ProcessLoss` with
     `AcknowledgedPrefix`, and holds exactly the acknowledged prefix.
   - Cancel-then-crash seals `UserCancel` with
     `recovered_after_process_loss`.
8. **Fault injection and idempotence** (conformance, plus
   `runtime_turns__test` for publication). Crashes are injected before the
   seal, after the seal, after the commit, and after publication. In every
   case there is exactly one committed partial with one digest. A reseal
   returns it. A commit replay answers the same receipt, and a different
   digest fails `StoppedPartialConflict`. No host sees `Stopped` before the
   commit.
9. **Reset and base** (`runtime_turns__test`, conformance). A retry after
   prose, followed by a checkpoint and a cut in the next iteration, keeps
   only the new attempt's post-checkpoint prose. The deleted-frame counts
   match.
10. **Reconnect, retention and fencing** (`//crates/lash:sessions_evidence__test`,
    conformance).
    - After a `Trimmed` gap, the read returns the same partial as the report.
    - The retention sweep removes the partial only with its receipt, and
      `removed_stopped_partial_count` counts it.
    - An append after the seal fails `CaptureSealed`, and an old epoch fails
      `CaptureWriterFenced`.
    - A fork's read of its parent's turn answers `Unknown`.
11. **Workbench** (`//examples/agent-workbench:agent-workbench__unit_test`).
    Discard, then send, gives a context equal to backtrack. Continue in
    context sends only through `send()`. Both work after a simulated
    reconnect. The history and `AssistantOutput` exclusion assertions run in
    both flows.

Two gates also run. `//crates/lash:ui_fixtures` covers the facade's new
exports. `//crates/lash-sim:cross_backend_store_differential__test --
store_trait_surface_is_fully_gated` must drive every `TurnCaptureStore`
method (`crates/lash-sim/tests/cross_backend_store_differential/trait_surface_gate.rs`).

### 7. What is deleted

Nothing ships to delete. The superseded `StoppedAssistantTailEvidence` design
was never merged. No compatibility path is added. Under the freeze, the new
fields and tables change shapes in place, and old catalogs are refused and
recreated (§3.5).

### 8. Lanes and file ownership

There are four lanes. Like ADR 0113's lanes, each is cut from `main` as soon
as this record lands, and none waits for FIG-3946. A lane that edits a file
FIG-3946 changes (the list in ADR 0113 §8) rebases over FIG-3946 when it
lands. The lanes integrate once, onto the integration branch
`fig-433/stopped-partials`, with no adapters. The final integration merge
onto `main`, and the one full gate on it, belong to the orchestrator.

**Contract first.** Lane R's first commit, **C0**, contains every shared type
and signature:

- the new `crates/lash-sansio/src/stopped_partial.rs` (§1.1, with the derivations,
  eligibility and digest), its `lib.rs` lines, the tool-input variants of
  `LlmStreamEvent` and `ToolInputIdentity` (§2.1), and
  `SessionStreamEvent::StoppedPartialAvailable` (§5.2);
- the new `crates/lash-core-store/src/store/capture.rs` (the segment and wire types
  of §3.2), with `TurnCaptureStore` in the store alias;
  the new `crates/lash-core-store/src/capture/reducer.rs` (§3.1, fully implemented,
  with its unit tests); the capture `StoreError` variants (§3.4); and
  `RuntimeCommit.stopped_partial` (§3.3);
- `CaptureWatermark` and the outcome fields (§4.2),
  `AssembledTurn.stopped_partial`, `RootTerminal.stopped_partial`,
  `TurnEvent::ToolOutputProgress`, `ToolContext::progress` and
  `ToolProgressSink` (§2.2);
- `TurnReport.stopped_partial` and `RemoteTurnReport.stopped_partial` (§1.4);
- the `build_resubmission` types of §5.3, without the function.

C0 must pass `kiln clippy` for `//crates/lash-sansio`,
`//crates/lash-core-store` and `//crates/lash-core-execution`, plus the
reducer's unit tests. The rest of the workspace may stay red until
integration. Lane S's first commit, **S1**, stacks on C0. It holds the
`lash-store-sql` statement sets, the SQLite implementation and the
`turn_capture_tests!` conformance suite, so it gives the other lanes a
working store. Lanes R and H rebase onto S1 to verify. All four lanes then
work in parallel.

| Lane | Owns |
|---|---|
| **P: provider adapters** | `crates/lash-provider-anthropic/src/stream.rs`; `crates/lash-provider-openai/src/{responses_shared/sse.rs,responses_shared.rs,chat.rs,codex/streaming.rs}`; `crates/lash-provider-google/src/stream.rs`; `crates/lash-llm-transport/src/conformance.rs`; each adapter's unit tests |
| **S: capture store, SQLite and PostgreSQL** (S1 first) | new `crates/lash-store-sql/src/session/{capture_turns,capture_writers,capture_frames,stopped_partials}.rs`; new `crates/lash-sqlite-store/src/capture.rs`; new `crates/lash-postgres-store/src/postgres/capture.rs`; new `crates/lash-conformance/src/conformance/turn_capture.rs` and its fixtures |
| **R: runtime capture, seal and terminal** (C0 first) | new `crates/lash-sansio/src/stopped_partial.rs`; new `crates/lash-core-store/src/store/capture.rs`; new `crates/lash-core-store/src/capture/**`; new `crates/lash-core/src/runtime/turn_driver/capture_writer.rs`; `crates/lash-core/src/runtime/turn_driver/{streaming.rs,streaming/host_forwarder.rs,effects.rs,local_effects.rs}`; `crates/lash-core/src/runtime/{turn_loop.rs,turn_loop/commit.rs,turn_observer.rs,assembly.rs}`; `crates/lash-core-execution/src/tool_provider.rs`; `crates/lash-core-execution/src/session/tool_execution/**` except `group.rs`; new `crates/lash-core/tests/runtime/stopped_partial.rs`; new `crates/lash-restate-test/tests/stopped_partial_recovery.rs` |
| **H: host surface, helper and workbench** | new `crates/lash/src/stopped_partial.rs`; new `examples/agent-workbench/src/main_sections/stopped_partial.rs`; `examples/agent-workbench/assets/index.html` (the panel); `examples/agent-workbench/src/restate/turn_follow.rs`; new `crates/lash/tests/stopped_partial_evidence.rs`; its workbench tests in `examples/agent-workbench/src/main_sections/tests/` |

The FIG-433 files above sit inside ADR 0112's globs (its §15) and are carved
out: ADR 0112's lanes do not edit them. No FIG-433 file is owned by an
ADR 0113 lane. `crates/lash-core-execution/src/session/tool_execution/group.rs`
belongs to ADR 0113's lane X, and FIG-433 does not edit it. A lane edits only
the files it owns and the shared files below, in the regions named. A change
another lane needs goes to that lane's owner. Each lane regenerates the BUILD
files of the crates whose files it adds.

**Shared files.** The ADR 0112 lane stays the owner of every file below. Each
other cutover writes only its named region, and whichever integration lands
later rebases and resolves. The ADR 0113 column copies the regions ADR 0113
§8 claims. FIG-433's regions are the ones ADR 0113 §8 set aside for it: the
capture segment, the capture tables, the partial-commit block and the
exports.

| Shared file | Owner (ADR 0112 lane) | FIG-433 lane and region | ADR 0113 lane and region |
|---|---|---|---|
| `crates/lash-core-store/src/store/mod.rs` | runtime | R (C0): `mod capture;`, its re-exports, `TurnCaptureStore` in the alias | R: `RuntimeCommit::frame_transition` and its builders |
| `crates/lash-core-store/src/store/runtime_commit.rs` | runtime | R (C0): the `stopped_partial` field and its identity | R: `frame_transition` where `RuntimeCommit` is defined |
| `crates/lash-core-store/src/store/error.rs` | runtime | R (C0): the capture variants only | R: the artifact variants only |
| `crates/lash-core-store/src/lib.rs` | runtime | R (C0): `mod capture;` and re-export lines | R: `mod` and re-export lines |
| `crates/lash-core-store/src/store/root.rs` | runtime | R (C0): `RootTerminal.stopped_partial` | — |
| `crates/lash-core-store/src/store/runtime_persistence_decorator.rs` | runtime | S: the capture operation block | — |
| `crates/lash-core-store/src/store/retention.rs`, `store/maintenance.rs` | runtime | S: the two report counters | — |
| `crates/lash-core-execution/src/runtime/vocabulary.rs` | runtime | R (C0): the `AssembledTurn` field and `TurnEvent::ToolOutputProgress` | R: delete `bind_artifact_stores` |
| `crates/lash-core-execution/src/runtime/effect/envelope.rs` | runtime | R (C0): the `CaptureWatermark` fields | — |
| `crates/lash-core-execution/src/lib.rs` | runtime | R (C0): re-export lines | R: re-export lines |
| `crates/lash-sansio/src/lib.rs`, `src/llm/types.rs`, `src/session_model/mod.rs` | runtime | R (C0): `mod` line, the tool-input variants, the observation variant | — |
| `crates/lash/src/lib.rs` | runtime | H: the stopped-partial exports | R: the export swap |
| `crates/lash/src/{turn.rs,send/follow.rs,durable_session.rs,session.rs}` | runtime | H: the report field, both projections, the durable report and the read | — |
| `crates/lash/BUILD.bazel` | runtime | H: the new test target | K: its new test target |
| `crates/lash-remote-protocol/src/turn_result.rs`, `core_conversions/turn_result.rs` | runtime | H: the field, conversion and validation | — |
| `crates/lash-restate/src/session_control.rs` | runtime | R: the lost-root publication | — |
| `crates/lash-restate-test/BUILD.bazel`, `crates/lash-core/tests/runtime_turns.rs` | runtime | R: the test target and `mod` line | — |
| `crates/lash-sqlite-store/src/schema.rs` | SQLite | S: the four capture tables | S: artifact tables and both cleanup tables |
| `crates/lash-sqlite-store/src/persistence/session_commit.rs` | SQLite | S: the partial-commit block, after the frame-transition block | S: the frame-transition block |
| `crates/lash-sqlite-store/src/session_deletion.rs` | SQLite | S: staging deletion in the delete transaction | S: frame fence and cleanup in the delete transaction |
| `crates/lash-sqlite-store/src/session_roots.rs` | SQLite | S: fence, seal and commit in `end_lost_root_conn` and the operator terminals | — |
| `crates/lash-sqlite-store/src/retention.rs` | SQLite | S: the partial sweep | — |
| `crates/lash-sqlite-store/src/lib.rs`, `crates/lash-store-sql/src/session.rs` | SQLite | S: `mod` lines | S: `mod` lines (`lib.rs`) |
| `crates/lash-postgres-store/schema.sql` | PostgreSQL | S: the four capture tables | P: artifact tables and cleanup table |
| `crates/lash-postgres-store/src/postgres/schema_shape/` | PostgreSQL | S: shape entries for the capture tables | P: shape entries for its tables |
| `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs` | PostgreSQL | S: the partial-commit block, after the frame-transition block | P: the frame-transition block |
| `crates/lash-postgres-store/src/postgres/session_factory.rs` | PostgreSQL | S: staging deletion in `delete_session_tx` | P: delete the drain; frame fence in delete; fork copy |
| `crates/lash-postgres-store/src/postgres/{session_roots.rs,evidence_retention.rs}` | PostgreSQL | S: as SQLite | — |
| `crates/lash-postgres-store/src/lib.rs` | PostgreSQL | S: `mod` line | P: delete the drain fields |
| `crates/lash-conformance/src/macros.rs`, `src/conformance/mod.rs` | readers | S: the `turn_capture_tests!` block and its `mod` line | K: the `artifact_referrer_tests!` block |
| `crates/lash-sim/tests/cross_backend_store_differential/trait_surface_gate.rs`, `crates/lash-sim/src/crash_matrix/deployment.rs`, `crates/lash-perf/src/runtime_perf/store.rs`, `crates/lash-core/src/testing/**` store doubles | readers | S: capture forwarders and the surface sweep | R: delete the `bind_artifact_stores` forwarders |
| `crates/lash-sim/src/provider/transport.rs`, `crates/lash-sim/src/provider_variation_matrix.rs` | readers | P: the tool-input events | — |
| `examples/agent-workbench/src/main_sections/bootstrap.rs` | readers | H: two route lines | — |

**The resolver.** ADR 0113 §8 reserved FIG-433 a region in
`crates/lash-restate/src/effect_host.rs`, which belongs to ADR 0113's lane X.
FIG-433 does not use it. The gate watch keeps today's attachment
(`effect_host.rs:636`), and the capture writer adds nothing there.

## Consequences

- A stopped turn's partial is durable from the moment it was published, and
  it survives reconnects, `Trimmed` gaps and worker loss. It is typed well
  enough that a host never has to rediscover vercel/ai#10638.
- Lash never feeds the partial back. A host that wants it in context
  resubmits ordinary text through `send()`. A host that does nothing gets
  today's backtrack, with an identical history.
- Observation delivery now waits for one store flush per batch. A failed
  flush stops the turn's publication instead of dropping content.
- Every `Stopped` terminal now publishes after its commit. A host never sees
  `Stopped` for a turn whose commit then fails.
- The store gains an eleventh segment and four tables on both backends.
  Old catalogs are refused and recreated under the freeze.
- Every streaming adapter now has to report argument streaming. A future
  adapter that buffers arguments privately fails the transport conformance
  law.
- Partials are session rows, not artifacts. ADR 0113's referrer edges and its
  cleanup executor never see them, and a quoted artifact reference keeps no
  bytes alive.

## Amendment (FIG-3562, 2026-09-29): the progress sink lives on `AttemptContext`

§2.2 places `progress()` on `ToolContext`. Under [ADR 0116](0116-tools-are-opaque.md), `AttemptContext` is the
only context a tool body sees, and `ToolContext` is crate-private runtime
state. `AttemptContext::progress(&self) -> ToolProgressSink` is the accessor,
with §2.2's semantics unchanged. `ToolContext` keeps the runtime's
`progress_reporter` so the attempt context can hand it on. §8's lane R owns
this region of `tool_provider.rs`, and [ADR 0116](0116-tools-are-opaque.md) §10.4 names the seam.

## Lane G amendment

Added 2026-09-29, after lanes R, P, S and H settled. It closes six gaps
lane R left open. Where it changes a section above, this text rules.

**A checkpoint that closes an interrupted call keeps the base (§3.1).** An
`Immediate` stop inside a tool batch does not backtrack the batch: the
batch fills every unsettled call with a cancelled result, and the
iteration's checkpoint commits those calls, as it did before this record.
That checkpoint no longer advances the capture base. The driver decides
where it issues the checkpoint, from recorded facts only: the turn
honoured its stop, a group wait of the batch lost to the stop, or a call
of the batch settled `Cancelled` (a tool child that answered its stop
before its turn resumed). The driver's base counter follows the same
decision, so a replay counts the same, and the hold ends with that
checkpoint. The stop's seal then covers the whole iteration: each
interrupted call is `Running` with every chunk the host received and
`OutcomeUnknown`, beside the model output that asked for it. A
`Cancelled` settlement is never captured as a result, because it is the
stop's own consequence, not a result before the cutoff. A settlement the
seal already fenced is not a fault. Two consequences follow. The partial
may quote content the transcript also holds: the call, with the
synthesized result the transcript keeps, and prose streamed before it in
that iteration. And if a turn continues after such a checkpoint, because a
tool cancelled itself, its next checkpoint advances the base and drops the
held frames. Until that checkpoint, a stop in the continued iteration seals
the held iteration with it, so its partial also quotes the iteration
before. An `AfterStep` stop still seals an empty partial.

**Unstreamed response blocks are captured (§3.1).** When a response
streamed no text, the driver publishes its text and reasoning blocks from
the completed response. The model call's writer now captures those blocks
before its step returns, under the identities the driver publishes them
with, as blocks with no start frame (the capture opens them). Calls the
adapter never streamed were already captured this way. If an assistant
response hook rewrites such a response, the capture holds the text before
the rewrite; FIG-4063 tracks that.

**A rebuilt tool child writes its turn's capture (§3.2).** A group tool
child with no live opener runs on a context the deployment builds. That
context now carries its turn's capture
(`facade_support::deployment_turn_tool_capture`) over the session's own
store. It is addressed by the physical turn that the child's recorded
parent invocation names and by its admitted scope's root. Its writer opens
like any other: it fences the dead attempt's epoch, retracts what that
attempt wrote, and marks the turn recovered. A tool attempt publishes its
progress on the stream of the dispatch it runs under, so a rebuilt child's
chunks ride its settlement with its other events.

**The machine's error is held too (§4.3).** A turn machine writes its
`Error` right before a stopped outcome. The driver holds the observer from
that `Error` once the machine has finished, so the whole terminal
publishes after the commit and a failed commit publishes none of it.

**The lost-root announcement (§4.4, §5.2).** lash-restate publishes
nothing. The lost-root pass reports the partials its terminal writes
sealed on `ParkReconcileReport::sealed_partials`, and the core that runs
the reconcile tick announces each through its Live Replay publisher. The
session's open runtime records it as a turn activity, which is how the
in-process tier publishes turn activities. The activity is the new
`TurnEvent::StoppedPartialAvailable { summary }`. A stopped turn's own
terminal publishes the same activity beside the session event, and both
use one id derived from the partial's turn. The announcement stays best
effort: a session that is open nowhere in the process is not told, and its
hosts read the partial by the root. The activity has no remote form yet.
A remote host gets the partial on `RemoteTurnReport` or through the read.
`RestateConfig` gains nothing: the core already holds the publisher, and
the engine only reports.

**A lost root is always recovered (§1.3).** The lost-root write seals with
`recovered_after_process_loss = true` and `AcknowledgedPrefix` coverage on
both backends, whatever the turn's own capture row recorded. Before, it
took both from that row, so a lost root read as complete and unrecovered.

## Integration amendment

Added 2026-09-29, when the branch merged onto `main` (FIG-433). Where it
changes a section or amendment above, this text rules.

**Both activities have remote forms (§2.2, §5.2).** A remote host follows a
session through its observation stream, which projects each turn activity
to a `RemoteTurnEvent`. That projection refused the two activities this
record adds, so a remote stream failed on every stopped turn.
`RemoteTurnEvent` now carries `ToolOutputProgress { call_id, chunk }` and
`StoppedPartialAvailable { summary }`, reusing the `lash-sansio` types.
Under ADR 0115 §4 a new event kind is additive within the negotiated
version. The Lane G amendment's "no remote form yet" is withdrawn.

**A refused root seals too (§1.3, §4.4).** `main` added
`RootTerminalCause::Refused` (FIG-4018): the root's run ended with a typed
refusal and no turn commit. Its terminal write seals like the other
uncommitted terminals, with `Other { RuntimeFailure }` and
`recovered_after_process_loss = false`. A partial the stopped turn already
sealed is reused and commits with the terminal.

**A later root can adopt a physical turn (§3.2).** An owed follow-on's
recovery runs as a root of its own, `follow-on:<turn>#<n>`, on a fresh
journal, and drives the physical turn a lost root staged (FIG-3946). The
turn's capture row is keyed by session and turn, so the adopting root's
first open or seal rebinds it. The earlier root's frames and writers are
deleted, the base restarts at zero, and the turn reads recovered. The lost
execution's staging never enters the adopting root's partial.

**The seal is drive-fenced (§4.3).** `SealTurnCapture` carries the drive
fence of the execution that seals, and the store checks it as it checks
the commit's (ADR 0105 §9). A successor may raise the drive epoch mid-turn,
which makes the stale execution's commit fail `StaleDriveFence`. That
execution's seal fails the same way, so it cannot leave a seal that blocks
the later drive of the same root. Root-terminal writes seal with no fence,
inside their own transaction.

**Capture-write retries (§4.1).** The runtime retries every refused capture
write, not only transient faults, so a deterministic refusal hangs the
turn. FIG-4069 limits the retry to transient faults.

**A tool attempt writes its capture outside its cancel watch (§2.2, §4.1).**
A tool attempt's recorded body races a live watch on its stop: the turn's
gate, or a group child's cancel fact over the ingress. The attempt now opens
its writer and persists its start before that race, and persists its
settlement after it, still inside the recorded step. What persists, and
before what it publishes, is unchanged. Only the tool's own run, with its
progress chunks, races the watch. An engine that sequences its steps
therefore sees the store writes as the step's own work, never as work left
running while the step waits on the watch's request: the Restate server
double's serial scheduler keeps one grant order per seed (FIG-4071).

**Frozen versions (§3.5).** The four capture tables change the stored
shapes in place: SQLite stays at schema version 99 and PostgreSQL at DDL
revision 141, as the version freeze requires. A catalog without the tables
is refused and recreated.

**The PostgreSQL head commit pays one round trip for capture.** Clearing
the committing turn's frames, writers and counters and reading its sealed
partial run as one data-modifying `WITH`. The pinned head commit gains that
one round trip, and only a stopped turn's commit adds a second, to mark its
partial committed. The capture tables name their `CHECK` constraints, so
the open-time check of an expanded catalog reads them as declared.

**Adapter law (§2.1).** Where an adapter decodes a closed call strictly,
`Part(ToolCall).input_json` may be its normalized encoding, provided it
parses to the same value as `ToolInputEnd::raw_arguments`. The capture
keeps the raw text the provider streamed.

**The PostgreSQL leg of §6.7** is FIG-4065. `stopped_partial_recovery__test`
runs on the Restate double's SQLite store only.

**The terminal's record-and-hold half** is `hold_terminal_sequence`
(§4.3). It records the sequence on the recorded assembly and holds it on the
observer; the commit releases it, and a failed commit abandons it.
