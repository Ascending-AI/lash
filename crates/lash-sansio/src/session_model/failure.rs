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
//!
//! [`FailureCode`] is the namespaced code carried on the envelope and in the
//! attempt journal: an opaque `{namespace, spelling}` pair where `lash` is
//! reserved for workspace-authored codes and provider, host, and plugin
//! vocabularies live in their own namespaces, never reinterpreted into Lash's.

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
    ///
    /// `Other` is a decode product, not a constructor: outside this crate it
    /// can only be produced by [`TurnFailureCode::from_wire`], so
    /// [`FailureCode::lash`] is reachable for arbitrary spellings only
    /// through a spelling decode — never by direct construction.
    #[non_exhaustive]
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

    /// Every named arm — all but [`Self::Other`] — in `as_str` table order,
    /// for cross-vocabulary collision tests in the durable kernel. The
    /// completeness assertion in this module's tests keeps the list pinned
    /// to the `as_str`/`from_wire` tables.
    #[doc(hidden)]
    pub const ALL_NAMED: &[Self] = &[
        Self::Stop,
        Self::ToolUse,
        Self::OutputLimit,
        Self::ContextOverflow,
        Self::ContentFilter,
        Self::ProviderError,
        Self::Cancelled,
        Self::UnknownTerminalReason,
        Self::UnsupportedEffort,
        Self::EffortNotConfigurable,
        Self::EffortRequired,
        Self::MalformedCapability,
        Self::EmptyResponse,
        Self::NativeToolCallNotAllowed,
        Self::InvalidDriverState,
        Self::InvalidTurnOptions,
        Self::BeforeLlmCallFailed,
        Self::AttachmentResolutionFailed,
        Self::PluginAssistantStream,
        Self::ProviderPanicked,
        Self::ProviderReplayOriginConflict,
        Self::StreamEvidenceBeforeResponseStart,
        Self::StreamEvidenceIdentityConflict,
        Self::UnsafeRetryAfterOutputStarted,
        Self::UnsafeRetryAfterResponseObserved,
        Self::UnsafeRetryWithoutTransportClassification,
        Self::UnsafeRetryAfterTerminalObserved,
        Self::ChargeSafetyGuaranteeRequired,
        Self::ChargeSafetyUnsafeRetryLimitExceeded,
        Self::ChargeSafetyDuplicateCostLimitExceeded,
        Self::RetryAfterExceedsCap,
        Self::ChargeSafetyRetryDenied,
        Self::InvalidStructuredOutput,
        Self::BodyReadFailed,
        Self::TokenUsageOverflow,
        Self::ReconfigureFailed,
        Self::SessionGraphScope,
        Self::MissingDone,
        Self::AssistantOutputRecoveredFromState,
        Self::InvalidTurnInput,
        Self::AgentFrameSwitchLimit,
        Self::ProtocolRestoreSession,
        Self::LifecycleHookFailed,
        Self::InvalidProviderEndpoint,
        Self::UnsupportedAttachmentCapability,
        Self::AttachmentSourceNotEncodable,
        Self::StoredAttachmentNotResolved,
        Self::ProviderFileMediaTypeRequired,
        Self::UnsupportedReasoningRetention,
        Self::ReasoningEncodingUnrepresentable,
        Self::InvalidToolCallInputJson,
        Self::CredentialInvalidGrant,
        Self::CredentialRefreshTransient,
        Self::CredentialRefreshFailed,
        Self::Timeout,
        Self::TaskJoinFailed,
        Self::SseEventTooLarge,
        Self::SseResponseTooLarge,
        Self::StreamEndedBeforeTerminalResponse,
        Self::StreamEndedBeforeMessageStop,
        Self::StreamEndedBeforeFinishReason,
        Self::EmptyStream,
        Self::ResponsesResumeNotStreaming,
        Self::ResponsesResumeEventMissingSequence,
        Self::WebsocketConnect,
        Self::WebsocketConnectTimeout,
        Self::WebsocketSend,
        Self::WebsocketIdleTimeout,
        Self::WebsocketReceive,
        Self::WebsocketProtocol,
        Self::WebsocketClosedBeforeCompleted,
    ];
}

