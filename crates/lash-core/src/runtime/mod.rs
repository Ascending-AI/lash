mod assembly;
mod builder;
pub(crate) mod causal;
mod clock;
mod config_ops;
pub use config_ops::SessionConfigPatch;
mod effect;
pub use effect::promise_semantics;
mod environment;
mod error;
mod event_pump;
mod host;
mod in_memory_store;
mod io;
mod lifecycle;
mod logical_turn;
mod observation;
mod process;
mod process_work_driver;
mod process_worker;
pub(crate) use process_worker::release_process_execution_permit_while;
mod queued_work_driver;
pub mod scenario_contracts;
mod session_api;
mod session_execution_lease;
mod session_manager;
mod session_ops;
mod state;
#[cfg(test)]
pub(crate) mod tests;
mod turn_boundary;
mod turn_commit_draft;
pub(crate) mod turn_control;
mod turn_driver;
mod turn_graph_editor;
mod turn_input_ingress;
mod turn_loop;
mod turn_queue;
mod usage;
mod wake_delivery_driver;

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::llm::types::{
    LlmOutputPart, LlmProviderTraceEvent, LlmProviderTraceSender, LlmRequest, LlmResponse,
    LlmStreamEvent, LlmUsage,
};
use crate::plugin::{
    CheckpointHookContext, PrepareTurnRequest, SessionConfigChangedContext, SessionRelation,
};
use crate::sansio::{LlmCallError, Response};
use crate::session_model::{
    Message, MessageRole, Part, PartKind, PruneState, RuntimeSessionPolicy, SessionPolicy,
    SessionStreamEvent, TokenUsage, make_error_event, reassign_part_ids, shared_parts,
    transport_stream_events,
};
use crate::{
    CheckpointKind, PersistentRuntimeServices, PluginOperationInvokeError, PromptHookContext,
    RuntimeServices, SandboxMessage, Session, SessionCreateRequest, SessionError, SessionHandle,
    SessionSnapshot, SessionStartPoint, ToolCallRecord, TurnFinish, TurnOutcome, TurnStop,
};
use crate::{Effect, TurnMachine};

use host::*;
use session_execution_lease::*;
use session_manager::*;
use turn_boundary::*;
use turn_commit_draft::*;
use turn_driver::*;

pub(super) fn runtime_error_from_store_commit(err: crate::store::StoreError) -> RuntimeError {
    match err {
        crate::store::StoreError::Contended => RuntimeError::new(
            RuntimeErrorCode::StoreCommitContended,
            "store commit is contended; retry the identical operation unchanged",
        ),
        err @ crate::store::StoreError::CommitNodeBudgetExceeded { .. } => RuntimeError::new(
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded,
            err.to_string(),
        ),
        err @ crate::store::StoreError::CommitByteBudgetExceeded { .. } => RuntimeError::new(
            RuntimeErrorCode::StoreCommitByteBudgetExceeded,
            err.to_string(),
        ),
        crate::store::StoreError::SessionExecutionLeaseExpired { session_id } => RuntimeError::new(
            RuntimeErrorCode::SessionExecutionLeaseLost,
            format!("session execution lease for session `{session_id}` was lost before commit"),
        ),
        err => RuntimeError::new(RuntimeErrorCode::StoreCommitFailed, err.to_string()),
    }
}

pub(super) fn session_commit_error(
    context: &str,
    source: crate::store::StoreError,
) -> SessionError {
    match source {
        source @ crate::store::StoreError::SessionDeleted { .. } => SessionError::Store {
            context: context.to_string(),
            source,
        },
        source => SessionError::Protocol(format!("{context}: {source}")),
    }
}

#[cfg(test)]
mod store_commit_error_tests {
    use super::{RuntimeErrorCode, runtime_error_from_store_commit};
    use crate::store::StoreError;

    #[test]
    fn commit_budget_errors_preserve_the_budget_kind_and_limits() {
        let node_error = runtime_error_from_store_commit(StoreError::CommitNodeBudgetExceeded {
            node_count: 513,
            max_nodes: 512,
        });
        assert_eq!(
            node_error.code,
            RuntimeErrorCode::StoreCommitNodeBudgetExceeded
        );
        assert!(node_error.message.contains("513 graph nodes"));
        assert!(node_error.message.contains("512-node transaction budget"));

        let byte_error = runtime_error_from_store_commit(StoreError::CommitByteBudgetExceeded {
            graph_delta_bytes: 900_000,
            checkpoint_bytes: 150_000,
            attachment_manifest_bytes: 1,
            total_bytes: 1_050_001,
            max_bytes: 1_048_576,
        });
        assert_eq!(
            byte_error.code,
            RuntimeErrorCode::StoreCommitByteBudgetExceeded
        );
        assert!(
            byte_error
                .message
                .contains("1050001 budgeted payload bytes")
        );
        assert!(
            byte_error
                .message
                .contains("1048576-byte transaction budget")
        );
    }
}

// `PromptUsage` is re-exported below alongside the runtime's own types.
pub use lash_sansio::PromptUsage;

