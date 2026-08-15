//! Token usage accounting and the streaming turn-activity event vocabulary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ensure_protocol_version;
use crate::llm::{RemoteLlmCallRecord, validate_llm_call_record};
use crate::registry_errors::{RemoteProtocolError, require_non_empty};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTokenLedgerEntry {
    pub source: String,
    pub model: String,
    pub usage: RemoteUsage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteTurnActivity {
    pub protocol_version: u32,
    pub sequence: u64,
    pub id: String,
    pub correlation_id: String,
    #[serde(flatten)]
    pub event: RemoteTurnEvent,
}

impl RemoteTurnActivity {
    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        require_non_empty("RemoteTurnActivity", "id", &self.id)?;
        require_non_empty("RemoteTurnActivity", "correlation_id", &self.correlation_id)?;
        match &self.event {
            RemoteTurnEvent::TurnInputApplied { applications } => {
                for application in applications {
                    application.validate()?;
                }
            }
            RemoteTurnEvent::ModelCallRecorded { record } => validate_llm_call_record(record)?,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnEvent {
    ModelRequestStarted {
        protocol_iteration: usize,
    },
    AssistantProseDelta {
        text: String,
    },
    ReasoningDelta {
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
        error: Option<String>,
        success: bool,
        duration_ms: u64,
        tool_call_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
    },
    ToolCallStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: serde_json::Value,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
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
    ChildUsage {
        session_id: String,
        source: String,
        model: String,
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
    RuntimeDiagnostic {
        kind: String,
        data: serde_json::Value,
    },
    Error {
        message: String,
    },
}
