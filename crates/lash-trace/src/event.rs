//! Trace events and their fieldless, compiler-derived kind vocabulary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    CellFailure, ExecCodeFailureReason, TextProjectionMetadata, TraceDomainCompletion,
    TraceDomainStatus, TraceDurableTimerStatus, TraceDurableWaitResolution,
    TraceEffectEnvelopeDiffEvent, TraceError, TraceExecToolCall, TraceJournaledEffectStatus,
    TraceLanguageExecution, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceLlmAttempt, TraceLlmRequest, TraceLlmResponse, TraceProgramStepOutcome,
    TracePromptComponent, TraceProviderReplayDropEvent, TraceProviderRequestEvent,
    TraceProviderStreamEvent, TraceRetryAttempt, TraceRuntimeStreamEvent, TraceStoreErrorClass,
    TraceTokenUsage, TraceToolCallOutcome, TraceToolCallOutput, TraceToolSpec, TraceToolTerminal,
    TraceTurnOutcome,
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
    /// One attachment was omitted so an otherwise valid session can continue.
    AttachmentDegraded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        source: lash_sansio::AttachmentMaterializationSource,
        reason: lash_sansio::AttachmentMaterializationReason,
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
    /// One real provider request attempt of a model call, made by the body
    /// that dispatched it. A call that retried reports one per attempt.
    LlmAttemptCompleted {
        attempt: TraceLlmAttempt,
    },
    /// The terminal of a durable domain operation that has no record of its
    /// own kind: a run, a process, a process segment, a host send or a tool
    /// intent. It is a logical record of the operation's scope.
    DomainCompleted {
        completion: TraceDomainCompletion,
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
        /// Lash's identity for the call (ADR 0117).
        call_id: lash_sansio::ToolCallId,
        /// The model provider's id for the call, when a model issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issuing_node_id: Option<String>,
    },
    /// A logical receipt from recorded Run events. A final is observed only
    /// after its protected presentation; a Deferred attempt has no terminal.
    ToolReceipt {
        call_id: lash_sansio::ToolCallId,
        name: String,
        started_at_ms: u64,
        terminal: Option<TraceToolTerminal>,
    },
    ToolCallCompleted {
        /// Lash's identity for the call (ADR 0117).
        call_id: lash_sansio::ToolCallId,
        /// The model provider's id for the call, when a model issued it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        name: String,
        args: Value,
        output: TraceToolCallOutput,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issuing_node_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempts: Option<Vec<TraceRetryAttempt>>,
    },
    ExecCodeStarted {
        code: String,
        code_chars: usize,
    },
    ExecCodeCompleted {
        duration_ms: u64,
        output: String,
        output_chars: usize,
        observation_count: usize,
        observation_projections: Vec<TextProjectionMetadata>,
        error: Option<CellFailure>,
        terminal_finish: Option<Value>,
        tool_calls: Vec<TraceExecToolCall>,
    },
    ExecCodeFailed {
        /// Closed failure classification; offline analysis keys on this rather
        /// than matching `error` prose.
        reason: ExecCodeFailureReason,
        /// Human-readable detail. Not a stable classification surface.
        error: String,
    },
    ObservationProjection {
        projections: Vec<TextProjectionMetadata>,
    },
    /// A journaled effect is about to cross its durable substrate's journal
    /// command boundary.
    JournaledEffectStarted {
        effect_name: String,
        effect_kind: String,
    },
    /// A journaled effect returned its recorded or newly executed outcome.
    JournaledEffectSettled {
        effect_name: String,
        effect_kind: String,
        status: TraceJournaledEffectStatus,
    },
    /// A durable wait has issued its park command.
    DurableWaitParked {
        wait_kind: String,
    },
    /// A durable wait resumed with a terminal resolution.
    DurableWaitResolved {
        started_at_ms: u64,
        wait_kind: String,
        resolution: TraceDurableWaitResolution,
    },
    /// A durable timer has been issued.
    DurableTimerStarted {
        duration_ms: u64,
    },
    /// A durable timer resumed or was cancelled.
    DurableTimerResolved {
        duration_ms: u64,
        status: TraceDurableTimerStatus,
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
        language: String,
        event: TraceLanguageExecution,
    },
    TurnCompleted {
        outcome: TraceTurnOutcome,
    },
    Custom {
        name: String,
        payload: Value,
    },
}

impl TraceEvent {
    /// Whether this build can decode a trace event kind. Readers use this
    /// before decoding so an unfamiliar observational event can be skipped.
    pub(crate) fn knows_kind(kind: &str) -> bool {
        kind.parse::<TraceEventKind>().is_ok()
    }

