//! Runtime kernel for Lash.
//!
//! The process kernel intentionally understands `ToolCall`, `SessionTurn`, and
//! `External` because those inputs carry runtime mechanisms core must enforce:
//! tool orchestration, child-session turns, and externally completed work. New
//! process runtimes should use `ProcessInput::Engine { kind, payload }` unless
//! core must understand their semantics to enforce a kernel mechanism.
//!
//! Protocols follow the same boundary: core owns the `HostTurnProtocol` state
//! shape and the `ProtocolDriverPlugin` slot, while external protocol crates
//! provide the driver implementation.

pub mod attachments;
pub mod chronological;
pub mod direct;
pub mod llm;
mod model;
pub mod plugin;
mod plugin_stack;
mod protocol_build;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod session_graph;
pub mod session_model;
mod stable_hash;
pub mod store;
pub mod task;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod tool_dispatch;
mod tool_provider;
pub mod tool_registry;
mod tool_result;
mod trace;
pub mod triggers;

pub use lash_sansio::sansio;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SANSIO_VERSION: &str = lash_sansio::VERSION;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Which layer owns replay of runtime effects.
///
/// This is a mechanical property of an effect controller, not an end-to-end
/// durability claim. Controller-owned replay means a controller journals or
/// otherwise replays completed effects; runtime-owned execution means Lash
/// invokes the local effect path directly on each turn attempt.
pub enum EffectReplayOwnership {
    #[default]
    Runtime,
    Controller,
}

// Re-exports
pub use attachments::{
    AttachmentProducer, AttachmentReclamationReport, AttachmentRootSet, AttachmentSourcePolicy,
    AttachmentSourcePolicyError, AttachmentStore, AttachmentStoreError, AttachmentStorePersistence,
    FileAttachmentStore, InMemoryAttachmentStore, NoopAttachmentManifest,
    OpenAttachmentSourcePolicy, SessionAttachmentStore, StoredAttachment, StoredBlobRef,
    reclaim_unreferenced_attachments,
};
pub use chronological::{
    BorrowedChronologicalEntry, BorrowedChronologicalMessage, BorrowedChronologicalPayload,
    ChronologicalEntry, ChronologicalPayload, ChronologicalProjection, visit_turn_view,
};
pub use direct::{
    DirectJsonSchema, DirectLlmClient, DirectLlmError, DirectLlmResult, DirectMessage,
    DirectOutputSpec, DirectPart, DirectRequest, DirectRole,
};
pub use lash_sansio::llm::types::{
    AttachmentSource, AttemptOutcome, AttemptRecord, ExecutionEvidence, GenerationOptions,
    LlmCallId, LlmCallRecord, LlmOutputPart, LlmRequest, LlmRequestScope, LlmResponse,
    LlmTerminalReason, NonNegativeFiniteF64, NonNegativeFiniteF64Error, NormalizedError,
    ProtocolPosition, ProviderFileScope, RetryDecision,
};
pub use lash_sansio::{
    AcceptedInjectedTurnInput, AttachmentCreateMeta, AttachmentId, AttachmentMeta, AttachmentRef,
    AttachmentTypeMetadata, BaseRenderCache, CheckpointDelivery, CheckpointKind,
    CompactToolContract, EffectId, ErrorEnvelope, ExecImage, ExecResponse, InvalidMediaType,
    LashSchema, LlmCallError, MediaType, Message, MessageOrigin, MessageRole, MessageSequence,
    ModelToolReturn, ModelToolReturnPart, Part, PartKind, PluginMessage, PluginRuntimeEvent,
    PreparedPrompt, ProjectionMode, PromptBuildInput, PromptBuiltin, PromptContext,
    PromptContribution, PromptContributionGate, PromptContributionSet, PromptFingerprint,
    PromptLayer, PromptSlot, PromptSlotLayer, PromptTemplate, PromptTemplateEntry,
    PromptTemplateSection, ProviderSchemaCapabilities, PruneState, RenderedPrompt,
    ResolvedPromptLayer, ResolvedSchema, Response, SchemaContract, SchemaDialect,
    SchemaProjectionOverride, SchemaProjectionPolicy, SchemaPurpose, SchemaResolutionError,
    SchemaResolutionRequest, SessionAppendNode, SessionStreamEvent, TextProjectionMetadata,
    TokenUsage, ToolActivation, ToolArgumentProjectionPolicy, ToolCallOutcome, ToolCallOutput,
    ToolCallRecord, ToolCallStatus, ToolCancellation, ToolCatalog, ToolCatalogBuildInput,
    ToolCatalogEntry, ToolContract, ToolControl, ToolDefinition, ToolFailure, ToolFailureClass,
    ToolFailureSource, ToolId, ToolManifest, ToolOutputContract, ToolRetryDisposition,
    ToolRetryPolicy, ToolValue, TurnCause, TurnFinish, TurnLimitFinalMessage, TurnOutcome,
    TurnStop, append_assistant_text_part, build_prompt, build_tool_catalog, build_turn,
    default_prompt_template, head_tail_truncate, messages_are_prompt_resume_safe,
    normalized_response_parts, project_anthropic_bedrock_schema, project_for_dialect,
    prompt_template_fingerprint, prompt_text_fingerprint, prompt_tool_names_fingerprint,
    reasoning_part, render_turn_causes_prompt, resolve_prompt_layers, resolve_schema, shared_parts,
    validate_tool_input, visible_response_parts, visible_response_text_from_parts,
};
pub use store::AttachmentOwnerKind;

