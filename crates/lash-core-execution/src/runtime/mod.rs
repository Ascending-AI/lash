use crate::TurnId;
pub use lash_core_store::turn_input_vocabulary::*;
pub mod causal;
pub(crate) use lash_core_ids::clock;
pub mod drive;
pub mod effect;
pub mod host;
#[cfg(feature = "testing")]
pub use lash_core_store::input_normalization as io;
pub mod process;
pub mod process_start;
pub mod trigger_delivery;
pub mod work;
pub(crate) use lash_core_store::queued_drain_policy;
use lash_core_store::session_catalog;
pub use lash_core_store::session_state as state;
use lash_core_store::session_store_factory_types;
pub use session_store_factory_types::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, SessionCreationHead,
    SessionStoreCreateRequest,
};
pub mod turn_control;
use lash_core_store::turn_failure_evidence;
pub use turn_failure_evidence::{
    ChargeSafetyRefusalEvidence, TurnFailureEvidence, TurnFailurePartialOutput,
    TurnFailureSettlement,
};
pub mod turn_queue;
#[cfg(feature = "testing")]
pub use lash_core_store::usage;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::usage;
mod park;
pub use park::{
    StoreParkRecovery, TurnLaneHead, head_input, head_input_root, input_root, record_root_park,
    turn_lane_head,
};
mod deployment_store_decorator;
pub use deployment_store_decorator::DeploymentStoreDecorator;
mod vocabulary;
pub use vocabulary::*;

pub use crate::store::QueuedWorkClass;