use assembly::{
    LlmDebugText, LlmDebugToolCall, LlmStreamAccumulator, LlmStreamDebugState, LlmStreamEventLog,
    LlmStreamState, LlmStreamSummary, TurnAssembler,
};
#[cfg(test)]
#[allow(unused_imports)]
use assembly::{classify_output_state, sanitize_assistant_output};
pub use builder::EmbeddedRuntimeBuilder;
pub use causal::process_event_invocation;
pub(crate) use causal::tool_retry_sleep_invocation;
pub use clock::{Clock, SystemClock};
pub(crate) use effect::RuntimeEffectControllerHandle;
pub use effect::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CanonicalRuntimeEffectEnvelope, CausalRef, EffectHost, EffectJournalIdentity,
    EffectJournalRetirement, ExecutionScope, ExternalCompletionError, InlineEffectHost,
    InlineRuntimeEffectController, LlmAttachmentSpec, LlmRequestSpec, ProcessCommand,
    ProcessEffectOutcome, ProcessTurnCancellation, Resolution, ResolveOutcome,
    RuntimeAwaitEventOptions, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeEffectReplayMismatchSummary, RuntimeEffectReplayTrace, RuntimeInvocation,
    RuntimeLlmCallOutcome, RuntimeReplay, RuntimeScope, RuntimeSleepOptions, RuntimeSubject,
    ScopedEffectController, SegmentProgress, ToolAttemptEffectOutcome, ToolAttemptLaunch,
    ToolBatchEffectOutcome, ToolCallLaunch, validate_replayed_effect_envelope,
};
pub use environment::{ParkedSession, RuntimeEnvironment, RuntimeEnvironmentBuilder};
pub use error::{RuntimeError, RuntimeErrorCause, RuntimeErrorCode};
#[doc(hidden)]
pub use event_pump::drive_with_event_pump;
pub use host::{EmbeddedRuntimeHost, ProcessRuntimeHost, RuntimeHostConfig};
pub use in_memory_store::{InMemorySessionStore, InMemorySessionStoreFactory};
use io::normalize_input_items;
pub use observation::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayGap, LiveReplayGapReason,
    LiveReplayResult, LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeResult,
    LiveReplaySubscription, RuntimeHandle, RuntimeObservation, SessionCursor, SessionCursorError,
    SessionObservation, SessionObservationEvent, SessionObservationEventPayload,
    SessionObservationSubscription, SessionProcessEventKind, SessionQueueEventKind, SessionResume,
    SessionRevision,
};
pub use process::{
    AbandonEvidence, AbandonRequest, AbandonWriter, DEFAULT_WAKE_DELIVERY_EXPIRY_MS,
    InMemoryProcessExecutionEnvStore, ObservedProcess, ObservedProcessEvent, ObservedWorkItem,
    ObserverInheritance, PROCESS_LEASE_SCHEMA_VERSION, PersistedSegmentHandover, ProcessAttach,
    ProcessAwaitOutput, ProcessAwaiter, ProcessCancelSummary, ProcessChange, ProcessChangeCursor,
    ProcessChangeHub, ProcessCompletionAuthority, ProcessCompletionOutcome,
    ProcessContinuationStore, ProcessEngine, ProcessEngineProcessContext, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext,
    ProcessEngineValidationContext, ProcessEvent, ProcessEventAppendPlan,
    ProcessEventAppendRequest, ProcessEventAppendResult, ProcessEventSemantics,
    ProcessEventSemanticsSpec, ProcessEventSink, ProcessEventType, ProcessExecutionContext,
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleSummary, ProcessId,
    ProcessIdentity, ProcessInfraError, ProcessInput, ProcessLease, ProcessLeaseClaimOutcome,
    ProcessLeaseCompletion, ProcessListFilter, ProcessListMode, ProcessLiveReferenceSummary,
    ProcessObserverBy, ProcessOpScope, ProcessOriginator, ProcessOutcome, ProcessProvenance,
    ProcessPruneReport, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessRunOutcome,
    ProcessService, ProcessSessionDeleteReport, ProcessSpawnProvenance, ProcessStartOptions,
    ProcessStartOutcome, ProcessStartPlan, ProcessStartRequest, ProcessStarted, ProcessStatus,
    ProcessStatusFilter, ProcessTerminalSemantics, ProcessTerminalSpec, ProcessTombstone,
    ProcessToolVisibilityFilter, ProcessValueSelector, ProcessWake, ProcessWakeDelivery,
    ProcessWakeDeliveryRequest, ProcessWakeSpec, ProcessWorkObserver, ProcessWorkSnapshot,
    ProjectionWatermark, RecoveryDisposition, SegmentHandover, SessionId, SessionScope,
    SessionScopeId, UnavailableProcessService, WAKE_ENQUEUING_STALE_AFTER_MS, WaitKind, WaitState,
    WakeDelivery, WakeDeliveryBlockedGroup, WakeDeliveryClaimOutcome, WakeDeliveryConfig,
    WakeDeliveryReport, WakeDeliveryState, WakeDiscardReason, apply_process_event_projection,
    apply_process_status_projection, current_epoch_ms, epoch_ms_from_system_time,
    fold_process_record, load_process_execution_env, materialize_process_event_semantics,
    persist_process_execution_env, prepare_process_event_append, prepare_process_registration,
    prepare_process_start, process_event_payload_hash, process_registration_with_observers_hash,
    process_runtime_session_ids, process_signal_event_type, process_signal_name_from_event_type,
    process_signal_wait_key, process_wake_delivery, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text, require_event_replay,
    system_time_from_epoch_ms, terminal_append_request, terminal_event_type_name,
    validate_generic_process_event_append, validate_process_signal_name, watch_process_registry,
    watch_process_registry_with_sink,
};
#[cfg(any(test, feature = "testing"))]
pub use process::{TestLocalProcessRegistry, TestProcessRegistryWriteExt};
pub use process_work_driver::{InlineProcessRunHandle, ProcessRunHandle, ProcessWorkDriver};
pub use process_worker::{
    DEFAULT_PROCESS_EXECUTION_CONCURRENCY, DurableProcessWorker, DurableProcessWorkerConfig,
    ProcessDrainReport, ProcessExecutionConcurrencyError,
};
pub use queued_work_driver::{
    QueuedWorkDriver, QueuedWorkRunError, QueuedWorkRunErrorClass, QueuedWorkRunHandle,
    QueuedWorkRunRequest, QueuedWorkWakeDisposition, QueuedWorkWakeFailure,
};
pub use scenario_contracts::{RUNTIME_SCENARIO_CONTRACTS, ScenarioContractSpec};
pub use session_manager::DirectCompletionClient;
pub use state::RuntimeSessionState;
use state::{
    append_session_nodes_to_state_with_clock, apply_session_checkpoint, apply_session_head,
    open_agent_frame_in_state_with_clock,
};
pub use turn_control::{
    TurnAddress, TurnAttach, TurnCancelOriginHint, TurnCancelOutcome, TurnCancelReceipt,
    TurnCancelRequest, TurnCancellationEvidence, TurnTerminal, TurnWorkDriver,
};
pub(crate) use turn_input_ingress::ingress_message_id;
pub use turn_input_ingress::{
    PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputCancelResult,
    PendingTurnInputCancelTarget, PendingTurnInputClaimDiagnostics, PendingTurnInputDraft,
    PendingTurnInputSuffixCancelOutcome, QueuedCheckpointTurnInput, TurnInputAcceptanceReceipt,
    TurnInputApplication, TurnInputCheckpointBoundary, TurnInputClaim, TurnInputClaimMode,
    TurnInputCompletion, TurnInputIngress, TurnInputState,
};
pub use turn_loop::ensure_durable_effect_input;
pub(crate) use turn_queue::process_wake_batch_draft_with_policy;
pub use turn_queue::{
    DeliveryPolicy, MergeKey, ProcessWakeSource, QueuedCheckpointWork, QueuedTurnWork,
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkClass, QueuedWorkCompletion, QueuedWorkItem, QueuedWorkPayload, SessionCommand,
    SessionCommandReceipt, SlotPolicy, WakeCoalescingKey, WakeTurnMode, WakeTurnPolicy,
    consumed_queued_work_batch, process_wake_batch_draft, process_wake_source_key,
};
pub use usage::{
    SessionUsageReport, TokenLedgerEntry, UsageReportRow, UsageTotals, diff_token_ledger,
    diff_usage_reports,
};
use usage::{merge_ledger_entry, merge_usage_delta_entries, normalize_prompt_usage};
pub use wake_delivery_driver::{WakeDeliveryDriveReport, WakeDeliveryDriver};

