mod awaiter;
mod declared_start;
mod definition;
mod definition_ref;
mod definition_store;
mod effect_summary;
mod engine;
mod events;
#[cfg(test)]
mod guarded_surface_tests;
pub(crate) mod identity_projection;
mod materialization;
pub(crate) mod model;
#[cfg(test)]
mod model_filter_tests;
mod observation;
mod observer_intent;
mod op_scope;
mod parent_end;
mod references;
mod registry;
mod registry_concerns;
pub(crate) mod registry_delegate;
pub mod registry_transitions;
mod scope_close;
mod service;
mod start_staging;
#[cfg(any(test, feature = "testing"))]
mod testing;
#[cfg(test)]
mod tests;
mod validation;
mod wake;
mod worker_engine;
mod worker_ownership;

pub use awaiter::{
    ProcessChangeHub, ProcessChangeSubscription, ProcessEventSink, ProcessEventSinkRegistration,
    WatchedRegistry, watch_process_registry, watch_process_registry_with_sink,
};
pub use declared_start::{
    DeclaredStartObligation, DeclaredStartObligationRefusal, DeclaredStartPhase,
    IsolatedStartRefusal, IsolatedToolStart, PhysicalProcessWorker, ProcessExecutionBoundary,
    StartCancelDecision, WorkerTerminationReceipt,
};
pub use definition::{
    InvalidProcessDefinitionId, ProcessDefinition, ProcessDefinitionDraft,
    ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionStoredError,
    ProcessDefinitionTarget,
};
pub use definition_ref::{
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
    ProcessDefinitionValue, ProcessEngineKind, ProcessSignature,
};
pub use definition_store::{
    DefinitionAcquisition, ProcessDefinitionStore, ResolvedProcessDefinition,
};
pub use effect_summary::{
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION, ProcessEffectNodeReport,
    ProcessEffectOccurrence, ProcessEffectOmissions, ProcessEffectOmittedCounts,
    ProcessEffectOutcomeClass, ProcessEffectReport, ProcessEffectReportError, tool_failure_code,
};
pub use engine::{
    AdmittedProcessIdentity, PersistedSegmentHandover, ProcessEngine, ProcessEngineAdmission,
    ProcessEngineProcessContext, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessInfraError,
    ProcessRunOutcome, SegmentHandover, WeakProcessEngineRegistry,
};
pub use events::{
    AbandonEvidence, AbandonWriter, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessAwaitOutput,
    ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventPage, ProcessEventPageEvents,
    ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventRelease,
    ProcessEventSemantics, ProcessEventSemanticsSpec, ProcessEventType, ProcessOutcomeNotRetained,
    ProcessResumeRefusal, ProcessSignal, ProcessSignalIdentity, ProcessSignalWaitBinding,
    ProcessTerminal, ProcessTerminalSemantics, ProcessTerminalSpec, ProcessValueSelector,
    ProcessWake, ProcessWakeDelivery, ProcessWakeSpec, WakeId, admitted_signal_wait,
    process_signal_event_type, process_signal_name_from_event_type, process_signal_wait_key,
    release_process_event_payload, restore_released_process_event_payload,
    runtime_lifecycle_event_type, terminal_append_request, terminal_event_type_name,
    validate_process_signal_name,
};
pub use materialization::materialize_process_event_semantics;
pub use model::{
    Ancestry, DeclaredProcessIdentity, HandleId, InvalidStartKey, Lifetime, LifetimeDecision,
    LifetimePolicy, PreparedProcessRegistration, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessCompletionOutcome, ProcessExecutionContext,
    ProcessExecutionEnvLoadError, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessHandleView, ProcessId, ProcessIdMint, ProcessIdentity, ProcessInput,
    ProcessLifecycleState, ProcessLineage, ProcessListFilter, ProcessListMode, ProcessObserverBy,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessRegistrationOutcome, ProcessRegistrationReceipt,
    ProcessSessionDeleteReport, ProcessSpawnProvenance, ProcessStartDeclaration,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartReceipt, ProcessStartRegistration,
    ProcessStartRequest, ProcessStartTarget, ProcessStarted, ProcessStatus, ProcessStatusFilter,
    ProcessTombstone, RetiredProcessStatus, SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId,
    ScopeRef, ScopeStorageError, SessionId, SessionScope, SessionScopeId, StartCx, StartCxError,
    StartKey, StoreRealization, TerminalProcessStatus, WaitKind, WaitState,
    artifact_referrer_ended, artifact_store_plugin_error, lifetime, load_process_execution_env,
    mint_process_id, process_child_session_id, process_session_turn_id,
    publish_process_execution_env,
};
pub use model::{ConsumerHold, PinnedTriggerDelivery, SessionTurnOutcome, TriggerDeliveryPin};
pub use observation::{
    ObservedProcess, ObservedProcessEvent, ObservedProcessEventLite, ObservedProcessEventPage,
    ObservedProcessEventReadOutcome, ObservedWorkItem, ObservedWorkItemState, ProcessWorkObserver,
    ProcessWorkSnapshot,
};
pub use observer_intent::{
    SessionObserverIntentSource, reconcile_session_process_observer_intents,
};
pub use op_scope::ProcessOpScope;
pub use parent_end::{
    ParentEndApplication, apply_parent_end_plan, end_parent_scope, end_session_runs,
    parent_end_delivery_key, parent_end_requester,
};
pub use references::ProcessLiveReferenceView;
#[cfg(any(test, feature = "testing"))]
pub use registry::reconcile_pruned_trigger_deliveries_interleaved;
#[cfg(any(test, feature = "testing"))]
pub use registry::{
    ConformanceProcessRegistry, ProcessEventLogTestSupport, ProcessRegistryTestSupport,
};
pub use registry::{
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NonTerminalProcessPage,
    ParentEndPlan, ProcessClockRebind, ProcessContinuationStore, ProcessEventLog, ProcessLifecycle,
    ProcessObserverRegistry, ProcessPruneReport, ProcessQuery, ProcessRegistrar,
    ProcessRegistrationProbe, ProcessRegistry, ProcessRegistryBinding, ProcessRegistryCursor,
    ProcessRetention, ProcessScopeFenceHosts, ProcessSegmentKey, ProcessTerminalPublication,
    ProcessToolIntents, ProcessWakeOutbox, ProjectionWatermark, SegmentHandoverCommit,
    SegmentStartMarker, WAKE_ENQUEUING_STALE_AFTER_MS, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryLifecycle, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, reconcile_pruned_trigger_deliveries,
    release_bound_trigger_delivery_pins,
};
pub use scope_close::RegistryScopeClose;
pub use service::{ProcessService, ProcessToolVisibilityFilter, UnavailableProcessService};
pub use start_staging::{
    ArtifactReferrerPorts, HostStartAdmission, ProcessStartStores, ReferrerAcquisition,
    RegisteredProcessStart, SessionTurnAdmission, register_process_start,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::*;
pub use validation::{
    ProcessEventAppendPlan, ProcessRegistrationRefusal, ProcessStartPlan, ProcessTransition,
    ProcessTransitionPlan, TriggerDeliveryBinding, abandoned_consumer_refusal,
    allocate_process_event_sequence, apply_process_event_projection, check_retained_start,
    check_trigger_delivery_start, fold_process_record, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_park_transitions, require_event_replay, validate_generic_process_event_append,
};

pub fn current_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}
pub use wake::{
    ProcessWakeDeliveryRequest, process_wake_delivery, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text,
};
pub use worker_engine::{WorkerCommand, WorkerProcessEngine};
