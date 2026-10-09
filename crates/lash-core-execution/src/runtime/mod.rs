use crate::TurnId;
pub use lash_core_store::turn_input_vocabulary::*;
pub mod actor;
pub mod attachment_delivery;
pub mod causal;
pub(crate) use lash_core_ids::clock;
pub mod effect;
pub mod host;
pub mod obligations;
mod owner;
#[cfg(feature = "testing")]
pub use lash_core_store::input_normalization as io;
pub use owner::ExecutionOwner;
pub(crate) use owner::not_a_session_runtime;
pub mod process;
pub mod work;
pub(crate) use lash_core_store::queued_drain_policy;
use lash_core_store::session_catalog;
pub use lash_core_store::session_state as state;
use lash_core_store::session_store_factory_types;
pub use session_store_factory_types::{
    ForkSessionReceipt, ForkSessionRequest, RetainedRevision, Retention, SessionCreationHead,
    SessionStoreCreateRequest, Target,
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
mod turn_lane;
pub use turn_lane::{head_input, head_input_run, input_run};
mod deployment_store_decorator;
#[cfg(any(test, feature = "testing"))]
pub use deployment_store_decorator::DeploymentOp;
pub use deployment_store_decorator::DeploymentStoreDecorator;
mod vocabulary;
pub use vocabulary::*;

/// The trace handle a host config carries ([`RuntimeHostConfig::tracing`]).
pub use crate::trace::{TraceEmitter, TraceRuntime};
pub use causal::process_event_invocation;
pub use causal::tool_retry_sleep_invocation;
pub use causal::{CommandReplayKey, command_invocation};
pub use clock::{Clock, ClockWallTime, SystemClock};
pub use effect::TurnCancelWait;
/// Runtime effect contracts, including local process execution capabilities.
pub use effect::{
    AdmittedDirectSend, AdmittedScope, AssistantResponseHookEvents, AssistantResponsePlan,
    AssistantStreamHookState, AwaitEventKey, AwaitEventWaitIdentity,
    CanonicalRuntimeEffectEnvelope, CausalRef, CheckpointAdmittedSet, CommandJournalGuard,
    EffectAddress, EffectJournalIdentity, EffectJournalRetirement, EffectOpener,
    EffectRetirementGate, ExecutionScope, ExternalCompletionError, JournalReplay, LlmRequestSpec,
    LlmStreamRecord, PresentationBinding, ProcessCommand, ProcessDriveStep, ProcessEffectOutcome,
    ProcessListSelection, ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation,
    RecordedKeyFence, RecordedKeyRange, RecordedKeys, RefusedWriteRange, Resolution,
    ResolveOutcome, RunAggregateWakePolicy, RuntimeAssistantResponseHooksOutcome,
    RuntimeAttribution, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace, RuntimeInvocation,
    RuntimeLlmCallOutcome, RuntimeReplay, RuntimeReplayAttribution, RuntimeSleepOptions,
    RuntimeSubject, SegmentProgress, ServedOnly, ServedOnlyRange, SleepSpec,
    ToolAttemptEffectOutcome, ToolAttemptLaunch, TurnPrelude, TurnPreludeRef, TurnPreludeStore,
    validate_replayed_effect_envelope,
};
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    DataRetentionConfig, DeltaCoalescing, DeltaCoalescingError, EmbeddedRuntimeHost,
    ProcessRuntimeHost, RuntimeControlConfig, RuntimeDurabilityConfig, RuntimeHostConfig,
    RuntimeProviderConfig,
};
pub use process::ProcessChangeSubscription;
pub use process::registry_transitions;
pub use process::{
    AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry, ArgsMismatch, ArgsMode,
    CanonicalProcessEventAppend, DeclaredProcessIdentity, DefinitionAcquisition, EngineAction,
    EngineEvent, EngineState, EngineStateFormat, EngineStepKind, EngineStepRefusal, EngineStepRun,
    EngineSteps, HandleId, InspectedProcessDefinition, InvalidProcessDefinitionId, InvalidStartKey,
    KeyName, Lifetime, LifetimeDecision, LifetimePolicy, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE,
    Material, NamesMaterial, NonTerminalProcessPage, ObservedProcess, ObservedProcessEvent,
    ObservedProcessEventLite, ObservedProcessEventPage, ObservedProcessEventReadOutcome,
    ObservedWorkItem, ObservedWorkItemState, ParentEndPlan, PreparedProcessRegistration,
    ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange, ProcessChangeCursor, ProcessChangeHub,
    ProcessClockRebind, ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessDefinition,
    ProcessDefinitionDraft, ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionStore,
    ProcessDefinitionStoredError, ProcessDefinitionTarget, ProcessDefinitionValue, ProcessDocument,
    ProcessDocumentProvider, ProcessDocumentRead, ProcessEngine, ProcessEngineAdmission,
    ProcessEngineKind, ProcessEngineRegistration, ProcessEngineRegistry, ProcessEvent,
    ProcessEventAppendPlan, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventKind, ProcessEventLite, ProcessEventLog,
    ProcessEventPage, ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode,
    ProcessEventReadOutcome, ProcessEventRelease, ProcessEventSink, ProcessEventSinkRegistration,
    ProcessExecutionContext, ProcessExecutionEnvLoadError, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleView, ProcessId, ProcessIdMint, ProcessIdentity,
    ProcessInfraError, ProcessInput, ProcessLifecycle, ProcessLifecycleFact, ProcessLifecycleState,
    ProcessLineage, ProcessListFilter, ProcessListMode, ProcessLiveReferenceView,
    ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope, ProcessOriginator,
    ProcessOriginatorFilter, ProcessOutcome, ProcessOutcomeNotRetained, ProcessParkReason,
    ProcessProvenance, ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRegistrar,
    ProcessRegistration, ProcessRegistrationOutcome, ProcessRegistrationReceipt,
    ProcessRegistrationRefusal, ProcessRegistry, ProcessRegistryCursor, ProcessResumeRefusal,
    ProcessRetention, ProcessRunOutcome, ProcessService, ProcessSessionDeleteReport,
    ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration, ProcessStartOptions,
    ProcessStartOutcome, ProcessStartPlan, ProcessStartReceipt, ProcessStartRegistration,
    ProcessStartRequest, ProcessStartTarget, ProcessStarted, ProcessStatus, ProcessStatusFilter,
    ProcessTerminal, ProcessTombstone, ProcessToolIntents, ProcessToolVisibilityFilter,
    ProcessTransition, ProcessTransitionPlan, ProcessWorkObserver, ProcessWorkSnapshot,
    ProjectionWatermark, ReleasedProcessEvent, ResolvedProcessDefinition, RetiredProcessStatus,
    SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId, ScopeRef, ScopeStorageError, SessionId,
    SessionObserverIntentSource, SessionScope, SessionScopeId, SettledOutput, SettledOutputRefusal,
    StartCx, StartCxError, StartKey, StepEffectSite, StepName, StepRequest, StoreRealization,
    TerminalProcessStatus, UnavailableProcessService, WaitKind, WaitState, WatchedRegistry,
    WeakProcessEngineRegistry, abandoned_consumer_refusal, allocate_process_event_sequence,
    apply_process_event_projection, artifact_referrer_ended, check_retained_start,
    current_epoch_ms, fold_process_record, lifetime, load_process_execution_env, mint_process_id,
    prepare_process_event_append, prepare_process_registration, prepare_process_start,
    prepare_process_transition, process_child_session_id, process_session_turn_id,
    publish_process_execution_env, reconcile_session_process_observer_intents,
    release_process_event_payload, restore_released_process_event, terminal_append_request,
    watch_process_registry,
};
pub use process::{
    ArtifactReferrerPorts, HostStartAdmission, PreparedProcessStart, ProcessStartStores,
    ReferrerAcquisition, RegisteredProcessStart, SessionTurnAdmission, StagedProcessStart,
    StagedRegistration, StartStaging, is_start_operation, register_process_start,
    stage_process_start, stage_store_local_start, start_operation_journal,
};
#[cfg(any(test, feature = "testing"))]
pub use process::{
    ConformanceProcessRegistry, EffectSummaryAppendFaults, PROCESS_REFUSAL_FIXTURE_START_KEY,
    ProcessEventLogTestSupport, ProcessRegistryFaults, ProcessRegistryTestSupport,
    TestProcessRegistryWriteExt, accepted_process_registration, refused_process_registrations,
};
pub use process::{ConsumerHold, SessionTurnOutcome};
pub use queued_drain_policy::default_queued_drain_policy;
pub(crate) use queued_drain_policy::shared_drain_mode_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainFamily, QueuedDrainPolicy,
    QueuedDrainRequest, QueuedDrainSelection,
};
pub use session_catalog::*;
pub use state::{RuntimeCheckpointComponents, RuntimeSessionAuthority, RuntimeSessionState};
pub use turn_control::{
    QueueWithdrawalPublisher, TurnAddress, TurnAttach, TurnCancelAffectedInput,
    TurnCancelInputOutcome, TurnCancelMode, TurnCancelOutcome, TurnCancelReceipt,
    TurnCancelRequest, TurnCancelUndeliveredInputPolicy, TurnCancellationEvidence, TurnTerminal,
    TurnWorkDriver,
};
#[cfg(feature = "testing")]
pub use turn_queue::SessionCommandSettlement;
pub use turn_queue::SessionCommandSettlementHandle;
pub use turn_queue::{
    AdmissionBoundary, AdmittedQueuedWork, CompactContextOutcome, DeliveryPolicy,
    OpenAgentFrameCommandOutcome, PluginOperationCommandOutcome, QueuedWorkAuthority,
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkBatchingConfig, QueuedWorkCompletion,
    QueuedWorkEnqueueOutcome, QueuedWorkPayload, SessionCommand, SessionCommandOutcome,
    SessionCommandReceipt, TurnLaneAdmissionPolicy,
};

