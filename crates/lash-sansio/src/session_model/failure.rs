//! Typed host-facing turn-failure vocabulary.
//!
//! A failing turn reaches a host as an [`ErrorEnvelope`](super::ErrorEnvelope)
//! and, once the turn is assembled, as a `TurnIssue`. Both carried their
//! classification as free-form strings, so a host that wanted to react to a
//! context-window overflow differently from a refused registration had to
//! match prose. The two enums here are that classification, and every
//! runtime-authored spelling is an arm.
//!
//! Wire form is unchanged: both types serialize as the same snake_case string
//! the field carried before, so a persisted snapshot or a peer payload written
//! by an older build decodes with no migration. The open arms
//! ([`TurnFailureKind::Unknown`], [`TurnFailureCode::Other`]) retain the exact
//! spelling they were given, so nothing is lost when a vocabulary this
//! workspace does not own — a provider's error code, a plugin's abort code, a
//! `RuntimeErrorCode` spelling, or a newer build's arm — crosses the boundary.

/// Where a turn failure came from.
///
/// Every arm but [`TurnFailureKind::Unknown`] is authored by this workspace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TurnFailureKind {
    /// Refreshing the live execution environment failed.
    ExecutionEnvironment,
    /// Turn input failed normalization before any provider work.
    InputValidation,
    /// A provider call, a provider transport, or model-capability validation.
    LlmProvider,
    /// A plugin aborted the turn or a plugin lifecycle hook failed.
    Plugin,
    /// A plugin's prompt contribution failed.
    PluginPrompt,
    /// A protocol extension's before-LLM-call hook failed.
    ProtocolBeforeLlmCall,
    /// The RLM protocol refused the model's cell.
    RlmProtocol,
    /// The RLM driver was handed an invalid state.
    RlmDriverState,
    /// The RLM driver was handed invalid turn options.
    RlmTurnOptions,
    /// The runtime itself refused to continue the turn.
    Runtime,
    /// A durable runtime-effect controller refused an effect.
    RuntimeEffectController,
    /// Token-usage accounting overflowed.
    TokenUsageAccounting,
    /// A kind authored outside this build's vocabulary, retained verbatim.
    /// Decoding a payload or snapshot written by a newer build yields this
    /// rather than failing.
    Unknown(String),
}