/// Who authored a [`FailureCode`] spelling.
///
/// A namespace names the vocabulary a code's spelling belongs to:
/// [`Namespace::LASH`] is reserved for codes this workspace mints,
/// [`Namespace::PROVIDER`] carries codes a provider emitted on the wire, and a
/// host or plugin names its own. Lash never interprets a spelling whose
/// namespace it does not own.
///
/// Host-minted namespaces are validated: they start with a lowercase ASCII
/// letter, continue with lowercase ASCII letters, digits, `_`, `.` or `-`,
/// and are at most 63 bytes. Namespaces decoded from the wire are kept
/// verbatim — decode never errors, and an unvalidated foreign namespace is
/// retained rather than rewritten.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Namespace(std::borrow::Cow<'static, str>);

/// Why a proposed host namespace was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidNamespace {
    name: String,
    reason: &'static str,
}

impl std::fmt::Display for InvalidNamespace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "invalid failure-code namespace `{}`: {}",
            self.name, self.reason
        )
    }
}

impl std::error::Error for InvalidNamespace {}

impl Namespace {
    /// The namespace reserved for codes this workspace authors.
    pub const LASH: Self = Self(std::borrow::Cow::Borrowed("lash"));
    /// The namespace for codes a provider emitted on the wire.
    pub const PROVIDER: Self = Self(std::borrow::Cow::Borrowed("provider"));

    /// The namespace [`FailureCode::from_foreign_wire`] gives a foreign
    /// spelling that cannot keep its own: bare spellings and spellings
    /// claiming a reserved namespace land here with their claim preserved
    /// verbatim, so nothing a foreign author sends can mint Lash or provider
    /// vocabulary.
    pub const FOREIGN: Self = Self(std::borrow::Cow::Borrowed("foreign"));

    /// A host-owned namespace.
    ///
    /// `lash` and `provider` are reserved by construction: a host names its
    /// own vocabulary so its codes can never be mistaken for platform- or
    /// provider-authored ones. The retired `adapter`/`refusal` namespaces are
    /// equally reserved — a trusted legacy decode still interprets them as
    /// Lash-authored, so a host may not mint under them. `name` must start
    /// with a lowercase ASCII letter, contain only lowercase ASCII letters,
    /// digits, `_`, `.` and `-`, and be at most 63 bytes.
    pub fn host(name: impl Into<String>) -> Result<Self, InvalidNamespace> {
        let name = name.into();
        let reason = if Self::from_wire(&name).is_reserved() {
            Some("`lash`, `provider`, `adapter` and `refusal` are reserved namespaces")
        } else if name.is_empty()
            || name.len() > 63
            || !name
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase())
            || !name.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-'
            })
        {
            Some(
                "must start with a lowercase letter, contain only lowercase letters, digits, `_`, `.` or `-`, and be at most 63 bytes",
            )
        } else {
            None
        };
        match reason {
            Some(reason) => Err(InvalidNamespace { name, reason }),
            None => Ok(Self(std::borrow::Cow::Owned(name))),
        }
    }

    /// The namespace an on-the-wire code carried, kept verbatim whether or
    /// not it satisfies [`Namespace::host`] validation.
    fn from_wire(name: &str) -> Self {
        match name {
            "lash" => Self::LASH,
            "provider" => Self::PROVIDER,
            _ => Self(std::borrow::Cow::Owned(name.to_string())),
        }
    }

    /// The namespace's wire spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the namespace belongs to vocabulary this workspace authored:
    /// `lash` plus the retired `adapter`/`refusal` namespaces pre-cutover
    /// rows still carry.
    fn is_lash_vocabulary(&self) -> bool {
        matches!(self.as_str(), "lash" | "adapter" | "refusal")
    }

    /// Whether the namespace is one no foreign author may mint: the
    /// workspace's `lash` vocabulary (including retired spellings) and the
    /// `provider` wire namespace.
    fn is_reserved(&self) -> bool {
        self.is_lash_vocabulary() || self.as_str() == "provider"
    }
}

