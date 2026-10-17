//! Event payloads that report token usage: the typed terminals of durable
//! domain operations and of single provider attempts, and a decoded stream
//! event.

use lash_protocol_types::{llm::types::LlmUsage, llm::types::StreamBlockEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum TraceDomainSubject {
    Process {
        kind: String,
        label: Option<String>,
    },
    Workflow {
        kind: String,
        name: Option<String>,
    },
    /// One occurrence of a workflow node, by its whole effect identity.
    WorkflowNode {
        at: lash_protocol_types::EffectIdentity,
    },
    ToolIntent {
        kind: lash_protocol_types::ToolIntentKind,
    },
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
    pub subject: TraceDomainSubject,
    /// Wall-clock epoch milliseconds the operation's scope retained as its
    /// start.
    pub started_at_ms: u64,
    pub status: TraceDomainStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The typed failure code of a failed operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<TraceFailureCode>,
}

impl TraceDomainCompletion {
    pub fn new(subject: TraceDomainSubject, started_at_ms: u64, status: TraceDomainStatus) -> Self {
        Self {
            subject,
            started_at_ms,
            status,
            tool_name: None,
            tool_call_id: None,
            error_code: None,
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

/// Operational request facts retained until a logical model call closes.
/// These contain no captured prompt, tool schema or host telemetry label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceModelCall {
    pub provider: Option<String>,
    pub request_model: String,
    pub stream: bool,
    pub reasoning: Option<String>,
    pub output_type: Option<String>,
    pub generation: lash_protocol_types::llm::types::GenerationOptions,
}

/// One real provider attempt's trace identity and provider-seam clocks.
/// Execution-local: none of these facts enters the stored execution ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceModelAttemptSpan {
    pub span_id: crate::W3cSpanId,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub provider: String,
    /// Lash stopped awaiting the host's terminal callback before it returned.
    pub terminal_delivery_timed_out: bool,
}

/// The execution attempt's facts, with trace metadata only for an attempt
/// that reached provider dispatch. A synthetic cut between attempts has no span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceModelAttempt {
    #[serde(flatten)]
    pub attempt: lash_protocol_types::llm::types::AttemptRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<TraceModelAttemptSpan>,
}

impl From<lash_protocol_types::llm::types::AttemptRecord> for TraceModelAttempt {
    fn from(attempt: lash_protocol_types::llm::types::AttemptRecord) -> Self {
        Self {
            attempt,
            span: None,
        }
    }
}

impl std::ops::Deref for TraceModelAttempt {
    type Target = lash_protocol_types::llm::types::AttemptRecord;
    fn deref(&self) -> &Self::Target {
        &self.attempt
    }
}
