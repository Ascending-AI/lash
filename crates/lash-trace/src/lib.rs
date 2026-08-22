//! Durable diagnostics for the lash runtime: the [`TraceSink`] channel and its
//! record vocabulary.
//!
//! A [`TraceSink`] receives one [`TraceRecord`] per runtime event — session and
//! turn lifecycle, prompt builds, rolling-history decisions, LLM calls,
//! per-tool start/completion, token usage, protocol steps, and Lashlang
//! execution-graph updates. Each record
//! carries a [`TraceContext`] (session / turn / graph-node identity) plus a
//! tagged [`TraceEvent`] payload; [`TraceEvent::kind`] is the single source of
//! truth for the `type` tag string that consumers match on.
//!
//! [`JsonlTraceSink`] writes one JSON line per record at schema
//! [`TRACE_SCHEMA_VERSION`]; [`TeeTraceSink`] fans out to several sinks; and the
//! optional `otel` feature adds an `OtelTraceSink` that converts each record to
//! an OpenTelemetry span. This is the *durable diagnostics* reporting channel —
//! distinct from the app-facing `TurnActivity` stream and the low-level
//! `SessionStreamEvent` stream that the runtime crates expose.
//!
//! For the full map of reporting channels, guidance on when to consume which,
//! and the schema-evolution policy that governs [`TRACE_SCHEMA_VERSION`], see
//! `docs/reporting.html`; for the attach-a-sink how-to, see `docs/tracing.html`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod lashlang_graph;
#[cfg(feature = "otel")]
pub mod otel;

pub use lashlang_graph::{
    TraceLashlangEdgeSelection, TraceLashlangGraph, TraceLashlangGraphChildLink,
    TraceLashlangGraphEdge, TraceLashlangGraphNode, TraceLashlangGraphStore,
    TraceLashlangNodeStatus,
};

/// Version of the durable trace JSONL schema, written to
/// [`TraceRecord::schema_version`] on every record.
///
/// Bump rules (the normative reporting-schema policy lives in
/// `docs/reporting.html`):
///
/// - Adding a new [`TraceEvent`] variant is breaking for closed-enum readers
///   and **does** bump this version. Adding an optional
///   (`skip_serializing_if`) field is additive only for readers that ignore
///   unknown fields.
/// - Renaming a field, removing a field, or changing the meaning of an existing
///   field is a breaking change and **does** bump this version.
/// - The free-form [`TraceEvent::Custom`] and [`TraceEvent::ProtocolStep`]
///   payloads are opaque `serde_json::Value`; adding to or reshaping the data
///   inside them never forces a bump. (This is why the `exec_code_completed`
///   diagnostic's `tool_calls` payload was purely additive.)
///
/// Version 5 adds the `composition_changed` event.
/// Version 6 adds the `provider_replay_dropped` event with typed minting and
/// serving LLM Provider routes.
/// Version 7 renames the Lashlang execution records to language-tagged ones:
/// `language_execution` carries the language id, and the graph, identity, node
/// and status payloads lose their Lashlang-specific names.
pub const TRACE_SCHEMA_VERSION: u32 = 7;

/// A durable trace record was written under a schema this reader does not support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceSchemaVersionError {
    pub actual: u32,
    pub expected: u32,
}

impl std::fmt::Display for TraceSchemaVersionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unsupported trace schema version {}; expected {}",
            self.actual, self.expected
        )
    }
}

impl std::error::Error for TraceSchemaVersionError {}