pub use work::{
    CommitAdmissionPolicy, CommitAdmissionPolicyError, DurableProcessWork, NoProcessWork,
    PollPacing, ProcessRegistryAwaiter, ProcessTerminalWait, ProcessWorkSubstrate,
    ProcessWorkWiring, RuntimePacingPolicy, WorkCadenceError, WorkCadencePolicy,
};

// Turn-execution vocabulary. These types and the phase-probe trait carry no
// runtime machinery, so they live one layer down in `lash-core-llm` where the
// plugin, tool-provider and tool-dispatch layers can name them without
// reaching up into the runtime. Re-exported here at their original paths.
pub use lash_core_llm::turn_vocabulary::{TurnExecutionMetrics, TurnIssue, TurnIssueSeverity};

pub use lash_core_store::effect_opener::EffectOpenerError;
pub use lash_core_store::runtime_error::{
    ExecutableGeneration, ExecutableGenerationRefusal, RecordedRefusal, RuntimeError,
    RuntimeErrorCause, RuntimeErrorCode, SessionStateVersionRefusal, StoredDataCorruption,
    TurnFailureCause,
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

/// Explicitly unstable internal instrumentation, outside the promised API.
#[doc(hidden)]
pub use lash_core_llm::turn_vocabulary::{
    RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
};
