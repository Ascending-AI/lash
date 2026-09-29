use super::*;
use crate::SessionId;
use crate::TurnId;

impl TurnProtocol for UnitTurnProtocol {
    type Event = ();
    type Termination = ();
    type DriverState = serde_json::Value;
}

/// Opaque identifier linking an effect to its response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
pub struct EffectId(pub u64);

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct PendingToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub args: Value,
    /// Opaque provider replay state carried through for the next request.
    pub replay: Option<ProviderReplayMeta>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct CompletedToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub output: ToolCallOutput,
    pub model_return: ModelToolReturn,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    /// See [`PendingToolCall::replay`].
    pub replay: Option<ProviderReplayMeta>,
}

/// How a step's flat tool slots fold back into the calls its response made.
///
/// A protocol that offers call sugar (the standard protocol's `batch`)
/// expands each sugared call into executable slots of the step's one tool
/// group, and records here how their results fold back into one result per
/// sugared call. The plan is a pure function of the recorded response and the
/// turn's admitted protocol configuration, so a replay recomputes it rather
/// than reading it back. Empty when the response held no sugar.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct ToolExpansionPlan {
    pub wrappers: Vec<ExpandedWrapper>,
}

impl ToolExpansionPlan {
    pub fn is_empty(&self) -> bool {
        self.wrappers.is_empty()
    }
}

/// One sugared call of the response and the slots its members took.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct ExpandedWrapper {
    /// Position of the wrapper call among the response's dispatched calls.
    pub source_position: u32,
    /// The provider's call id and replay metadata, kept for the transcript.
    pub call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub replay: Option<ProviderReplayMeta>,
    /// One row per member, in member order.
    pub rows: Vec<ExpandedRow>,
}

