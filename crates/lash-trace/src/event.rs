//! Trace events and their fieldless, compiler-derived kind vocabulary.

use std::collections::BTreeMap;

use lash_protocol_types::{llm::types::AttemptRecord, llm::types::LlmUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    CellFailure, ExecCodeFailureReason, TextProjectionMetadata, TraceAttemptObservation,
    TraceDomainCompletion, TraceDomainStatus, TraceEffectEnvelopeDiffEvent, TraceError,
    TraceExecToolCall, TraceLanguageExecution, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus, TraceLlmRequest, TraceLlmResponse, TraceNodeFact,
    TraceProgramStepOutcome, TracePromptComponent, TraceProviderEvent,
    TraceProviderReplayDropEvent, TraceRetryAttempt, TraceRuntimeStreamEvent, TraceStoreErrorClass,
    TraceToolCallOutcome, TraceToolCallOutput, TraceToolSpec, TraceTurnOutcome,
};

#[derive(
    Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema, strum::EnumDiscriminants,
)]
#[strum_discriminants(name(TraceEventKind))]
#[strum_discriminants(
    doc = "The event kinds this build emits and recognises, derived from TraceEvent."
)]
#[strum_discriminants(derive(
    strum::EnumString,
    strum::IntoStaticStr,
    strum::Display,
    strum::VariantArray,
    Hash
))]
#[strum_discriminants(strum(serialize_all = "snake_case"))]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "TraceEvent is a public DTO; keeping event payloads inline preserves ergonomic pattern matching"
)]
pub enum TraceEvent {
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
    /// A model call's prompt did not compose, so nothing was sent: the
    /// decision evidence. `plan` is the resolved plan the composition ran
    /// (absent when the plan itself did not resolve), `limits` the plan's
    /// configured limits, `error` the typed failure with its site and the
    /// measured bytes, and `elapsed_ms` the time the composition took.
    PromptCompositionFailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<Value>,
        limits: Value,
        error: Value,
        elapsed_ms: u64,
    },
    /// One attachment was omitted so an otherwise valid session can continue.
    AttachmentDegraded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// On the wire `attachment_position`: a record's own `position`
        /// is its place in the trace.
        #[serde(rename = "attachment_position")]
        position: lash_protocol_types::llm::attachment_delivery::AttachmentPosition,
        reason: lash_protocol_types::AttachmentMaterializationReason,
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
    CompactionNeeded {
        used_tokens: usize,
        max_context_tokens: usize,
        threshold_tokens: usize,
    },
    CompactionStarted {
        source_messages: usize,
        instructions_present: bool,
    },
    CompactionCompleted {
        summary_nodes: usize,
    },
    PromptViewAttachmentsPruned {
        used_tokens: usize,
        max_context_tokens: usize,
        pruned_attachments: usize,
    },
    LlmCallStarted {
        request: TraceLlmRequest,
    },
    LlmCallCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<crate::TraceModelCall>,
        request: TraceLlmRequest,
        #[serde(default)]
        response_metadata: BTreeMap<String, Value>,
        response: TraceLlmResponse,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<LlmUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<Vec<crate::TraceModelAttempt>>,
    },
    LlmCallFailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<crate::TraceModelCall>,
        request: TraceLlmRequest,
        error: TraceError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream_summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<Vec<crate::TraceModelAttempt>>,
    },
    /// One real provider request attempt of a model call, reported by the
    /// body that dispatched it when the attempt is sealed. `attempt` is the
    /// record the call's `attempts` ledger carries under the same ordinal. A
    /// call that retried reports one per attempt.
    LlmAttemptCompleted {
        #[serde(default)]
        response_metadata: BTreeMap<String, Value>,
        attempt: AttemptRecord,
        observation: TraceAttemptObservation,
    },
    /// The terminal of a durable domain operation that has no record of its
    /// own kind: a process or a tool intent. It is a logical record of the
    /// operation's scope.
    DomainCompleted {
        completion: TraceDomainCompletion,
    },
    ProviderEvent {
        event: TraceProviderEvent,
    },
    ProviderReplayDropped {
        event: TraceProviderReplayDropEvent,
    },
    EffectEnvelopeDiff {
        event: TraceEffectEnvelopeDiffEvent,
    },
    RuntimeStreamEvent {
        event: TraceRuntimeStreamEvent,
    },
    ToolCallStarted {
        /// Lash's identity for the call (ADR 0117).
        call_id: lash_protocol_types::ToolCallId,
        /// The model provider's id for the call, when a model issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issuing_node_id: Option<String>,
    },
    ToolCallCompleted {
        /// Lash's identity for the call (ADR 0117).
        call_id: lash_protocol_types::ToolCallId,
        /// The model provider's id for the call, when a model issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Exporters outside lash cannot read the store, so provider correlation travels with the trace.
        provider_call_id: Option<String>,
        /// Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
        name: TraceToolName,
        /// Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
        args: TraceToolArgs,
        /// Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
        output: TraceToolCallOutput,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Exporters outside lash cannot read the store, so issuer correlation travels with the trace.
        issuing_node_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// Exporters outside lash cannot read the journal, so the observed retry ladder travels with the trace.
        attempts: Option<Vec<TraceRetryAttempt>>,
        /// What the body of the attempt that settled the call set on its own
        /// tool span: in memory only, stamped by the span's closer.
        #[serde(default, skip_serializing_if = "crate::ToolSpanAnnotations::is_empty")]
        span_attributes: crate::ToolSpanAnnotations,
    },
    /// A code cell's span opens: once per execution of the cell, starting at
    /// the cell's retained first start.
    ExecCodeStarted {
        language: String,
        code_chars: usize,
    },
    /// A code cell answered, and the commit that consumed its answer closes
    /// its span. `started_at_ms` is the cell's first start, which a
    /// suspension or a re-run keeps; `duration_ms` runs from it.
    ExecCodeCompleted {
        /// Observed bytes discarded after the model reply's first cell.
        #[serde(default, skip_serializing_if = "is_zero")]
        trailing_discarded: usize,
        code: String,
        language: String,
        started_at_ms: u64,
        duration_ms: u64,
        output: String,
        output_chars: usize,
        observation_count: usize,
        observation_projections: Vec<TextProjectionMetadata>,
        error: Option<CellFailure>,
        terminal_finish: Option<Value>,
        tool_calls: Vec<TraceExecToolCall>,
    },
    /// A code cell answered with a failure, and the commit that consumed its
    /// answer closes its span, from its first start.
    ExecCodeFailed {
        /// Observed bytes discarded after the model reply's first cell.
        #[serde(default, skip_serializing_if = "is_zero")]
        trailing_discarded: usize,
        code: String,
        language: String,
        started_at_ms: u64,
        /// The observed window, absent when no invocation existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// Closed failure classification; offline analysis keys on this rather
        /// than matching `error` prose.
        reason: ExecCodeFailureReason,
        /// Human-readable detail. Not a stable classification surface.
        error: String,
    },
    /// A code cell its turn's terminal stopped before it answered: that
    /// terminal's owner closes its span, from its first start. A discard is
    /// not a failure.
    ExecCodeDiscarded {
        /// Observed bytes discarded after the model reply's first cell.
        #[serde(default, skip_serializing_if = "is_zero")]
        trailing_discarded: usize,
        code: String,
        language: String,
        started_at_ms: u64,
        duration_ms: u64,
    },
    ObservationProjection {
        projections: Vec<TextProjectionMetadata>,
    },
    /// The runtime received a typed, non-retryable store integrity failure.
    StoreErrorObserved {
        operation: String,
        error_class: TraceStoreErrorClass,
        message: String,
    },
    /// Compile/link evidence a protocol plugin reports before it executes a
    /// program the model submitted as one protocol step.
    ProgramStep {
        /// Protocol iteration of the submitted program within its turn.
        step_index: usize,
        #[serde(flatten)]
        outcome: TraceProgramStepOutcome,
    },
    ProtocolStep {
        plugin_id: String,
        payload: Value,
    },
    LanguageExecution {
        /// The source dialect, when the execution records one. Graph artifacts
        /// are dialect-free and leave it absent.
        language: Option<String>,
        event: TraceLanguageExecution,
    },
    /// The admitted body of a process step started.
    StepBodyStarted {
        step: crate::StepBodyStarted,
    },
    TurnCompleted {
        /// Admitted input messages, captured by the terminal owner from durable input.
        input: Vec<crate::TraceLlmMessage>,
        /// The assistant answer this turn settled with, governed as OutputMessages.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        output: Vec<crate::TraceLlmMessage>,
        /// Model calls admitted by this turn, including failed calls; absent
        /// when the terminal owner cannot recover that fact.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inference_calls: Option<u32>,
        /// Tool calls admitted by the turn and its cells, including failures.
        /// The terminal owner reads the stored count plus acknowledged appends;
        /// absent only for a record with no known execution counts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<u64>,
        inputs: usize,
        outcome: TraceTurnOutcome,
    },
    /// A host call that admits durable work ended: its producer span's one
    /// record.
    ProducerSettled {
        producer: crate::ProducerKind,
        #[serde(flatten)]
        outcome: crate::ProducerOutcome,
        /// A process start's time per phase it reached, in phase order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        phases: Vec<crate::ProcessStartPhaseTime>,
    },
    /// A built-in tool check selected one terminal reply over others. The
    /// same fact reaches session observers as a plugin runtime event.
    ToolCheckConflict {
        /// The plugin whose reply won.
        plugin_id: String,
        conflict: lash_protocol_types::ToolCheckConflict,
    },
    /// An extension span or point ended: its one whole record, with what
    /// it buffered. Its position names the span and its parent.
    ExtensionSpan(crate::ExtensionSpanRecord),
    /// What one extension body's contributions lost, with the body's
    /// address: emitted once when the body exits, only when not empty.
    BodyOmissions(crate::TraceOmissionDiagnostic),
    /// A context-pressure hook's decision opened a compaction frame for the
    /// turn. `task` is the frame's model-facing task.
    ContextPressure {
        plugin_id: String,
        hook_id: String,
        task: String,
        frame_node_id: String,
    },
}

