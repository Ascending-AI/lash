use crate::TurnId;
pub use lash_core_store::turn_input_vocabulary::*;
pub mod causal;
pub(crate) use lash_core_ids::clock;
pub mod effect;
pub mod host;
pub mod in_memory_store;
#[cfg(feature = "testing")]
pub use lash_core_store::input_normalization as io;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::input_normalization as io;
pub mod native_substrate;
pub mod process;
#[cfg(feature = "testing")]
pub use lash_core_effect::session_execution_lease;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_effect::session_execution_lease;
pub(crate) use lash_core_store::queued_drain_policy;
use lash_core_store::session_catalog;
pub use lash_core_store::session_state as state;
use lash_core_store::session_store_factory_types;
pub use session_store_factory_types::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, SessionStoreCreateRequest,
};
pub mod turn_control;
use lash_core_store::turn_failure_evidence;
pub use turn_failure_evidence::{
    ChargeSafetyRefusalEvidence, TurnFailureEvidence, TurnFailurePartialOutput,
    TurnFailureSettlement,
};
pub mod turn_queue;
use lash_core_ids::worker_capacity;
#[cfg(feature = "testing")]
pub use lash_core_store::usage;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::usage;
mod vocabulary;
pub use vocabulary::*;

// `PromptUsage` is re-exported below alongside the runtime's own types.
pub use lash_sansio::PromptUsage;

pub use crate::store::QueuedWorkClass;