#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuntimeTurnPhase {
    ContextTransform,
    BeforeTurnHooks,
    PromptBuild,
    EffectLoop,
    FinalizeTurn,
    PersistTurn,
    FinalCommit,
    PostPersistHooks,
}

#[doc(hidden)]
pub trait RuntimeTurnPhaseProbe: Send + Sync {
    fn begin(&self, phase: RuntimeTurnPhase);
    fn end(&self, phase: RuntimeTurnPhase);
    fn begin_named(&self, _phase: &str) {}
    fn end_named(&self, _phase: &str) {}
}

#[doc(hidden)]
#[derive(Clone, Default)]
pub struct RuntimeTurnPhaseProbeSlot {
    probes: Arc<StdMutex<HashMap<crate::SessionScopeId, Arc<dyn RuntimeTurnPhaseProbe>>>>,
}

impl RuntimeTurnPhaseProbeSlot {
    pub fn set_for_session(
        &self,
        session_id: impl Into<String>,
        probe: Arc<dyn RuntimeTurnPhaseProbe>,
    ) {
        self.set_for_scope(&crate::SessionScope::new(session_id), probe);
    }

    pub fn set_for_scope(
        &self,
        scope: &crate::SessionScope,
        probe: Arc<dyn RuntimeTurnPhaseProbe>,
    ) {
        self.probes
            .lock()
            .expect("runtime phase probe slot")
            .insert(scope.id(), probe);
    }

    pub fn get_for_scope(
        &self,
        scope: &crate::SessionScope,
    ) -> Option<Arc<dyn RuntimeTurnPhaseProbe>> {
        let probes = self.probes.lock().expect("runtime phase probe slot");
        probes.get(&scope.id()).cloned().or_else(|| {
            probes
                .get(&crate::SessionScope::new(&scope.session_id).id())
                .cloned()
        })
    }
}