impl TurnFailureKind {
    /// The stable snake_case spelling carried on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ExecutionEnvironment => "execution_environment",
            Self::InputValidation => "input_validation",
            Self::LlmProvider => "llm_provider",
            Self::Plugin => "plugin",
            Self::PluginPrompt => "plugin_prompt",
            Self::ProtocolBeforeLlmCall => "protocol_before_llm_call",
            Self::RlmProtocol => "rlm_protocol",
            Self::RlmDriverState => "rlm_driver_state",
            Self::RlmTurnOptions => "rlm_turn_options",
            Self::Runtime => "runtime",
            Self::RuntimeEffectController => "runtime_effect_controller",
            Self::TokenUsageAccounting => "token_usage_accounting",
            Self::Unknown(spelling) => spelling,
        }
    }

    /// Classify a wire spelling. An unrecognized spelling is retained as
    /// [`TurnFailureKind::Unknown`] instead of being refused.
    pub fn from_wire(spelling: &str) -> Self {
        match spelling {
            "execution_environment" => Self::ExecutionEnvironment,
            "input_validation" => Self::InputValidation,
            "llm_provider" => Self::LlmProvider,
            "plugin" => Self::Plugin,
            "plugin_prompt" => Self::PluginPrompt,
            "protocol_before_llm_call" => Self::ProtocolBeforeLlmCall,
            "rlm_protocol" => Self::RlmProtocol,
            "rlm_driver_state" => Self::RlmDriverState,
            "rlm_turn_options" => Self::RlmTurnOptions,
            "runtime" => Self::Runtime,
            "runtime_effect_controller" => Self::RuntimeEffectController,
            "token_usage_accounting" => Self::TokenUsageAccounting,
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// Why a turn failed, within its [`TurnFailureKind`].
///
/// Every arm but [`TurnFailureCode::Other`] is a spelling this workspace
/// authors at a known producer site. `Other` carries a vocabulary owned
/// elsewhere — a provider or transport error code, a plugin's abort code, or a
/// `RuntimeErrorCode` spelling from the durable kernel, whose enum is
/// `#[non_exhaustive]` and deliberately not mirrored onto the wire.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TurnFailureCode {
    // ─── provider terminal reasons (`LlmTerminalReason::code`) ───────────
    /// The provider completed normally. Paired with a failure only when a
    /// later stage refused the completed call.
    Stop,
    /// The provider asked for tool use.
    ToolUse,
    /// The provider hit its output token limit.
    OutputLimit,
    /// The request exceeded the model's context window.
    ContextOverflow,
    /// The provider's content filter refused the request or the response.
    ContentFilter,
    /// The provider reported a failure of its own.
    ProviderError,
    /// The call was cancelled.
    Cancelled,
    /// The provider's terminal reason was not classified.
    UnknownTerminalReason,

    // ─── model-capability validation ─────────────────────────────────────
    /// The requested reasoning effort is not supported by the model.
    UnsupportedEffort,
    /// The model does not accept a reasoning-effort selection.
    EffortNotConfigurable,
    /// The model requires a reasoning-effort selection and none was given.
    EffortRequired,
    /// The host-supplied model capability is malformed.
    MalformedCapability,

    // ─── protocol drivers ────────────────────────────────────────────────
    /// The model returned no assistant text (or text and tool calls).
    EmptyResponse,
    /// The model emitted a native tool call the RLM protocol does not allow.
    NativeToolCallNotAllowed,
    /// The RLM driver was resumed in a state it cannot drive.
    InvalidDriverState,
    /// The RLM driver was handed turn options it cannot honour.
    InvalidTurnOptions,
    /// A protocol extension's before-LLM-call hook failed.
    BeforeLlmCallFailed,

    // ─── provider call and stream ────────────────────────────────────────
    /// A message attachment could not be resolved for the request.
    AttachmentResolutionFailed,
    /// A plugin's assistant-stream contribution failed.
    PluginAssistantStream,
    /// The provider implementation panicked.
    ProviderPanicked,
    /// A replayed provider call disagreed with its recorded origin.
    ProviderReplayOriginConflict,
    /// Provider execution evidence arrived before the response was
    /// established.
    StreamEvidenceBeforeResponseStart,
    /// Provider execution evidence conflicted with the identity already
    /// established for the response.
    StreamEvidenceIdentityConflict,

    // ─── charge-safe retry refusals ──────────────────────────────────────
    /// The provider had already begun paid output, so the attempt could not
    /// be regenerated without an idempotency or resume guarantee.
    UnsafeRetryAfterOutputStarted,
    /// The observed provider response was not in a charge-safe retry class.
    UnsafeRetryAfterResponseObserved,
    /// The provider failure carried no transport classification, so no
    /// charge-safe retry class could be established.
    UnsafeRetryWithoutTransportClassification,
    /// The attempt had already reached a terminal response.
    UnsafeRetryAfterTerminalObserved,
    /// The host charge-safety policy requires a retry guarantee the provider
    /// does not offer.
    ChargeSafetyGuaranteeRequired,
    /// The host charge-safety policy's unsafe-retry budget is exhausted.
    ChargeSafetyUnsafeRetryLimitExceeded,
    /// The host charge-safety policy's duplicate-cost budget is exhausted.
    ChargeSafetyDuplicateCostLimitExceeded,
    /// The provider's requested retry delay exceeds the host's cap.
    RetryAfterExceedsCap,
    /// The host charge-safety policy denied the retry outright.
    ChargeSafetyRetryDenied,
    /// Structured output validation failed before the call could complete.
    InvalidStructuredOutput,
    /// The provider response body could not be read.
    BodyReadFailed,

    /// / A token-usage counter overflowed while accumulating turn usage.
    TokenUsageOverflow,
    /// Refreshing the live execution environment failed.
    ReconfigureFailed,
    /// The assembled turn's session graph could not be scoped.
    SessionGraphScope,
    /// The turn stream ended without a `Done` event.
    MissingDone,
    /// Assistant output was recovered from persisted messages because none was assembled.
    AssistantOutputRecoveredFromState,
    /// Turn input failed normalization.
    InvalidTurnInput,
    /// The turn exceeded its agent-frame-switch limit.
    AgentFrameSwitchLimit,
    /// Restoring resident protocol session state after commit failed.
    ProtocolRestoreSession,
    /// A plugin lifecycle hook failed.
    LifecycleHookFailed,

    // ─── provider adapters and transports ────────────────────────────────
    /// The provider endpoint configuration is not a usable URL or socket.
    InvalidProviderEndpoint,
    /// The request used an attachment capability the provider does not offer.
    UnsupportedAttachmentCapability,
    /// A message attachment could not be encoded for the request.
    AttachmentSourceNotEncodable,
    /// A stored attachment could not be resolved for the request.
    StoredAttachmentNotResolved,
    /// A provider-file attachment requires an explicit media type.
    ProviderFileMediaTypeRequired,
    /// The model does not support the requested reasoning-retention mode.
    UnsupportedReasoningRetention,
    /// Reasoning evidence could not be encoded for the provider's wire form.
    ReasoningEncodingUnrepresentable,
    /// A provider tool call carried arguments that were not valid JSON.
    InvalidToolCallInputJson,
    /// The credential refresh was rejected; the host must re-authenticate.
    CredentialInvalidGrant,
    /// The credential refresh failed transiently.
    CredentialRefreshTransient,
    /// The credential refresh failed.
    CredentialRefreshFailed,
    /// The call timed out.
    Timeout,
    /// A driver task join failed.
    TaskJoinFailed,
    /// An SSE event exceeded the configured byte limit.
    SseEventTooLarge,
    /// An SSE response exceeded the configured total byte limit.
    SseResponseTooLarge,
    /// The provider stream ended before a terminal response was observed.
    StreamEndedBeforeTerminalResponse,
    /// The provider stream ended before its terminal message marker.
    StreamEndedBeforeMessageStop,
    /// The provider stream ended before a finish reason was observed.
    StreamEndedBeforeFinishReason,
    /// The provider stream carried no events at all.
    EmptyStream,
    /// A responses-resume request was issued for a non-streaming call.
    ResponsesResumeNotStreaming,
    /// A responses-resume stream event lacked its sequence number.
    ResponsesResumeEventMissingSequence,
    /// The websocket connection could not be established.
    WebsocketConnect,
    /// The websocket connection timed out while connecting.
    WebsocketConnectTimeout,
    /// Writing to the websocket failed.
    WebsocketSend,
    /// The websocket idled past its timeout.
    WebsocketIdleTimeout,
    /// Reading from the websocket failed.
    WebsocketReceive,
    /// The websocket carried a protocol violation.
    WebsocketProtocol,
    /// The websocket closed before the response completed.
    WebsocketClosedBeforeCompleted,

    /// A code from a vocabulary this type does not own, retained verbatim:
    /// provider and transport error codes, plugin abort codes, kernel
    /// `RuntimeErrorCode` spellings, and arms authored by a newer build.
    Other(String),
}

impl TurnFailureCode {
    /// The stable snake_case spelling carried on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::ToolUse => "tool_use",
            Self::OutputLimit => "output_limit",
            Self::ContextOverflow => "context_overflow",
            Self::ContentFilter => "content_filter",
            Self::ProviderError => "provider_error",
            Self::Cancelled => "cancelled",
            Self::UnknownTerminalReason => "unknown",
            Self::UnsupportedEffort => "unsupported_effort",
            Self::EffortNotConfigurable => "effort_not_configurable",
            Self::EffortRequired => "effort_required",
            Self::MalformedCapability => "malformed_capability",
            Self::EmptyResponse => "empty_response",
            Self::NativeToolCallNotAllowed => "native_tool_call_not_allowed",
            Self::InvalidDriverState => "invalid_driver_state",
            Self::InvalidTurnOptions => "invalid_turn_options",
            Self::BeforeLlmCallFailed => "before_llm_call_failed",
            Self::AttachmentResolutionFailed => "attachment_resolution_failed",
            Self::PluginAssistantStream => "plugin_assistant_stream",
            Self::ProviderPanicked => "provider_panicked",
            Self::ProviderReplayOriginConflict => "provider_replay_origin_conflict",
            Self::StreamEvidenceBeforeResponseStart => "stream_evidence_before_response_start",
            Self::StreamEvidenceIdentityConflict => "stream_evidence_identity_conflict",
            Self::UnsafeRetryAfterOutputStarted => "unsafe_retry_after_output_started",
            Self::UnsafeRetryAfterResponseObserved => "unsafe_retry_after_response_observed",
            Self::UnsafeRetryWithoutTransportClassification => {
                "unsafe_retry_without_transport_classification"
            }
            Self::UnsafeRetryAfterTerminalObserved => "unsafe_retry_after_terminal_observed",
            Self::ChargeSafetyGuaranteeRequired => "charge_safety_guarantee_required",
            Self::ChargeSafetyUnsafeRetryLimitExceeded => {
                "charge_safety_unsafe_retry_limit_exceeded"
            }
            Self::ChargeSafetyDuplicateCostLimitExceeded => {
                "charge_safety_duplicate_cost_limit_exceeded"
            }
            Self::RetryAfterExceedsCap => "retry_after_exceeds_cap",
            Self::ChargeSafetyRetryDenied => "charge_safety_retry_denied",
            Self::InvalidStructuredOutput => "invalid_structured_output",
            Self::BodyReadFailed => "body_read_failed",
            Self::TokenUsageOverflow => "token_usage_overflow",
            Self::ReconfigureFailed => "reconfigure_failed",
            Self::SessionGraphScope => "session_graph_scope",
            Self::MissingDone => "missing_done",
            Self::AssistantOutputRecoveredFromState => "assistant_output_recovered_from_state",
            Self::InvalidTurnInput => "invalid_turn_input",
            Self::AgentFrameSwitchLimit => "agent_frame_switch_limit",
            Self::ProtocolRestoreSession => "protocol_restore_session",
            Self::LifecycleHookFailed => "lifecycle_hook_failed",
            Self::InvalidProviderEndpoint => "invalid_provider_endpoint",
            Self::UnsupportedAttachmentCapability => "unsupported_attachment_capability",
            Self::AttachmentSourceNotEncodable => "attachment_source_not_encodable",
            Self::StoredAttachmentNotResolved => "stored_attachment_not_resolved",
            Self::ProviderFileMediaTypeRequired => "provider_file_media_type_required",
            Self::UnsupportedReasoningRetention => "unsupported_reasoning_retention",
            Self::ReasoningEncodingUnrepresentable => "reasoning_encoding_unrepresentable",
            Self::InvalidToolCallInputJson => "invalid_tool_call_input_json",
            Self::CredentialInvalidGrant => "credential_invalid_grant",
            Self::CredentialRefreshTransient => "credential_refresh_transient",
            Self::CredentialRefreshFailed => "credential_refresh_failed",
            Self::Timeout => "timeout",
            Self::TaskJoinFailed => "task_join_failed",
            Self::SseEventTooLarge => "sse_event_too_large",
            Self::SseResponseTooLarge => "sse_response_too_large",
            Self::StreamEndedBeforeTerminalResponse => "stream_ended_before_terminal_response",
            Self::StreamEndedBeforeMessageStop => "stream_ended_before_message_stop",
            Self::StreamEndedBeforeFinishReason => "stream_ended_before_finish_reason",
            Self::EmptyStream => "empty_stream",
            Self::ResponsesResumeNotStreaming => "responses_resume_not_streaming",
            Self::ResponsesResumeEventMissingSequence => "responses_resume_event_missing_sequence",
            Self::WebsocketConnect => "websocket_connect",
            Self::WebsocketConnectTimeout => "websocket_connect_timeout",
            Self::WebsocketSend => "websocket_send",
            Self::WebsocketIdleTimeout => "websocket_idle_timeout",
            Self::WebsocketReceive => "websocket_receive",
            Self::WebsocketProtocol => "websocket_protocol",
            Self::WebsocketClosedBeforeCompleted => "websocket_closed_before_completed",
            Self::Other(spelling) => spelling,
        }
    }

    /// Whether this code was authored by charge-safety or retry policy while
    /// refusing a regeneration.
    pub fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::UnsafeRetryAfterOutputStarted
                | Self::UnsafeRetryAfterResponseObserved
                | Self::UnsafeRetryWithoutTransportClassification
                | Self::UnsafeRetryAfterTerminalObserved
                | Self::ChargeSafetyGuaranteeRequired
                | Self::ChargeSafetyUnsafeRetryLimitExceeded
                | Self::ChargeSafetyDuplicateCostLimitExceeded
                | Self::RetryAfterExceedsCap
                | Self::ChargeSafetyRetryDenied
        )
    }

    /// Classify a wire spelling. An unrecognized spelling is retained as
    /// [`TurnFailureCode::Other`] instead of being refused.
    pub fn from_wire(spelling: &str) -> Self {
        match spelling {
            "stop" => Self::Stop,
            "tool_use" => Self::ToolUse,
            "output_limit" => Self::OutputLimit,
            "context_overflow" => Self::ContextOverflow,
            "content_filter" => Self::ContentFilter,
            "provider_error" => Self::ProviderError,
            "cancelled" => Self::Cancelled,
            "unknown" => Self::UnknownTerminalReason,
            "unsupported_effort" => Self::UnsupportedEffort,
            "effort_not_configurable" => Self::EffortNotConfigurable,
            "effort_required" => Self::EffortRequired,
            "malformed_capability" => Self::MalformedCapability,
            "empty_response" => Self::EmptyResponse,
            "native_tool_call_not_allowed" => Self::NativeToolCallNotAllowed,
            "invalid_driver_state" => Self::InvalidDriverState,
            "invalid_turn_options" => Self::InvalidTurnOptions,
            "before_llm_call_failed" => Self::BeforeLlmCallFailed,
            "attachment_resolution_failed" => Self::AttachmentResolutionFailed,
            "plugin_assistant_stream" => Self::PluginAssistantStream,
            "provider_panicked" => Self::ProviderPanicked,
            "provider_replay_origin_conflict" => Self::ProviderReplayOriginConflict,
            "stream_evidence_before_response_start" => Self::StreamEvidenceBeforeResponseStart,
            "stream_evidence_identity_conflict" => Self::StreamEvidenceIdentityConflict,
            "unsafe_retry_after_output_started" => Self::UnsafeRetryAfterOutputStarted,
            "unsafe_retry_after_response_observed" => Self::UnsafeRetryAfterResponseObserved,
            "unsafe_retry_without_transport_classification" => {
                Self::UnsafeRetryWithoutTransportClassification
            }
            "unsafe_retry_after_terminal_observed" => Self::UnsafeRetryAfterTerminalObserved,
            "charge_safety_guarantee_required" => Self::ChargeSafetyGuaranteeRequired,
            "charge_safety_unsafe_retry_limit_exceeded" => {
                Self::ChargeSafetyUnsafeRetryLimitExceeded
            }
            "charge_safety_duplicate_cost_limit_exceeded" => {
                Self::ChargeSafetyDuplicateCostLimitExceeded
            }
            "retry_after_exceeds_cap" => Self::RetryAfterExceedsCap,
            "charge_safety_retry_denied" => Self::ChargeSafetyRetryDenied,
            "invalid_structured_output" => Self::InvalidStructuredOutput,
            "body_read_failed" => Self::BodyReadFailed,
            "token_usage_overflow" => Self::TokenUsageOverflow,
            "reconfigure_failed" => Self::ReconfigureFailed,
            "session_graph_scope" => Self::SessionGraphScope,
            "missing_done" => Self::MissingDone,
            "assistant_output_recovered_from_state" => Self::AssistantOutputRecoveredFromState,
            "invalid_turn_input" => Self::InvalidTurnInput,
            "agent_frame_switch_limit" => Self::AgentFrameSwitchLimit,
            "protocol_restore_session" => Self::ProtocolRestoreSession,
            "lifecycle_hook_failed" => Self::LifecycleHookFailed,
            "invalid_provider_endpoint" => Self::InvalidProviderEndpoint,
            "unsupported_attachment_capability" => Self::UnsupportedAttachmentCapability,
            "attachment_source_not_encodable" => Self::AttachmentSourceNotEncodable,
            "stored_attachment_not_resolved" => Self::StoredAttachmentNotResolved,
            "provider_file_media_type_required" => Self::ProviderFileMediaTypeRequired,
            "unsupported_reasoning_retention" => Self::UnsupportedReasoningRetention,
            "reasoning_encoding_unrepresentable" => Self::ReasoningEncodingUnrepresentable,
            "invalid_tool_call_input_json" => Self::InvalidToolCallInputJson,
            "credential_invalid_grant" => Self::CredentialInvalidGrant,
            "credential_refresh_transient" => Self::CredentialRefreshTransient,
            "credential_refresh_failed" => Self::CredentialRefreshFailed,
            "timeout" => Self::Timeout,
            "task_join_failed" => Self::TaskJoinFailed,
            "sse_event_too_large" => Self::SseEventTooLarge,
            "sse_response_too_large" => Self::SseResponseTooLarge,
            "stream_ended_before_terminal_response" => Self::StreamEndedBeforeTerminalResponse,
            "stream_ended_before_message_stop" => Self::StreamEndedBeforeMessageStop,
            "stream_ended_before_finish_reason" => Self::StreamEndedBeforeFinishReason,
            "empty_stream" => Self::EmptyStream,
            "responses_resume_not_streaming" => Self::ResponsesResumeNotStreaming,
            "responses_resume_event_missing_sequence" => Self::ResponsesResumeEventMissingSequence,
            "websocket_connect" => Self::WebsocketConnect,
            "websocket_connect_timeout" => Self::WebsocketConnectTimeout,
            "websocket_send" => Self::WebsocketSend,
            "websocket_idle_timeout" => Self::WebsocketIdleTimeout,
            "websocket_receive" => Self::WebsocketReceive,
            "websocket_protocol" => Self::WebsocketProtocol,
            "websocket_closed_before_completed" => Self::WebsocketClosedBeforeCompleted,
            other => Self::Other(other.to_string()),
        }
    }
}