pub use causal::process_event_invocation;
pub use causal::tool_retry_sleep_invocation;
pub use clock::{Clock, ClockWallTime, SystemClock};
pub use effect::await_event_coordinator;
pub use effect::effect_replay_driver;
pub use effect::promise_semantics;
/// Runtime effect contracts, including local process and trigger execution capabilities.
pub use effect::{
    AdmittedScope, AdmittedScopeError, AssistantResponseHookEvents, AwaitEventKey,
    AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, CanonicalRuntimeEffectEnvelope,
    CausalRef, CheckpointClaimSet, ChildDrainOutcome, CompletionKeyPreparation, DrainedChild,
    EffectAddress, EffectGroupHandle, EffectGroupMembership, EffectHost, EffectJournalIdentity,
    EffectJournalRetirement, EffectOpener, EffectRetirementGate, ExecutionScope,
    ExternalCompletionError, GroupChildBinding, GroupDrainReport, GroupExecutors, GroupSettlement,
    GroupWakePolicy, LlmRequestSpec, LoserPolicy, NativeEffectHost, NativeRuntimeEffectController,
    ProcessCommand, ProcessEffectOutcome, ProcessLocalExecution, ProcessOutcomeObserver,
    ProcessTurnCancellation, QueuedLaneAcquisition, QueuedLaneAttempt, QueuedLaneGuard,
    QueuedLaneHolder, QueuedLaneProbe, Resolution, ResolveOutcome,
    RuntimeAssistantResponseHooksOutcome, RuntimeAttribution, RuntimeAwaitEventOptions,
    RuntimeDirectLlmOutcome, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectFailureDisposition,
    RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    RuntimeInvocation, RuntimeLlmCallOutcome, RuntimeReplay, RuntimeReplayAttribution,
    RuntimeSleepOptions, RuntimeSubject, ScopeBoundController, ScopedEffectController,
    SegmentProgress, SleepSpec, StoreEffectGroupDrain, TOOL_ATTEMPT_CAPTURE_VERSION,
    TOOL_CHILD_REQUEST_VERSION, TOOL_SETTLEMENT_VERSION, ToolAttemptCapture,
    ToolAttemptEffectOutcome, ToolAttemptLaunch, ToolBatchEffectOutcome, ToolCallLaunch,
    ToolChildAdmission, ToolChildCompletionRouting, ToolChildDriver, ToolChildRequest,
    ToolChildScope, ToolIntentOutcomeSink, ToolIntentPreparation, ToolIntentSubmissionGuard,
    ToolInvocationEffectOutcome, ToolSettlement, ToolUsageDelta, ToolUsageLedger,
    TriggerLocalExecution, TurnCancelClosureOwnerBinding, TurnCancellationAuthority,
    TurnControlAttachment, TurnControlAuthorityOwner, TurnControlBinding, TurnControlBindingId,
    TurnControlBindingIdError, TurnControlParticipation, concrete_turn_cancellation_authority,
    effect_groups_unsupported, refuse_unhonored_group_membership,
    turn_control_binding_id_for_scope, validate_replayed_effect_envelope,
};
#[cfg(feature = "testing")]
pub use effect::{RuntimeEffectControllerHandle, TurnCancelWait};
#[cfg(not(feature = "testing"))]
pub(crate) use effect::{RuntimeEffectControllerHandle, TurnCancelWait};
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    DEFAULT_ENGINE_CHILD_MAX_ATTEMPTS, EmbeddedRuntimeHost, ProcessRuntimeHost,
    RuntimeControlConfig, RuntimeDurabilityConfig, RuntimeHostConfig, RuntimePromptConfig,
    RuntimeProviderConfig, RuntimeTracingConfig,
};
#[cfg(any(test, feature = "testing"))]
pub use in_memory_store::RawSessionExecutionLeaseRow;
#[cfg(any(test, feature = "testing"))]
pub use in_memory_store::in_memory_lineage_handles;
pub use in_memory_store::{InMemorySessionStore, InMemorySessionStoreFactory};
pub use lash_core_ids::execution_permit::{
    ensure_process_execution_permit, release_process_execution_permit_while,
};
pub use native_substrate::{
    NativeProcessAdmissionDriver, NativeProcessWork, NativeSubstrateConfig,
    NativeSubstrateConfigError, NoQueuedWork, ProcessTerminalWait, ProcessWorkSubstrate,
    ProcessWorkWiring, QueuedWorkSubstrate, SessionDrainOutcome, SessionWorkTarget,
    WakeDeliveryDriveReport, WakeDeliveryDriver, WorkCadencePolicy, WorkerSweepPolicy,
};
#[cfg(any(test, feature = "testing"))]
pub use process::reconcile_pruned_trigger_deliveries_interleaved;
pub use process::registry_transitions;
pub use process::{
    AbandonEvidence, AbandonRequest, AbandonWriter, AdmittedProcessIdentity, ArtifactOwner,
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, DeclaredProcessIdentity, HandleId,
    InMemoryProcessExecutionEnvStore, ObservedProcess, ObservedProcessEvent,
    ObservedProcessEventLite, ObservedProcessEventPage, ObservedProcessEventReadOutcome,
    ObservedWorkItem, ObservedWorkItemState, OnParentEnd, PARENT_SCOPE_STORAGE_PAYLOAD_VERSION,
    PROCESS_LEASE_SCHEMA_VERSION, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ParentEndPlan, ParentScope,
    ParentScopeStorageError, PersistedSegmentHandover, ProcessArtifactCleanup,
    ProcessArtifactCleanupAck, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessChangeHub, ProcessClockRebind, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessContinuationStore, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionValue, ProcessEngine,
    ProcessEngineAdmission, ProcessEngineKind, ProcessEngineProcessContext,
    ProcessEngineRegistration, ProcessEngineRegistry, ProcessEngineRunContext,
    ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessEvent, ProcessEventAppendPlan,
    ProcessEventAppendReceipt, ProcessEventAppendRequest, ProcessEventHistoryRetention,
    ProcessEventLite, ProcessEventLog, ProcessEventPage, ProcessEventPageEvents,
    ProcessEventPageMore, ProcessEventPageToken, ProcessEventPageTokenStoreExt,
    ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventSemantics,
    ProcessEventSemanticsSpec, ProcessEventSink, ProcessEventType, ProcessExecutionContext,
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleView, ProcessId,
    ProcessIdentity, ProcessIncarnation, ProcessInfraError, ProcessInput, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessLeaseSchemaVersionError,
    ProcessLeases, ProcessLifecycle, ProcessLifecyclePolicy, ProcessListFilter, ProcessListMode,
    ProcessLiveReferenceView, ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance,
    ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRef, ProcessRegistrar,
    ProcessRegistration, ProcessRegistrationDisposition, ProcessRegistrationOutcome,
    ProcessRegistrationProbe, ProcessRegistrationRefusal, ProcessRegistry, ProcessRegistryBinding,
    ProcessRetention, ProcessRunOutcome, ProcessScopeFenceHosts, ProcessService,
    ProcessSessionDeleteReport, ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartPlan, ProcessStartRequest,
    ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalSemantics,
    ProcessTerminalSpec, ProcessTombstone, ProcessToolIntents, ProcessToolVisibilityFilter,
    ProcessTransition, ProcessTransitionPlan, ProcessValueSelector, ProcessWake,
    ProcessWakeDelivery, ProcessWakeDeliveryRequest, ProcessWakeOutbox, ProcessWakeSpec,
    ProcessWorkObserver, ProcessWorkSnapshot, ProcessWorklistCursor, ProcessWorklistPage,
    ProjectionWatermark, RecoveryContract, SegmentHandover, SessionId, SessionObserverIntentSource,
    SessionScope, SessionScopeId, StoreRealization, UnavailableProcessService,
    WAKE_ENQUEUING_STALE_AFTER_MS, WaitKind, WaitState, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryDisposition, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, WatchedRegistry, allocate_process_event_sequence,
    apply_process_event_projection, apply_process_status_projection,
    artifact_owner_is_permanently_retired, artifact_staging_owner_edge_is_missing,
    current_epoch_ms, ensure_process_lease_schema_version, fold_process_record,
    load_process_execution_env, materialize_process_event_semantics, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_registration_fingerprint, process_runtime_session_ids, process_signal_await_key,
    process_signal_event_type, process_signal_name_from_event_type, process_signal_wait_key,
    process_wake_delivery, process_wake_input_from_event_payload, process_wake_turn_cause,
    process_wake_turn_text, publish_process_execution_env, reconcile_pruned_trigger_deliveries,
    reconcile_session_process_observer_intents, require_event_replay,
    settle_started_process_engine_artifacts, settle_started_process_execution_env,
    terminal_append_request, terminal_event_type_name, validate_generic_process_event_append,
    validate_process_signal_name, watch_process_registry, watch_process_registry_with_sink,
};
#[cfg(any(test, feature = "testing"))]
pub use process::{
    ConformanceProcessRegistry, PROCESS_REFUSAL_FIXTURE_PROCESS_ID, ProcessEventLogTestSupport,
    ProcessRegistryTestSupport, TestLocalProcessRegistry, TestProcessRegistryWriteExt,
    accepted_process_registration, refused_process_registrations,
};
pub use process::{
    ProcessAdmissionDeferred, ProcessAdmissionIntake, ProcessAdmissionReport, ProcessDrainDeferred,
    ProcessDrainReport, ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation,
    ProcessWorkerFault,
};
pub use queued_drain_policy::default_queued_drain_policy;
pub(crate) use queued_drain_policy::shared_drain_mode_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest,
    QueuedDrainSelection,
};
pub use session_catalog::*;
pub use state::{RuntimeCheckpointComponents, RuntimeSessionState};
pub use turn_control::{
    TurnAddress, TurnAttach, TurnCancelAffectedInput, TurnCancelClosureAuthorization,
    TurnCancelClosureAuthorizationOutcome, TurnCancelClosureProposal, TurnCancelClosureSettlement,
    TurnCancelDisposition, TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode,
    TurnCancelOriginHint, TurnCancelOutcome, TurnCancelReceipt, TurnCancelRequest,
    TurnCancelRequestRecord, TurnCancellationEvidence, TurnTerminal, TurnWorkDriver,
};
#[cfg(feature = "testing")]
pub use turn_queue::SessionCommandSettlement;
#[cfg(not(feature = "testing"))]
pub(crate) use turn_queue::SessionCommandSettlement;
pub use turn_queue::SessionCommandSettlementHandle;
pub use turn_queue::{
    DeliveryPolicy, PROCESS_WAKE_MERGE_KEY, ProcessWakeSource, QueuedCheckpointWork,
    QueuedTurnWork, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkBatchPayloads, QueuedWorkBatchingConfig, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkClaimData, QueuedWorkClaimPolicy, QueuedWorkCompletion, QueuedWorkCompletionData,
    QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload, SessionCommand,
    SessionCommandPayload, SessionCommandReceipt, TurnWorkPayload, process_wake_batch_draft,
    process_wake_batch_draft_with_delivery_policy, process_wake_source_key,
};
pub use usage::{
    LedgerUsageDisposition, ReconciledUsageAttempt, SessionUsageReport, TokenLedgerEntry,
    UnreportedLedgerAttempt, UnreportedUsageAttempt, UsageDispositionError,
    UsageReconciliationReport, UsageReportRow, UsageTotals, diff_token_ledger, diff_usage_reports,
    outstanding_unreported_attempts,
};
pub use worker_capacity::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};

// Turn-execution vocabulary. These types and the phase-probe trait carry no
// runtime machinery, so they live one layer down in `lash-core-llm` where the
// plugin, tool-provider and tool-dispatch layers can name them without
// reaching up into the runtime. Re-exported here at their original paths.
pub use lash_core_llm::turn_vocabulary::{
    AssistantOutput, OutputState, RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
    TurnExecutionMetrics, TurnIssue, TurnIssueSeverity,
};

pub use lash_core_store::effect_opener::EffectOpenerError;
pub use lash_core_store::runtime_error::{RuntimeError, RuntimeErrorCause, RuntimeErrorCode};

pub use crate::direct_completion_client::DirectCompletionClient;

mod normalized_item {
    pub use lash_core_store::input_normalization::NormalizedItem;
}

// The relocated `runtime::tests` binaries name this type; the `testing` feature
// is the seam that lets them, and the non-testing public surface is unchanged.
#[cfg(feature = "testing")]
pub use normalized_item::NormalizedItem;
#[cfg(not(feature = "testing"))]
pub(crate) use normalized_item::NormalizedItem;
