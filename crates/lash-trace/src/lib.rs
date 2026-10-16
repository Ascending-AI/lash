//! Runtime diagnostics for Lash: the [`TraceSink`] channel and its
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
//! [`TraceSink`] is the host seam: lash ships no trace writer, so a host that
//! wants records in a file or on a console implements its own sink (see
//! `docs/architecture/tracing.md`). Records carry schema
//! [`TRACE_SCHEMA_VERSION`]; [`TeeTraceSink`] fans out to several sinks; and the
//! optional `otel` feature adds `otel::OtelTelemetry`, a sink that exports
//! each sampled closing or whole record as a span under lash's own ids. This is the diagnostics reporting channel —
//! distinct from the app-facing `TurnActivity` stream and the low-level
//! `SessionStreamEvent` stream that the runtime crates expose.
//!
//! For the full map of reporting channels, guidance on when to consume which,
//! and the schema-evolution policy that governs [`TRACE_SCHEMA_VERSION`], see
//! [the tracing contract](https://github.com/Ascending-AI/lash/blob/main/docs/architecture/tracing.md).
//! Admission exports have a retained reconciliation obligation and may duplicate
//! between export and its record; other diagnostics are best effort and may be
//! lost or repeated after a crash. See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash).
mod limits;
pub use limits::ObservationWorkLimits;
pub mod policy;
pub use policy::{
    AttrKey, ContentLimits, PolicyOperand, PolicyRelation, PolicyValue, PublicationHold,
    TraceAttributeWriter, TraceDetail, TraceDetailPolicy, TraceExportPolicy, TraceLimits,
    TraceMetrics, TraceOmission, TraceOmissionCounts, TraceOmissionDiagnostic, TraceOmissionLedger,
    TraceOmissionReport, TraceOmissionSite, TraceOmissionSiteKind, TracePolicyError,
    TracePolicyField, TracePolicySelection, TracePolicySnapshot, TraceValue, WriterRemaining,
};

use lash_sansio::{
    BatchId, CausalRef, InputId, ProcessId, SessionId, ToolCallId, ToolIntentIdentity, TurnId,
};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod content;
mod content_block;
mod domain;
mod event;
mod flight_recorder;
mod language_execution;
mod language_execution_failure;
#[cfg(feature = "otel")]
pub mod otel;
mod position;
#[cfg(any(test, feature = "testing"))]
mod recording;
pub mod telemetry;
mod time;
mod workflow_overlay;

pub use content::{
    Capture, CaptureSelector, ContentCategory, ContentField, ContentHook, ContentMarks,
    ContentPath, ContentPolicy, ContentValue, ContentView, RecordContent, SelectorView,
    TraceNarrowing,
};
pub use content_block::{TraceContentBlock, TraceToolResultBlock};
pub use domain::{
    TraceAttemptObservation, TraceDomainCompletion, TraceDomainStatus, TraceDomainSubject,
    TraceModelAttempt, TraceModelAttemptSpan, TraceModelCall, TraceRuntimeStreamEvent,
    TraceRuntimeStreamPayload,
};
pub use event::{TraceEvent, TraceEventKind};
pub use flight_recorder::{
    FlightRecorderOutcome, FlightRecorderRecord, FlightRecorderSettings, FlightRecorderSink,
    FlightRecorderSnapshot,
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
pub use position::{SpanRole, TracePosition, TraceSpanKind, TraceSpanKindSet};
#[cfg(any(test, feature = "testing"))]
pub use recording::RecordingTraceSink;
pub use telemetry::metrics::registry::{RefinableKey, attribute_reserve};
pub use telemetry::{
    DurableTraceScope, EmissionPermit, EmissionSource, InvalidTraceCarrier, InvalidTraceLinks,
    LashIdentity, ProducerKind, ProducerOutcome, ProducerSpan, ProducerSpec, SpanKey,
    TRACESTATE_CHAR_LIMIT, TRACESTATE_MEMBER_LIMIT, TraceAttemptId, TraceAttemptLink, TraceCall,
    TraceCaller, TraceCarrier, TraceCause, TraceIdentity, TraceLinkMode, TraceLinks, TraceParent,
    TraceRecordIdentity, TraceScopeAdmission, TraceScopeId, TraceScopeKind, TraceScopeOffer,
    TraceScopeOwner, TraceStateBudget, TraceStateLimits, TraceToolOwner, TraceTransitionKind,
    W3cSpanId, W3cTraceFlags, W3cTraceId, W3cTraceState, derive_span,
};
pub use time::TraceTimestamp;
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

/// Version of the trace record schema, written to
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
/// the graph snapshot becomes the workflow execution overlay. FIG-6299
/// gives every record its W3C `position` (its span, parent, links, role,
/// kind and times), types `timestamp` as lash's `TraceTimestamp` with the
/// same wire form, renames `attachment_degraded`'s own `position` to
/// `attachment_position`, adds `producer_settled` and the `discarded` tool
/// outcome, and drops the context's `run_id`, `metadata` and
/// `parent_graph_node_id`.
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
pub const TRACE_SCHEMA_VERSION: u32 = 37;

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

/// The stable lash ids a span and its records belong to. Every field is a
/// retained lash identity, the same on every owner: a host keys its own
/// concepts on these.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// The logical run; a host may choose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnId>,
    /// The turn's head input, or a producer's accepted input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<InputId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessId>,
    /// A batch send's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_intent: Option<ToolIntentIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<ToolCallId>,
    /// A code cell's graph key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// A model call's id: the `chat` span's `lash.llm_call.id`, and the
    /// `llm_call_id` of the call's request scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_call: Option<String>,
}