impl std::fmt::Display for Namespace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A failure code namespaced by who authored the spelling.
///
/// An opaque `{namespace, spelling}` pair: `lash` is reserved for codes this
/// workspace mints through [`FailureCode::lash`], `provider` carries codes a
/// provider emitted on the wire, and hosts and plugins carry their own
/// namespaces via [`FailureCode::foreign`]. Lash never interprets a spelling
/// whose namespace it does not own.
///
/// Serializes as `"<namespace>:<spelling>"`. Decode is split by trust:
/// [`FailureCode::from_wire`] is the trusted decode for rows Lash itself
/// journaled (a pre-cutover namespaced value keeps both halves verbatim, and
/// a bare spelling decodes as a lash code when it parses as a
/// [`TurnFailureCode`] arm and as a provider code otherwise);
/// [`FailureCode::from_foreign_wire`] is the ingress decode for spellings a
/// foreign author delivered, and never grants a reserved namespace.
///
/// The pair sits behind one box so `Option<FailureCode>` stays pointer-sized
/// on the hot `Result<_, LlmTransportError>` path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FailureCode(Box<FailureCodePair>);

/// The pair a [`FailureCode`] boxes: who authored the spelling, and the
/// spelling itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FailureCodePair {
    namespace: Namespace,
    spelling: String,
}

impl FailureCode {
    /// A code this workspace authored — the only way to mint a `lash` code.
    ///
    /// `code` is typed vocabulary: the named arms are workspace spellings,
    /// and [`TurnFailureCode::Other`] is `#[non_exhaustive]`, so the only
    /// `Other` values a caller can hold came from a spelling decode. Foreign
    /// spellings belong to [`FailureCode::foreign`] or, at an ingress
    /// boundary, [`FailureCode::from_foreign_wire`].
    pub fn lash(code: TurnFailureCode) -> Self {
        Self(Box::new(FailureCodePair {
            namespace: Namespace::LASH,
            spelling: code.as_str().to_string(),
        }))
    }

    /// A code a provider emitted on the wire.
    pub fn provider(spelling: impl Into<String>) -> Self {
        Self::pair(Namespace::PROVIDER, spelling)
    }

    /// A code in a caller-owned namespace — how hosts and plugins carry their
    /// own vocabulary without Lash reinterpreting the spelling.
    ///
    /// Reserved namespaces are refused: `lash` (and its retired
    /// `adapter`/`refusal` spellings) comes from [`FailureCode::lash`] and
    /// `provider` wire codes from [`FailureCode::provider`].
    pub fn foreign(
        namespace: Namespace,
        spelling: impl Into<String>,
    ) -> Result<Self, InvalidNamespace> {
        if namespace.is_reserved() {
            return Err(InvalidNamespace {
                name: namespace.as_str().to_string(),
                reason: "`lash`, `provider`, `adapter` and `refusal` are reserved namespaces",
            });
        }
        Ok(Self::pair(namespace, spelling))
    }

    /// The pair a constructor mints once namespace ownership is settled.
    fn pair(namespace: Namespace, spelling: impl Into<String>) -> Self {
        Self(Box::new(FailureCodePair {
            namespace,
            spelling: spelling.into(),
        }))
    }

    /// The namespace that owns this code's spelling.
    pub fn namespace(&self) -> &Namespace {
        &self.0.namespace
    }

    /// The spelling within its namespace.
    pub fn spelling(&self) -> &str {
        &self.0.spelling
    }

    /// `"<namespace>:<spelling>"` — the host-facing render.
    pub fn namespaced(&self) -> String {
        format!("{}:{}", self.0.namespace.as_str(), self.0.spelling)
    }

    /// The [`TurnFailureCode`] this code carries, when it carries one.
    ///
    /// Parses only when the spelling lives in vocabulary this workspace owns:
    /// `lash`, plus the retired `adapter`/`refusal` namespaces pre-cutover
    /// durable rows still carry. A spelling in any other namespace returns
    /// `None` — it is never reinterpreted into a Lash code.
    pub fn turn_code(&self) -> Option<TurnFailureCode> {
        self.0
            .namespace
            .is_lash_vocabulary()
            .then(|| TurnFailureCode::from_wire(&self.0.spelling))
    }