/// Refuses a durable trace record whose exact schema version is unsupported.
pub fn ensure_trace_schema_version(actual: u32) -> Result<(), TraceSchemaVersionError> {
    if actual == TRACE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(TraceSchemaVersionError {
            actual,
            expected: TRACE_SCHEMA_VERSION,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceLevel {
    #[default]
    Standard,
    Extended,
}

impl TraceLevel {
    pub fn is_extended(self) -> bool {
        matches!(self, Self::Extended)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Stable id of the span this record represents (e.g. `turn:<session>:<turn>`,
    /// `llm:<call_id>`, `tool:<call_id>`). Populated by the runtime for turn /
    /// llm / tool / session records so a consumer can build a nested span tree
    /// from `(graph_node_id, parent_graph_node_id)` with a single `id -> span`
    /// map. Lashlang execution sets its own graph node id here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_node_id: Option<String>,
    /// Id of the enclosing span — the value of some other record's
    /// `graph_node_id`. A turn's parent is its causal origin (the spawning tool
    /// call / effect, via `caused_by`) when known, otherwise the session root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_graph_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

impl TraceContext {
    pub fn for_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn for_turn_index(mut self, turn_index: usize) -> Self {
        self.turn_index = Some(turn_index);
        self
    }

    pub fn for_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn for_protocol_iteration(mut self, protocol_iteration: usize) -> Self {
        self.protocol_iteration = Some(protocol_iteration);
        self
    }

    pub fn for_llm_call(mut self, llm_call_id: impl Into<String>) -> Self {
        self.llm_call_id = Some(llm_call_id.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceRecord {
    pub schema_version: u32,
    pub id: String,
    pub timestamp: String,
    pub context: TraceContext,
    #[serde(flatten)]
    pub event: TraceEvent,
}

#[derive(Deserialize)]
struct TraceRecordWire {
    schema_version: u32,
    id: String,
    timestamp: String,
    context: TraceContext,
    #[serde(flatten)]
    event: TraceEvent,
}

impl<'de> Deserialize<'de> for TraceRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Read the self-describing JSON value first so the schema fence runs
        // before the current event shape is interpreted. A pre-cutover record
        // must report its version mismatch even when its payload is no longer
        // structurally valid for this build.
        let value = Value::deserialize(deserializer)?;
        let schema_version = value
            .get("schema_version")
            .cloned()
            .ok_or_else(|| serde::de::Error::missing_field("schema_version"))
            .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))?;
        ensure_trace_schema_version(schema_version).map_err(serde::de::Error::custom)?;

        let wire: TraceRecordWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema_version: wire.schema_version,
            id: wire.id,
            timestamp: wire.timestamp,
            context: wire.context,
            event: wire.event,
        })
    }
}

impl TraceRecord {
    pub fn new(context: TraceContext, event: TraceEvent) -> Self {
        Self::new_with_timestamp(context, event, chrono::Utc::now())
    }

    pub fn new_with_timestamp(
        context: TraceContext,
        event: TraceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema_version: TRACE_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: timestamp.to_rfc3339(),
            context,
            event,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "TraceEvent is a public DTO; keeping event payloads inline preserves ergonomic pattern matching"
)]
pub enum TraceEvent {
    SessionStarted {
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        metadata: BTreeMap<String, Value>,
    },
    TurnStarted {
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        metadata: BTreeMap<String, Value>,
    },
    PromptBuilt {
        prompt_hash: String,
        prompt_chars: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        components: Vec<TracePromptComponent>,
    },
    /// Complete model-facing composition captured only when its fingerprint
    /// changes for a resident session.
    CompositionChanged {
        /// SHA-256 of the rendered system prompt plus ordered fingerprints of
        /// the model-facing tool contracts.
        fingerprint: String,
        rendered_system_prompt: String,
        /// Full model-facing tool contracts in request order. This is kept
        /// even when empty so the event is a self-contained snapshot.
        tool_schemas: Vec<TraceToolSpec>,
    },
    RollingHistoryCompactionNeeded {
        context_budget_tokens: usize,
        max_context_tokens: usize,
        threshold_tokens: usize,
    },
    RollingHistoryCompactionStarted {
        source_messages: usize,
        instructions_present: bool,
    },
    RollingHistoryCompactionCompleted {
        summary_nodes: usize,
    },
    RollingHistoryPromptPruned {
        context_budget_tokens: usize,
        max_context_tokens: usize,
        dropped_prefix_messages: usize,
        retained_messages: usize,
    },
    LlmCallStarted {
        request: TraceLlmRequest,
    },
    LlmCallCompleted {
        response: TraceLlmResponse,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TraceTokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<Vec<TraceRetryAttempt>>,
    },
    LlmCallFailed {
        error: TraceError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<Vec<TraceRetryAttempt>>,
    },
    ProviderRequest {
        event: TraceProviderRequestEvent,
    },
    ProviderReplayDropped {
        event: TraceProviderReplayDropEvent,
    },
    EffectEnvelopeDiff {
        event: TraceEffectEnvelopeDiffEvent,
    },
    ProviderStreamEvent {
        event: TraceProviderStreamEvent,
    },
    RuntimeStreamEvent {
        event: TraceRuntimeStreamEvent,
    },
    ToolCallStarted {
        call_id: Option<String>,
        name: String,
        args: Value,
    },
    ToolCallCompleted {
        call_id: Option<String>,
        name: String,
        args: Value,
        output: TraceToolCallOutput,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<Vec<TraceRetryAttempt>>,
    },
    /// A Restate `ctx.run` effect is about to cross its journal command boundary.
    JournaledEffectStarted {
        effect_name: String,
        effect_kind: String,
    },
    /// A Restate `ctx.run` effect returned its recorded or newly executed outcome.
    JournaledEffectSettled {
        effect_name: String,
        effect_kind: String,
        status: String,
    },
    /// A Restate durable wait has issued its park command.
    DurableWaitParked {
        wait_kind: String,
    },
    /// A Restate durable wait resumed with a terminal resolution.
    DurableWaitResolved {
        wait_kind: String,
        resolution: String,
    },
    /// A Restate durable timer has been issued.
    DurableTimerStarted {
        duration_ms: u64,
    },
    /// A Restate durable timer resumed or was cancelled.
    DurableTimerResolved {
        duration_ms: u64,
        status: String,
    },
    /// The durable controller requested a quiescent handler-segment handover.
    DurableSegmentBoundary {
        reason: String,
        effects_executed: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        journaled_bytes_estimate: Option<u64>,
    },
    /// The runtime received a typed, non-retryable store integrity failure.
    StoreErrorObserved {
        operation: String,
        error_class: String,
        message: String,
    },
    ProtocolStep {
        plugin_id: String,
        payload: Value,
    },
    TokenUsage {
        usage: TraceTokenUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cumulative: Option<TraceTokenUsage>,
    },
    LanguageExecution {
        language: String,
        event: TraceLanguageExecution,
    },
    TurnCompleted {
        status: String,
        done_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_frame_switch: Option<TraceAgentFrameSwitch>,
    },
    Custom {
        name: String,
        payload: Value,
    },
}

/// One provider or tool retry attempt projected from the retry owner's sealed
/// record. The trace does not own retry bookkeeping; it only renders it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRetryAttempt {
    pub ordinal: u32,
    pub outcome: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_evidence: Option<TraceExecutionEvidence>,
}

/// Provider-reported facts attached to one sealed LLM attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceExecutionEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_interruption: Option<String>,
}

impl TraceEvent {
    /// The `type` tag serde writes for this variant. This is the single source
    /// of truth for event-kind strings — typed JSONL consumers (e.g. trace sinks)
    /// match on the enum and read the kind from here rather than re-deriving
    /// tag strings by hand. The match is exhaustive on purpose: a new variant
    /// fails to compile here until it is given a kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session_started",
            Self::TurnStarted { .. } => "turn_started",
            Self::PromptBuilt { .. } => "prompt_built",
            Self::CompositionChanged { .. } => "composition_changed",
            Self::RollingHistoryCompactionNeeded { .. } => "rolling_history_compaction_needed",
            Self::RollingHistoryCompactionStarted { .. } => "rolling_history_compaction_started",
            Self::RollingHistoryCompactionCompleted { .. } => {
                "rolling_history_compaction_completed"
            }
            Self::RollingHistoryPromptPruned { .. } => "rolling_history_prompt_pruned",
            Self::LlmCallStarted { .. } => "llm_call_started",
            Self::LlmCallCompleted { .. } => "llm_call_completed",
            Self::LlmCallFailed { .. } => "llm_call_failed",
            Self::ProviderRequest { .. } => "provider_request",
            Self::ProviderReplayDropped { .. } => "provider_replay_dropped",
            Self::EffectEnvelopeDiff { .. } => "effect_envelope_diff",
            Self::ProviderStreamEvent { .. } => "provider_stream_event",
            Self::RuntimeStreamEvent { .. } => "runtime_stream_event",
            Self::ToolCallStarted { .. } => "tool_call_started",
            Self::ToolCallCompleted { .. } => "tool_call_completed",
            Self::JournaledEffectStarted { .. } => "journaled_effect_started",
            Self::JournaledEffectSettled { .. } => "journaled_effect_settled",
            Self::DurableWaitParked { .. } => "durable_wait_parked",
            Self::DurableWaitResolved { .. } => "durable_wait_resolved",
            Self::DurableTimerStarted { .. } => "durable_timer_started",
            Self::DurableTimerResolved { .. } => "durable_timer_resolved",
            Self::DurableSegmentBoundary { .. } => "durable_segment_boundary",
            Self::StoreErrorObserved { .. } => "store_error_observed",
            Self::ProtocolStep { .. } => "protocol_step",
            Self::TokenUsage { .. } => "token_usage",
            Self::LanguageExecution { .. } => "language_execution",
            Self::TurnCompleted { .. } => "turn_completed",
            Self::Custom { .. } => "custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceToolCallOutput {
    pub outcome: TraceToolCallOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<Value>,
}

impl TraceToolCallOutput {
    pub fn status(&self) -> TraceToolCallStatus {
        match self.outcome {
            TraceToolCallOutcome::Success(_) => TraceToolCallStatus::Success,
            TraceToolCallOutcome::Failure(_) => TraceToolCallStatus::Failure,
            TraceToolCallOutcome::Cancelled(_) => TraceToolCallStatus::Cancelled,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status() == TraceToolCallStatus::Success
    }

    pub fn value_for_projection(&self) -> Value {
        match &self.outcome {
            TraceToolCallOutcome::Success(value)
            | TraceToolCallOutcome::Failure(value)
            | TraceToolCallOutcome::Cancelled(value) => value.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum TraceToolCallOutcome {
    Success(Value),
    Failure(Value),
    Cancelled(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceToolCallStatus {
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TracePromptComponent {
    pub id: String,
    pub kind: String,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceLlmRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_variant: Option<String>,
    pub messages: Vec<TraceLlmMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<TraceAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<TraceToolSpec>,
    pub tool_choice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_spec: Option<Value>,
    pub stream: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceLlmMessage {
    pub role: String,
    pub blocks: Vec<TraceContentBlock>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        cache_breakpoint: bool,
    },
    Attachment {
        attachment_idx: usize,
    },
    ToolCall {
        call_id: Option<String>,
        tool_name: String,
        input_json: Value,
        item_id: Option<String>,
        has_signature: bool,
    },
    ToolResult {
        call_id: Option<String>,
        tool_name: Option<String>,
        content: String,
    },
    Reasoning {
        text: String,
        item_id: Option<String>,
        summary: Vec<String>,
        has_encrypted: bool,
        redacted: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceAttachment {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_len: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceLlmResponse {
    pub text: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Value>,
    /// Which of the caller's generation options the adapter put on the wire,
    /// as the runtime's `GenerationReceipt` serializes. Absent when the
    /// adapter does not report, which is not the same as reporting that
    /// nothing was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceProviderRequestEvent {
    pub provider: String,
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub endpoint: String,
    pub body_len: usize,
    /// SHA-256 of the exact serialized wire bytes. `body_json` is a parsed
    /// structured view whose re-serialization may not reproduce these bytes.
    pub body_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_json: Option<Value>,
    /// Why `body_json` is absent when the request body itself was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_json_omitted_reason: Option<String>,
}

/// Opaque replay state rejected before an LLM Provider request was serialized.
/// A missing minting route identifies a session written before provenance was
/// introduced (or another unstamped producer); it is intentionally not
/// inferred from the currently selected route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceProviderReplayKind {
    ResponseText,
    Reasoning,
    ToolCall,
}

impl TraceProviderReplayKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::ResponseText => "response_text",
            Self::Reasoning => "reasoning",
            Self::ToolCall => "tool_call",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceProviderReplayDropReason {
    Unstamped,
    ForeignRoute,
}

impl TraceProviderReplayDropReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Unstamped => "unstamped",
            Self::ForeignRoute => "foreign_route",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceProviderRouteIdentity {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceProviderReplayDropEvent {
    pub replay_kind: TraceProviderReplayKind,
    pub reason: TraceProviderReplayDropReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minting_route: Option<TraceProviderRouteIdentity>,
    pub serving_route: TraceProviderRouteIdentity,
}

/// Structural differences between the canonical envelopes on a failed durable
/// replay validation. This event may contain prompt and tool-result values and
/// must therefore only be emitted through the extended-trace gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceEffectEnvelopeDiffEvent {
    pub recorded_envelope_hash: String,
    pub reconstructed_envelope_hash: String,
    pub divergent_paths: Vec<TraceEffectEnvelopeDiffEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceEffectEnvelopeDiffEntry {
    pub path: String,
    pub recorded: TraceEffectEnvelopeDiffValue,
    pub reconstructed: TraceEffectEnvelopeDiffValue,
}

/// One side of a divergent canonical-envelope value.
///
/// Large values are omitted whole. Their exact serialized length and digest
/// remain available, but no prefix is retained.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TraceEffectEnvelopeDiffValue {
    Missing,
    Present {
        json_len: usize,
        json_sha256: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_json: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_json_omitted_reason: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceProviderStreamEvent {
    pub provider: String,
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_index: Option<i64>,
    pub raw_len: usize,
    pub raw_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceTokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceAgentFrameSwitch {
    pub frame_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRuntimeScope {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
}

impl TraceRuntimeScope {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: None,
            turn_index: None,
            protocol_iteration: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceRuntimeSubject {
    Effect { effect_id: String, kind: String },
    Process { process_id: String },
}

impl TraceRuntimeSubject {
    pub fn graph_key(&self, scope: &TraceRuntimeScope) -> String {
        match self {
            Self::Effect { effect_id, .. } => match scope.turn_id.as_deref() {
                Some(turn_id) if !turn_id.is_empty() => {
                    format!("effect:{}:{turn_id}:{effect_id}", scope.session_id)
                }
                _ => format!("effect:{}:{effect_id}", scope.session_id),
            },
            Self::Process { process_id } => format!("process:{process_id}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLanguageExecutionIdentity {
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    pub module_ref: String,
    pub entry_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_ref: Option<String>,
    pub entry_name: String,
}

impl TraceLanguageExecutionIdentity {
    pub fn graph_key(&self) -> String {
        self.subject.graph_key(&self.scope)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLanguageExecution {
    pub event_key: String,
    pub identity: TraceLanguageExecutionIdentity,
    #[serde(flatten)]
    pub payload: TraceLanguageExecutionPayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceLanguageExecutionPayload {
    ExecutionStarted {
        execution_map: TraceLanguageExecutionMap,
    },
    ExecutionFinished {
        status: TraceLanguageExecutionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    NodeStarted {
        node_id: String,
        node_kind: String,
        label: String,
        occurrence: u64,
    },
    NodeCompleted {
        node_id: String,
        node_kind: String,
        label: String,
        occurrence: u64,
    },
    NodeFailed {
        node_id: String,
        node_kind: String,
        label: String,
        occurrence: u64,
        error: String,
    },
    BranchSelected {
        node_id: String,
        occurrence: u64,
        edge_id: String,
        selected: TraceBranchSelection,
    },
    ChildStarted {
        parent_node_id: String,
        occurrence: u64,
        child: TraceLanguageChildExecution,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLanguageChildExecution {
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_name: Option<String>,
}

impl TraceLanguageChildExecution {
    pub fn graph_key(&self) -> String {
        self.subject.graph_key(&self.scope)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceLanguageExecutionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceBranchSelection {
    Then,
    Else,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLanguageExecutionMap {
    pub module_ref: String,
    pub entry_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_ref: Option<String>,
    pub entry_name: String,
    #[serde(default)]
    pub nodes: Vec<TraceLanguageExecutionMapNode>,
    #[serde(default)]
    pub edges: Vec<TraceLanguageExecutionMapEdge>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLanguageExecutionMapNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_metadata: Option<TraceLabelMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLabelMetadata {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLanguageExecutionMapEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceError {
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TraceSinkError {
    #[error("failed to serialize trace record: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to create trace directory {path}: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("failed to open trace file {path}: {source}")]
    Open { path: PathBuf, source: io::Error },
    #[error("failed to write trace file {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

pub trait TraceSink: Send + Sync {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError>;

    /// Force any buffered trace data this sink owns to durable storage.
    ///
    /// Hosts call this before process exit so records that a sink has not yet
    /// committed are not lost. The default is a no-op: sinks that write each
    /// record through on [`append`](Self::append) (or that delegate durability
    /// to a host-owned exporter) have nothing of their own to flush. Sinks that
    /// buffer — or that can force an `fsync` — override this.
    fn flush(&self) -> Result<(), TraceSinkError> {
        Ok(())
    }
}

pub struct JsonlTraceSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlTraceSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TraceSink for JsonlTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let line = serde_json::to_string(record)?;
        let _guard = self.lock.lock_recover();
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| TraceSinkError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| TraceSinkError::Open {
                path: self.path.clone(),
                source,
            })?;
        writeln!(file, "{line}").map_err(|source| TraceSinkError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// `fsync` the trace file to durable storage.
    ///
    /// Each [`append`](Self::append) opens, appends, and closes the file, so a
    /// record's bytes already reach the OS as it is written — this sink holds no
    /// in-process buffer. `flush` goes one step further and forces an `fsync` so
    /// the OS page cache is pushed to disk, which is the honest guarantee a host
    /// wants before exit. If no record has been written yet the file may not
    /// exist; that is a no-op rather than an error (nothing to sync), and we do
    /// not create an empty file just to sync it.
    fn flush(&self) -> Result<(), TraceSinkError> {
        let _guard = self.lock.lock_recover();
        match OpenOptions::new().append(true).open(&self.path) {
            Ok(file) => file.sync_all().map_err(|source| TraceSinkError::Write {
                path: self.path.clone(),
                source,
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(TraceSinkError::Open {
                path: self.path.clone(),
                source,
            }),
        }
    }
}

/// Writes each trace record as one JSON line to stderr — handy for `cargo run`
/// debugging without a trace file.
///
/// The newline is appended to the serialized record so both leave in one
/// `write_all`: stderr is unbuffered and `eprintln!` emits one syscall per
/// format fragment, so a host logging from another task could land between a
/// record and its newline and sever the line for anything reading the merged
/// output a line at a time. The buffering that fixes it is the `String`, not
/// the handle.
#[derive(Default)]
pub struct StderrTraceSink {
    lock: Mutex<()>,
}

impl TraceSink for StderrTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let _guard = self.lock.lock_recover();
        let mut stderr = io::stderr().lock();
        stderr
            .write_all(line.as_bytes())
            .and_then(|()| stderr.flush())
            .map_err(|source| TraceSinkError::Write {
                path: PathBuf::from("<stderr>"),
                source,
            })
    }
}

/// Fans each trace record out to several sinks in order (e.g. stderr + a JSONL
/// file).
///
/// Every sink runs on every call and the first error is returned once they all
/// have: the sinks are independent destinations, so a stderr that a supervisor
/// closed must not cost the run its durable JSONL file. Only the first error is
/// carried — a caller acts on "tracing degraded", not on a list.
pub struct TeeTraceSink {
    sinks: Vec<Arc<dyn TraceSink>>,
}

impl TeeTraceSink {
    pub fn new(sinks: impl IntoIterator<Item = Arc<dyn TraceSink>>) -> Self {
        Self {
            sinks: sinks.into_iter().collect(),
        }
    }
}

impl TraceSink for TeeTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut first_error = None;
        for sink in &self.sinks {
            if let Err(error) = sink.append(record) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Flush every wrapped sink, then report the first error.
    fn flush(&self) -> Result<(), TraceSinkError> {
        let mut first_error = None;
        for sink in &self.sinks {
            if let Err(error) = sink.flush() {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

pub fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_ref());
    format!("{:x}", hasher.finalize())
}

pub fn json_hash(value: &Value) -> String {
    sha256_hex(serde_json::to_vec(value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that fails every call, standing in for a closed stderr.
    struct FailingSink;

    impl TraceSink for FailingSink {
        fn append(&self, _record: &TraceRecord) -> Result<(), TraceSinkError> {
            Err(TraceSinkError::Write {
                path: PathBuf::from("<failing>"),
                source: io::Error::from(io::ErrorKind::BrokenPipe),
            })
        }

        fn flush(&self) -> Result<(), TraceSinkError> {
            Err(TraceSinkError::Write {
                path: PathBuf::from("<failing>"),
                source: io::Error::from(io::ErrorKind::BrokenPipe),
            })
        }
    }

    #[test]
    fn a_failing_sink_does_not_rob_later_sinks_in_a_tee() {
        // The bot tees stderr first and its durable JSONL file second, so a
        // supervisor closing stderr must not cost the run its trace file.
        let dir = std::env::temp_dir().join(format!("lash-trace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let tee = TeeTraceSink::new([
            Arc::new(FailingSink) as Arc<dyn TraceSink>,
            Arc::new(JsonlTraceSink::new(&path)),
        ]);

        let append = tee.append(&TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::Custom {
                name: "test.event".to_string(),
                payload: serde_json::json!({"ok": true}),
            },
        ));

        assert!(
            append.is_err(),
            "the first sink's failure is still reported"
        );
        assert!(tee.flush().is_err(), "a failing flush is reported too");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"type\":\"custom\""),
            "the later sink still received the record"
        );
    }

    #[test]
    fn jsonl_sink_writes_record() {
        let dir = std::env::temp_dir().join(format!("lash-trace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let sink = JsonlTraceSink::new(&path);
        sink.append(&TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::Custom {
                name: "test.event".to_string(),
                payload: serde_json::json!({"ok": true}),
            },
        ))
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"type\":\"custom\""));
        assert!(text.contains("\"session_id\":\"root\""));
    }

    #[test]
    fn tool_start_and_frame_switch_records_are_jsonl_shaped() {
        let started = TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::ToolCallStarted {
                call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({"path": "README.md"}),
            },
        );
        let completed = TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::TurnCompleted {
                status: "completed".to_string(),
                done_reason: "modelstop".to_string(),
                agent_frame_switch: Some(TraceAgentFrameSwitch {
                    frame_key: "frame-key/v1/example".to_string(),
                }),
            },
        );

        let started_json = serde_json::to_value(started).unwrap();
        assert_eq!(started_json["type"], "tool_call_started");
        assert_eq!(started_json["call_id"], "call-1");

        let completed_json = serde_json::to_value(completed).unwrap();
        assert_eq!(completed_json["type"], "turn_completed");
        assert_eq!(
            completed_json["agent_frame_switch"]["frame_key"],
            "frame-key/v1/example"
        );
    }

    #[test]
    fn language_execution_records_are_jsonl_shaped() {
        let identity = TraceLanguageExecutionIdentity {
            scope: TraceRuntimeScope::new("s1"),
            subject: TraceRuntimeSubject::Process {
                process_id: "p1".to_string(),
            },
            module_ref: "module".to_string(),
            entry_kind: "process".to_string(),
            entry_ref: Some("component:0".to_string()),
            entry_name: "main".to_string(),
        };
        let event = TraceLanguageExecution {
            event_key: "process:p1:node:n1:1:started".to_string(),
            identity,
            payload: TraceLanguageExecutionPayload::NodeStarted {
                node_id: "n1".to_string(),
                node_kind: "resource_operation".to_string(),
                label: "read_file".to_string(),
                occurrence: 1,
            },
        };
        let record = TraceRecord::new(
            TraceContext::default().for_session("s1"),
            TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event,
            },
        );

        let json = serde_json::to_value(&record).expect("serialize language execution");
        assert_eq!(json["type"], "language_execution");
        assert_eq!(json["language"], "lashlang");
        assert_eq!(json["event"]["kind"], "node_started");
        assert_eq!(json["event"]["event_key"], "process:p1:node:n1:1:started");

        let round_trip =
            serde_json::from_value::<TraceRecord>(json).expect("deserialize language execution");
        assert!(matches!(
            round_trip.event,
            TraceEvent::LanguageExecution {
                language,
                event: TraceLanguageExecution {
                    payload: TraceLanguageExecutionPayload::NodeStarted { .. },
                    ..
                }
            } if language == "lashlang"
        ));
    }

    #[test]
    fn tool_completion_serializes_typed_failure_output() {
        let record = TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::ToolCallCompleted {
                call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                args: serde_json::json!({"path": "missing"}),
                output: TraceToolCallOutput {
                    outcome: TraceToolCallOutcome::Failure(serde_json::json!({
                        "class": "invalid_request",
                        "code": "invalid_tool_args",
                        "message": "bad args",
                        "source": "runtime",
                        "retry": { "type": "never" },
                        "raw": { "path": "missing" }
                    })),
                    control: None,
                },
                duration_ms: 3,
                attempts: None,
            },
        );

        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["type"], "tool_call_completed");
        assert_eq!(json["output"]["outcome"]["status"], "failure");
        assert_eq!(
            json["output"]["outcome"]["payload"]["code"],
            "invalid_tool_args"
        );
        assert_eq!(
            json["output"]["outcome"]["payload"]["raw"]["path"],
            "missing"
        );
    }

    #[test]
    fn event_kind_matches_serialized_type_tag() {
        let events = [
            TraceEvent::SessionStarted {
                metadata: Default::default(),
            },
            TraceEvent::TurnStarted {
                metadata: Default::default(),
            },
            TraceEvent::ToolCallStarted {
                call_id: None,
                name: "read_file".to_string(),
                args: Value::Null,
            },
            TraceEvent::Custom {
                name: "x".to_string(),
                payload: Value::Null,
            },
        ];
        for event in events {
            let kind = event.kind();
            let json = serde_json::to_value(&event).expect("serialize event");
            assert_eq!(json["type"], kind, "kind() disagrees with serde tag");
        }
    }

    #[test]
    fn jsonl_sink_creates_parent_directories() {
        let dir = std::env::temp_dir().join(format!("lash-trace-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("trace.jsonl");
        let sink = JsonlTraceSink::new(&path);
        sink.append(&TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::RuntimeStreamEvent {
                event: TraceRuntimeStreamEvent {
                    sequence: 1,
                    elapsed_ms: 0,
                    event_name: "delta".to_string(),
                    raw_text: Some("hello".to_string()),
                    visible_text: Some("hello".to_string()),
                    item_id: None,
                    output_index: None,
                    call_id: None,
                    tool_name: None,
                    input_json: None,
                    usage: None,
                },
            },
        ))
        .unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
