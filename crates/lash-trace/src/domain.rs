//! Event payloads that report token usage: the typed terminals of durable
//! domain operations and of single provider attempts, and a decoded stream
//! event.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{TraceFailureCode, TraceLlmAttemptOutcome, TraceNormalizedError};

/// One provider request attempt, as the provider seam reported it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLlmAttempt {
    /// The attempt's ordinal within its call, from 1.
    pub ordinal: u32,
    /// The provider that served the attempt, when the provider seam named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model the request named.
    pub request_model: String,
    /// The model the response named, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Wall-clock epoch milliseconds the attempt was dispatched, when the
    /// provider seam timed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Wall-clock epoch milliseconds the attempt ended, when the provider
    /// seam timed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    pub outcome: TraceLlmAttemptOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TraceNormalizedError>,
    /// Provider-reported usage only. Absence is not zero usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TraceTokenUsage>,
}

/// The durable domain operation a [`TraceEvent::DomainCompleted`] ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceDomainOperation {
    Run,
    Process,
    ProcessSegment,
    Send,
    ToolIntent,
}

/// How a durable domain operation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceDomainStatus {
    Completed,
    Failed,
    Cancelled,
    /// The operation ended without finishing its work here: a segment that
    /// yielded to a wait or handed over, a send that was reused.
    Yielded,
}

/// The terminal of one durable domain operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceDomainCompletion {
    pub operation: TraceDomainOperation,
    /// Wall-clock epoch milliseconds the operation's scope retained as its
    /// start.
    pub started_at_ms: u64,
    pub status: TraceDomainStatus,
    /// The provider the operation's work was served by, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The typed kind of a tool intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_kind: Option<String>,
    /// The typed failure code of a failed operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<TraceFailureCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TraceTokenUsage>,
}

impl TraceDomainCompletion {
    pub fn new(
        operation: TraceDomainOperation,
        started_at_ms: u64,
        status: TraceDomainStatus,
    ) -> Self {
        Self {
            operation,
            started_at_ms,
            status,
            provider: None,
            model: None,
            tool_name: None,
            tool_call_id: None,
            intent_kind: None,
            error_code: None,
            usage: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceTokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceRuntimeStreamEvent {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The streamed block's own identity. Several blocks can share one
    /// `item_id` (OpenAI summary parts of one reasoning item), so block-level
    /// granularity needs this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TraceTokenUsage>,
}