    /// Decode a wire spelling: `"<namespace>:<spelling>"`, split at the first
    /// colon with the remainder kept verbatim.
    ///
    /// This is the trusted decode — for rows Lash itself journaled and for
    /// serde of those durable records. Decode never errors. A namespaced
    /// value keeps both halves verbatim — an unrecognized namespace is
    /// retained, not rewritten into a Lash one. A bare (pre-cutover)
    /// spelling decodes as a lash code when it parses as a
    /// [`TurnFailureCode`] arm and as a provider code otherwise.
    ///
    /// For spellings a foreign author delivered, use
    /// [`FailureCode::from_foreign_wire`] instead — it never grants a
    /// reserved namespace.
    pub fn from_wire(spelling: &str) -> Self {
        match spelling.split_once(':') {
            Some((namespace, spelling)) => Self::pair(Namespace::from_wire(namespace), spelling),
            None => match TurnFailureCode::from_wire(spelling) {
                TurnFailureCode::Other(_) => Self::provider(spelling),
                code => Self::lash(code),
            },
        }
    }

    /// Decode a wire spelling a foreign author delivered — a host, a plugin,
    /// or an await-event resolver. Unlike [`FailureCode::from_wire`], this
    /// never grants reserved namespace ownership: a value claiming `lash`,
    /// `provider`, `adapter`, or `refusal` lands in the [`Namespace::FOREIGN`]
    /// namespace with its claim preserved verbatim, and so does a bare
    /// spelling with no namespace of its own. Every other namespaced value
    /// keeps both halves verbatim.
    pub fn from_foreign_wire(spelling: &str) -> Self {
        match spelling.split_once(':') {
            Some((namespace, rest)) if !Namespace::from_wire(namespace).is_reserved() => {
                Self::pair(Namespace::from_wire(namespace), rest)
            }
            _ => Self::pair(Namespace::FOREIGN, spelling),
        }
    }
}