/// A record's correlation: the stable ids it belongs to and the effect that
/// made it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// The turn's head input, or a producer's accepted input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_id: Option<InputId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<ProcessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<BatchId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_intent: Option<ToolIntentIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    /// A code cell's graph key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// The workflow-graph join key of a language-execution record: the
    /// observed node's structural id, equal to the `WorkflowGraph` node id
    /// (ADR 0100 R0). A child start carries its parent node's id, and
    /// execution start and finish leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<String>,
    /// The replay key of the effect invocation that made the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_key: Option<String>,
    /// The fact that caused the effect invocation that made the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_call_id: Option<String>,
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

    pub fn for_process(mut self, process_id: impl Into<ProcessId>) -> Self {
        self.process_id = Some(process_id.into());
        self
    }

    pub fn for_call(mut self, call_id: impl Into<ToolCallId>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }

    /// The stable ids this record belongs to.
    pub fn ids(&self) -> TraceIds {
        TraceIds {
            session: self.session_id.clone(),
            turn: self.turn_id.clone(),
            send: self.send_id.clone(),
            process: self.process_id.clone(),
            batch: self.batch_id.clone(),
            tool_intent: self.tool_intent.clone(),
            call: self.call_id.clone(),
            cell: self.cell.clone(),
            llm_call: self.llm_call_id.clone(),
        }
    }

    /// The context of a record that belongs to `ids` and no effect.
    pub fn of_ids(ids: TraceIds) -> Self {
        Self {
            session_id: ids.session,
            turn_id: ids.turn,
            send_id: ids.send,
            process_id: ids.process,
            batch_id: ids.batch,
            tool_intent: ids.tool_intent,
            call_id: ids.call,
            cell: ids.cell,
            llm_call_id: ids.llm_call,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceRecord {
    pub schema_version: u32,
    pub id: String,
    pub timestamp: TraceTimestamp,
    /// Whether this record carries content or had it omitted under the
    /// host's [`ContentPolicy`] policy: how to read an empty content field.
    pub content: ContentMarks,
    pub context: TraceContext,
    /// The record's place in the trace. `None` only for an unscoped
    /// diagnostic and for a record of a scope nothing observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<TracePosition>,
    /// What lash did not keep of the contributions to this record, by
    /// reason; its id and position are the omissions' address. Empty, and
    /// omitted, when nothing was refused.
    #[serde(default, skip_serializing_if = "TraceOmissionCounts::is_empty")]
    pub omissions: TraceOmissionCounts,
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
            timestamp: TraceTimestamp,
            content: ContentMarks,
            context: TraceContext,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            position: Option<TracePosition>,
            #[serde(default, skip_serializing_if = "TraceOmissionCounts::is_empty")]
            omissions: TraceOmissionCounts,
            #[serde(flatten)]
            event: TraceEvent,
        }

        TraceRecordSchema::json_schema(generator)
    }
}

#[derive(Deserialize)]
struct TraceRecordWire {
    schema_version: u32,
    id: String,
    timestamp: TraceTimestamp,
    content: ContentMarks,
    context: TraceContext,
    #[serde(default)]
    position: Option<TracePosition>,
    #[serde(default)]
    omissions: TraceOmissionCounts,
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
            content: wire.content,
            context: wire.context,
            position: wire.position,
            omissions: wire.omissions,
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
            TraceToolCallOutcome::Discarded => TraceToolCallStatus::Discarded,
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
            TraceToolCallOutcome::Discarded => Value::Null,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum TraceToolCallOutcome {
    Success(Value),
    Failure(Value),
    Cancelled(Value),
    /// The call never settled: the session closed while it ran, and its
    /// turn's terminal owner closed its span. It has no output.
    Discarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceToolCallStatus {
    Success,
    Failure,
    Cancelled,
    Discarded,
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceLlmRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_instructions: Option<String>,
    pub composition_fingerprint: String,
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// The host's [`ContentPolicy`] policy withheld it.
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
        Ok(Value::Object(tag)) if tag.len() == 1 => tag.keys().next().cloned().unwrap_or_default(),
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

/// The product graph's consumer of language-execution observations: the
/// process and language graph a host reads, not telemetry.
///
/// It is fed through its own path, not as a [`TraceSink`]: it takes no
/// emission permit and sees each record on replay too, because it folds every
/// record by the record's own identity. A record sink never receives these
/// observations through it.
pub trait ProductObservationSink: Send + Sync {
    fn observe(&self, record: &TraceRecord);
}

/// Fans each trace record out to several sinks in order (e.g. a host's
/// console sink and its file sink).
///
/// Every sink runs on every call and the first error is returned once they all
/// have: the sinks are independent destinations, so a console stream that a
/// supervisor closed must not cost the run its durable trace file. Only the first error is
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

/// Cross-crate engine support, outside the facade's host contract.
#[doc(hidden)]
pub mod core_internal {
    /// Starts a record at its core ceiling or its leaf parent's narrowing.
    pub fn content_narrowing(
        policy: &crate::TracePolicySnapshot,
        parent: Option<&crate::TraceNarrowing>,
    ) -> crate::TraceNarrowing {
        parent.map_or_else(
            || crate::TraceNarrowing::new(policy),
            crate::TraceNarrowing::inherited,
        )
    }
}