#[doc(hidden)]
pub struct RuntimeNamedPhase {
    probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    phase: &'static str,
}

impl RuntimeNamedPhase {
    pub fn begin(
        probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
        phase: &'static str,
    ) -> RuntimeNamedPhase {
        if let Some(probe) = probe.as_ref() {
            probe.begin_named(phase);
        }
        RuntimeNamedPhase { probe, phase }
    }
}

impl Drop for RuntimeNamedPhase {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.as_ref() {
            probe.end_named(self.phase);
        }
    }
}

/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Text { text: String },
    Attachment { source: crate::AttachmentSource },
}

impl InputItem {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn attachment(source: crate::AttachmentSource) -> Self {
        Self::Attachment { source }
    }
}

/// Host-provided per-turn input.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnInput {
    pub items: Vec<InputItem>,
    /// Per-turn override for protocol-owned turn options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_turn_options: Option<crate::ProtocolTurnOptions>,
    /// Optional externally-stable trace turn id. Normal runtime callers leave
    /// this empty and the runtime generates one per outer turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_turn_id: Option<String>,
    #[serde(skip)]
    pub protocol_extension: Option<ProtocolTurnExtensionHandle>,
    #[serde(skip)]
    pub turn_context: TurnContext,
}

impl TurnInput {
    pub fn empty() -> Self {
        Self::items(std::iter::empty())
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::items([InputItem::text(text)])
    }

    pub fn items(items: impl IntoIterator<Item = InputItem>) -> Self {
        Self {
            items: items.into_iter().collect(),
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: TurnContext::default(),
        }
    }

    pub fn with_attachment(mut self, source: crate::AttachmentSource) -> Self {
        self.items.push(InputItem::attachment(source));
        self
    }

    pub fn with_protocol_turn_options(mut self, options: crate::ProtocolTurnOptions) -> Self {
        self.protocol_turn_options = Some(options);
        self
    }

    pub fn with_trace_turn_id(mut self, trace_turn_id: impl Into<String>) -> Self {
        self.trace_turn_id = Some(trace_turn_id.into());
        self
    }
}

/// Per-turn, in-process side channel of typed plugin inputs.
///
/// This is an `Any`-keyed map of live Rust values handed to plugins for a
/// single turn. It is deliberately **not** serializable: the values never
/// survive a process boundary, so durable effect-host runs explicitly reject a
/// turn that carries any live inputs (see
/// [`LiveTurnInputs::durable_effect_rejection`]). Durable callers must instead
/// encode replayable data in
/// `protocol_turn_options` or persisted plugin state.
#[derive(Clone, Default)]
pub struct LiveTurnInputs {
    inputs: HashMap<&'static str, Arc<dyn Any + Send + Sync>>,
}

impl LiveTurnInputs {
    fn insert<T>(&mut self, plugin_id: &'static str, input: T)
    where
        T: Send + Sync + 'static,
    {
        self.inputs.insert(plugin_id, Arc::new(input));
    }

    fn get<T>(&self, plugin_id: &'static str) -> Option<&T>
    where
        T: 'static,
    {
        self.inputs
            .get(plugin_id)
            .and_then(|input| input.downcast_ref::<T>())
    }

    fn contains(&self, plugin_id: &'static str) -> bool {
        self.inputs.contains_key(plugin_id)
    }

    pub fn plugin_ids(&self) -> Vec<&'static str> {
        self.inputs.keys().copied().collect()
    }

    /// Returns an error when live per-turn inputs would make a durable effect
    /// host replay depend on process-local values.
    pub(crate) fn durable_effect_rejection(&self) -> Result<(), RuntimeError> {
        if self.inputs.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::new(
            RuntimeErrorCode::DurableEffectLivePluginInput,
            "durable effect hosts do not support live TurnContext plugin inputs; encode replayable data in protocol_turn_options or persisted plugin state",
        ))
    }
}

#[derive(Clone, Default)]
pub struct TurnContext {
    plugin_inputs: LiveTurnInputs,
    provider: Option<crate::ProviderHandle>,
    prompt: crate::PromptLayer,
    local_cancel_origin: TurnCancelOriginHint,
}