/// Project a successful tool control into its terminal turn outcome.
///
/// Agent-frame seeds are typed at their serde boundary, so a terminal outcome
/// can never advertise nodes that the commit materializer would have to drop.
pub fn turn_outcome_from_tool_control(
    tool_name: &str,
    control: &ToolControl,
) -> Option<TurnOutcome> {
    match control {
        ToolControl::SwitchAgentFrame {
            frame_id,
            initial_nodes,
            task: Some(task),
        } if !frame_id.trim().is_empty() && !task.trim().is_empty() => {
            Some(TurnOutcome::AgentFrameSwitch {
                frame_id: frame_id.clone(),
                task: task.clone(),
                initial_nodes: initial_nodes.clone(),
            })
        }
        ToolControl::Finish { value } => Some(TurnOutcome::Finished(TurnFinish::ToolValue {
            tool_name: tool_name.to_string(),
            value: value.to_json_value(),
        })),
        ToolControl::Fail { failure } => Some(TurnOutcome::Stopped(TurnStop::ToolError {
            tool_name: tool_name.to_string(),
            value: failure.to_json_value(),
        })),
        ToolControl::SwitchAgentFrame { .. } => None,
    }
}
pub use protocol_build::ProtocolBuildInput;
pub use tool_registry::{
    PLUGIN_TOOL_SOURCE_ID, ReconfigureError, ToolRegistry, ToolRestoreReport, ToolSourceHandle,
    ToolState, ToolStateEntry,
};
pub use tool_result::{CancelHint, PendingCompletion, TimeoutBehavior, ToolResult};
pub use triggers::{
    InMemoryTriggerStore, TriggerCommand, TriggerCommandOutcome, TriggerDeliveryEmitOutcome,
    TriggerDeliveryEmitReport, TriggerDeliveryReservation, TriggerDeliveryReservationStatus,
    TriggerEffectResult, TriggerEmitReport, TriggerEvent, TriggerEventCatalog, TriggerEventKey,
    TriggerEventType, TriggerIngressResult, TriggerInputBinding, TriggerManifestMembership,
    TriggerMutationDisposition, TriggerMutationReceipt, TriggerOccurrenceFilter,
    TriggerOccurrenceRecord, TriggerOccurrenceRequest, TriggerOperationError, TriggerOwnerScope,
    TriggerRegistration, TriggerRouter, TriggerStore, TriggerSubscriptionDraft,
    TriggerSubscriptionFilter, TriggerSubscriptionRecord, TriggerTargetSummary,
    default_trigger_source_key, deterministic_delivery_process_id, deterministic_occurrence_id,
    deterministic_subscription_id, empty_trigger_source_key, evaluate_trigger_mutation,
    evaluate_trigger_prune, trigger_command_hash, trigger_event_type,
    trigger_occurrence_request_hash, trigger_operation_receipt_id,
    trigger_subscription_definition_hash, validate_subscription_key,
    validate_trigger_occurrence_request,
};
pub const PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, serde::Serialize)]
pub struct ProtocolTurnOptions {
    pub schema_version: u32,
    pub payload: serde_json::Value,
}