/// A failure code namespaced by who authored the spelling.
///
/// `code` fields that used to mix provider-emitted slugs, adapter-authored
/// diagnostics, and Lash charge-safety refusals in one bare string now carry
/// the namespace with the spelling, so a reader never has to guess which
/// vocabulary a value belongs to — or mistake a numeric provider slug for an
/// HTTP status. Serializes as `"<namespace>:<spelling>"`; a bare legacy
/// spelling decodes as `Adapter` when it names a [`TurnFailureCode`] arm and
/// as `Provider` otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FailureCode {
    /// A code the provider emitted on the wire — an open vocabulary this
    /// workspace does not own.
    Provider(String),
    /// A code the Lash adapter or transport authored while normalizing or
    /// driving the provider exchange.
    Adapter(TurnFailureCode),
    /// A code Lash charge-safety or retry policy authored while refusing the
    Refusal(TurnFailureCode),
}

impl FailureCode {
    /// The namespace that owns this code's spelling.
    pub fn namespace(&self) -> &'static str {
        match self {
            Self::Provider(_) => "provider",
            Self::Adapter(_) => "adapter",
            Self::Refusal(_) => "refusal",
        }
    }

    /// The spelling within its namespace.
    pub fn spelling(&self) -> &str {
        match self {
            Self::Provider(code) => code,
            Self::Adapter(code) | Self::Refusal(code) => code.as_str(),
        }
    }

    /// `"<namespace>:<spelling>"` — the host-facing render.
    pub fn namespaced(&self) -> String {
        format!("{}:{}", self.namespace(), self.spelling())
    }

    /// Decode a namespaced or legacy bare spelling.
    pub fn from_wire(spelling: &str) -> Self {
        match spelling.split_once(':') {
            Some(("provider", code)) => Self::Provider(code.to_string()),
            Some(("adapter", code)) => Self::Adapter(TurnFailureCode::from_wire(code)),
            Some(("refusal", code)) => Self::Refusal(TurnFailureCode::from_wire(code)),
            _ => match TurnFailureCode::from_wire(spelling) {
                // A legacy bare spelling: workspace-authored spellings decode
                // as their authored namespace, foreign spellings as provider
                // codes.
                TurnFailureCode::Other(_) => Self::Provider(spelling.to_string()),
                code if code.is_refusal() => Self::Refusal(code),
                code => Self::Adapter(code),
            },
        }
    }
}