impl TurnContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_plugin_input<T>(&mut self, plugin_id: &'static str, input: T)
    where
        T: Send + Sync + 'static,
    {
        self.plugin_inputs.insert(plugin_id, input);
    }

    pub fn set_provider(&mut self, provider: crate::ProviderHandle) {
        self.provider = Some(provider);
    }

    pub fn provider(&self) -> Option<&crate::ProviderHandle> {
        self.provider.as_ref()
    }

    #[doc(hidden)]
    pub fn set_local_cancel_origin_hint(&mut self, hint: TurnCancelOriginHint) {
        self.local_cancel_origin = hint;
    }

    pub(crate) fn local_cancel_origin_hint(&self) -> TurnCancelOriginHint {
        self.local_cancel_origin.clone()
    }

    pub fn plugin_input<T>(&self, plugin_id: &'static str) -> Option<&T>
    where
        T: 'static,
    {
        self.plugin_inputs.get(plugin_id)
    }

    pub fn has_plugin_input(&self, plugin_id: &'static str) -> bool {
        self.plugin_inputs.contains(plugin_id)
    }

    pub fn has_live_plugin_inputs(&self) -> bool {
        !self.plugin_inputs.inputs.is_empty()
    }

    pub fn live_plugin_input_ids(&self) -> Vec<&'static str> {
        self.plugin_inputs.plugin_ids()
    }

    /// Live plugin inputs for this turn. The durable boundary inspects this to
    /// reject turns carrying non-serializable live state.
    pub(crate) fn live_plugin_inputs(&self) -> &LiveTurnInputs {
        &self.plugin_inputs
    }

    pub fn set_prompt_template(&mut self, template: crate::PromptTemplate) {
        self.prompt.template = Some(template);
    }

    pub fn add_prompt_contribution(&mut self, contribution: crate::PromptContribution) {
        self.prompt.add_contribution(contribution);
    }

    pub fn replace_prompt_slot(
        &mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) {
        self.prompt.replace_slot(slot, contributions);
    }

    pub fn clear_prompt_slot(&mut self, slot: crate::PromptSlot) {
        self.prompt.clear_slot(slot);
    }

    pub fn set_prompt_layer(&mut self, prompt: crate::PromptLayer) {
        self.prompt = prompt;
    }

    pub fn prompt_layer(&self) -> &crate::PromptLayer {
        &self.prompt
    }
}

impl fmt::Debug for TurnContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnContext")
            .field("plugin_inputs", &self.plugin_inputs.plugin_ids())
            .field("has_provider", &self.provider.is_some())
            .field("has_prompt_layer", &(!self.prompt.is_empty()))
            .finish()
    }
}

#[derive(Clone)]
pub struct ProtocolTurnExtensionHandle(Arc<dyn ProtocolTurnExtension>);

impl ProtocolTurnExtensionHandle {
    pub fn new(extension: impl ProtocolTurnExtension + 'static) -> Self {
        Self(Arc::new(extension))
    }

    pub fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }

    pub fn prompt_contributions(&self) -> Vec<crate::PromptContribution> {
        self.0.prompt_contributions()
    }
}

impl fmt::Debug for ProtocolTurnExtensionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtocolTurnExtensionHandle(..)")
    }
}

pub trait ProtocolTurnExtension: Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn prompt_contributions(&self) -> Vec<crate::PromptContribution> {
        Vec::new()
    }
}

#[derive(Clone)]
pub struct ProtocolSessionExtensionHandle(Arc<dyn ProtocolSessionExtension>);

impl ProtocolSessionExtensionHandle {
    pub fn new(extension: impl ProtocolSessionExtension + 'static) -> Self {
        Self(Arc::new(extension))
    }

    pub fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }
}

impl fmt::Debug for ProtocolSessionExtensionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtocolSessionExtensionHandle(..)")
    }
}

pub trait ProtocolSessionExtension: Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug)]
pub(super) enum NormalizedItem {
    Text(String),
    Attachment(crate::AttachmentSource),
}

/// Canonical assistant output payload.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssistantOutput {
    pub safe_text: String,
    pub raw_text: String,
    pub state: OutputState,
}

/// Quality and usability of assembled terminal output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    Usable,
    EmptyOutput,
    TracebackOnly,
    RecoveredFromError,
}

/// Code execution output observed during a turn.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeOutputRecord {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// High-level execution summary for a completed turn.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ExecutionSummary {
    #[serde(default)]
    pub had_tool_calls: bool,
    #[serde(default)]
    pub had_code_execution: bool,
    /// Wall-clock turn start as epoch milliseconds, read from the runtime
    /// [`Clock`]. The measurement window opens when the runtime starts
    /// claiming the turn (session-execution lease / queued-work claim), so
    /// it covers the whole host-visible turn. `0` when the turn predates
    /// this field.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Whole-turn duration in milliseconds — claim through final commit and
    /// post-persist hooks — measured on the runtime [`Clock`]'s monotonic
    /// source. `0` when the turn predates this field.
    #[serde(default)]
    pub duration_ms: u64,
}

/// Structured issue surfaced during turn execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnIssue {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<crate::LlmTerminalReason>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Whether the failing operation is safe to retry, when the source
    /// carried a typed signal (provider transports classify retryability;
    /// terminal LLM responses are deterministic and report `Some(false)`).
    /// `None` means the source did not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Typed provider-failure classification, present only when the issue
    /// came from a classified LLM provider/transport failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_failure_kind: Option<crate::ProviderFailureKind>,
}

/// Canonical high-level turn result returned to hosts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AssembledTurn {
    pub state: SessionSnapshot,
    pub outcome: crate::TurnOutcome,
    /// Durable request evidence, present exactly when `outcome` is cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<TurnCancellationEvidence>,
    pub assistant_output: AssistantOutput,
    pub execution: ExecutionSummary,
    #[serde(default)]
    pub token_usage: TokenUsage,
    /// Per-(session, source, model) ledger entries for child sessions whose
    /// LLM calls completed during this turn. `token_usage` above is the
    /// parent's own LLM tokens; `total_usage` (on the embed-facing
    /// `TurnResult`) sums both.
    #[serde(default)]
    pub children_usage: Vec<TokenLedgerEntry>,
    /// Provider calls made by this session during the turn, in protocol order.
    /// Child-session calls remain on the child turn result.
    #[serde(default)]
    pub llm_calls: Vec<crate::LlmCallRecord>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default)]
    pub errors: Vec<TurnIssue>,
}