fn empty_protocol_turn_payload() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolTurnOptionsError {
    #[error(
        "protocol turn options are missing schema_version and were written by unsupported pre-versioned state (expected {expected})"
    )]
    MissingSchemaVersion { expected: u32 },
    #[error(
        "protocol turn options schema_version {actual} is not supported by this binary (expected {expected})"
    )]
    UnsupportedSchemaVersion { actual: u32, expected: u32 },
    #[error(
        "protocol turn options schema_version {actual} is invalid (expected integer {expected})"
    )]
    InvalidSchemaVersion { actual: String, expected: u32 },
    #[error("failed to decode protocol turn options payload: {0}")]
    Decode(#[source] serde_json::Error),
}

fn parse_protocol_turn_options_schema_version(
    value: Option<serde_json::Value>,
) -> Result<u32, ProtocolTurnOptionsError> {
    let expected = PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
    let Some(value) = value else {
        return Err(ProtocolTurnOptionsError::MissingSchemaVersion { expected });
    };
    let Some(actual) = value
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
    else {
        return Err(ProtocolTurnOptionsError::InvalidSchemaVersion {
            actual: value.to_string(),
            expected,
        });
    };
    ensure_protocol_turn_options_schema_version(actual)?;
    Ok(actual)
}

fn ensure_protocol_turn_options_schema_version(
    actual: u32,
) -> Result<(), ProtocolTurnOptionsError> {
    let expected = PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION;
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolTurnOptionsError::UnsupportedSchemaVersion { actual, expected })
    }
}

impl<'de> serde::Deserialize<'de> for ProtocolTurnOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct ProtocolTurnOptionsWire {
            schema_version: Option<serde_json::Value>,
            #[serde(default = "empty_protocol_turn_payload")]
            payload: serde_json::Value,
        }

        let wire = ProtocolTurnOptionsWire::deserialize(deserializer)?;
        let schema_version = parse_protocol_turn_options_schema_version(wire.schema_version)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema_version,
            payload: wire.payload,
        })
    }
}

impl Default for ProtocolTurnOptions {
    fn default() -> Self {
        Self::empty()
    }
}