    /// - [`Self::LlmCallFailed`], [`Self::EffectEnvelopeDiff`], and
    ///   [`Self::StoreErrorObserved`] always;
    /// - [`Self::DomainCompleted`] only with [`TraceDomainStatus::Failed`];
    /// - [`Self::JournaledEffectSettled`] only with
    ///   [`TraceJournaledEffectStatus::Failed`];
    /// - [`Self::DurableTimerResolved`] only with [`TraceDurableTimerStatus::Failed`];
    /// - [`Self::DurableWaitResolved`] only with [`TraceDurableWaitResolution::Failed`];
    /// - [`Self::ToolCallCompleted`] only with [`TraceToolCallOutcome::Failure`];
    /// - [`Self::TurnCompleted`] only with [`TraceTurnOutcome::Failed`], for any
    ///   [`crate::TraceTurnFailureReason`] (`Incomplete`, `InvalidInput`, `MaxTurns`,
    ///   `ToolFailure`, `ProviderError`, `ContextOverflow`, `PluginAbort`,
    ///   `RuntimeError`, `SubmittedError`, or `ToolError`); and
    /// - [`Self::ProgramStep`] when compile/link failed; and
    /// - [`Self::LanguageExecution`] for
    ///   [`TraceLanguageExecutionPayload::NodeFailed`] or
    ///   [`TraceLanguageExecutionPayload::ExecutionFinished`] with
    ///   [`TraceLanguageExecutionStatus::Failed`].
    ///
    /// All other event outcomes are not failures. In particular,
    /// [`TraceTurnOutcome::AgentFrameSwitch`], completed or cancelled turns,
    /// and successful or cancelled tool calls return `false`.
    pub fn is_failed(&self) -> bool {
        match self {
            Self::LlmCallFailed { .. }
            | Self::EffectEnvelopeDiff { .. }
            | Self::StoreErrorObserved { .. } => true,
            Self::ProgramStep { outcome, .. } => match outcome {
                TraceProgramStepOutcome::Ok => false,
                TraceProgramStepOutcome::Failure { .. } => true,
            },
            Self::JournaledEffectSettled { status, .. } => status.is_failed(),
            Self::DurableTimerResolved { status, .. } => status.is_failed(),
            Self::DurableWaitResolved { resolution, .. } => resolution.is_failed(),
            Self::ToolCallCompleted { output, .. } => match &output.outcome {
                TraceToolCallOutcome::Failure(_) => true,
                TraceToolCallOutcome::Success(_) | TraceToolCallOutcome::Cancelled(_) => false,
            },
            Self::ToolReceipt { terminal, .. } => matches!(
                terminal,
                Some(TraceToolTerminal::Denied | TraceToolTerminal::Aborted)
            ),
            Self::TurnCompleted { outcome, .. } => outcome.is_failed(),
            Self::DomainCompleted { completion } => completion.status == TraceDomainStatus::Failed,
            Self::LanguageExecution { event, .. } => match &event.payload {
                TraceLanguageExecutionPayload::NodeFailed { .. } => true,
                TraceLanguageExecutionPayload::ExecutionFinished { status, .. } => match status {
                    TraceLanguageExecutionStatus::Failed => true,
                    TraceLanguageExecutionStatus::Running
                    | TraceLanguageExecutionStatus::Completed
                    | TraceLanguageExecutionStatus::Cancelled => false,
                },
                TraceLanguageExecutionPayload::ExecutionStarted { .. }
                | TraceLanguageExecutionPayload::NodeStarted { .. }
                | TraceLanguageExecutionPayload::NodeWaiting { .. }
                | TraceLanguageExecutionPayload::NodeResumed { .. }
                | TraceLanguageExecutionPayload::NodeCancelled { .. }
                | TraceLanguageExecutionPayload::NodeCompleted { .. }
                | TraceLanguageExecutionPayload::BranchSelected { .. }
                | TraceLanguageExecutionPayload::ChildStarted { .. } => false,
            },
            Self::TurnStarted { .. }
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
            | Self::ProviderRequest { .. }
            | Self::ProviderReplayDropped { .. }
            | Self::ProviderStreamEvent { .. }
            | Self::RuntimeStreamEvent { .. }
            | Self::ToolCallStarted { .. }
            | Self::ExecCodeStarted { .. }
            | Self::ExecCodeCompleted { .. }
            | Self::ExecCodeFailed { .. }
            | Self::ObservationProjection { .. }
            | Self::JournaledEffectStarted { .. }
            | Self::DurableWaitParked { .. }
            | Self::DurableTimerStarted { .. }
            | Self::ProtocolStep { .. }
            | Self::Custom { .. } => false,
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