impl From<TurnFailureCode> for FailureCode {
    fn from(code: TurnFailureCode) -> Self {
        Self::lash(code)
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

impl From<crate::llm::types::LlmTerminalReason> for FailureCode {
    /// A completed call's terminal reason as a `lash`-namespaced failure
    /// code, via its [`TurnFailureCode`] arm.
    fn from(reason: crate::llm::types::LlmTerminalReason) -> Self {
        Self::lash(TurnFailureCode::from(reason))
    }
}

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
    use super::{FailureCode, Namespace, TurnFailureCode, TurnFailureKind};

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

    /// Namespaced failure codes round-trip; decode never errors and never
    /// lands a foreign spelling in `lash`.
    #[test]
    fn failure_code_decode_table() {
        // Namespaced values keep both halves verbatim and round-trip through
        // `to_string` and serde.
        let namespaced = [
            ("lash:timeout", "lash", "timeout"),
            (
                "provider:insufficient_quota",
                "provider",
                "insufficient_quota",
            ),
            ("adapter:timeout", "adapter", "timeout"),
            (
                "refusal:unsafe_retry_after_output_started",
                "refusal",
                "unsafe_retry_after_output_started",
            ),
            ("agent_workbench:spend_cap", "agent_workbench", "spend_cap"),
            // The remainder after the first colon is the spelling, verbatim.
            ("plugin:hook:failed", "plugin", "hook:failed"),
            ("provider:", "provider", ""),
            (":dangling", "", "dangling"),
        ];
        for (wire, namespace, spelling) in namespaced {
            let code = FailureCode::from_wire(wire);
            assert_eq!(code.namespace().as_str(), namespace, "{wire}");
            assert_eq!(code.spelling(), spelling, "{wire}");
            assert_eq!(code.to_string(), wire, "{wire}");
            let json = serde_json::to_string(&code).expect("serialize");
            assert_eq!(
                serde_json::from_str::<FailureCode>(&json).expect("deserialize"),
                code
            );
        }

        // A bare spelling is the pre-cutover form: a known Lash arm decodes as
        // lash vocabulary, anything else stays foreign under `provider`.
        let bare = [
            ("timeout", "lash", Some(TurnFailureCode::Timeout)),
            (
                "unsafe_retry_after_output_started",
                "lash",
                Some(TurnFailureCode::UnsafeRetryAfterOutputStarted),
            ),
            ("insufficient_quota", "provider", None),
            ("429", "provider", None),
            ("", "provider", None),
        ];
        for (wire, namespace, turn_code) in bare {
            let code = FailureCode::from_wire(wire);
            assert_eq!(code.namespace().as_str(), namespace, "{wire}");
            assert_eq!(code.turn_code(), turn_code, "{wire}");
        }

        // A foreign spelling never lands in `lash`, and `turn_code` never
        // parses outside the workspace's own namespaces — including a
        // spelling that collides with a Lash arm.
        let foreign = FailureCode::from_wire("host:timeout");
        assert_eq!(foreign.turn_code(), None);
        let pre_cutover = FailureCode::from_wire("adapter:timeout");
        assert_eq!(pre_cutover.turn_code(), Some(TurnFailureCode::Timeout));
    }

    #[test]
    fn lash_codes_mint_only_through_turn_failure_code() {
        let code = FailureCode::lash(TurnFailureCode::Timeout);
        assert_eq!(code.namespace(), &Namespace::LASH);
        assert_eq!(code.to_string(), "lash:timeout");
        assert_eq!(code.turn_code(), Some(TurnFailureCode::Timeout));

        let host = Namespace::host("agent_workbench").expect("valid host namespace");
        let code = FailureCode::foreign(host.clone(), "spend_cap").expect("foreign namespace");
        assert_eq!(code.namespace(), &host);
        assert_eq!(code.to_string(), "agent_workbench:spend_cap");
        assert_eq!(code.turn_code(), None);
    }

    /// Foreign ingress never grants a reserved namespace: values claiming
    /// `lash`/`provider`/`adapter`/`refusal` and bare spellings land in the
    /// `foreign` namespace with their claim preserved, while a genuine
    /// foreign pair keeps both halves.
    #[test]
    fn foreign_wire_decode_never_grants_a_reserved_namespace() {
        let cases = [
            ("lash:timeout", "foreign", "lash:timeout"),
            ("adapter:timeout", "foreign", "adapter:timeout"),
            (
                "refusal:unsafe_retry_after_output_started",
                "foreign",
                "refusal:unsafe_retry_after_output_started",
            ),
            (
                "provider:insufficient_quota",
                "foreign",
                "provider:insufficient_quota",
            ),
            ("timeout", "foreign", "timeout"),
            ("agent_workbench:spend_cap", "agent_workbench", "spend_cap"),
            ("plugin:hook:failed", "plugin", "hook:failed"),
        ];
        for (wire, namespace, spelling) in cases {
            let code = FailureCode::from_foreign_wire(wire);
            assert_eq!(code.namespace().as_str(), namespace, "{wire}");
            assert_eq!(code.spelling(), spelling, "{wire}");
            assert_eq!(code.to_string(), format!("{namespace}:{spelling}"));
            assert_eq!(code.turn_code(), None, "{wire}");
        }
    }

    /// `FailureCode::foreign` refuses every namespace a foreign author may
    /// not mint, so host code cannot forge Lash or provider vocabulary.
    #[test]
    fn foreign_construction_rejects_reserved_namespaces() {
        for reserved in ["lash", "provider", "adapter", "refusal"] {
            let namespace = Namespace::from_wire(reserved);
            assert!(
                FailureCode::foreign(namespace, "timeout").is_err(),
                "{reserved} must be rejected"
            );
        }
    }

    #[test]
    fn all_named_covers_every_named_arm() {
        assert_eq!(
            TurnFailureCode::ALL_NAMED.len(),
            71,
            "a new named arm must be added to ALL_NAMED"
        );
        for code in TurnFailureCode::ALL_NAMED {
            assert!(!matches!(code, TurnFailureCode::Other(_)), "{code:?}");
            assert_eq!(
                &TurnFailureCode::from_wire(code.as_str()),
                code,
                "every named arm must round-trip through from_wire"
            );
        }
    }

    #[test]
    fn host_namespace_rejects_reserved_and_malformed_names() {
        for reserved in ["lash", "provider", "adapter", "refusal"] {
            assert!(
                Namespace::host(reserved).is_err(),
                "{reserved} must be rejected"
            );
        }
        for malformed in [
            "",
            "Lash",
            "9lives",
            "ns:colon",
            "with space",
            "-lead",
            ".lead",
            "still_valid_but_too_long_namespace_name_that_exceeds_the_limit_x",
        ] {
            assert!(
                Namespace::host(malformed).is_err(),
                "{malformed} must be rejected"
            );
        }
        for valid in [
            "agent_workbench",
            "figments",
            "acme-2",
            "host.name",
            "still_valid_but_exactly_long_enough_namespace_name_at_the_limit",
        ] {
            assert!(Namespace::host(valid).is_ok(), "{valid} must be accepted");
        }
    }
}
