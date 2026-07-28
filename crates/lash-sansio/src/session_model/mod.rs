pub mod message;
pub mod prompt;

pub use message::{
    BaseRenderCache, Message, MessageRole, MessageSequence, Part, PartAttachment, PartKind,
    PruneState, RenderedPrompt, append_rendered_prompt, messages_are_prompt_resume_safe,
    render_prompt, render_transcript_prompt, shared_parts,
};
pub use prompt::{
    MAIN_AGENT_INTRO, PromptBuiltin, PromptLayer, PromptSlot, PromptSlotLayer, PromptTemplate,
    PromptTemplateEntry, PromptTemplateSection, ResolvedPromptLayer, default_prompt_template,
    resolve_prompt_layers,
};

use std::sync::Arc;

use crate::MessageOrigin;
use crate::ToolDefinition;
use crate::llm::types::LlmToolSpec;
use crate::plugin::{CheckpointKind, PluginMessage, PluginRuntimeEvent};

/// Durable protocol payload stored in session history.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProtocolEvent {
    pub plugin_id: String,
    pub payload: serde_json::Value,
}

impl ProtocolEvent {
    pub fn typed<T>(plugin_id: impl Into<String>, event: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self {
            plugin_id: plugin_id.into(),
            payload: serde_json::to_value(event)?,
        })
    }

    pub fn decode<T>(&self, expected_plugin_id: &str) -> Result<Option<T>, serde_json::Error>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        if self.plugin_id != expected_plugin_id {
            return Ok(None);
        }
        serde_json::from_value(self.payload.clone()).map(Some)
    }
}

/// Typed node accepted at session-graph append boundaries.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// justification: append nodes are public durable DTOs kept inline to preserve their established Rust construction API.
#[allow(clippy::large_enum_variant)]
pub enum SessionAppendNode {
    Message {
        message: PluginMessage,
    },
    ProtocolEvent {
        event: ProtocolEvent,
    },
    Plugin {
        plugin_type: String,
        #[serde(default)]
        body: serde_json::Value,
    },
}

impl SessionAppendNode {
    pub fn message(message: PluginMessage) -> Self {
        Self::Message { message }
    }

    pub fn plugin(plugin_type: impl Into<String>, body: serde_json::Value) -> Self {
        Self::Plugin {
            plugin_type: plugin_type.into(),
            body,
        }
    }

    pub fn protocol_event(event: ProtocolEvent) -> Self {
        Self::ProtocolEvent { event }
    }
}

/// Durable semantic history stored in the session graph and replayed into
/// future prompts. Unlike [`SessionStreamEvent`], these records are committed
/// state rather than transient UI/progress signals.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
// justification: the generic protocol event is caller-defined durable state and must retain the public inline history shape.
#[allow(clippy::large_enum_variant)]
pub enum SessionHistoryRecord<PE = ()> {
    Conversation(ConversationRecord),
    Protocol(PE),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub role: MessageRole,
    pub parts: Arc<Vec<Part>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AcceptedInjectedTurnInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub message: PluginMessage,
}

impl ConversationRecord {
    pub fn from_message(message: Message) -> Self {
        Self {
            id: message.id,
            role: message.role,
            parts: message.parts,
            origin: message.origin,
        }
    }

    pub fn to_message(&self) -> Message {
        Message {
            id: self.id.clone(),
            role: self.role,
            parts: Arc::clone(&self.parts),
            origin: self.origin.clone(),
        }
    }
}

/// Token usage statistics from an LLM call.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl TokenUsage {
    pub fn total(&self) -> i64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_write_input_tokens
    }

    pub fn input_total(&self) -> i64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_write_input_tokens
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_write_input_tokens += other.cache_write_input_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
    }
}