/// Where one member of a sugared call went.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpandedRow {
    /// The member runs in flat slot `slot` of the step's tool group.
    Slot {
        member_index: u32,
        tool: String,
        slot: u32,
    },
    /// The member was refused before the group opened.
    Refused {
        member_index: u32,
        tool: String,
        error: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TurnCause {
    pub id: String,
    pub event_type: String,
    pub origin: MessageOrigin,
    pub text: String,
}

impl TurnCause {
    pub fn to_event_message(&self) -> Message {
        Message {
            id: self.id.clone(),
            role: MessageRole::Event,
            parts: Arc::new(vec![Part::text(
                format!("{}.p0", self.id),
                self.text.clone(),
                None,
            )]),
            origin: Some(self.origin.clone()),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct CheckpointDelivery {
    /// Normal user messages admitted by durable turn-input ingress.
    ///
    /// These are already fully materialized rather than plugin messages so
    /// they retain the same `origin: None` representation as user input that
    /// starts a turn.
    #[serde(default)]
    pub committed_user_messages: Vec<Message>,
    pub messages: Vec<PluginMessage>,
    pub transient_messages: Vec<PluginMessage>,
    pub turn_causes: Vec<TurnCause>,
}

pub fn render_turn_causes_prompt(causes: &[TurnCause]) -> Option<String> {
    if causes.is_empty() {
        return None;
    }

    let mut rendered = String::from("=== TURN EVENTS ===");
    for (index, cause) in causes.iter().enumerate() {
        rendered.push_str("\n\n");
        rendered.push_str(&format!(
            "--- event[{index}] · {} · {} ---\n",
            cause.event_type, cause.id
        ));
        rendered.push_str("Origin: ");
        rendered.push_str(&render_message_origin(&cause.origin));
        rendered.push_str("\n\n");
        rendered.push_str(cause.text.trim());
    }
    Some(rendered)
}

fn render_message_origin(origin: &MessageOrigin) -> String {
    match origin {
        MessageOrigin::Plugin {
            plugin_id,
            transient,
        } => {
            if *transient {
                format!("plugin {plugin_id} (transient)")
            } else {
                format!("plugin {plugin_id}")
            }
        }
        MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            ..
        } => match wake_id {
            Some(wake_id) => {
                format!("process {process_id} {event_type} #{sequence} ({wake_id})")
            }
            None => format!("process {process_id} {event_type} #{sequence}"),
        },
        MessageOrigin::TurnInput { turn_id, input_id } => match input_id {
            Some(input_id) => format!("turn input {input_id} on turn {turn_id}"),
            None => format!("turn input on turn {turn_id}"),
        },
        MessageOrigin::TurnOutput { turn_id, source } => match source {
            crate::TurnOutputSource::Runtime => format!("turn output on turn {turn_id}"),
            crate::TurnOutputSource::Plugin { plugin_id } => {
                format!("turn output from plugin {plugin_id} on turn {turn_id}")
            }
        },
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum LogEvent {
    LlmDebug {
        session_id: SessionId,
        protocol_iteration: usize,
        usage: TokenUsage,
        provider_usage: Option<Value>,
        request_body: Option<String>,
        response_text: String,
        response_parts: Option<Value>,
    },
    LlmError {
        session_id: SessionId,
        protocol_iteration: usize,
        request_body: Option<String>,
        retryable: bool,
        code: Option<crate::session_model::FailureCode>,
        /// The transport's failure classification. `ProviderFailureKind::Unknown`
        /// means the failure carried no provider kind; the trace projection
        /// renders it as an absent kind.
        #[serde(default)]
        kind: crate::llm::types::ProviderFailureKind,
        terminal_reason: LlmTerminalReason,
    },
}

/// An effect the host must fulfil.
//
// `Clone` is implemented by hand below rather than derived: the derive would
// demand `M: Clone`, but only `M::Event` is ever cloned (and `TurnProtocol`
// already guarantees `Event: Clone`), so a manual impl keeps `Effect<M>`
// cloneable for every protocol — which the turn checkpoint relies on.
#[derive(Debug, Serialize, serde::Deserialize)]
// justification: effects are short-lived machine states whose generic protocol payload remains inline for checkpoint cloning.
#[allow(clippy::large_enum_variant)]
pub enum Effect<M: TurnProtocol = UnitTurnProtocol> {
    /// Sync the execution environment the next protocol iteration runs
    /// under: the system prompt, tool schema and projector inputs its model
    /// call is built from. Every sync — the protocol-start one included —
    /// returns the environment, and the host journals it, so a redriven
    /// iteration's model call is built from the surface its live pass saw,
    /// not from the live registry (FIG-3538, FIG-3587).
    SyncExecutionEnvironment {
        id: EffectId,
    },
    LlmCall {
        id: EffectId,
        request: Arc<LlmRequest>,
    },
    ToolCalls {
        id: EffectId,
        /// The flat executable slots of the step's one tool group.
        calls: Vec<PendingToolCall>,
        /// How the slots fold back into the response's calls. The host runs
        /// `calls` and answers one result per slot, in slot order; the
        /// machine folds them.
        #[serde(default, skip_serializing_if = "ToolExpansionPlan::is_empty")]
        expansion: ToolExpansionPlan,
    },
    ExecCode {
        id: EffectId,
        language: String,
        code: String,
    },
    /// Run a host/plugin checkpoint before the machine continues or completes.
    Checkpoint {
        id: EffectId,
        checkpoint: CheckpointKind,
    },
    /// Host-implemented fire-and-forget logging.
    Log {
        event: LogEvent,
    },
    Emit(SessionStreamEvent),
    /// Prompt-history progress that may be durably persisted by the host.
    ///
    /// This is separate from [`SessionStreamEvent`]: UI stream events can be partial,
    /// duplicated, or display-only, while `Progress` is emitted only after the
    /// state machine has applied semantic message or protocol-step changes.
    Progress {
        messages: MessageSequence,
        event_delta: Vec<SessionHistoryRecord<M::Event>>,
        protocol_iteration: usize,
    },
    /// Turn is done.
    Done {
        messages: MessageSequence,
        event_delta: Vec<SessionHistoryRecord<M::Event>>,
        protocol_iteration: usize,
    },
    /// Report completed tool calls that the protocol refused before dispatch.
    ///
    /// The host emits the shared tool lifecycle pair for these calls. Turn
    /// accounting is emitted separately by the machine immediately after this
    /// effect, preserving `Started` before the accounting completion record.
    ReportToolCalls {
        completed: Vec<CompletedToolCall>,
    },
}

impl<M: TurnProtocol> Clone for Effect<M> {
    fn clone(&self) -> Self {
        match self {
            Self::SyncExecutionEnvironment { id } => Self::SyncExecutionEnvironment { id: *id },
            Self::LlmCall { id, request } => Self::LlmCall {
                id: *id,
                request: Arc::clone(request),
            },
            Self::ToolCalls {
                id,
                calls,
                expansion,
            } => Self::ToolCalls {
                id: *id,
                calls: calls.clone(),
                expansion: expansion.clone(),
            },
            Self::ReportToolCalls { completed } => Self::ReportToolCalls {
                completed: completed.clone(),
            },
            Self::ExecCode { id, language, code } => Self::ExecCode {
                id: *id,
                language: language.clone(),
                code: code.clone(),
            },
            Self::Checkpoint { id, checkpoint } => Self::Checkpoint {
                id: *id,
                checkpoint: *checkpoint,
            },
            Self::Log { event } => Self::Log {
                event: event.clone(),
            },
            Self::Emit(event) => Self::Emit(event.clone()),
            Self::Progress {
                messages,
                event_delta,
                protocol_iteration,
            } => Self::Progress {
                messages: messages.clone(),
                event_delta: event_delta.clone(),
                protocol_iteration: *protocol_iteration,
            },
            Self::Done {
                messages,
                event_delta,
                protocol_iteration,
            } => Self::Done {
                messages: messages.clone(),
                event_delta: event_delta.clone(),
                protocol_iteration: *protocol_iteration,
            },
        }
    }
}

/// Error details from a failed LLM call.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct LlmCallError {
    #[serde(serialize_with = "serialize_provider_failure_message")]
    pub message: String,
    pub retryable: bool,
    /// Required transport classification. Non-provider failures explicitly
    /// carry `ProviderFailureKind::Unknown`; missing or future kinds are refused.
    pub kind: crate::llm::types::ProviderFailureKind,
    #[serde(default, skip_serializing, skip_deserializing)]
    pub raw: Option<String>,
    /// Namespaced failure code: `provider` spellings are provider-owned,
    /// `lash` spellings are Lash-authored (pre-cutover `adapter`/`refusal`
    /// namespaces decode as Lash vocabulary), and every other namespace is a
    /// foreign-owned pair carried verbatim.
    pub code: Option<crate::session_model::FailureCode>,
    pub terminal_reason: LlmTerminalReason,
    pub request_body: Option<String>,
    /// Output and usage observed before the failed stream ended. Partial tool
    /// calls in this response are retained for diagnosis but never executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_response: Option<Box<LlmResponse>>,
}

fn serialize_provider_failure_message<S>(_: &String, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str("provider call failed")
}

/// A response to a previously emitted effect.
pub enum Response {
    /// Live execution environment sync completed.
    ExecutionEnvironmentSynced {
        id: EffectId,
        result: Result<Option<ExecutionEnvironmentSync>, String>,
    },
    /// Full LLM response.
    LlmComplete {
        id: EffectId,
        result: Result<LlmResponse, LlmCallError>,
        /// When true, text deltas were already emitted during streaming,
        /// so the driver should skip emitting `TextDelta` events.
        text_streamed: bool,
    },
    /// Native tool results.
    ToolResults {
        id: EffectId,
        results: Vec<CompletedToolCall>,
    },
    /// Mode code execution result.
    ExecResult {
        id: EffectId,
        result: Result<crate::ExecResponse, String>,
    },
    /// Checkpoint result with optional injected messages.
    Checkpoint {
        id: EffectId,
        delivery: CheckpointDelivery,
    },
}

/// The projector inputs that vary across a turn's protocol iterations.
///
/// Every value here is derived from recorded turn state — the committed
/// usage record and the journaled execution-environment sync — never read
/// live at projection time. The host fills it when the machine is built and
/// refreshes it through each journaled [`ExecutionEnvironmentSync`], so a
/// redriven iteration replays the recorded inputs instead of re-deriving
/// them from plugin cells (FIG-3538).
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct ProjectorTurnInputs {
    /// The turn's recorded prompt-usage figure: the previous turn's committed
    /// usage, constant across this turn's iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_usage: Option<TokenUsage>,
    /// The protocol-rendered view of execution-bound variables, refreshed at
    /// each iteration boundary. `None` means the protocol exposes no such
    /// surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_variables_prompt: Option<Arc<str>>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct ExecutionEnvironmentSync {
    pub system_prompt: Arc<str>,
    pub tool_specs: Arc<Vec<LlmToolSpec>>,
    /// The projector's recorded-state inputs for this iteration, journaled
    /// with the rest of the sync so a redrive replays them verbatim. `None`
    /// (including records written before this field existed) leaves the
    /// machine's current inputs in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projector_turn_inputs: Option<ProjectorTurnInputs>,
}

impl Response {
    /// The id of the effect this response answers.
    pub(super) fn effect_id(&self) -> EffectId {
        match self {
            Self::ExecutionEnvironmentSynced { id, .. }
            | Self::LlmComplete { id, .. }
            | Self::ToolResults { id, .. }
            | Self::ExecResult { id, .. }
            | Self::Checkpoint { id, .. } => *id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum CheckpointResumeAction {
    PrepareIteration,
    Finish(TurnOutcome),
}

/// Work a turn hands the host and waits on: the one definition of each
/// waiting kind's payload.
///
/// The machine holds it, under its effect id, while the host fulfils it; the
/// host-facing [`Effect`] is projected from it by [`PendingWork::to_effect`]
/// on first delivery and again on every redelivery after a checkpoint
/// restore, so the two cannot diverge. When the answer arrives the machine
/// hands the driver-owned part (the request and driver state) back to the
/// protocol driver.
//
// `Clone` is implemented by hand below for the same reason as on `Effect`:
// the derive would demand `M: Clone`.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum PendingWork<M: TurnProtocol = UnitTurnProtocol> {
    /// Sync the execution environment the next protocol iteration runs
    /// under. The machine requests this itself at each iteration boundary.
    SyncExecutionEnvironment,
    Llm {
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
    },
    Tools {
        /// The flat executable slots of the step's one tool group.
        calls: Vec<PendingToolCall>,
        /// How the slots fold back into the response's calls. Empty when the
        /// response held no sugar, and then absent from the encoding.
        #[serde(default, skip_serializing_if = "ToolExpansionPlan::is_empty")]
        expansion: ToolExpansionPlan,
    },
    Exec {
        language: String,
        code: String,
        driver_state: M::DriverState,
    },
    Checkpoint {
        checkpoint: CheckpointKind,
        on_empty: CheckpointResumeAction,
    },
}

impl<M: TurnProtocol> Clone for PendingWork<M> {
    fn clone(&self) -> Self {
        match self {
            Self::SyncExecutionEnvironment => Self::SyncExecutionEnvironment,
            Self::Llm {
                request,
                driver_state,
            } => Self::Llm {
                request: Arc::clone(request),
                driver_state: driver_state.clone(),
            },
            Self::Tools { calls, expansion } => Self::Tools {
                calls: calls.clone(),
                expansion: expansion.clone(),
            },
            Self::Exec {
                language,
                code,
                driver_state,
            } => Self::Exec {
                language: language.clone(),
                code: code.clone(),
                driver_state: driver_state.clone(),
            },
            Self::Checkpoint {
                checkpoint,
                on_empty,
            } => Self::Checkpoint {
                checkpoint: *checkpoint,
                on_empty: on_empty.clone(),
            },
        }
    }
}

impl<M: TurnProtocol> PendingWork<M> {
    /// The effect that hands this work to the host under `id`. The only
    /// constructor of a waiting effect.
    pub(super) fn to_effect(&self, id: EffectId) -> Effect<M> {
        match self {
            Self::SyncExecutionEnvironment => Effect::SyncExecutionEnvironment { id },
            Self::Llm { request, .. } => Effect::LlmCall {
                id,
                request: Arc::clone(request),
            },
            Self::Tools { calls, expansion } => Effect::ToolCalls {
                id,
                calls: calls.clone(),
                expansion: expansion.clone(),
            },
            Self::Exec { language, code, .. } => Effect::ExecCode {
                id,
                language: language.clone(),
                code: code.clone(),
            },
            Self::Checkpoint { checkpoint, .. } => Effect::Checkpoint {
                id,
                checkpoint: *checkpoint,
            },
        }
    }

    /// Whether `response` is the kind of answer this work waits for.
    pub(super) fn answered_by(&self, response: &Response) -> bool {
        match self {
            Self::SyncExecutionEnvironment => {
                matches!(response, Response::ExecutionEnvironmentSynced { .. })
            }
            Self::Llm { .. } => matches!(response, Response::LlmComplete { .. }),
            Self::Tools { .. } => matches!(response, Response::ToolResults { .. }),
            Self::Exec { .. } => matches!(response, Response::ExecResult { .. }),
            Self::Checkpoint { .. } => matches!(response, Response::Checkpoint { .. }),
        }
    }
}

// justification: driver actions are single-step machine values and boxing generic driver state would add allocation to every iteration.
#[allow(clippy::large_enum_variant)]
pub enum DriverAction<M: TurnProtocol = UnitTurnProtocol> {
    Emit(SessionStreamEvent),
    AppendEvents(Vec<SessionHistoryRecord<M::Event>>),
    /// Hand the host this work and wait for its answer.
    Start(PendingWork<M>),
    AdvanceProtocolIteration,
    /// Finish for a cancellation whose host evidence was already observed.
    FinishCancelled {
        evidence: crate::TurnCancellationEvidence,
    },
    Finish(TurnOutcome),
    /// Report completed tool calls that were refused before host dispatch.
    ReportToolCalls {
        completed: Vec<CompletedToolCall>,
    },
}

pub struct DriverContextView<'a, M: TurnProtocol = UnitTurnProtocol> {
    pub(super) config: &'a TurnMachineConfig<M>,
    pub(super) messages: &'a MessageSequence,
    pub(super) events: &'a [SessionHistoryRecord<M::Event>],
    pub(super) turn_causes: &'a [TurnCause],
    pub(super) protocol_iteration: usize,
    pub(super) protocol_run_offset: usize,
    pub(super) observed_cancellation: Option<&'a crate::TurnCancellationEvidence>,
}

impl<'a, M: TurnProtocol> DriverContextView<'a, M> {
    pub fn project_llm_request(&self, use_tools: bool) -> Arc<LlmRequest> {
        self.config.projector.project(ProjectorContext {
            config: self.config,
            messages: self.messages,
            events: self.events,
            turn_causes: self.turn_causes,
            protocol_iteration: self.protocol_iteration,
            use_tools,
            projector_turn_inputs: &self.config.projector_turn_inputs,
        })
    }

    pub fn protocol_iteration(&self) -> usize {
        self.protocol_iteration
    }

    /// The version this fleet's writers emit for the surface registered under
    /// `constant`, whose build-newest version is `build_newest` (FIG-3796).
    /// Drivers stamp durable envelopes through this — usually via the
    /// `driver_writer_version!` macro — never the bare build constant.
    pub fn writer_version(&self, constant: &'static str, build_newest: u32) -> u32 {
        self.config
            .writer_formats
            .writer_version(constant, build_newest)
    }

    pub fn protocol_run_offset(&self) -> usize {
        self.protocol_run_offset
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.config.turn_id
    }

    pub fn turn_budget(&self) -> crate::TurnBudget {
        self.config.turn_budget
    }

    pub fn no_progress_budget(&self) -> crate::NoProgressBudget {
        self.config.no_progress_budget
    }

    pub fn generation(&self) -> &crate::llm::types::GenerationOptions {
        &self.config.generation
    }

    pub fn termination(&self) -> &M::Termination {
        &self.config.termination
    }

    /// Host cancellation evidence observed before the current response was
    /// handed to the protocol driver.
    pub fn observed_cancellation(&self) -> Option<&crate::TurnCancellationEvidence> {
        self.observed_cancellation
    }

    pub fn autonomous(&self) -> bool {
        self.config.autonomous
    }

    pub fn messages(&self) -> &MessageSequence {
        self.messages
    }

    pub fn events(&self) -> &[SessionHistoryRecord<M::Event>] {
        self.events
    }

    pub fn turn_causes(&self) -> &[TurnCause] {
        self.turn_causes
    }
}

pub struct ProjectorContext<'a, M: TurnProtocol = UnitTurnProtocol> {
    pub config: &'a TurnMachineConfig<M>,
    pub messages: &'a MessageSequence,
    pub events: &'a [SessionHistoryRecord<M::Event>],
    pub turn_causes: &'a [TurnCause],
    pub protocol_iteration: usize,
    pub use_tools: bool,
    /// Recorded-state inputs for this projection. Borrowed from the machine
    /// config — which the journaled execution-environment sync keeps equal to
    /// the recorded value — so a redrive projects from the same inputs
    /// (FIG-3538).
    pub projector_turn_inputs: &'a ProjectorTurnInputs,
}

/// **Purity contract (ADR 0105 §6).** Every method is synchronous, takes
/// `&self` and has no side effects: a replay calls it again over the same
/// recorded inputs and must reach the same decision. Interior mutability in
/// an implementor is a contract violation.
pub trait ContextProjector<M: TurnProtocol = UnitTurnProtocol>: Send + Sync {
    fn project(&self, ctx: ProjectorContext<'_, M>) -> Arc<LlmRequest>;
}

#[derive(Clone, Debug, Default)]
pub struct ChatContextProjector;

impl<M: TurnProtocol> ContextProjector<M> for ChatContextProjector {
    fn project(&self, ctx: ProjectorContext<'_, M>) -> Arc<LlmRequest> {
        let rendered_prompt = render_messages_for_projector(ctx.messages, ctx.turn_causes);
        let mut messages = rendered_prompt.messages;
        if let Some(turn_events) = render_turn_causes_prompt(ctx.turn_causes) {
            messages.push(crate::llm::types::LlmMessage::text(
                crate::llm::types::LlmRole::User,
                Arc::from(turn_events),
            ));
        }

        Arc::new(LlmRequest {
            instructions: (!ctx.config.system_prompt.trim().is_empty())
                .then(|| Arc::from(ctx.config.system_prompt.trim())),
            model: ctx.config.model.clone(),
            messages,
            resolved_stored: Default::default(),
            tools: if ctx.use_tools {
                Arc::clone(&ctx.config.tool_specs)
            } else {
                Arc::new(Vec::new())
            },
            tool_choice: if ctx.use_tools {
                LlmToolChoice::Auto
            } else {
                LlmToolChoice::None
            },
            model_variant: ctx.config.model_variant.clone(),
            model_capability: ctx.config.model_capability.clone(),
            extra_body: ctx.config.extra_body.clone(),
            generation: ctx.config.generation.clone(),
            scope: crate::llm::types::LlmRequestScope::new(
                ctx.config.session_id.clone(),
                ctx.config.agent_frame_id.clone(),
                format!(
                    "{}:sansio:llm:{}",
                    ctx.config.session_id, ctx.protocol_iteration
                ),
            ),
            output_spec: None,
            stream_events: None,
            provider_trace: None,
        })
    }
}

fn render_messages_for_projector(
    messages: &MessageSequence,
    turn_causes: &[TurnCause],
) -> crate::RenderedPrompt {
    if turn_causes.is_empty() {
        return messages.render_prompt();
    }

    let active_cause_ids = turn_causes
        .iter()
        .map(|cause| cause.id.as_str())
        .collect::<HashSet<_>>();
    let filtered = messages
        .iter()
        .filter(|message| {
            !(matches!(message.role, MessageRole::Event)
                && active_cause_ids.contains(message.id.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    render_prompt(filtered.as_slice())
}

/// **Purity contract (ADR 0105 §6).** Every method is synchronous, takes
/// `&self` and has no side effects: a replay calls it again over the same
/// recorded inputs and must reach the same decision. Interior mutability in
/// an implementor is a contract violation.
pub trait ProtocolDriverHandle<M: TurnProtocol = UnitTurnProtocol>: Send + Sync {
    /// Project raw provider text onto the assistant-visible prose surface.
    /// Protocols that embed executable markup override this so terminal
    /// provider paths cannot bypass their normal visibility rules.
    fn project_visible_assistant_prose(&self, text: &str) -> String {
        text.to_string()
    }

    /// Whether a completed response that hit the provider output limit should
    /// still reach the protocol. Most protocols treat it as an incomplete
    /// terminal turn; protocols with a partial-response grammar can convert it
    /// into typed repair feedback.
    fn handles_output_limit_response(&self) -> bool {
        false
    }

    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_, M>) -> Vec<DriverAction<M>>;
    /// Answer the [`PendingWork::Llm`] the driver started, handing back its
    /// `request` and `driver_state`.
    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_, M>,
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
        llm_response: LlmResponse,
        text_streamed: bool,
    ) -> Vec<DriverAction<M>>;
    /// Fold the step's per-slot results back into one result per call of the
    /// response, as `plan` records. Runs before anything is appended or
    /// emitted, so only folded calls reach the stream, the transcript and
    /// [`Self::handle_tool_results`]. A driver that never starts tool work
    /// with a non-empty plan keeps the default, which returns the slots.
    fn fold_tool_results(
        &self,
        plan: &ToolExpansionPlan,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<CompletedToolCall> {
        debug_assert!(
            plan.is_empty(),
            "a driver that expands tool calls must fold them"
        );
        completed
    }
    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_, M>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction<M>>;
    /// Answer the [`PendingWork::Exec`] the driver started, handing back its
    /// `driver_state`.
    fn handle_exec_result(
        &self,
        ctx: DriverContextView<'_, M>,
        driver_state: M::DriverState,
        result: Result<crate::ExecResponse, String>,
    ) -> Vec<DriverAction<M>>;
}

/// Configuration for a `TurnMachine` instance.
pub struct TurnMachineConfig<M: TurnProtocol = UnitTurnProtocol> {
    pub protocol_driver: Arc<dyn ProtocolDriverHandle<M>>,
    pub projector: Arc<dyn ContextProjector<M>>,
    pub sync_execution_environment: bool,
    pub model: String,
    /// Model context-window size in tokens, if known. Lets the kernel
    /// reclassify a zero-output `OutputLimit` terminal reason as
    /// `ContextOverflow` when the prompt nearly filled the window. `None`
    /// disables that refinement.
    pub max_context_tokens: Option<usize>,
    pub turn_budget: crate::TurnBudget,
    /// Bound on consecutive provider attempts that commit no successful
    /// execution. Enforced by the protocol driver, which is the only layer
    /// that can tell a productive attempt from a stalled one.
    pub no_progress_budget: crate::NoProgressBudget,
    pub model_variant: crate::ReasoningSelection,
    pub model_capability: crate::llm::capability::ModelCapability,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub generation: crate::llm::types::GenerationOptions,
    pub autonomous: bool,
    pub tool_specs: Arc<Vec<LlmToolSpec>>,
    pub system_prompt: Arc<str>,
    /// The projector's recorded-state inputs for the upcoming iteration.
    /// Filled by the host from recorded turn state and refreshed by each
    /// journaled [`ExecutionEnvironmentSync`].
    pub projector_turn_inputs: ProjectorTurnInputs,
    pub session_id: SessionId,
    /// The committed active frame whose history is being projected.
    pub agent_frame_id: String,
    pub turn_id: TurnId,
    pub emit_llm_trace: bool,
    /// The fleet's writer-version table (FIG-3796): drivers stamp durable
    /// envelopes through `DriverContextView::writer_version`, which resolves
    /// here rather than at build time.
    pub writer_formats: Arc<dyn crate::WriterFormats>,
    pub termination: M::Termination,
}

#[cfg(test)]
mod llm_call_error_tests {
    use super::LlmCallError;
    use crate::llm::types::ProviderFailureKind;

    #[test]
    fn journaled_provider_error_omits_free_text() {
        const SECRET: &str = "api_key= secret Authorization: Basic abc";
        let error = LlmCallError {
            message: SECRET.to_string(),
            retryable: false,
            kind: ProviderFailureKind::Http,
            raw: Some(SECRET.to_string()),
            code: Some(crate::session_model::FailureCode::provider(
                "rate_limit_exceeded",
            )),
            terminal_reason: crate::llm::types::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: None,
        };
        let journaled = serde_json::to_string(&error).expect("serialize effect result");
        assert!(!journaled.contains(SECRET));
        assert!(!journaled.contains("Basic abc"));
        assert!(journaled.contains("provider:rate_limit_exceeded"));
        let replayed: LlmCallError = serde_json::from_str(&journaled).expect("replay error");
        assert_eq!(replayed.message, "provider call failed");
        assert_eq!(replayed.raw, None);
    }

    #[test]
    fn llm_call_error_requires_a_recognized_journal_kind() {
        let mut wire = serde_json::json!({"message":"rate limited","retryable":true,"raw":null,"code":"429","terminal_reason":"provider_error","request_body":null});
        assert!(
            serde_json::from_value::<LlmCallError>(wire.clone())
                .unwrap_err()
                .to_string()
                .contains("kind")
        );
        wire["kind"] = serde_json::json!("future_kind");
        assert!(
            serde_json::from_value::<LlmCallError>(wire.clone())
                .unwrap_err()
                .to_string()
                .contains("future_kind")
        );
        for (literal, expected) in [
            ("transport", ProviderFailureKind::Transport),
            ("timeout", ProviderFailureKind::Timeout),
            ("http", ProviderFailureKind::Http),
            ("stream", ProviderFailureKind::Stream),
            ("auth", ProviderFailureKind::Auth),
            ("validation", ProviderFailureKind::Validation),
            ("quota", ProviderFailureKind::Quota),
            ("unsupported", ProviderFailureKind::Unsupported),
            ("unknown", ProviderFailureKind::Unknown),
        ] {
            wire["kind"] = serde_json::json!(literal);
            let decoded = serde_json::from_value::<LlmCallError>(wire.clone()).unwrap();
            assert_eq!(decoded.kind, expected);
            assert_eq!(serde_json::to_value(decoded).unwrap()["kind"], literal);
        }
    }
}

// ─── Internal state ───
