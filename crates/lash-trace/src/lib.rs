//! Durable diagnostics for the lash runtime: the [`TraceSink`] channel and its
//! record vocabulary.
//!
//! A [`TraceSink`] receives one [`TraceRecord`] per runtime event — turn
//! lifecycle, prompt builds, compaction decisions, LLM calls,
//! per-tool start/completion, per-call token usage, protocol steps, and Lash VM
//! execution-graph updates. Each record
//! carries a [`TraceContext`] (session / turn / graph-node identity) plus a
//! tagged [`TraceEvent`] payload; [`TraceEventKind`] derives the kind vocabulary
//! from those variants for recognising the `type` tags consumers match on.
//!
//! [`JsonlTraceSink`] writes one JSON line per record at schema
//! [`TRACE_SCHEMA_VERSION`]; [`TeeTraceSink`] fans out to several sinks; and the
//! optional `otel` feature adds `otel::OtelTelemetry`, which projects permitted
//! domain completions under retained admission anchors. This is the *durable diagnostics* reporting channel —
//! distinct from the app-facing `TurnActivity` stream and the low-level
//! `SessionStreamEvent` stream that the runtime crates expose.
//!
//! For the full map of reporting channels, guidance on when to consume which,
//! and the schema-evolution policy that governs [`TRACE_SCHEMA_VERSION`], see
//! `docs/reporting.html`; for the attach-a-sink how-to, see `docs/tracing.html`.
mod limits;
pub use limits::{ObservationWorkLimits, TraceLimits};

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod content;
mod content_block;
mod domain;
mod event;
mod flight_recorder;
mod jsonl_records;
mod language_execution;
mod language_execution_failure;
#[cfg(feature = "otel")]
pub mod otel;
pub mod telemetry;
mod workflow_overlay;