/// Structured error payload carried on [`SessionStreamEvent::Error`] (and
/// [`SessionStreamEvent::RetryStatus`]).
///
/// Durability: this type appears inside persisted session snapshots and turn
/// checkpoints, so every field added after the initial shape must stay
/// additive — `#[serde(default)]` on decode and
/// `#[serde(skip_serializing_if = "Option::is_none")]` on encode — to keep
/// old snapshots decodable and new snapshots readable by older readers.
/// Transient runtime-stream signal for live consumers.
///
/// These events may be partial, duplicated, or display-only and are not proof
/// of durable session history. Persisted semantic history uses
/// [`SessionHistoryRecord`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorEnvelope {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<crate::llm::types::LlmTerminalReason>,
    pub user_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Whether the failing operation is safe to retry, when the source
    /// carries a typed signal (provider transports classify retryability).
    /// `None` means the source did not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Typed provider-failure classification, set only when the error came
    /// from an LLM provider/transport failure whose kind was classified
    /// (an unclassified `Unknown` kind is surfaced as `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_failure_kind: Option<crate::llm::types::ProviderFailureKind>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
// justification: this public streaming DTO stays inline to avoid per-event allocation and preserve consumer pattern matching.
#[allow(clippy::large_enum_variant)]
pub enum SessionStreamEvent {
    #[serde(rename = "text_delta")]
    TextDelta { content: String },
    /// Streaming update for the model's reasoning summary ("thinking").
    /// The UI renders these incrementally in a muted/italic style;
    /// reasoning content is never fed back to the model on subsequent
    /// turns.
    #[serde(rename = "reasoning_delta")]
    ReasoningDelta { content: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: crate::ToolCallOutput,
        duration_ms: u64,
    },
    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
    },
    #[serde(rename = "message")]
    Message { text: String, kind: String },
    #[serde(rename = "llm_request")]
    LlmRequest {
        protocol_iteration: usize,
        message_count: usize,
        tool_list: String,
    },
    #[serde(rename = "llm_response")]
    LlmResponse {
        protocol_iteration: usize,
        content: String,
        duration_ms: u64,
    },
    #[serde(rename = "token_usage")]
    TokenUsage {
        protocol_iteration: usize,
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    #[serde(rename = "child_token_usage")]
    ChildTokenUsage {
        session_id: String,
        source: String,
        model: String,
        protocol_iteration: usize,
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    #[serde(rename = "retry_status")]
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ErrorEnvelope>,
    },
    #[serde(rename = "injected_turn_input_accepted")]
    InjectedTurnInputAccepted {
        inputs: Vec<AcceptedInjectedTurnInput>,
        checkpoint: CheckpointKind,
    },
    #[serde(rename = "injected_messages_committed")]
    InjectedMessagesCommitted {
        messages: Vec<PluginMessage>,
        checkpoint: CheckpointKind,
    },
    #[serde(rename = "plugin_event")]
    PluginEvent {
        plugin_id: String,
        event: PluginRuntimeEvent,
    },
    /// Semantic result for a completed turn. `Done` remains the machine
    /// lifecycle marker emitted after this event.
    #[serde(rename = "turn_outcome")]
    TurnOutcome { outcome: TurnOutcome },
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        envelope: Option<ErrorEnvelope>,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Finished(TurnFinish),
    AgentFrameSwitch {
        frame_id: String,
        task: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        initial_nodes: Vec<SessionAppendNode>,
    },
    Stopped(TurnStop),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFinish {
    AssistantMessage {
        text: String,
    },
    FinalValue {
        value: serde_json::Value,
    },
    ToolValue {
        tool_name: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStop {
    Cancelled,
    Incomplete,
    InvalidInput,
    MaxTurns,
    ToolFailure,
    ProviderError,
    PluginAbort,
    RuntimeError,
    SubmittedError {
        value: serde_json::Value,
    },
    ToolError {
        tool_name: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnTerminationPolicyState {
    turn_limit_final_scheduled: bool,
}

impl Default for TurnTerminationPolicyState {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnTerminationPolicyState {
    pub fn new() -> Self {
        Self {
            turn_limit_final_scheduled: false,
        }
    }

    pub fn should_force_exit_after_grace_turn(&self) -> bool {
        self.turn_limit_final_scheduled
    }

    pub fn turn_limit_final_to_schedule(
        &self,
        protocol_iteration: usize,
        protocol_run_offset: usize,
        max_turns: Option<usize>,
    ) -> Option<usize> {
        if self.turn_limit_final_scheduled {
            return None;
        }
        let max = max_turns?;
        if protocol_iteration < protocol_run_offset + max {
            return None;
        }
        Some(max)
    }

    pub fn mark_turn_limit_final_scheduled(&mut self) {
        self.turn_limit_final_scheduled = true;
    }
}

pub fn make_error_envelope(
    kind: &str,
    code: Option<&str>,
    terminal_reason: Option<crate::llm::types::LlmTerminalReason>,
    user_message: impl Into<String>,
    raw: Option<String>,
) -> ErrorEnvelope {
    let user_message = user_message.into();
    ErrorEnvelope {
        kind: kind.to_string(),
        code: code.map(str::to_string),
        terminal_reason,
        user_message,
        raw: raw.map(|s| truncate_raw_error(s.trim())),
        retryable: None,
        provider_failure_kind: None,
    }
}

pub fn make_error_event(
    kind: &str,
    code: Option<&str>,
    user_message: impl Into<String>,
    raw: Option<String>,
) -> SessionStreamEvent {
    let user_message = user_message.into();
    SessionStreamEvent::Error {
        message: user_message.clone(),
        envelope: Some(make_error_envelope(kind, code, None, user_message, raw)),
    }
}

pub fn truncate_raw_error(s: &str) -> String {
    const MAX_RAW: usize = 4000;
    let raw_len = s.chars().count();
    if raw_len <= MAX_RAW {
        return s.to_string();
    }
    let keep = MAX_RAW / 2;
    let head = s.chars().take(keep).collect::<String>();
    let tail = s
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let omitted = raw_len.saturating_sub(keep * 2);
    format!("{head}\n\n... ({omitted} chars omitted) ...\n\n{tail}")
}

pub fn reassign_part_ids(message_id: &str, parts: &mut [Part]) {
    for (idx, part) in parts.iter_mut().enumerate() {
        part.id = format!("{message_id}.p{idx}");
    }
}

pub fn model_tool_specs_iter<'a>(
    tools: impl IntoIterator<Item = &'a ToolDefinition>,
) -> Vec<LlmToolSpec> {
    tools
        .into_iter()
        .map(|tool| {
            let model_tool = tool.model_tool();
            LlmToolSpec {
                name: model_tool.name,
                description: model_tool.description,
                input_schema: model_tool.input_schema,
                output_schema: model_tool.output_schema,
            }
        })
        .collect()
}

pub fn model_tool_specs(tools: &[ToolDefinition]) -> Vec<LlmToolSpec> {
    model_tool_specs_iter(tools.iter())
}

#[cfg(test)]
mod tests {
    use super::{ErrorEnvelope, SessionStreamEvent, TurnOutcome};
    use crate::llm::types::{LlmTerminalReason, ProviderFailureKind};

    // ─── ErrorEnvelope durable-snapshot compatibility ──────────────────
    //
    // `ErrorEnvelope` is persisted inside session snapshots and turn
    // checkpoints. The retryability fields added after the initial shape
    // must decode from legacy JSON (absent fields → `None`) and must not
    // appear on the wire when unset, so old readers keep decoding new
    // snapshots too.

    #[test]
    fn error_envelope_decodes_legacy_snapshot_without_retryability_fields() {
        let legacy = r#"{
            "kind":"llm_provider",
            "code":"429",
            "terminal_reason":"provider_error",
            "user_message":"LLM error: rate limited",
            "raw":"{\"error\":\"rate_limited\"}"
        }"#;
        let envelope: ErrorEnvelope = serde_json::from_str(legacy).expect("legacy envelope");
        assert_eq!(envelope.kind, "llm_provider");
        assert_eq!(envelope.retryable, None);
        assert_eq!(envelope.provider_failure_kind, None);

        // The legacy shape embedded in a persisted `SessionStreamEvent::Error`
        // record decodes the same way.
        let legacy_event = r#"{
            "type":"error",
            "message":"LLM error: rate limited",
            "envelope":{"kind":"llm_provider","user_message":"LLM error: rate limited"}
        }"#;
        let event: SessionStreamEvent = serde_json::from_str(legacy_event).expect("legacy event");
        match event {
            SessionStreamEvent::Error { envelope, .. } => {
                let envelope = envelope.expect("envelope");
                assert_eq!(envelope.retryable, None);
                assert_eq!(envelope.provider_failure_kind, None);
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[test]
    fn error_envelope_roundtrips_retryability_fields() {
        let envelope = ErrorEnvelope {
            kind: "llm_provider".to_string(),
            code: Some("429".to_string()),
            terminal_reason: Some(LlmTerminalReason::ProviderError),
            user_message: "LLM error: rate limited".to_string(),
            raw: None,
            retryable: Some(true),
            provider_failure_kind: Some(ProviderFailureKind::Quota),
        };
        let json = serde_json::to_value(&envelope).expect("serialize envelope");
        assert_eq!(json["retryable"], serde_json::json!(true));
        assert_eq!(json["provider_failure_kind"], serde_json::json!("quota"));
        let decoded: ErrorEnvelope = serde_json::from_value(json).expect("decode envelope");
        assert_eq!(decoded.retryable, Some(true));
        assert_eq!(
            decoded.provider_failure_kind,
            Some(ProviderFailureKind::Quota)
        );
    }

    #[test]
    fn error_envelope_omits_unset_retryability_fields_on_the_wire() {
        let envelope = ErrorEnvelope {
            kind: "plugin".to_string(),
            code: Some("plugin_abort".to_string()),
            terminal_reason: None,
            user_message: "stopped".to_string(),
            raw: None,
            retryable: None,
            provider_failure_kind: None,
        };
        let json = serde_json::to_value(&envelope).expect("serialize envelope");
        let object = json.as_object().expect("object");
        assert!(!object.contains_key("retryable"));
        assert!(!object.contains_key("provider_failure_kind"));
    }

    #[test]
    fn provider_failure_kind_decodes_unknown_future_codes() {
        // Forward compatibility: a snapshot written by a newer runtime with a
        // kind this build does not know decodes as `Unknown`.
        let decoded: ProviderFailureKind =
            serde_json::from_value(serde_json::json!("some_future_kind")).expect("future kind");
        assert_eq!(decoded, ProviderFailureKind::Unknown);
        for kind in [
            ProviderFailureKind::Transport,
            ProviderFailureKind::Timeout,
            ProviderFailureKind::Http,
            ProviderFailureKind::Stream,
            ProviderFailureKind::Auth,
            ProviderFailureKind::Validation,
            ProviderFailureKind::Quota,
            ProviderFailureKind::Unsupported,
            ProviderFailureKind::Unknown,
        ] {
            let json = serde_json::to_value(kind).expect("serialize kind");
            assert_eq!(json, serde_json::json!(kind.code()));
            let round: ProviderFailureKind = serde_json::from_value(json).expect("decode kind");
            assert_eq!(round, kind);
        }
    }

    #[test]
    fn agent_frame_switch_decodes_legacy_event_without_initial_nodes() {
        let legacy = r#"{
            "type":"turn_outcome",
            "outcome":{
                "agent_frame_switch":{
                    "frame_id":"frame-2",
                    "task":"continue"
                }
            }
        }"#;
        let event: SessionStreamEvent =
            serde_json::from_str(legacy).expect("legacy frame switch event");
        match event {
            SessionStreamEvent::TurnOutcome {
                outcome:
                    TurnOutcome::AgentFrameSwitch {
                        frame_id,
                        task,
                        initial_nodes,
                    },
            } => {
                assert_eq!(frame_id, "frame-2");
                assert_eq!(task, "continue");
                assert!(initial_nodes.is_empty());
            }
            other => panic!("expected agent-frame switch event, got {other:?}"),
        }
    }
}