impl ProtocolTurnOptions {
    pub fn empty() -> Self {
        Self {
            schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    pub fn from_payload(payload: serde_json::Value) -> Self {
        Self {
            schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            payload,
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.payload {
            serde_json::Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    pub fn merged_with_override(&self, override_options: &Self) -> Self {
        match (&self.payload, &override_options.payload) {
            (serde_json::Value::Object(base), serde_json::Value::Object(overrides)) => {
                let mut payload = base.clone();
                payload.extend(overrides.clone());
                Self {
                    schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
                    payload: serde_json::Value::Object(payload),
                }
            }
            _ => override_options.clone(),
        }
    }

    pub fn typed<T>(value: T) -> Result<Self, serde_json::Error>
    where
        T: serde::Serialize,
    {
        Ok(Self {
            schema_version: PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            payload: serde_json::to_value(value)?,
        })
    }

    pub fn decode<T>(&self) -> Result<T, ProtocolTurnOptionsError>
    where
        T: serde::de::DeserializeOwned,
    {
        ensure_protocol_turn_options_schema_version(self.schema_version)?;
        serde_json::from_value(self.payload.clone()).map_err(ProtocolTurnOptionsError::Decode)
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ProtocolDriverState {
    pub plugin_id: String,
    pub payload: serde_json::Value,
}

impl ProtocolDriverState {
    pub fn new(plugin_id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            payload,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HostTurnProtocol;

impl lash_sansio::TurnProtocol for HostTurnProtocol {
    type Event = crate::session_model::ProtocolEvent;
    type Termination = ProtocolTurnOptions;
    type DriverState = ProtocolDriverState;
}

pub type Effect = lash_sansio::Effect<HostTurnProtocol>;
pub type DriverAction = lash_sansio::DriverAction<HostTurnProtocol>;
pub type DriverContextView<'a> = lash_sansio::DriverContextView<'a, HostTurnProtocol>;
pub type TurnDriverConfig = lash_sansio::TurnDriverConfig<HostTurnProtocol>;
pub type TurnDriverPreamble = lash_sansio::TurnDriverPreamble<HostTurnProtocol>;
pub type ProjectorContext<'a> = lash_sansio::ProjectorContext<'a, HostTurnProtocol>;
pub type PreparedTurnMachine = lash_sansio::PreparedTurnMachine<HostTurnProtocol>;
pub type SansIoTurnInput = lash_sansio::SansIoTurnInput<HostTurnProtocol>;
pub type TurnMachine = lash_sansio::TurnMachine<HostTurnProtocol>;
pub type TurnMachineConfig = lash_sansio::TurnMachineConfig<HostTurnProtocol>;
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{OtelTraceOptions, OtelTraceSink};
pub use lash_trace::{
    JsonlTraceSink, TraceAttachment, TraceBranchSelection, TraceContentBlock, TraceContext,
    TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue,
    TraceError, TraceEvent, TraceLabelMetadata, TraceLevel, TraceLlmMessage, TraceLlmRequest,
    TraceLlmResponse, TracePromptComponent, TraceProviderRequestEvent, TraceProviderStreamEvent,
    TraceRecord, TraceRuntimeScope, TraceRuntimeStreamEvent, TraceRuntimeSubject, TraceSink,
    TraceSinkError, TraceTokenUsage, TraceToolSpec,
};
pub use llm::transport::{LlmTransportError, ProviderFailure, ProviderFailureKind};
pub use model::{ModelLimits, ModelSpec};
pub use plugin::{
    AgentFrameAssignment, AgentFrameId, AgentFrameReason, AgentFrameRecord, AgentFrameStatus,
    AppendSessionNodesRequest, AppendSessionNodesResult, AssistantResponseHookContext,
    AssistantResponseTransform, AssistantStreamHookContext, AssistantStreamTransform,
    CheckpointHookContext, CompactionContext, ContextCompaction, ContextCompactor, ContextError,
    ContextRegistrations, DirectCompletion, DirectLlmCompletion, OpenAgentFrameRequest,
    OpenAgentFrameResult, PersistentRuntimeServices, PluginCommand, PluginCommandContext,
    PluginCommandOutcome, PluginCommandReceipt, PluginDirective, PluginError,
    PluginExtensionContribution, PluginExtensions, PluginFactory, PluginHost, PluginLifecycleEvent,
    PluginLifecycleEventHook, PluginOperation, PluginOperationDef, PluginOperationFailure,
    PluginOperationInvokeError, PluginOperationKind, PluginOptions, PluginOwned, PluginQuery,
    PluginQueryContext, PluginRegistrar, PluginRuntimeDirective, PluginSession,
    PluginSessionContext, PluginSessionSnapshot, PluginSnapshotArtifact, PluginSnapshotEntry,
    PluginSnapshotMeta, PluginSpec, PluginSpecFactory, PluginTask, PluginTaskContext,
    PluginTaskOutcome, PluginTaskReceipt, ProcessEngineContributionContext, PromptHookContext,
    ProtocolBeforeLlmCallContext, ProtocolLlmCallAction, RuntimeServices,
    SessionConfigChangedContext, SessionContextOverlay, SessionCreateRequest, SessionGraphService,
    SessionHandle, SessionLifecycleService, SessionParam, SessionPlugin, SessionPluginSource,
    SessionReadView, SessionRelation, SessionSnapshot, SessionStartPoint,
    SessionStateChangedContext, SessionStateService, SessionToolAccess, SessionTurnInput,
    SessionTurnRequest, SnapshotReader, SnapshotWriter, SubagentSessionContext,
    ToolCatalogContribution, ToolResultProjectionContext, ToolResultProjector,
    TriggerEventRegistrations, TurnContextTransform, TurnHookContext, TurnResultHookContext,
    TurnResultSummary, TurnTransformContext, plugin_operation_def,
};
pub use plugin_stack::PluginStack;
pub use provider::{
    CacheControlDialect, CacheRetention, EmptyProviderResolver, LlmTimeouts, MapProviderResolver,
    ModelCapability, ModelEffortValidationCategory, ModelEffortValidationError, Provider,
    ProviderBinding, ProviderCompletion, ProviderCompletionError, ProviderComponents,
    ProviderFactory, ProviderHandle, ProviderOptions, ProviderResolutionError, ProviderSpec,
    ReasoningCapability, ReasoningDisableEncoding, ReasoningEncoding, ReasoningSelection,
    RequestTimeout, RuntimeProviderResolver, SamplingCapability, SingleProviderResolver,
    StreamTermination,
};
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestLocalProcessRegistry;
#[cfg(any(test, feature = "testing"))]
pub use runtime::TestProcessRegistryWriteExt;
pub use runtime::promise_semantics;
pub use runtime::{
    AbandonEvidence, AbandonRequest, AbandonWriter, AgentFrameRun, AssembledTurn, AssistantOutput,
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, CausalRef, Clock,
    CodeOutputRecord, DefaultProcessCancelAbility, DeliveryPolicy, DirectCompletionClient,
    DurableProcessWorker, DurableProcessWorkerConfig, EffectHost, EmbeddedRuntimeBuilder,
    EmbeddedRuntimeHost, EventSink, ExecutionScope, ExecutionSummary, ExternalCompletionError,
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, InMemoryProcessExecutionEnvStore,
    InMemorySessionStore, InMemorySessionStoreFactory, InlineEffectHost, InlineProcessRunHandle,
    InlineRuntimeEffectController, InputItem, LashRuntime, LiveReplayGap, LiveReplayGapReason,
    LiveReplayResult, LiveReplayStore, LiveReplayStoreError, LiveReplaySubscribeResult,
    LiveReplaySubscription, MergeKey, NoopEventSink, NoopTurnActivitySink, ObservedProcess,
    ObservedProcessEvent, ObservedWorkItem, OutputState, PROCESS_LEASE_SCHEMA_VERSION,
    ParkedSession, PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputCancelResult,
    PendingTurnInputCancelTarget, PendingTurnInputClaimDiagnostics, PendingTurnInputDraft,
    PendingTurnInputSuffixCancelOutcome, PersistedSegmentHandover, ProcessAttach,
    ProcessAwaitOutput, ProcessAwaiter, ProcessCancelAbility, ProcessCancelAllRequest,
    ProcessCancelRequest, ProcessCancelSource, ProcessCancelSummary, ProcessChangeCursor,
    ProcessChangeHub, ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessDrainReport,
    ProcessEngine, ProcessEngineProcessContext, ProcessEngineRegistry, ProcessEngineRunContext,
    ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessEngineValidationContext,
    ProcessEvent, ProcessEventAppendPlan, ProcessEventAppendRequest, ProcessEventAppendResult,
    ProcessEventSink, ProcessEventType, ProcessExecutionContext, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleDescriptor, ProcessHandleGrant, ProcessHandleSummary,
    ProcessId, ProcessIdentity, ProcessInfraError, ProcessInput, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessLifecycleStatus, ProcessListFilter,
    ProcessListMode, ProcessLiveReferenceSummary, ProcessOpScope, ProcessOriginator,
    ProcessProvenance, ProcessPruneReport, ProcessRecord, ProcessRegistration, ProcessRegistry,
    ProcessRunHandle, ProcessRunOutcome, ProcessRuntimeHost, ProcessService,
    ProcessSessionDeleteReport, ProcessSpawnProvenance, ProcessStartGrant, ProcessStartOptions,
    ProcessStartOutcome, ProcessStartPlan, ProcessStartRequest, ProcessStarted, ProcessStatus,
    ProcessStatusFilter, ProcessTerminalSemantics, ProcessTerminalSpec, ProcessTerminalState,
    ProcessValueSelector, ProcessWake, ProcessWakeDedupeKey, ProcessWakeDelivery,
    ProcessWakeDeliveryRequest, ProcessWakeSpec, ProcessWorkDriver, ProcessWorkObserver,
    ProcessWorkSnapshot, PromptUsage, ProtocolSessionExtension, ProtocolSessionExtensionHandle,
    ProtocolTurnExtension, ProtocolTurnExtensionHandle, QueuedWorkDriver, QueuedWorkRunError,
    QueuedWorkRunErrorClass, QueuedWorkRunHandle, QueuedWorkRunRequest, QueuedWorkWakeDisposition,
    QueuedWorkWakeFailure, RecoveryDisposition, Residency, Resolution, ResolveOutcome,
    RuntimeEnvironment, RuntimeEnvironmentBuilder, RuntimeError, RuntimeErrorCode, RuntimeHandle,
    RuntimeHostConfig, RuntimeObservation, ScopedEffectController, SegmentHandover,
    SegmentProgress, SessionCommand, SessionCommandReceipt, SessionCursor, SessionCursorError,
    SessionObservation, SessionObservationEvent, SessionObservationEventPayload,
    SessionObservationSubscription, SessionProcessEventKind, SessionQueueEventKind, SessionResume,
    SessionRevision, SessionScope, SessionScopeId, SessionStoreCreateRequest, SessionStoreFactory,
    SessionUsageReport, SlotPolicy, SystemClock, TerminationPolicy, TokenLedgerEntry,
    ToolCallLaunch, TurnActivity, TurnActivityId, TurnActivitySink, TurnAddress, TurnAttach,
    TurnCancelOriginHint, TurnCancelOutcome, TurnCancelReceipt, TurnCancelRequest,
    TurnCancellationEvidence, TurnContext, TurnEvent, TurnInput, TurnInputAcceptanceReceipt,
    TurnInputApplication, TurnInputCheckpointBoundary, TurnInputClaim, TurnInputClaimMode,
    TurnInputCompletion, TurnInputIngress, TurnInputState, TurnIssue, TurnOptions, TurnTerminal,
    TurnWorkDriver, UnavailableProcessService, UsageReportRow, UsageTotals, WaitKind, WaitState,
    apply_process_event_projection, apply_process_status_projection, current_epoch_ms,
    diff_token_ledger, diff_usage_reports, ensure_durable_effect_input, epoch_ms_from_system_time,
    fold_process_record, process_runtime_session_ids, process_signal_event_type,
    process_signal_name_from_event_type, process_signal_wait_key, process_wake_delivery,
    system_time_from_epoch_ms, terminal_append_request, terminal_event_type_name,
    validate_process_signal_name, watch_process_registry, watch_process_registry_with_sink,
};
pub use runtime::{DEFAULT_PROCESS_EXECUTION_CONCURRENCY, ProcessExecutionConcurrencyError};
#[allow(unused_imports)]
pub(crate) use runtime::{
    LlmAttachmentSpec, ProcessEventSemantics, QueuedCheckpointTurnInput, QueuedCheckpointWork,
    QueuedTurnWork, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkCompletion, QueuedWorkItem, QueuedWorkPayload,
    RuntimeReplay, RuntimeSubject, load_process_execution_env, materialize_process_event_semantics,
    persist_process_execution_env, prepare_process_event_append, prepare_process_registration,
    prepare_process_start, process_event_invocation, process_event_payload_hash,
    process_wake_batch_draft, process_wake_input_from_event_payload, process_wake_turn_cause,
    process_wake_turn_text, require_event_replay,
};
pub use session_model::{
    PLUGIN_RUNTIME_PROTOCOL_PLUGIN_ID, PersistedPluginRuntimeEvent,
    plugin_runtime_event_from_protocol, plugin_runtime_protocol_event,
};
// Effect / process-control types consumed by external effect hosts (e.g.
// lash-restate's workflows) and their integration tests. Kept on the public
// surface; the rest of the runtime block above stays crate-internal.
pub use runtime::{
    CanonicalRuntimeEffectEnvelope, LlmRequestSpec, ProcessCommand, ProcessEffectOutcome,
    ProcessEventSemanticsSpec, RuntimeAwaitEventOptions, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeEffectReplayMismatchSummary, RuntimeEffectReplayTrace, RuntimeInvocation, RuntimeScope,
    RuntimeSessionState, RuntimeSleepOptions, ToolAttemptEffectOutcome, ToolAttemptLaunch,
    ToolBatchEffectOutcome, validate_replayed_effect_envelope,
};
pub use schemars::JsonSchema;
pub(crate) use session::RuntimeExecutionTracing;
pub use session::{
    ExecRequest, InjectedTurnInput, RuntimeExecutionContext, Session, SessionError, ToolInvocation,
    ToolInvocationReply,
};
pub use session_graph::{
    PersistedSessionConfig, PersistedTurnState, SessionGraph, SessionMessageTreeNode,
    SessionNodePayload, SessionNodeRecord,
};
pub use session_model::context::PreparedContext;
pub use session_model::{ConversationRecord, ProtocolEvent, SessionHistoryRecord};
pub use session_model::{RuntimeSessionPolicy, SessionPolicy, SessionSpec};
pub use store::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, BlobRef, GcReport,
    LeaseOwnerIdentity, LeaseOwnerLiveness, LeaseTimings, LeaseTimingsError, QueuedWorkStore,
    RuntimePersistence, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseCompletion, SessionExecutionLeaseFence,
    SessionExecutionLeaseStore, SessionMeta, SessionPickerInfo, SessionReadScope, StoreError,
    StoreMaintenance, TurnInputStore, VacuumReport,
};
#[allow(unused_imports)]
pub(crate) use store::{
    GraphCommitDelta, PersistedSessionRead, RuntimeCommitResult, SessionCheckpoint,
    SessionHeadMeta, ensure_supported_schema_version, load_persisted_session_state,
    load_persisted_session_state_active_path,
};
pub use store::{
    HydratedSessionCheckpoint, OperationId, RuntimeCommit, RuntimeTurnCommitStamp, SessionHead,
    refresh_persisted_session_state,
};
pub use tool_provider::{
    PreparedToolBatch, PreparedToolBatchCall, PreparedToolCall, ProgressSender, SandboxMessage,
    ToolCall, ToolChildExecutionTraceHook, ToolChildProcessStarted, ToolContext,
    ToolExecutionGrant, ToolPrepareCall, ToolPrepareContext, ToolProvider, ToolSessionAdmin,
    ToolSessionModel, ToolSessionProcessAdmin, ToolTriggerClient,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_agent_frame_seed_is_rejected_at_the_serde_boundary() {
        let err = serde_json::from_value::<ToolControl>(serde_json::json!({
            "type": "switch_agent_frame",
            "frame_id": "delegate",
            "initial_nodes": [{ "not": "a session append node" }],
            "task": "continue the work"
        }))
        .expect_err("invalid seed cannot construct a tool control");

        assert!(err.to_string().contains("kind"), "unexpected error: {err}");
    }

    #[test]
    fn protocol_turn_options_missing_payload_deserializes_to_empty_object() {
        let options: ProtocolTurnOptions = serde_json::from_value(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION
        }))
        .expect("deserialize options");

        assert!(options.is_empty());
        assert_eq!(options.schema_version, PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION);
        assert_eq!(options.payload, serde_json::json!({}));
    }

    #[test]
    fn protocol_turn_options_explicit_null_is_not_empty() {
        let options: ProtocolTurnOptions = serde_json::from_value(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
            "payload": null
        }))
        .expect("deserialize options");

        assert!(!options.is_empty());
        assert_eq!(options.payload, serde_json::Value::Null);
    }

    #[test]
    fn protocol_turn_options_missing_schema_version_rejects_preversioned_state() {
        let err =
            serde_json::from_value::<ProtocolTurnOptions>(serde_json::json!({ "payload": {} }))
                .expect_err("pre-versioned options should fail");

        assert!(
            err.to_string().contains(
                "missing schema_version and were written by unsupported pre-versioned state"
            ),
            "{err}"
        );
    }

    #[test]
    fn protocol_turn_options_unsupported_schema_version_rejects_state() {
        let err = serde_json::from_value::<ProtocolTurnOptions>(serde_json::json!({
            "schema_version": PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION + 1,
            "payload": {}
        }))
        .expect_err("unsupported options version should fail");

        assert!(
            err.to_string().contains("is not supported by this binary"),
            "{err}"
        );
    }

    #[test]
    fn root_exports_do_not_reintroduce_removed_session_state_shapes() {
        let source = include_str!("lib.rs");
        let removed_envelope = ["SessionState", "Envelope"].concat();
        let removed_persisted = ["PersistedSession", "Snapshot"].concat();
        let removed_history_rewriter = ["History", "Rewriter"].concat();
        let removed_rewrite_trigger = ["Rewrite", "Trigger"].concat();
        let removed_rewrite_context = ["Rewrite", "Context"].concat();
        let removed_history_state = ["History", "State"].concat();
        let removed_history_metadata = ["History", "Rewrite", "Metadata"].concat();

        assert!(!source.contains(&removed_envelope));
        assert!(!source.contains(&removed_persisted));
        assert!(!source.contains(&removed_history_rewriter));
        assert!(!source.contains(&removed_rewrite_trigger));
        assert!(!source.contains(&removed_rewrite_context));
        assert!(!source.contains(&removed_history_state));
        assert!(!source.contains(&removed_history_metadata));
    }

    fn public_reexport_block(source: &str, module: &str) -> String {
        let start = format!("pub use {module}::{{");
        let mut block = String::new();
        let mut collecting = false;
        for line in source.lines() {
            if line.trim_start().starts_with(&start) {
                collecting = true;
            }
            if collecting {
                block.push_str(line);
                block.push('\n');
                if line.trim_end() == "};" {
                    break;
                }
            }
        }
        assert!(!block.is_empty(), "missing public {module} re-export block");
        block
    }

    #[test]
    fn root_runtime_exports_exclude_internal_runtime_records() {
        let runtime_exports = public_reexport_block(include_str!("lib.rs"), "runtime");
        for removed in [
            "RuntimeEffectCommand",
            "RuntimeEffectEnvelope",
            "RuntimeEffectKind",
            "RuntimeEffectOutcome",
            "RuntimeInvocation",
            "RuntimeScope",
            "RuntimeSessionState",
            "QueuedWorkBatch",
            "QueuedWorkBatchDraft",
            "QueuedWorkPayload",
            "prepare_process_registration",
            "process_wake_batch_draft",
            "require_event_replay",
        ] {
            assert!(
                !runtime_exports.contains(removed),
                "runtime root export leaked {removed}"
            );
        }
    }

    #[test]
    fn root_store_exports_exclude_wire_records() {
        let store_exports = public_reexport_block(include_str!("lib.rs"), "store");
        for removed in [
            "SessionHead",
            "SessionCheckpoint",
            "RuntimeCommit",
            "HydratedSessionCheckpoint",
            "PersistedSessionRead",
            "GraphCommitDelta",
        ] {
            assert!(
                !store_exports.contains(removed),
                "store root export leaked {removed}"
            );
        }
    }

    #[test]
    fn removed_manager_and_host_trait_names_stay_removed() {
        let removed_manager = ["Runtime", "Session", "Manager"].concat();
        let removed_host = ["Runtime", "Session", "Host"].concat();
        let sources = [
            include_str!("runtime/session_manager/mod.rs"),
            include_str!("plugin/runtime_host.rs"),
            include_str!("tool_dispatch/context.rs"),
            include_str!("tool_provider.rs"),
        ];

        for source in sources {
            assert!(!source.contains(&removed_manager));
            assert!(!source.contains(&removed_host));
        }
    }
}
