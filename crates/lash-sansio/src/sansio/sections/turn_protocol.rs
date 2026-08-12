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
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    /// See [`PendingToolCall::replay`].
    pub replay: Option<ProviderReplayMeta>,
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
            parts: Arc::new(vec![Part::text(format!("{}.p0", self.id), self.text.clone(), None)]),
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
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub enum LogEvent {
    LlmDebug {
        session_id: String,
        protocol_iteration: usize,
        usage: TokenUsage,
        provider_usage: Option<Value>,
        request_body: Option<String>,
        response_text: String,
        response_parts: Option<Value>,
    },
    LlmError {
        session_id: String,
        protocol_iteration: usize,
        request_body: Option<String>,
        message: String,
        retryable: bool,
        raw: Option<String>,
        code: Option<String>,
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
    /// Sync the live execution environment before the turn proceeds.
    ///
    /// `update_machine_config` is only needed after the turn has
    /// already advanced at least once and the host may need to swap in
    /// a refreshed system prompt or tool schema for the next
    /// protocol iteration. Initial syncs are host-only because the machine was
    /// already constructed from a fresh execution environment.
    SyncExecutionEnvironment {
        id: EffectId,
        update_machine_config: bool,
    },
    /// Start an LLM call.
    LlmCall {
        id: EffectId,
        request: Arc<LlmRequest>,
    },
    /// Cancel an in-progress LLM stream.
    CancelLlm { id: EffectId },
    /// Execute one or more driver-scheduled tool calls.
    ToolCalls {
        id: EffectId,
        calls: Vec<PendingToolCall>,
    },
    /// Execute a protocol-owned code block.
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
    Log { event: LogEvent },
    /// Fire-and-forget event (no response needed).
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
}

impl<M: TurnProtocol> Clone for Effect<M> {
    fn clone(&self) -> Self {
        match self {
            Self::SyncExecutionEnvironment {
                id,
                update_machine_config,
            } => Self::SyncExecutionEnvironment {
                id: *id,
                update_machine_config: *update_machine_config,
            },
            Self::LlmCall { id, request } => Self::LlmCall {
                id: *id,
                request: Arc::clone(request),
            },
            Self::CancelLlm { id } => Self::CancelLlm { id: *id },
            Self::ToolCalls { id, calls } => Self::ToolCalls {
                id: *id,
                calls: calls.clone(),
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

impl<M: TurnProtocol> Effect<M> {
    fn id(&self) -> Option<EffectId> {
        match self {
            Self::SyncExecutionEnvironment { id, .. }
            | Self::LlmCall { id, .. }
            | Self::CancelLlm { id }
            | Self::ToolCalls { id, .. }
            | Self::ExecCode { id, .. }
            | Self::Checkpoint { id, .. } => Some(*id),
            Self::Log { .. } | Self::Emit(_) | Self::Progress { .. } | Self::Done { .. } => None,
        }
    }
}

/// Error details from a failed LLM call.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct LlmCallError {
    pub message: String,
    pub retryable: bool,
    /// Typed transport classification of the failure. Defaults to
    /// [`ProviderFailureKind::Unknown`] for wrappers that are not provider
    /// failures (and when decoding effect journals written before the field
    /// existed).
    #[serde(default)]
    pub kind: crate::llm::types::ProviderFailureKind,
    pub raw: Option<String>,
    pub code: Option<String>,
    pub terminal_reason: LlmTerminalReason,
    pub request_body: Option<String>,
    /// Output and usage observed before the failed stream ended. Partial tool
    /// calls in this response are retained for diagnosis but never executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_response: Option<Box<LlmResponse>>,
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

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct ExecutionEnvironmentSync {
    pub system_prompt: Arc<str>,
    pub tool_specs: Arc<Vec<LlmToolSpec>>,
}

pub struct WaitingLlmState<M: TurnProtocol = UnitTurnProtocol> {
    pub request: Arc<LlmRequest>,
    driver_state: Option<M::DriverState>,
}

impl<M: TurnProtocol> WaitingLlmState<M> {
    pub fn take_driver_state(&mut self) -> Option<M::DriverState> {
        self.driver_state.take()
    }
}

pub struct WaitingExecState<M: TurnProtocol = UnitTurnProtocol> {
    driver_state: M::DriverState,
}

impl<M: TurnProtocol> WaitingExecState<M> {
    pub fn into_driver_state(self) -> M::DriverState {
        self.driver_state
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub enum CheckpointResumeAction {
    PrepareIteration,
    Finish(TurnOutcome),
}

// justification: driver actions are single-step machine values and boxing generic driver state would add allocation to every iteration.
#[allow(clippy::large_enum_variant)]
pub enum DriverAction<M: TurnProtocol = UnitTurnProtocol> {
    Emit(SessionStreamEvent),
    AppendEvents(Vec<SessionHistoryRecord<M::Event>>),
    StartLlm {
        request: Arc<LlmRequest>,
        driver_state: Option<M::DriverState>,
    },
    StartTools {
        calls: Vec<PendingToolCall>,
    },
    StartExec {
        language: String,
        code: String,
        driver_state: M::DriverState,
    },
    StartCheckpoint {
        checkpoint: CheckpointKind,
        on_empty: CheckpointResumeAction,
    },
    AdvanceProtocolIteration,
    ScheduleTurnLimitFinal {
        message: Message,
    },
    Finish(TurnOutcome),
}

pub struct DriverContextView<'a, M: TurnProtocol = UnitTurnProtocol> {
    config: &'a TurnMachineConfig<M>,
    messages: &'a MessageSequence,
    events: &'a [SessionHistoryRecord<M::Event>],
    turn_causes: &'a [TurnCause],
    protocol_iteration: usize,
    protocol_run_offset: usize,
    termination: &'a TurnTerminationPolicyState,
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
        })
    }

    pub fn protocol_iteration(&self) -> usize {
        self.protocol_iteration
    }

    pub fn protocol_run_offset(&self) -> usize {
        self.protocol_run_offset
    }

    pub fn turn_id(&self) -> &str {
        &self.config.turn_id
    }

    pub fn turn_budget(&self) -> crate::TurnBudget {
        self.config.turn_budget
    }

    pub fn generation(&self) -> &crate::llm::types::GenerationOptions {
        &self.config.generation
    }

    pub fn termination(&self) -> &M::Termination {
        &self.config.termination
    }

    pub fn autonomous(&self) -> bool {
        self.config.autonomous
    }

    pub fn should_force_exit_after_grace_turn(&self) -> bool {
        self.termination.should_force_exit_after_grace_turn()
    }

    pub fn turn_limit_final_to_schedule(&self) -> Option<usize> {
        self.termination.turn_limit_final_to_schedule(
            self.protocol_iteration,
            self.protocol_run_offset,
            self.config.turn_budget,
        )
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
}

pub trait ContextProjector<M: TurnProtocol = UnitTurnProtocol>: Send + Sync {
    fn project(&self, ctx: ProjectorContext<'_, M>) -> Arc<LlmRequest>;
}

#[derive(Clone, Debug, Default)]
pub struct ChatContextProjector;

impl<M: TurnProtocol> ContextProjector<M> for ChatContextProjector {
    fn project(&self, ctx: ProjectorContext<'_, M>) -> Arc<LlmRequest> {
        let rendered_prompt = render_messages_for_projector(ctx.messages, ctx.turn_causes);
        let attachments: Vec<AttachmentSource> = rendered_prompt.attachments;
        let mut messages = rendered_prompt.messages;
        if let Some(turn_events) = render_turn_causes_prompt(ctx.turn_causes) {
            messages.push(crate::llm::types::LlmMessage::text(
                crate::llm::types::LlmRole::User,
                Arc::from(turn_events),
            ));
        }
        if !ctx.config.system_prompt.trim().is_empty() {
            messages.insert(
                0,
                crate::llm::types::LlmMessage::text(
                    crate::llm::types::LlmRole::System,
                    Arc::clone(&ctx.config.system_prompt),
                ),
            );
        }

        Arc::new(LlmRequest {
            model: ctx.config.model.clone(),
            messages,
            attachments,
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
            generation: ctx.config.generation.clone(),
            scope: crate::llm::types::LlmRequestScope::new(
                ctx.config.session_id.clone(),
                format!("{}:frame:sansio", ctx.config.session_id),
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
    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_, M>,
        waiting: WaitingLlmState<M>,
        llm_response: LlmResponse,
        text_streamed: bool,
    ) -> Vec<DriverAction<M>>;
    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_, M>,
        completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction<M>>;
    fn handle_exec_result(
        &self,
        ctx: DriverContextView<'_, M>,
        waiting: WaitingExecState<M>,
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
    pub model_variant: crate::ReasoningSelection,
    pub model_capability: crate::llm::capability::ModelCapability,
    pub generation: crate::llm::types::GenerationOptions,
    pub autonomous: bool,
    pub tool_specs: Arc<Vec<LlmToolSpec>>,
    pub system_prompt: Arc<str>,
    pub session_id: String,
    pub turn_id: String,
    pub emit_llm_trace: bool,
    pub termination: M::Termination,
    pub turn_limit_final_message: crate::TurnLimitFinalMessage,
}

#[cfg(test)]
mod llm_call_error_tests {
    use super::LlmCallError;
    use crate::llm::types::ProviderFailureKind;

    #[test]
    fn llm_call_error_decodes_journal_entries_that_predate_kind() {
        // `LlmCallError` is serialized inside durable effect journals
        // (`RuntimeEffectOutcome::LlmCall`). Entries written before the typed
        // `kind` field existed must decode with `Unknown`.
        let legacy = r#"{
            "message":"rate limited",
            "retryable":true,
            "raw":null,
            "code":"429",
            "terminal_reason":"provider_error",
            "request_body":null
        }"#;
        let decoded: LlmCallError = serde_json::from_str(legacy).expect("legacy call error");
        assert!(decoded.retryable);
        assert_eq!(decoded.kind, ProviderFailureKind::Unknown);
    }
}

// ─── Internal state ───