pub use content::{CONTENT_POLICY_OMISSION, TelemetryContent};
pub use content_block::{TraceContentBlock, TraceToolResultBlock};
pub use domain::{
    TraceAttemptObservation, TraceDomainCompletion, TraceDomainStatus, TraceDomainSubject,
    TraceRuntimeStreamEvent, TraceRuntimeStreamPayload,
};
pub use event::{TraceEvent, TraceEventKind};
pub use flight_recorder::{
    FlightRecorderOutcome, FlightRecorderRecord, FlightRecorderSettings, FlightRecorderSink,
    FlightRecorderSnapshot,
};
use jsonl_records::truncate_torn_tail;
pub use jsonl_records::{
    JsonlTraceReadError, TraceRead, parse_jsonl_records, parse_trace_jsonl_records,
};
pub use language_execution::{
    LanguageExecutionObservation, StepBodyStarted, StepBodyStartedObservation,
    TraceLanguageExecutionPayload, TraceNodeAwaited, TraceNodeFact, TraceNodeWaitKind,
    TraceNodeWaitResolution, WorkflowDocumentEntry, WorkflowDocumentRef,
};
pub use language_execution_failure::TraceLanguageExecutionFailure;
pub use lash_sansio::llm::types::GenerationReceipt;
use lash_sansio::llm::types::{LlmOutputPart, LlmProviderTraceDirection};
pub use lash_sansio::{
    CellFailure, CellFailureKind, ExecCodeFailureReason, TextProjectionMetadata,
};
pub use telemetry::{
    AttemptObservation, DurableTraceScope, EmissionPermit, EmissionSource, InvalidTraceCarrier,
    InvalidTraceLinks, TRACE_LINK_LIMIT, TRACESTATE_CHAR_LIMIT, TRACESTATE_MEMBER_LIMIT,
    TraceAdmissionCandidate, TraceAnchor, TraceAttemptId, TraceCandidateOutcome, TraceCarrier,
    TraceCause, TraceDomainProjector, TraceHostOperation, TraceLinks, TraceRecordIdentity,
    TraceScopeAdmission, TraceScopeFactory, TraceScopeId, TraceScopeKind, TraceScopeOffer,
    TraceScopeOwner, TraceToolOwner, TraceTransitionKind, UntracedScopes, W3cSpanId, W3cTraceFlags,
    W3cTraceId, W3cTraceState,
};
pub use workflow_overlay::{
    DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT, WorkflowExecutionOverlay,
    WorkflowExecutionOverlayAccumulator, WorkflowOverlayCall, WorkflowOverlayChildLink,
    WorkflowOverlayConflict, WorkflowOverlayConflictKind, WorkflowOverlayCoverage,
    WorkflowOverlayDocument, WorkflowOverlayDocumentBinding, WorkflowOverlayEventIdentity,
    WorkflowOverlayExecutionTransition, WorkflowOverlayFact, WorkflowOverlayFoldError,
    WorkflowOverlayHistoryEvent, WorkflowOverlayMismatch, WorkflowOverlayNodeTransition,
    WorkflowOverlayOccurrence, WorkflowOverlaySettlement, WorkflowOverlaySite,
    WorkflowOverlaySiteReport, WorkflowOverlaySiteRetention, WorkflowOverlaySiteState,
    WorkflowOverlayTerminal, WorkflowOverlayTerminalRecord, WorkflowOverlayTerminalStatus,
    WorkflowTaskSite, fold_workflow_overlay,
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
///   inside them never forces a bump.
///
/// Version 5 adds the `composition_changed` event.
/// Version 6 adds the `provider_replay_dropped` event with typed minting and
/// serving LLM Provider routes.
/// Version 7 renames the Lash VM execution records to language-tagged ones:
/// `language_execution` carries the language id, and the graph, identity, node
/// and status payloads lose their LashVm-specific names.
/// Version 8 types the remaining free-form outcome strings: the
/// `journaled_effect_settled`, `durable_wait_resolved` and
/// `durable_timer_resolved` statuses become closed enums, `turn_completed`
/// replaces its `status`/`done_reason`/`agent_frame_switch` triple with one
/// [`TraceTurnOutcome`] whose variants own the done reason, the frame switch
/// and the cancellation evidence, and the language execution map drops the
/// four identity fields already carried by
/// [`TraceLanguageExecutionIdentity`].
/// Version 9 promotes the four runtime exec diagnostic phases from opaque
/// [`TraceEvent::ProtocolStep`] payloads to typed events, types each completed
/// exec's tool-call roll-up, and removes the redundant `tool_call_count` and
/// `terminal_finish_present` fields. Version 10 renames the exec completion's
/// projection metadata list to match the merged observation representation.
/// Version 11 adds typed charge-safety decisions to retry attempts. Version 12
/// types retry-attempt outcomes and adds generation disposition and
/// provider-reported usage to each LLM attempt projection. Version 13 adds the
/// typed `attachment_degraded` continuation event. Version 14 adds the
/// requested model identifier to completed LLM responses because the
/// independently developed v13 changes would otherwise collide. Version 15
/// replaces the exec completion's string error with a structured cell failure
/// carrying its policy, program, or host kind.
/// Version 16 removes the two unemitted lifecycle and standalone usage events;
/// turn starts and completed LLM calls retain lifecycle and per-call usage evidence.
/// Version 17 adds compile/link outcomes for every code mode program step.
/// Version 18 carries attachment sources through request blocks.
/// Version 19 adds the attempt usage disposition to retry attempts so an
/// aborted or failed call whose usage never arrived is distinguishable from
/// a free one.
/// Version 20 carries admitted effect addresses, independently optional
/// attribution, and complete trigger cause identity in trace graph subjects.
/// Version 21 gives a context-window overflow its own turn failure reason so
/// a trace reader can tell a recoverable overflow from a provider error.
/// Version 22 types the last free-form outcome string in the crate — a retry
/// attempt's `usage_disposition` becomes a closed enum, so a record spelling
/// it any other way is refused instead of decoded — and drops the unreachable `rejected` branch-edge
/// selection, replacing it with the typed arm the `branch_selected` event
/// already carried.
/// Version 23 adds a closed [`ExecCodeFailureReason`] to `exec_code_failed` so
/// the classification survives JSONL and OTel without string-matching the
/// human-readable `error` text.
/// Version 24 (FIG-3371) adds `block_id` to [`TraceRuntimeStreamEvent`] so a
/// provider item's sub-blocks — e.g. OpenAI `rs_*:summary:0` / `:summary:1` —
/// stay distinguishable; `item_id` alone collapses them to the item.
/// Version 25 uses generic compaction and prompt-view event names.
/// Trace failures retain typed terminal and provider-failure vocabularies,
/// plus one namespaced failure code. OTel derives `error.type` from the kind.
/// Version 27 (FIG-3460) unifies workflow node identity and adds structured
/// execution sites, language-node/tool cross-links, and engine correlation.
/// Version 28 (FIG-3461) adds generation-qualified observation identity,
/// typed child process identity, and replay-stable bounded graph snapshots.
/// Version 29 (FIG-1961) renames the compaction decision fields from
/// `context_budget_tokens` to `used_tokens`: the value is now the checked
/// provider-reported usage, not a derived budget snapshot.
/// Version 30 (FIG-3463) adds typed failure provenance to language observations.
/// Version 31 (FIG-3474) adds occurrence-level wait/resume/cancel facts, derives branch
/// skips from selection and map membership, and closes workflow site kinds.
/// Version 32 (FIG-3535) adds `PromptViewAttachmentsPruned`, emitted when
/// old-attachment pruning changes the prompt view without a tail-window cut.
/// Version 33 (FIG-3515) gives a traced tool result its ordered content:
/// `tool_result.content` is a list of text and attachment blocks, one
/// result per call, instead of a string beside loose attachment blocks.
/// Version 34 (FIG-3571) keys language observations by carrier IR node ids:
/// the same source traces under different node ids than version 33.
/// Version 35 (FIG-3670) renames the language-execution identity's engine
/// invocation field to `engine_execution_id`: the kernel names no engine.
///
/// Version 36 (FIG-3607) names a process by its minted, never-reused id: a
/// language execution's generation is its attempt alone, graph keys drop the
/// `:incarnation:` segment, and child links drop `incarnation` and
/// `child_incarnation`. It changes in place under the pre-1.0 version freeze
/// (FIG-3846): FIG-4029 drops `prompt_view_pruned`, because context-pressure
/// compaction starts a frame instead of pruning the prompt view. FIG-5528
/// reports model calls in the runtime's own types: one `LlmUsage`, the sealed
/// `AttemptRecord` for every attempt, typed terminal reason, output parts and
/// generation receipt, and one `provider_event` whose typed direction
/// replaces `provider_request` and `provider_stream_event`. FIG-5526
/// types `runtime_stream_event`: its payload is a closed enum whose block
/// arm carries the stream-block lifecycle hosts see, with the block's full
/// identity, in place of a free-text event name beside optional fields.
/// FIG-5576 removes the execution map: `execution_started` names its
/// workflow document by reference, node facts name a site and drop its
/// static kind and label, `branch_selected` drops its edge id, the
/// `step_body_started` event binds an admitted process step to its site, and
/// the graph snapshot becomes the workflow execution overlay.
///
/// version_guard(
///     shapes(
///         path = "crates/lash-trace/src/*.rs",
///         cover(
///             TraceRecord, TraceEvent, TraceTurnOutcome, TraceTurnCancellationEvidence,
///             TraceTurnCompletionReason, TraceTurnFailureReason, TraceLanguageExecutionPayload,
///             StepBodyStarted, TraceNodeFact, TraceNodeWaitKind, TraceNodeAwaited,
///             TraceNodeWaitResolution,
///         ),
///     ),
///     items(path = "crates/lash-trace/src/workflow_overlay.rs", fold_workflow_overlay),
///     shapes(
///         path = "crates/lash-trace/src/workflow_overlay/model.rs",
///         cover(
///             WorkflowExecutionOverlay, WorkflowOverlayOccurrence, WorkflowOverlaySite,
///             WorkflowOverlaySiteReport, WorkflowOverlayTerminalRecord,
///             WorkflowOverlayTerminalStatus, WorkflowOverlayEventIdentity,
///         ),
///     ),
///     shapes(
///         path = "crates/lash-trace/src/language_execution_failure.rs",
///         cover(TraceLanguageExecutionFailure),
///     ),
/// )
/// version_surface = "coexist"
/// format_outside_manifest = "trace wire: gates a trace reader, not state lash reopens"
pub const TRACE_SCHEMA_VERSION: u32 = 36;

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

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TraceLevel {
    #[default]
    Standard,
    Extended,
}

impl TraceLevel {
    /// Standard detail preset: lifecycle records, with extended provider
    /// payload records disabled. This is an export-volume choice, not a
    /// workload-measured optimum.
    pub const fn standard() -> Self {
        Self::Standard
    }

    pub fn is_extended(self) -> bool {
        matches!(self, Self::Extended)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Stable id of the span this record represents (e.g. `turn:<session>:<turn>`,
    /// `llm:<call_id>`, `tool:<call_id>`). Populated by the runtime for turn /
    /// llm / tool / session records so a consumer can build a nested span tree
    /// from `(graph_node_id, parent_graph_node_id)` with a single `id -> span`
    /// map. A language-execution record for a node event carries the observed
    /// node's structural id here, equal to the `WorkflowGraph` node id (ADR
    /// 0100 R0), so a host joins trace to graph on this key; a child start
    /// carries its parent node's id, and execution start and finish leave it
    /// unset.
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
    pub fn for_session(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn for_turn_index(mut self, turn_index: usize) -> Self {
        self.turn_index = Some(turn_index);
        self
    }

    pub fn for_turn(mut self, turn_id: impl Into<TurnId>) -> Self {
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

/// Node id of the session root span in the `graph_node_id` key space.
///
/// These `*_node_id` functions are the single definition of that key space:
/// the runtime stamps them onto [`TraceContext::graph_node_id`] /
/// [`TraceContext::parent_graph_node_id`], and span exporters key their
/// `id -> span` maps with the same strings.
pub fn session_node_id(session_id: &SessionId) -> String {
    format!("session:{session_id}")
}

/// Node id of the turn span this context belongs to, or `None` when the
/// context carries no session or no turn identity at all.
pub fn turn_node_id(context: &TraceContext) -> Option<String> {
    let session_id = context.session_id.as_deref()?;
    if let Some(turn_id) = context.turn_id.as_deref() {
        Some(format!("turn:{session_id}:{turn_id}"))
    } else {
        context
            .turn_index
            .map(|turn_index| format!("turn:{session_id}:idx{turn_index}"))
    }
}

/// Node id of one LLM-call span.
pub fn llm_node_id(llm_call_id: &str) -> String {
    format!("llm:{llm_call_id}")
}

/// Node id of one tool-call span.
pub fn tool_node_id(call_id: &str) -> String {
    format!("tool:{call_id}")
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceRecord {
    pub schema_version: u32,
    pub id: String,
    #[serde(with = "trace_timestamp_serde")]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Whether this record carries content or had it omitted under the
    /// host's [`TelemetryContent`] policy: how to read an empty content field.
    pub content: TelemetryContent,
    pub context: TraceContext,
    #[serde(flatten)]
    pub event: TraceEvent,
}

/// The record's JSON Schema describes its serialized form: the timestamp is an
/// RFC 3339 string, and the event's `type` tag and fields sit beside the
/// envelope fields.
impl schemars::JsonSchema for TraceRecord {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TraceRecord".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Schema-only mirror of the serialized record; Serde never reads it.
        /// One durable trace record: the envelope fields plus the flattened
        /// event, whose `type` tag names the variant.
        #[derive(schemars::JsonSchema)]
        #[allow(dead_code, reason = "fields exist only to describe the schema")]
        struct TraceRecordSchema {
            schema_version: u32,
            id: String,
            #[schemars(schema_with = "rfc3339_timestamp_schema")]
            timestamp: String,
            content: TelemetryContent,
            context: TraceContext,
            #[serde(flatten)]
            event: TraceEvent,
        }

        TraceRecordSchema::json_schema(generator)
    }
}

fn rfc3339_timestamp_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": "string", "format": "date-time" })
}

#[derive(Deserialize)]
struct TraceRecordWire {
    schema_version: u32,
    id: String,
    #[serde(with = "trace_timestamp_serde")]
    timestamp: chrono::DateTime<chrono::Utc>,
    content: TelemetryContent,
    context: TraceContext,
    #[serde(flatten)]
    event: TraceEvent,
}

mod trace_timestamp_serde {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(timestamp: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&timestamp.to_rfc3339())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let timestamp = String::deserialize(deserializer)?;
        DateTime::parse_from_rfc3339(&timestamp)
            .map(|timestamp| timestamp.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
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
            content: wire.content,
            context: wire.context,
            event: wire.event,
        })
    }
}

/// The compile/link result, independent of the program's later runtime outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TraceProgramStepOutcome {
    /// The program compiled and linked successfully.
    Ok,
    /// Setup, parsing, or linking rejected the program before execution.
    Failure {
        /// Bounded compiler or linker feedback, using the raw-error trace limit.
        diagnostic: String,
    },
}

pub use lash_sansio::FailureCode as TraceFailureCode;
pub use lash_sansio::llm::types::{
    LlmTerminalReason as TraceLlmTerminalReason, NormalizedError as TraceNormalizedError,
    ProviderFailureKind as TraceProviderFailureKind, RetryClass as TraceRetryClass,
    RetryDeclineCause as TraceRetryDeclineCause, RetryWait as TraceRetryWait,
};

/// One tool attempt projected from its retry owner's sealed record. A model
/// call's attempts are its sealed `AttemptRecord`s, carried as they are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceRetryAttempt {
    pub ordinal: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    pub detail: TraceRetryAttemptDetail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TraceRetryAttemptDetail {
    Tool { outcome: TraceToolAttemptOutcome },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum TraceToolAttemptOutcome {
    Completed,
    Failed {
        class: lash_sansio::ToolFailureClass,
        code: String,
        message: String,
        source: lash_sansio::ToolFailureSource,
        suggested_delay_ms: Option<u64>,
    },
    Cancelled {
        message: String,
        source: lash_sansio::ToolFailureSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<lash_sansio::CancelOrigin>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceStoreErrorClass {
    StoredDataCorrupt,
    MonotonicCounterOverflow,
}

impl TraceStoreErrorClass {
    pub fn wire_tag(self) -> &'static str {
        match self {
            Self::StoredDataCorrupt => "stored_data_corrupt",
            Self::MonotonicCounterOverflow => "monotonic_counter_overflow",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum TraceToolCallOutcome {
    Success(Value),
    Failure(Value),
    Cancelled(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceToolCallStatus {
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceExecToolCall {
    pub call_id: lash_sansio::ToolCallId,
    pub name: String,
    pub status: TraceToolCallStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TracePromptComponent {
    pub id: String,
    pub kind: String,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLlmRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_variant: Option<String>,
    pub messages: Vec<TraceLlmMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<TraceToolSpec>,
    pub tool_choice: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_spec: Option<Value>,
    pub stream: bool,
}

impl TraceLlmRequest {
    /// Refs in message order, derived from their owning blocks.
    pub fn attachments(&self) -> Vec<&TraceAttachment> {
        self.messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                TraceContentBlock::Attachment { reference } => Some(reference.as_ref()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLlmMessage {
    pub role: String,
    pub blocks: Vec<TraceContentBlock>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceAttachment {
    pub id: String,
    pub media_type: String,
    pub byte_len: u64,
    /// `message` or `tool_result`.
    pub position: String,
    /// The delivery form name. Absent on a logical summary taken before send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_form: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLlmResponse {
    pub text: String,
    pub duration_ms: u64,
    /// The model identifier requested for the corresponding LLM call.
    pub request_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TraceLlmTerminalReason>,
    /// The model's output parts. Opaque provider replay payloads (encrypted
    /// reasoning, signatures, provider-owned replay blobs) are not captured;
    /// every other field is the part as the runtime holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<LlmOutputPart>>,
    /// Which of the caller's generation options the adapter put on the wire.
    /// Absent when the adapter does not report, which is not the same as
    /// reporting that nothing was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_disposition: Option<GenerationReceipt>,
}

/// Why a provider observation's parsed `raw_json` view is absent although the
/// raw bytes were observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceProviderBodyOmission {
    SizeLimit,
    InvalidJson,
    /// The host's [`TelemetryContent`] policy withheld it.
    ContentPolicy,
}

/// One raw provider observation of a model call: the request Lash sent or
/// one event of the provider's response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceProviderEvent {
    pub provider: String,
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub direction: LlmProviderTraceDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_index: Option<i64>,
    pub raw_len: usize,
    /// SHA-256 of the exact wire bytes. `raw_json` is a parsed structured
    /// view whose re-serialization may not reproduce these bytes.
    pub raw_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_json_omitted_reason: Option<TraceProviderBodyOmission>,
}

/// Opaque replay state rejected before an LLM Provider request was serialized.
/// A missing minting route identifies a session written before provenance was
/// introduced (or another unstamped producer); it is intentionally not
/// inferred from the currently selected route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceProviderRouteIdentity {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceEffectEnvelopeDiffEvent {
    pub recorded_envelope_hash: String,
    pub reconstructed_envelope_hash: String,
    pub divergent_paths: Vec<TraceEffectEnvelopeDiffEntry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceEffectEnvelopeDiffEntry {
    pub path: String,
    pub recorded: TraceEffectEnvelopeDiffValue,
    pub reconstructed: TraceEffectEnvelopeDiffValue,
}

/// One side of a divergent canonical-envelope value.
///
/// Large values are omitted whole. Their exact serialized length and digest
/// remain available, but no prefix is retained.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceAgentFrameSwitch {
    pub frame_key: String,
}

/// The snake_case wire tag serde writes for a unit-variant trace enum.
///
/// Reading the tag back from serde keeps one spelling per variant: a
/// `#[serde(rename_all)]` change or a variant rename moves the wire tag and
/// every reader of it at once, instead of leaving a hand-written copy behind
/// to drift.
fn wire_tag<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(tag)) => tag,
        _ => String::new(),
    }
}

/// Durable evidence that the traced turn was cancelled. Mirrors the runtime's
/// `TurnCancellationEvidence`, which only [`TurnStop::Cancelled`] can carry, so
/// a cancelled trace outcome can never be stated without saying which request
/// produced it.
///
/// [`TurnStop::Cancelled`]: lash_sansio::session_model::TurnStop::Cancelled
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceTurnCancellationEvidence {
    pub request_id: String,
    /// Opaque host-domain data. Lash records and returns it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The single terminal outcome of a traced turn.
///
/// One closed shape replaces the former `status` / `done_reason` /
/// `agent_frame_switch` triple: each variant owns exactly the data its state
/// can carry, so an agent-frame switch cannot be reported without a frame key
/// and a cancellation cannot be reported without its evidence. Cancellation is
/// its own variant rather than a failure reason — [`Self::is_failed`] is
/// `false` for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TraceTurnOutcome {
    /// The turn reached a terminal result the caller asked for.
    Completed {
        done_reason: TraceTurnCompletionReason,
    },
    /// The turn ended by handing control to another agent frame.
    AgentFrameSwitch { frame_switch: TraceAgentFrameSwitch },
    /// The turn was cancelled. Not a failure.
    Cancelled {
        evidence: TraceTurnCancellationEvidence,
    },
    /// The turn stopped short of a result.
    Failed { done_reason: TraceTurnFailureReason },
}

impl TraceTurnOutcome {
    /// A cancelled turn is a deliberate stop, not a failure.
    pub fn is_failed(&self) -> bool {
        match self {
            Self::Failed { .. } => true,
            Self::Completed { .. } | Self::AgentFrameSwitch { .. } | Self::Cancelled { .. } => {
                false
            }
        }
    }

    /// The `status` tag serde writes for this variant, read back from serde
    /// rather than repeated by hand so a rename cannot leave a second,
    /// drifting spelling behind.
    pub fn status_tag(&self) -> String {
        self.string_field("status").unwrap_or_default()
    }

    /// The reason tag for this outcome, in the spelling the pre-v8 flat
    /// `done_reason` string carried: the completion or failure reason for
    /// [`Self::Completed`] and [`Self::Failed`], and the status tag itself
    /// (`agent_frame_switch`, `cancelled`) for the two variants that carry no
    /// separate reason. Read back from serde like [`Self::status_tag`], so the
    /// spelling cannot drift from the wire.
    pub fn done_reason_tag(&self) -> String {
        self.string_field("done_reason")
            .unwrap_or_else(|| self.status_tag())
    }

    fn string_field(&self, field: &str) -> Option<String> {
        match serde_json::to_value(self).ok()? {
            Value::Object(mut map) => match map.remove(field) {
                Some(Value::String(tag)) => Some(tag),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Why a [`TraceTurnOutcome::Completed`] turn finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceTurnCompletionReason {
    AssistantMessage,
    /// A declared Finish control ended the turn.
    Finished,
}

impl TraceTurnCompletionReason {
    /// The snake_case tag serde writes for this variant.
    pub fn wire_tag(&self) -> String {
        wire_tag(self)
    }
}

/// Why a [`TraceTurnOutcome::Failed`] turn stopped. Mirrors the non-cancelled
/// `TurnStop` reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceTurnFailureReason {
    Incomplete,
    InvalidInput,
    MaxTurns,
    ToolFailure,
    ProviderError,
    ContextOverflow,
    PluginAbort,
    RuntimeError,
    /// The durable follow-on reached its chain's frame-switch bound.
    AgentFrameSwitchLimit,
    SubmittedError,
    ToolError,
}

impl TraceTurnFailureReason {
    /// The snake_case tag serde writes for this variant.
    pub fn wire_tag(&self) -> String {
        wire_tag(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceRuntimeScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
}

impl TraceRuntimeScope {
    pub fn none() -> Self {
        Self {
            session_id: None,
            turn_id: None,
            turn_index: None,
            protocol_iteration: None,
        }
    }

    pub fn for_session(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            ..Self::none()
        }
    }

    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self::for_session(session_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceRuntimeSubject {
    Effect {
        address: lash_sansio::EffectAddress,
        effect_id: String,
    },
    Process {
        process_id: ProcessId,
    },
}

impl TraceRuntimeSubject {
    pub fn graph_key(&self) -> String {
        match self {
            Self::Effect { address, .. } => address.graph_key(),
            Self::Process { process_id } => format!("process:{process_id}"),
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct TraceLanguageExecutionGeneration {
    attempt: u32,
}

impl TraceLanguageExecutionGeneration {
    /// One durable attempt of a process. The process id is minted once and
    /// never reused, so it already names the lifetime; the attempt is the
    /// only generation left to tell apart.
    pub const fn new(attempt: u32) -> Self {
        Self { attempt }
    }

    pub const fn attempt(self) -> u32 {
        self.attempt
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct TraceLanguageExecutionIdentity {
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    pub document: WorkflowDocumentRef,
    pub entry_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_execution_id: Option<String>,
    /// One-based durable attempt of the process. Absent only for a foreground
    /// effect execution that is not process-admitted.
    #[serde(flatten)]
    pub generation: Option<TraceLanguageExecutionGeneration>,
}

#[derive(Deserialize)]
struct TraceLanguageExecutionIdentityWire {
    scope: TraceRuntimeScope,
    subject: TraceRuntimeSubject,
    document: WorkflowDocumentRef,
    entry_name: String,
    engine_execution_id: Option<String>,
    attempt: Option<u32>,
}

impl<'de> Deserialize<'de> for TraceLanguageExecutionIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TraceLanguageExecutionIdentityWire::deserialize(deserializer)?;
        Ok(Self {
            scope: wire.scope,
            subject: wire.subject,
            document: wire.document,
            entry_name: wire.entry_name,
            engine_execution_id: wire.engine_execution_id,
            generation: wire.attempt.map(TraceLanguageExecutionGeneration::new),
        })
    }
}

impl TraceLanguageExecutionIdentity {
    pub const fn attempt(&self) -> Option<u32> {
        match self.generation {
            Some(generation) => Some(generation.attempt()),
            None => None,
        }
    }

    pub fn graph_key(&self) -> String {
        match self.generation {
            Some(generation) => format!(
                "{}:attempt:{attempt}",
                self.subject.graph_key(),
                attempt = generation.attempt(),
            ),
            None => self.subject.graph_key(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLanguageExecution {
    pub event_key: String,
    pub identity: TraceLanguageExecutionIdentity,
    #[serde(flatten)]
    pub payload: TraceLanguageExecutionPayload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLanguageChildExecution {
    pub scope: TraceRuntimeScope,
    pub process_id: lash_sansio::ProcessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<WorkflowDocumentRef>,
}

impl TraceLanguageChildExecution {
    pub fn graph_key(&self) -> Option<String> {
        self.attempt
            .map(|attempt| format!("process:{}:attempt:{attempt}", self.process_id,))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceLanguageExecutionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TraceLanguageExecutionStatus {
    /// Whether no later execution status may replace this status.
    pub const fn is_terminal(self) -> bool {
        match self {
            Self::Running => false,
            Self::Completed | Self::Failed | Self::Cancelled => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceBranchSelection {
    Then,
    Else,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceError {
    pub retryable: bool,
    pub terminal_reason: TraceLlmTerminalReason,
    pub failure_kind: TraceProviderFailureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<TraceFailureCode>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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

#[expect(
    clippy::disallowed_methods,
    reason = "JsonlTraceSink is the filesystem trace sink; the host injects the path (FIG-2971)"
)]
impl TraceSink for JsonlTraceSink {
    /// Write the record as one `line\n` call.
    ///
    /// The record and its newline go out in a single `write_all`: issuing them
    /// as two writes (`writeln!` on a fresh handle does exactly that) lets a
    /// kill or a short write tear between the record and its terminator, and
    /// the next append would glue onto the partial line. When the file's last
    /// line is already unterminated — a torn record left behind — the tail is
    /// truncated first, so every line in the file stays a complete record.
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
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
            .read(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| TraceSinkError::Open {
                path: self.path.clone(),
                source,
            })?;
        truncate_torn_tail(&mut file)
            .and_then(|()| file.write_all(line.as_bytes()))
            .map_err(|source| TraceSinkError::Write {
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
mod tests;