/// Result of driving one logical host turn through any AgentFrame switches.
///
/// A frame switch is an internal runtime continuation, similar to compaction
/// from a host's perspective. Callers that need a final answer can use
/// [`LashRuntime::stream_turn_with_agent_frames`] and inspect `final_turn()`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentFrameRun {
    pub turns: Vec<AssembledTurn>,
}

impl AgentFrameRun {
    pub fn final_turn(&self) -> Option<&AssembledTurn> {
        self.turns.last()
    }

    pub fn into_final_turn(mut self) -> Option<AssembledTurn> {
        self.turns.pop()
    }

    pub fn frame_switch_count(&self) -> usize {
        self.turns
            .iter()
            .filter(|turn| matches!(turn.outcome, crate::TurnOutcome::AgentFrameSwitch { .. }))
            .count()
    }
}

/// Termination policy knobs.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TerminationPolicy {
    #[serde(default)]
    pub treat_missing_done_as_failure: bool,
}

impl Default for TerminationPolicy {
    fn default() -> Self {
        Self {
            treat_missing_done_as_failure: true,
        }
    }
}

/// Host application sink for low-level streaming runtime events.
/// `SessionStreamEvent` is protocol-specific preview/progress data.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    fn is_noop(&self) -> bool {
        false
    }

    async fn emit(&self, event: SessionStreamEvent);
}

/// No-op sink useful for callers that only care about final state.
pub struct NoopEventSink;

/// Static no-op event sink for callers that need a `&dyn EventSink` default.
pub static NOOP_EVENT_SINK: NoopEventSink = NoopEventSink;

#[async_trait::async_trait]
impl EventSink for NoopEventSink {
    fn is_noop(&self) -> bool {
        true
    }

    async fn emit(&self, _event: SessionStreamEvent) {}
}

/// Stable identifier for a semantic turn activity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnActivityId(pub Arc<str>);

impl TurnActivityId {
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    pub fn fresh() -> Self {
        Self(Arc::from(uuid::Uuid::new_v4().to_string()))
    }
}

/// App-facing semantic activity emitted during a turn.
///
/// `id` is unique per emitted activity event. `correlation_id` groups related
/// events in the same logical activity, such as code start/completion, tool
/// start/completion, or text deltas from one output block.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnActivity {
    pub id: TurnActivityId,
    pub correlation_id: TurnActivityId,
    #[serde(flatten)]
    pub event: TurnEvent,
}

impl TurnActivity {
    pub fn new(correlation_id: TurnActivityId, event: TurnEvent) -> Self {
        Self {
            id: TurnActivityId::fresh(),
            correlation_id,
            event,
        }
    }

    pub fn independent(event: TurnEvent) -> Self {
        let correlation_id = TurnActivityId::fresh();
        Self::new(correlation_id, event)
    }
}