impl TraceEvent {
    /// - [`Self::LlmCallFailed`], [`Self::EffectEnvelopeDiff`], and
    ///   [`Self::StoreErrorObserved`] always;
    /// - [`Self::ExecCodeFailed`] always and [`Self::ExecCodeCompleted`] when
    ///   its executor reported a [`CellFailure`];
    /// - [`Self::DomainCompleted`] only with [`TraceDomainStatus::Failed`];
    /// - [`Self::ToolCallCompleted`] only with [`TraceToolCallOutcome::Failure`];
    /// - [`Self::TurnCompleted`] only with [`TraceTurnOutcome::Failed`], for any
    ///   [`crate::TraceTurnFailureReason`] (`Incomplete`, `InvalidInput`, `MaxTurns`,
    ///   `ToolFailure`, `ProviderError`, `ContextOverflow`, `PluginAbort`,
    ///   `RuntimeError`, `AgentFrameSwitchLimit`, `SubmittedError`, or
    ///   `ToolError`); and
    /// - [`Self::ProgramStep`] when compile/link failed; and
    /// - [`Self::LanguageExecution`] for
    ///   [`TraceNodeFact::Failed`] or
    ///   [`TraceLanguageExecutionPayload::ExecutionFinished`] with
    ///   [`TraceLanguageExecutionStatus::Failed`].
    ///
    /// All other event outcomes are not failures. In particular,
    /// [`TraceTurnOutcome::AgentFrameSwitch`], completed or cancelled turns,
    /// and successful or cancelled tool calls return `false`.
    pub fn is_failed(&self) -> bool {
        match self {
            Self::LlmCallFailed { .. }
            | Self::PromptCompositionFailed { .. }
            | Self::EffectEnvelopeDiff { .. }
            | Self::StoreErrorObserved { .. }
            | Self::ExecCodeFailed { .. } => true,
            Self::ExecCodeCompleted { error, .. } => error.is_some(),
            Self::ProgramStep { outcome, .. } => match outcome {
                TraceProgramStepOutcome::Ok => false,
                TraceProgramStepOutcome::Failure { .. } => true,
            },
            Self::ToolCallCompleted { output, .. } => match &output.outcome {
                TraceToolCallOutcome::Failure(_) => true,
                TraceToolCallOutcome::Success(_)
                | TraceToolCallOutcome::Cancelled(_)
                | TraceToolCallOutcome::Discarded => false,
            },
            Self::TurnCompleted { outcome, .. } => outcome.is_failed(),
            Self::ProducerSettled { outcome, .. } => {
                matches!(outcome, crate::ProducerOutcome::Refused { .. })
            }
            Self::DomainCompleted { completion } => completion.status == TraceDomainStatus::Failed,
            Self::LanguageExecution { event, .. } => match &event.payload {
                TraceLanguageExecutionPayload::Node { fact, .. } => {
                    matches!(fact, TraceNodeFact::Failed { .. })
                }
                TraceLanguageExecutionPayload::ExecutionFinished { status, .. } => match status {
                    TraceLanguageExecutionStatus::Failed => true,
                    TraceLanguageExecutionStatus::Running
                    | TraceLanguageExecutionStatus::Completed
                    | TraceLanguageExecutionStatus::Cancelled => false,
                },
                TraceLanguageExecutionPayload::ExecutionStarted { .. }
                | TraceLanguageExecutionPayload::Counts { .. } => false,
            },
            Self::TurnStarted { .. }
            | Self::ExecCodeDiscarded { .. }
            | Self::PromptBuilt { .. }
            | Self::AttachmentDegraded { .. }
            | Self::CompositionChanged { .. }
            | Self::CompactionNeeded { .. }
            | Self::CompactionStarted { .. }
            | Self::CompactionCompleted { .. }
            | Self::PromptViewAttachmentsPruned { .. }
            | Self::LlmCallStarted { .. }
            | Self::LlmCallCompleted { .. }
            | Self::LlmAttemptCompleted { .. }
            | Self::ProviderEvent { .. }
            | Self::ProviderReplayDropped { .. }
            | Self::RuntimeStreamEvent { .. }
            | Self::ToolCallStarted { .. }
            | Self::ExecCodeStarted { .. }
            | Self::ObservationProjection { .. }
            | Self::ProtocolStep { .. }
            | Self::StepBodyStarted { .. }
            | Self::ToolCheckConflict { .. }
            | Self::BodyOmissions(_)
            | Self::ContextPressure { .. } => false,
            Self::ExtensionSpan(span) => span.error_type.is_some(),
        }
    }

    /// The kind of this event, from the same variants that define its serde tag.
    pub fn kind(&self) -> TraceEventKind {
        self.into()
    }
}

impl TraceEventKind {
    /// The `type` tag written by serde for this event kind.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Tool arguments observed at completion, distinct from arguments that are unavailable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "availability", content = "value", rename_all = "snake_case")]
pub enum TraceToolArgs {
    Value(Value),
    Unavailable,
}

impl From<Value> for TraceToolArgs {
    fn from(value: Value) -> Self {
        Self::Value(value)
    }
}

/// A tool's recorded name, or its identity when no recorded name is available.
/// Exporters use an identity to name the span, but omit `gen_ai.tool.name`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TraceToolName {
    Name(String),
    Identity {
        tool_id: lash_vocabulary::tool_contract::ToolId,
    },
}

impl TraceToolName {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Name(name) => Some(name),
            Self::Identity { .. } => None,
        }
    }
    /// The name or typed identity used in the span's display label.
    pub fn label(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Identity { tool_id } => tool_id.as_str(),
        }
    }
}
impl From<String> for TraceToolName {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}
impl From<&str> for TraceToolName {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}
