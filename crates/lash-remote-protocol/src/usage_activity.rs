//! Token usage accounting and the streaming turn-activity event vocabulary.

use lash_sansio::TurnId;
use lash_sansio::llm::types::{StreamBlockIdentity, StreamBlockKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::llm::{RemoteLlmCallRecord, validate_llm_call_record};
use crate::registry_errors::{RemoteProtocolError, require_non_empty};
use crate::{
    RemoteAdmissionBoundary, RemotePluginMessage, RemoteTurnCause, RemoteTurnInputCheckpoint,
};

// Wire mirror of the runtime usage counters. This is a deliberately versioned
// protocol boundary, kept independent of the internal types so the wire format
// stays stable across internal refactors. The `From` converters in
// `core_conversions::llm` destructure their `lash_core` source exhaustively
// (no `..`), so adding a counter upstream is a compile error until it is mirrored
// here too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl RemoteUsage {
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnActivity {
    pub sequence: u64,
    pub id: String,
    pub correlation_id: String,
    #[serde(flatten)]
    pub event: RemoteTurnEvent,
}

impl RemoteTurnActivity {
    /// Activities nested in reports or observations remain bare bodies.
    pub fn encode_json(
        &self,
        negotiated: &crate::Negotiated,
    ) -> Result<Vec<u8>, serde_json::Error> {
        crate::Envelope::at(negotiated, self).encode_json()
    }

    /// A previous peer cannot deserialize a newly added `type` variant. Probe
    /// the sibling version first so that mixed-version streams return
    /// [`RemoteProtocolError::Unsupported`] instead of an
    /// opaque unknown-variant error.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        let activity =
            crate::Envelope::<Self>::decode_json(bytes, crate::REMOTE_PROTOCOL)?.into_body();
        activity.validate()?;
        Ok(activity)
    }

    #[cfg(test)]
    pub(crate) fn decode_json_expecting_protocol_version(
        bytes: &[u8],
        expected_version: u32,
    ) -> Result<Self, RemoteProtocolError> {
        let activity = crate::Envelope::<Self>::decode_json_expecting_protocol_version(
            bytes,
            expected_version,
        )?
        .into_body();
        activity.validate()?;
        Ok(activity)
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        require_non_empty("RemoteTurnActivity", "id", &self.id)?;
        require_non_empty("RemoteTurnActivity", "correlation_id", &self.correlation_id)?;
        match &self.event {
            RemoteTurnEvent::TurnStarted { turn_id } => {
                require_non_empty("RemoteTurnEvent::TurnStarted", "turn_id", turn_id)?;
            }
            RemoteTurnEvent::TurnInputApplied { applications } => {
                for application in applications {
                    application.validate()?;
                }
            }
            RemoteTurnEvent::ModelCallRecorded { record } => validate_llm_call_record(record)?,
            RemoteTurnEvent::StoppedPartialAvailable { summary } => {
                let context = "RemoteTurnEvent::StoppedPartialAvailable";
                require_non_empty(context, "session_id", &summary.id.session_id)?;
                require_non_empty(context, "root", &summary.id.root)?;
                require_non_empty(context, "turn_id", &summary.id.turn_id)?;
            }
            RemoteTurnEvent::CodeBlockCompleted {
                error: Some(error), ..
            } => {
                require_non_empty("RemoteCellFailure", "message", &error.message)?;
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCellFailureKind {
    Policy,
    Program,
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteCellFailure {
    pub kind: RemoteCellFailureKind,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnEvent {
    TurnStarted {
        turn_id: TurnId,
    },
    ModelRequestStarted {
        protocol_iteration: usize,
    },
    CheckpointRecorded {
        protocol_iteration: usize,
    },
    AssistantProseDelta {
        text: String,
        block: StreamBlockIdentity,
    },
    ReasoningDelta {
        text: String,
        block: StreamBlockIdentity,
    },
    StreamBlockStarted {
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
    },
    StreamBlockCompleted {
        kind: StreamBlockKind,
        block: StreamBlockIdentity,
        text: String,
    },
    ModelAttemptReset {
        assistant_prose_correlation_ids: Vec<String>,
        reasoning_correlation_ids: Vec<String>,
    },
    ModelCallRecorded {
        record: RemoteLlmCallRecord,
    },
    CodeBlockStarted {
        language: String,
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    CodeBlockCompleted {
        language: String,
        output: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RemoteCellFailure>,
        success: bool,
        duration_ms: u64,
        tool_call_ids: Vec<lash_sansio::ToolCallId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    /// A tool call started. `call_id` is lash's identity for the call;
    /// `provider_call_id` is the model provider's correlation, when a model
    /// issued the call.
    ToolCallStarted {
        call_id: lash_sansio::ToolCallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolCallCompleted {
        call_id: lash_sansio::ToolCallId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: serde_json::Value,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolIntentOutcome {
        call_id: lash_sansio::ToolCallId,
        outcome: crate::RemoteToolIntentExecutionOutcome,
    },
    FinalValue {
        value: serde_json::Value,
    },
    ToolValue {
        tool_name: String,
        value: serde_json::Value,
    },
    Usage {
        protocol_iteration: usize,
        usage: RemoteUsage,
        cumulative: RemoteUsage,
    },
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    TurnInputApplied {
        applications: Vec<crate::observations::RemoteTurnInputApplication>,
    },
    QueuedWorkStarted {
        boundary: RemoteAdmissionBoundary,
        batch_ids: Vec<String>,
        causes: Vec<RemoteTurnCause>,
    },
    QueuedMessagesCommitted {
        messages: Vec<RemotePluginMessage>,
        checkpoint: RemoteTurnInputCheckpoint,
    },
    PluginRuntime {
        plugin_id: String,
        event: serde_json::Value,
    },
    Error {
        message: String,
    },
    /// One progress chunk a running tool reported, published once it was
    /// persisted to its turn's capture (ADR 0114 §2.2).
    ToolOutputProgress {
        call_id: lash_sansio::ToolCallId,
        chunk: lash_sansio::ToolOutputChunk,
    },
    /// A stopped turn's partial is durable (ADR 0114 §5.2). It holds identity
    /// and facts, never payload: the host reads the partial from the turn's
    /// report or by the root.
    StoppedPartialAvailable {
        summary: lash_sansio::StoppedPartialSummary,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_cell_failure_refuses_an_empty_message_by_field_name() {
        let activity = RemoteTurnActivity {
            sequence: 1,
            id: "activity-1".to_string(),
            correlation_id: "correlation-1".to_string(),
            event: RemoteTurnEvent::CodeBlockCompleted {
                language: "typescript".to_string(),
                output: String::new(),
                error: Some(RemoteCellFailure {
                    kind: RemoteCellFailureKind::Host,
                    message: "  ".to_string(),
                }),
                success: false,
                duration_ms: 0,
                tool_call_ids: Vec::new(),
                graph_key: None,
            },
        };

        assert!(matches!(
            activity.validate(),
            Err(RemoteProtocolError::MissingRequiredField {
                type_name: "RemoteCellFailure",
                field: "message",
            })
        ));
    }
}