impl std::fmt::Display for FailureCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.namespaced())
    }
}

impl serde::Serialize for FailureCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.namespaced())
    }
}

impl schemars::JsonSchema for FailureCode {
    fn schema_name() -> String {
        "FailureCode".to_string()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

impl<'de> serde::Deserialize<'de> for FailureCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = FailureCode;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a namespaced failure code")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<FailureCode, E> {
                Ok(FailureCode::from_wire(value))
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

macro_rules! string_wire_serde {
    ($ty:ty, $expecting:literal) => {
        impl std::fmt::Display for $ty {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl schemars::JsonSchema for $ty {
            fn schema_name() -> String {
                stringify!($ty).to_string()
            }

            fn json_schema(
                generator: &mut schemars::r#gen::SchemaGenerator,
            ) -> schemars::schema::Schema {
                <String as schemars::JsonSchema>::json_schema(generator)
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;

                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $ty;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str($expecting)
                    }

                    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<$ty, E> {
                        Ok(<$ty>::from_wire(value))
                    }
                }

                deserializer.deserialize_str(Visitor)
            }
        }
    };
}

string_wire_serde!(TurnFailureKind, "a turn-failure kind");
string_wire_serde!(TurnFailureCode, "a turn-failure code");

impl From<crate::llm::types::LlmTerminalReason> for TurnFailureCode {
    /// A completed call's terminal reason as a failure code. The reason also
    /// rides the envelope's typed `terminal_reason` field; this keeps the
    /// `code` spelling the boundary carried before it was typed.
    fn from(reason: crate::llm::types::LlmTerminalReason) -> Self {
        use crate::llm::types::LlmTerminalReason as Reason;
        match reason {
            Reason::Stop => Self::Stop,
            Reason::ToolUse => Self::ToolUse,
            Reason::OutputLimit => Self::OutputLimit,
            Reason::ContextOverflow => Self::ContextOverflow,
            Reason::ContentFilter => Self::ContentFilter,
            Reason::ProviderError => Self::ProviderError,
            Reason::Cancelled => Self::Cancelled,
            Reason::Unknown => Self::UnknownTerminalReason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureCode, TurnFailureCode, TurnFailureKind};

    /// Every arm's spelling survives a round trip through the wire form, so a
    /// typed arm never silently degrades into an open arm.
    #[test]
    fn every_arm_round_trips_through_its_wire_spelling() {
        let kinds = [
            TurnFailureKind::ExecutionEnvironment,
            TurnFailureKind::InputValidation,
            TurnFailureKind::LlmProvider,
            TurnFailureKind::Plugin,
            TurnFailureKind::PluginPrompt,
            TurnFailureKind::ProtocolBeforeLlmCall,
            TurnFailureKind::RlmProtocol,
            TurnFailureKind::RlmDriverState,
            TurnFailureKind::RlmTurnOptions,
            TurnFailureKind::Runtime,
            TurnFailureKind::RuntimeEffectController,
            TurnFailureKind::TokenUsageAccounting,
        ];
        for kind in kinds {
            assert_eq!(TurnFailureKind::from_wire(kind.as_str()), kind);
        }

        let codes = [
            TurnFailureCode::Stop,
            TurnFailureCode::ToolUse,
            TurnFailureCode::OutputLimit,
            TurnFailureCode::ContextOverflow,
            TurnFailureCode::ContentFilter,
            TurnFailureCode::ProviderError,
            TurnFailureCode::Cancelled,
            TurnFailureCode::UnknownTerminalReason,
            TurnFailureCode::UnsupportedEffort,
            TurnFailureCode::EffortNotConfigurable,
            TurnFailureCode::EffortRequired,
            TurnFailureCode::MalformedCapability,
            TurnFailureCode::EmptyResponse,
            TurnFailureCode::NativeToolCallNotAllowed,
            TurnFailureCode::InvalidDriverState,
            TurnFailureCode::InvalidTurnOptions,
            TurnFailureCode::BeforeLlmCallFailed,
            TurnFailureCode::AttachmentResolutionFailed,
            TurnFailureCode::PluginAssistantStream,
            TurnFailureCode::ProviderPanicked,
            TurnFailureCode::ProviderReplayOriginConflict,
            TurnFailureCode::StreamEvidenceBeforeResponseStart,
            TurnFailureCode::StreamEvidenceIdentityConflict,
            TurnFailureCode::UnsafeRetryAfterOutputStarted,
            TurnFailureCode::UnsafeRetryAfterResponseObserved,
            TurnFailureCode::UnsafeRetryWithoutTransportClassification,
            TurnFailureCode::UnsafeRetryAfterTerminalObserved,
            TurnFailureCode::ChargeSafetyGuaranteeRequired,
            TurnFailureCode::ChargeSafetyUnsafeRetryLimitExceeded,
            TurnFailureCode::ChargeSafetyDuplicateCostLimitExceeded,
            TurnFailureCode::RetryAfterExceedsCap,
            TurnFailureCode::ChargeSafetyRetryDenied,
            TurnFailureCode::InvalidStructuredOutput,
            TurnFailureCode::BodyReadFailed,
            TurnFailureCode::TokenUsageOverflow,
            TurnFailureCode::ReconfigureFailed,
            TurnFailureCode::SessionGraphScope,
            TurnFailureCode::MissingDone,
            TurnFailureCode::AssistantOutputRecoveredFromState,
            TurnFailureCode::InvalidTurnInput,
            TurnFailureCode::AgentFrameSwitchLimit,
            TurnFailureCode::ProtocolRestoreSession,
            TurnFailureCode::LifecycleHookFailed,
            TurnFailureCode::InvalidProviderEndpoint,
            TurnFailureCode::UnsupportedAttachmentCapability,
            TurnFailureCode::AttachmentSourceNotEncodable,
            TurnFailureCode::StoredAttachmentNotResolved,
            TurnFailureCode::ProviderFileMediaTypeRequired,
            TurnFailureCode::UnsupportedReasoningRetention,
            TurnFailureCode::ReasoningEncodingUnrepresentable,
            TurnFailureCode::InvalidToolCallInputJson,
            TurnFailureCode::CredentialInvalidGrant,
            TurnFailureCode::CredentialRefreshTransient,
            TurnFailureCode::CredentialRefreshFailed,
            TurnFailureCode::Timeout,
            TurnFailureCode::TaskJoinFailed,
            TurnFailureCode::SseEventTooLarge,
            TurnFailureCode::SseResponseTooLarge,
            TurnFailureCode::StreamEndedBeforeTerminalResponse,
            TurnFailureCode::StreamEndedBeforeMessageStop,
            TurnFailureCode::StreamEndedBeforeFinishReason,
            TurnFailureCode::EmptyStream,
            TurnFailureCode::ResponsesResumeNotStreaming,
            TurnFailureCode::ResponsesResumeEventMissingSequence,
            TurnFailureCode::WebsocketConnect,
            TurnFailureCode::WebsocketConnectTimeout,
            TurnFailureCode::WebsocketSend,
            TurnFailureCode::WebsocketIdleTimeout,
            TurnFailureCode::WebsocketReceive,
            TurnFailureCode::WebsocketProtocol,
            TurnFailureCode::WebsocketClosedBeforeCompleted,
        ];
        for code in codes {
            assert_eq!(TurnFailureCode::from_wire(code.as_str()), code);
        }
    }

    /// A spelling this build does not own keeps its exact bytes, so a payload
    /// or snapshot written by another build survives a decode and re-encode.
    #[test]
    fn an_unowned_spelling_is_retained_verbatim() {
        let kind = TurnFailureKind::from_wire("a_kind_from_a_newer_peer");
        assert_eq!(
            kind,
            TurnFailureKind::Unknown("a_kind_from_a_newer_peer".to_string())
        );
        assert_eq!(kind.as_str(), "a_kind_from_a_newer_peer");

        let code = TurnFailureCode::from_wire("session_execution_lease_lost");
        assert_eq!(
            code,
            TurnFailureCode::Other("session_execution_lease_lost".to_string())
        );
        assert_eq!(code.as_str(), "session_execution_lease_lost");
    }

    /// Namespaced failure codes round-trip, and legacy bare spellings decode
    /// into the namespace that authored them.
    #[test]
    fn failure_code_namespaces_round_trip_and_decode_legacy_bare_spellings() {
        let cases = [
            (
                FailureCode::Provider("insufficient_quota".to_string()),
                "provider:insufficient_quota",
            ),
            (
                FailureCode::Adapter(TurnFailureCode::Timeout),
                "adapter:timeout",
            ),
            (
                FailureCode::Refusal(TurnFailureCode::UnsafeRetryAfterOutputStarted),
                "refusal:unsafe_retry_after_output_started",
            ),
        ];
        for (code, namespaced) in cases {
            assert_eq!(code.to_string(), namespaced);
            assert_eq!(FailureCode::from_wire(namespaced), code);
            let json = serde_json::to_string(&code).expect("serialize");
            assert_eq!(
                serde_json::from_str::<FailureCode>(&json).expect("deserialize"),
                code
            );
        }

        assert_eq!(
            FailureCode::from_wire("timeout"),
            FailureCode::Adapter(TurnFailureCode::Timeout)
        );
        assert_eq!(
            FailureCode::from_wire("unsafe_retry_after_output_started"),
            FailureCode::Refusal(TurnFailureCode::UnsafeRetryAfterOutputStarted)
        );
        assert_eq!(
            FailureCode::from_wire("insufficient_quota"),
            FailureCode::Provider("insufficient_quota".to_string())
        );
    }
}