/// App-facing semantic event payload for a turn activity.
///
/// Unlike [`SessionStreamEvent`], these events are stable application signals rather
/// than low-level runtime/debug events. Public streams carry these payloads
/// inside [`TurnActivity`] so every emitted item has identity.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// justification: public turn events are transient stream DTOs kept inline for allocation-free emission and stable pattern matching.
#[allow(clippy::large_enum_variant)]
pub enum TurnEvent {
    QueuedWorkStarted {
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: Vec<String>,
        causes: Vec<crate::TurnCause>,
    },
    ModelRequestStarted {
        protocol_iteration: usize,
    },
    AssistantProseDelta {
        text: Arc<str>,
    },
    ReasoningDelta {
        text: Arc<str>,
    },
    /// Retracts visible text emitted by a provider attempt that will be retried.
    ///
    /// Observers remove only prose and reasoning deltas whose correlation ids
    /// appear here. The reset is itself replayed in order, so reconnecting
    /// observers converge on the same visible text as live observers.
    ModelAttemptReset {
        assistant_prose_correlation_ids: Vec<TurnActivityId>,
        reasoning_correlation_ids: Vec<TurnActivityId>,
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
        /// Graph key of the enclosing code block, when this tool call ran
        /// inside one. `None` when the call did not run inside a code block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
        /// Call id of the parent batch tool call, when this call is a child of
        /// a `batch` dispatch. `None` for top-level tool calls.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        name: String,
        args: serde_json::Value,
        output: crate::ToolCallOutput,
        duration_ms: u64,
        /// Graph key of the enclosing code block, when this tool call ran
        /// inside one. `None` when the call did not run inside a code block.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        graph_key: Option<String>,
        /// Call id of the parent batch tool call, when this call is a child of
        /// a `batch` dispatch. `None` for top-level tool calls.
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
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    ChildUsage {
        session_id: String,
        source: String,
        model: String,
        protocol_iteration: usize,
        usage: TokenUsage,
        cumulative: TokenUsage,
    },
    RetryStatus {
        wait_seconds: u64,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    PluginRuntime {
        plugin_id: String,
        event: crate::PluginRuntimeEvent,
    },
    QueuedInputAccepted {
        applications: Vec<crate::TurnInputApplication>,
    },
    QueuedMessagesCommitted {
        messages: Vec<crate::PluginMessage>,
        checkpoint: crate::CheckpointKind,
    },
    Error {
        message: String,
    },
}

#[async_trait::async_trait]
pub trait TurnActivitySink: Send + Sync {
    fn is_noop(&self) -> bool {
        false
    }

    async fn emit(&self, activity: TurnActivity);

    /// Emit activity with the identity of the physical turn that produced it.
    ///
    /// Sinks that only consume turn-local activity can keep implementing
    /// [`emit`](Self::emit). Observation sinks override this method to carry
    /// turn identity on their enclosing event without adding it to
    /// [`TurnActivity`].
    async fn emit_for_turn(&self, turn_id: &str, activity: TurnActivity) {
        let _ = turn_id;
        self.emit(activity).await;
    }
}

pub struct NoopTurnActivitySink;

/// Static no-op turn-activity sink for callers that need a `&dyn TurnActivitySink` default.
pub static NOOP_TURN_ACTIVITY_SINK: NoopTurnActivitySink = NoopTurnActivitySink;

#[async_trait::async_trait]
impl TurnActivitySink for NoopTurnActivitySink {
    fn is_noop(&self) -> bool {
        true
    }

    async fn emit(&self, _activity: TurnActivity) {}
}

/// Optional sinks and scoped effect controller passed to one of [`LashRuntime`]'s
/// turn-driving entry points (`stream_turn`,
/// `stream_turn_with_agent_frames`).
///
/// Construct via [`TurnOptions::new`] and chain `with_*` builders. Event sinks
/// default to no-op sinks. Execution scope is explicit and required at every
/// runtime boundary that can execute nondeterministic work.
pub struct TurnOptions<'a> {
    events: Option<&'a dyn EventSink>,
    turn_events: Option<&'a dyn TurnActivitySink>,
    scoped_effect_controller: ScopedEffectController<'a>,
    cancel: CancellationToken,
    local_cancel_origin: Option<TurnCancelOriginHint>,
}

impl<'a> TurnOptions<'a> {
    pub fn new(
        cancel: CancellationToken,
        scoped_effect_controller: ScopedEffectController<'a>,
    ) -> Self {
        Self {
            events: None,
            turn_events: None,
            scoped_effect_controller,
            cancel,
            local_cancel_origin: None,
        }
    }

    pub fn with_events(mut self, events: &'a dyn EventSink) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_turn_events(mut self, turn_events: &'a dyn TurnActivitySink) -> Self {
        self.turn_events = Some(turn_events);
        self
    }

    #[doc(hidden)]
    pub fn with_local_cancel_origin_hint(mut self, hint: TurnCancelOriginHint) -> Self {
        self.local_cancel_origin = Some(hint);
        self
    }

    pub(crate) fn local_cancel_origin_hint(&self) -> Option<TurnCancelOriginHint> {
        self.local_cancel_origin.clone()
    }

    pub(crate) fn events_or_noop(&self) -> &'a dyn EventSink {
        self.events.unwrap_or(&NOOP_EVENT_SINK)
    }

    pub(crate) fn turn_events_or_noop(&self) -> &'a dyn TurnActivitySink {
        self.turn_events.unwrap_or(&NOOP_TURN_ACTIVITY_SINK)
    }

    pub(crate) fn execution_scope_id(&self) -> &str {
        self.scoped_effect_controller.scope_id()
    }

    pub(crate) fn scoped_effect_controller(&self) -> ScopedEffectController<'a> {
        self.scoped_effect_controller.clone()
    }
}

enum RuntimeStreamEvent {
    Session(SessionStreamEvent),
    Turn(TurnActivity),
}

#[derive(Clone)]
pub struct SessionStoreCreateRequest {
    pub session_id: String,
    pub relation: SessionRelation,
    pub policy: SessionPolicy,
}

impl SessionStoreCreateRequest {
    pub fn parent_session_id(&self) -> Option<&str> {
        self.relation.parent_session_id()
    }
}

/// A durable turn boundary whose continuation checkpoint is currently retained.
///
/// Past turn boundaries are not retained by default. A point remains available
/// while explicitly pinned or while it is the leaf of at least one live
/// session head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkPoint {
    pub node_id: String,
    pub checkpoint_ref: crate::BlobRef,
    pub source_session_id: String,
    /// Provider and model captured by the nearest retained frame boundary.
    pub config: crate::PersistedSessionConfig,
    pub pinned: bool,
}

/// Create a new session head at retained history without writing graph nodes.
#[derive(Clone, Debug)]
pub struct ForkSessionRequest {
    pub session_id: String,
    pub node_id: String,
    pub relation: SessionRelation,
    pub policy: SessionPolicy,
}

/// Durable identity returned after a zero-node fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkSessionResult {
    pub session_id: String,
    pub node_id: String,
    /// Session that originally wrote `node_id`. This is process-observer
    /// provenance, not a required source-session argument to the fork.
    pub source_session_id: String,
}