pub use causal::process_event_invocation;
pub use causal::tool_retry_sleep_invocation;
pub use causal::{CommandReplayKey, command_invocation};
pub use clock::{Clock, ClockWallTime, SystemClock};
pub use effect::TurnCancelWait;
pub use effect::await_event_identity;
/// Runtime effect contracts, including local process and trigger execution capabilities.
pub use effect::{
    AdmittedScope, AssistantResponseHookEvents, AssistantStreamHookState, AwaitEventKey,
    AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, CanonicalRuntimeEffectEnvelope,
    CausalRef, CheckpointAdmittedSet, CommandJournalGuard, CompletionKeyPreparation, EffectAddress,
    EffectGroupDrainBudget, EffectGroupHandle, EffectGroupMembership, EffectHost,
    EffectJournalIdentity, EffectJournalRetirement, EffectOpener, EffectRetirementGate,
    ExecutionScope, ExternalCompletionError, GroupChildBinding, GroupChildCancelWatch,
    GroupExecutors, GroupReopen, GroupSettlement, GroupWakePolicy, JournalReplay, LlmRequestSpec,
    LlmStreamRecord, LoserPolicy, ProcessCommand, ProcessDriveStep, ProcessEffectOutcome,
    ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation, RecordedJournal,
    RecordedKeyFence, RecordedKeyRange, RecordedKeys, RefusedWriteRange, Resolution,
    ResolveOutcome, RuntimeAssistantResponseHooksOutcome, RuntimeAttribution,
    RuntimeAwaitEventOptions, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    RuntimeInvocation, RuntimeLlmCallOutcome, RuntimeReplay, RuntimeReplayAttribution,
    RuntimeSleepOptions, RuntimeSubject, ScopeBoundController, ScopedEffectController,
    SegmentProgress, ServedOnly, ServedOnlyRange, SleepSpec, TOOL_ATTEMPT_CAPTURE_VERSION,
    TOOL_CHILD_REQUEST_VERSION, TOOL_PRESENTATION_VERSION, TOOL_SETTLEMENT_VERSION,
    ToolAttemptCapture, ToolAttemptEffectOutcome, ToolAttemptLaunch, ToolChildAdmission,
    ToolChildCompletionRouting, ToolChildDriver, ToolChildRebuildRefusal, ToolChildRequest,
    ToolChildScope, ToolChildSessionFacts, ToolIntentOutcomeSink, ToolIntentPreparation,
    ToolIntentSubmissionGuard, ToolInvocationEffectOutcome, ToolSettlement, ToolUsageDelta,
    ToolUsageLedger, TriggerLocalExecution, TurnCancelClosureOwnerBinding,
    TurnCancellationAuthority, TurnControlAttachment, TurnControlBinding, TurnControlBindingId,
    TurnControlBindingIdError, UnrecordedSessionSources, effect_groups_unsupported,
    refuse_unhonored_group_membership, turn_control_binding_id_for_scope,
    validate_replayed_effect_envelope,
};
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    EmbeddedRuntimeHost, ProcessRuntimeHost, RuntimeControlConfig, RuntimeDurabilityConfig,
    RuntimeHostConfig, RuntimePromptConfig, RuntimeProviderConfig, RuntimeTracingConfig,
};
pub use process::ProcessChangeSubscription;
#[cfg(any(test, feature = "testing"))]
pub use process::reconcile_pruned_trigger_deliveries_interleaved;
pub use process::registry_transitions;
pub use process::{
    AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry,
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, DeclaredProcessIdentity, HandleId, InvalidProcessDefinitionId,
    InvalidStartKey, Lifetime, LifetimeDecision, LifetimePolicy,
    MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NonTerminalProcessPage, ObservedProcess,
    ObservedProcessEvent, ObservedProcessEventLite, ObservedProcessEventPage,
    ObservedProcessEventReadOutcome, ObservedWorkItem, ObservedWorkItemState,
    PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ParentEndApplication, ParentEndPlan,
    PersistedSegmentHandover, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessChangeHub, ProcessClockRebind, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessContinuationStore, ProcessDefinition, ProcessDefinitionDraft,
    ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionTarget,
    ProcessDefinitionValue, ProcessEngine, ProcessEngineAdmission, ProcessEngineKind,
    ProcessEngineProcessContext, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessEvent,
    ProcessEventAppendPlan, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog, ProcessEventPage,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemantics, ProcessEventSemanticsSpec, ProcessEventSink,
    ProcessEventSinkRegistration, ProcessEventType, ProcessExecutionContext,
    ProcessExecutionEnvLoadError, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessHandleView, ProcessId, ProcessIdMint, ProcessIdentity, ProcessInfraError, ProcessInput,
    ProcessLifecycle, ProcessLineage, ProcessListFilter, ProcessListMode, ProcessLiveReferenceView,
    ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope, ProcessOriginator,
    ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance, ProcessPruneReport, ProcessQuery,
    ProcessRecord, ProcessRegistrar, ProcessRegistration, ProcessRegistrationOutcome,
    ProcessRegistrationProbe, ProcessRegistrationReceipt, ProcessRegistrationRefusal,
    ProcessRegistry, ProcessRegistryBinding, ProcessRegistryCursor, ProcessResumeRefusal,
    ProcessRetention, ProcessRunOutcome, ProcessScopeFenceHosts, ProcessSegmentKey, ProcessService,
    ProcessSessionDeleteReport, ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartPlan, ProcessStartReceipt,
    ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter,
    ProcessTerminalPublication, ProcessTerminalSemantics, ProcessTerminalSpec, ProcessTombstone,
    ProcessToolIntents, ProcessToolVisibilityFilter, ProcessTransition, ProcessTransitionPlan,
    ProcessValueSelector, ProcessWake, ProcessWakeDelivery, ProcessWakeDeliveryRequest,
    ProcessWakeOutbox, ProcessWakeSpec, ProcessWorkObserver, ProcessWorkSnapshot,
    ProjectionWatermark, RegistryScopeClose, SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId,
    ScopeRef, ScopeStorageError, SegmentHandover, SegmentStartMarker, SessionId,
    SessionObserverIntentSource, SessionScope, SessionScopeId, StartCx, StartCxError, StartKey,
    StoreRealization, UnavailableProcessService, WAKE_ENQUEUING_STALE_AFTER_MS, WaitKind,
    WaitState, WakeDelivery, WakeDeliveryBlockedGroup, WakeDeliveryClaimOutcome,
    WakeDeliveryConfig, WakeDeliveryLifecycle, WakeDeliveryReport, WakeDeliveryState,
    WakeDiscardReason, WatchedRegistry, WeakProcessEngineRegistry, abandoned_consumer_refusal,
    allocate_process_event_sequence, apply_parent_end_plan, apply_process_event_projection,
    apply_process_status_projection, artifact_referrer_ended, check_retained_start,
    current_epoch_ms, end_parent_scope, end_session_roots, fold_process_record, lifetime,
    load_process_execution_env, materialize_process_event_semantics, mint_process_id,
    parent_end_delivery_key, parent_end_requester, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_child_session_id, process_park_transitions, process_runtime_session_ids,
    process_signal_event_type, process_signal_name_from_event_type, process_signal_wait_key,
    process_wake_delivery, process_wake_input_from_event_payload, process_wake_turn_cause,
    process_wake_turn_text, publish_process_execution_env, reconcile_pruned_trigger_deliveries,
    reconcile_session_process_observer_intents, release_bound_trigger_delivery_pins,
    require_event_replay, terminal_append_request, terminal_event_type_name,
    validate_generic_process_event_append, validate_process_signal_name, watch_process_registry,
    watch_process_registry_with_sink,
};
pub use process::{
    ArtifactReferrerPorts, ProcessStartStores, ReferrerAcquisition, RegisteredProcessStart,
    register_process_start,
};
#[cfg(any(test, feature = "testing"))]
pub use process::{
    ConformanceProcessRegistry, EffectSummaryAppendFaults, PROCESS_REFUSAL_FIXTURE_START_KEY,
    ProcessEventLogTestSupport, ProcessRegistryFaults, ProcessRegistryTestSupport,
    TestProcessRegistryWriteExt, accepted_process_registration, fail_parent_end_once,
    refused_process_registrations,
};
pub use process::{ConsumerHold, PinnedTriggerDelivery, SessionTurnOutcome, TriggerDeliveryPin};
pub use queued_drain_policy::default_queued_drain_policy;
pub(crate) use queued_drain_policy::shared_drain_mode_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest,
    QueuedDrainSelection,
};
pub use session_catalog::*;
pub use state::{RuntimeCheckpointComponents, RuntimeSessionState};
pub use turn_control::{
    LocalTurnStop, StopDeliveryGuard, TurnAddress, TurnAttach, TurnCancelAffectedInput,
    TurnCancelAffectedWake, TurnCancelClosureAuthorization, TurnCancelClosureAuthorizationOutcome,
    TurnCancelClosureProposal, TurnCancelClosureSettlement, TurnCancelGatePair,
    TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode, TurnCancelOutcome,
    TurnCancelReceipt, TurnCancelRequest, TurnCancelRequestRecord,
    TurnCancelUndeliveredInputPolicy, TurnCancellationEvidence, TurnTerminal, TurnWorkDriver,
    retry_cancel_watch, run_step_body_until_cancelled,
};
#[cfg(feature = "testing")]
pub use turn_queue::SessionCommandSettlement;
pub use turn_queue::SessionCommandSettlementHandle;
pub use turn_queue::{
    AdmissionBoundary, AdmittedQueuedWork, DeliveryPolicy, PROCESS_WAKE_MERGE_KEY,
    ProcessWakeSource, QueuedCheckpointWork, QueuedWorkAuthority, QueuedWorkBatch,
    QueuedWorkBatchDraft, QueuedWorkBatchPayloads, QueuedWorkBatchingConfig, QueuedWorkCompletion,
    QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload, SessionCommand,
    SessionCommandPayload, SessionCommandReceipt, TurnLaneAdmissionPolicy, TurnWorkPayload,
    process_wake_batch_draft, process_wake_batch_draft_with_delivery_policy,
    process_wake_source_key,
};
pub use usage::{
    LedgerUsageOutcome, ReconciledUsageAttempt, SessionUsageReport, SessionUsageTotals,
    TokenLedgerEntry, UnreportedLedgerAttempt, UnreportedUsageAttempt, UsageOutcomeError,
    UsageReconciliationReport, UsageReportRow, UsageTotalRow, UsageTotals, diff_token_ledger,
    diff_usage_reports, outstanding_unreported_attempts,
};
pub use work::{
    NoProcessWork, NoSessionWork, ProcessRegistryAwaiter, ProcessTerminalWait,
    ProcessWorkSubstrate, ProcessWorkWiring, SessionDriver, SessionWorkEngine,
    WakeDeliveryDriveReport, WakeDeliveryDriver, WorkCadenceError, WorkCadencePolicy,
};

// Turn-execution vocabulary. These types and the phase-probe trait carry no
// runtime machinery, so they live one layer down in `lash-core-llm` where the
// plugin, tool-provider and tool-dispatch layers can name them without
// reaching up into the runtime. Re-exported here at their original paths.
pub use lash_core_llm::turn_vocabulary::{
    AssistantOutput, OutputState, RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
    TurnExecutionMetrics, TurnIssue, TurnIssueSeverity,
};

pub use lash_core_store::effect_opener::EffectOpenerError;
pub use lash_core_store::runtime_error::{
    ExecutableGeneration, ExecutableGenerationRefusal, RuntimeError, RuntimeErrorCause,
    RuntimeErrorCode, SessionStateVersionRefusal, TurnFailureCause,
};

pub use crate::direct_completion_client::DirectCompletionClient;

#[cfg(feature = "testing")]
mod normalized_item {
    pub use lash_core_store::input_normalization::NormalizedItem;
}

// The relocated `runtime::tests` binaries name this type; the `testing` feature
// is the seam that lets them, and the non-testing public surface is unchanged.
#[cfg(feature = "testing")]
pub use normalized_item::NormalizedItem;
