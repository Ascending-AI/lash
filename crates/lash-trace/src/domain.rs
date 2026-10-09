//! Event payloads that report token usage: the typed terminals of durable
//! domain operations and of single provider attempts, and a decoded stream
//! event.

use lash_sansio::llm::types::StreamBlockEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use lash_sansio::llm::types::LlmUsage;

use crate::TraceFailureCode;

/// What the provider seam observed around one dispatched attempt. The
/// attempt's facts are its sealed `AttemptRecord`, reported beside this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceAttemptObservation {
    /// The provider that served the attempt, when the provider seam named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model the request named.
    pub request_model: String,
    /// Wall-clock epoch milliseconds the attempt was dispatched, when the
    /// provider seam timed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Wall-clock epoch milliseconds the attempt was sealed, when the
    /// provider seam timed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
}

/// The durable domain operation a [`TraceEvent::DomainCompleted`] ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceDomainOperation {
    Process,
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
    pub usage: Option<LlmUsage>,
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

/// One decoded provider stream event, in the order the runtime folded it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceRuntimeStreamEvent {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub payload: TraceRuntimeStreamPayload,
}

/// What a [`TraceRuntimeStreamEvent`] reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceRuntimeStreamPayload {
    /// One step of a streamed block's lifecycle, as hosts saw it: the same
    /// payload the session stream and turn activity carry, with the block's
    /// full identity. Several blocks can share one item (OpenAI summary parts
    /// of one reasoning item); the block's `ordinal` orders them.
    Block {
        event: StreamBlockEvent,
        /// The provider's text before stream transforms rewrote it, when it
        /// was observed apart from the event's own text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_text: Option<String>,
    },
    /// A completed assistant-text part of the response.
    TextPart {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
    },
    /// A completed reasoning item of the response.
    ReasoningPart {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
    },
    /// A completed tool call of the response.
    ToolCallPart {
        call_id: String,
        tool_name: String,
        input_json: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
    },
    /// Usage the provider reported mid-stream.
    Usage { usage: LlmUsage },
}