#[async_trait::async_trait]
pub trait SessionStoreFactory: Send + Sync {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::store::RuntimePersistence>, crate::StoreError>;

    async fn open_existing_store(
        &self,
        _request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn crate::store::RuntimePersistence>>, String> {
        Ok(None)
    }

    /// Report whether the permanent host-facing session tombstone exists.
    async fn session_was_deleted(&self, _session_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String>;

    /// Retain the continuation checkpoint for `node_id`.
    ///
    /// A new pin can be created only while some live head is exactly at the
    /// node, because an unpinned past checkpoint is ordinarily already
    /// collectible. Re-pinning an existing point is idempotent.
    async fn pin(&self, node_id: &str) -> Result<ForkPoint, crate::StoreError> {
        let _ = node_id;
        Err(crate::StoreError::UnsupportedStoreOperation { operation: "pin" })
    }

    /// Release an explicit continuation pin. A live head at the same node
    /// continues to make that tip forkable.
    async fn unpin(&self, node_id: &str) -> Result<(), crate::StoreError> {
        let _ = node_id;
        Err(crate::StoreError::UnsupportedStoreOperation { operation: "unpin" })
    }

    /// Enumerate retained continuation points. This includes pinned past turns
    /// and unpinned live tips, de-duplicated by node id.
    async fn fork_points(&self) -> Result<Vec<ForkPoint>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "fork_points",
        })
    }

    /// Add a new session-head root at a retained point without writing graph
    /// nodes. Returns [`crate::StoreError::ForkPointNotRetained`] for the
    /// ordinary case where a past turn was not pinned before its head moved.
    async fn fork_at(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionResult, crate::StoreError> {
        let _ = request;
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "fork_at",
        })
    }

    /// The attachment GC root set across every session this factory owns. An
    /// uncommitted intent is forgotten only when it is old enough and its
    /// durable owner is dead: a superseding session turn commit or an absent
    /// process row. Ownerless host puts use age alone. Factories with no
    /// attachment story default to empty; durable factories evaluate and prune
    /// the predicate atomically. Exposed to the GC lever via the blanket
    /// [`AttachmentRootSet`](crate::AttachmentRootSet) implementation.
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::store::StoreError> {
        let _ = intent_grace_cutoff_epoch_ms;
        Ok(std::collections::BTreeSet::new())
    }

    /// Whether ANY session this factory owns currently holds a GC-live ref for
    /// `attachment_id` under that same age plus owner-reachability rule. The
    /// single-id counterpart to
    /// [`Self::live_attachment_refs`], used by the attachment GC lever's
    /// delete-time root re-check so it need not re-materialize the whole root set
    /// per candidate blob. The default re-materializes the root set and tests
    /// membership; the durable factories override with a targeted single-id query
    /// (Postgres one indexed `SELECT`; SQLite iterates its per-session databases
    /// only until the first hit).
    ///
    /// Unlike [`Self::live_attachment_refs`], this MUST NOT forget aged intents —
    /// it is a read-only probe run after the reconciling snapshot was already
    /// taken.
    async fn has_live_attachment_ref(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::store::StoreError> {
        Ok(self
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await?
            .contains(attachment_id))
    }
}

/// Generic runtime for CLI or programmatic embedding.
pub struct LashRuntime {
    pub(in crate::runtime) session: Option<Session>,
    pub(in crate::runtime) policy: SessionPolicy,
    pub(in crate::runtime) host: RuntimeHost,
    pub(in crate::runtime) services: RuntimeServices,
    pub(in crate::runtime) state: RuntimeSessionState,
    pub(in crate::runtime) runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub(in crate::runtime) managed_sessions: Arc<Mutex<HashMap<String, RuntimeHandle>>>,
    pub(in crate::runtime) managed_turns: Arc<Mutex<HashMap<String, ManagedSessionTurn>>>,
    /// Protocol-owned turn options for this session.
    pub(in crate::runtime) protocol_turn_options: crate::ProtocolTurnOptions,
    /// Session-scoped token cost ledger. Shared by ALL
    /// `RuntimeSessionServices` instances created from this runtime
    /// (both per-turn and async maintenance). Entries accumulate here
    /// and are drained into `state.token_ledger` at turn-commit time.
    pub(in crate::runtime) shared_token_ledger: Arc<std::sync::Mutex<Vec<TokenLedgerEntry>>>,
    pub(in crate::runtime) process_sync_needed: Arc<AtomicBool>,
    pub(in crate::runtime) turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    /// Lease-guard identity retained across a successful physical-turn commit.
    /// A match proves no release/reacquisition boundary occurred before the
    /// next physical turn on this handle.
    pub(in crate::runtime) last_committed_lease_continuity:
        Option<session_execution_lease::SessionExecutionLeaseContinuity>,
    /// Most recent physical turn committed by this runtime, paired with the
    /// resulting session revision for observation-envelope attribution.
    pub(in crate::runtime) last_committed_observation_turn: Option<(u64, String)>,
    /// Set only after this handle itself has attempted a durable graph load.
    pub(in crate::runtime) graph_loaded_from_store: bool,
}
