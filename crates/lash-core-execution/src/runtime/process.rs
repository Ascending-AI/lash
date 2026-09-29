mod awaiter;
mod definition_ref;
mod effect_summary;
mod engine;
mod events;
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

pub use awaiter::{
    ProcessChangeHub, ProcessEventSink, ProcessEventSinkRegistration, WatchedRegistry,
    watch_process_registry, watch_process_registry_with_sink,
};
pub use definition_ref::{
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
    ProcessDefinitionValue, ProcessEngineKind, ProcessSignature,
};
pub use effect_summary::{
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION, ProcessEffectNodeSummary,
    ProcessEffectOmissions, ProcessEffectOmittedCounts, ProcessEffectOutcomeClass,
    ProcessEffectSummary, ProcessEffectSummaryError, ProcessEffectSummaryOccurrence,
    tool_failure_code,
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
    ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventSemantics,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessResumeRefusal, ProcessTerminalSemantics,
    ProcessTerminalSpec, ProcessValueSelector, ProcessWake, ProcessWakeDelivery, ProcessWakeSpec,
    process_signal_event_type, process_signal_name_from_event_type, process_signal_wait_key,
    runtime_lifecycle_event_type, terminal_append_request, terminal_event_type_name,
    validate_process_signal_name,
};
pub use materialization::materialize_process_event_semantics;
pub use model::{
    Ancestry, DeclaredProcessIdentity, HandleId, InvalidStartKey, Lifetime, LifetimeDecision,
    LifetimePolicy, ProcessCancelReceipt, ProcessChange, ProcessChangeCursor,
    ProcessCompletionOutcome, ProcessExecutionContext, ProcessExecutionEnvLoadError,
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleView, ProcessId,
    ProcessIdMint, ProcessIdentity, ProcessInput, ProcessLineage, ProcessListFilter,
    ProcessListMode, ProcessObserverBy, ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome,
    ProcessProvenance, ProcessRecord, ProcessRegistration, ProcessRegistrationOutcome,
    ProcessRegistrationReceipt, ProcessSessionDeleteReport, ProcessSpawnProvenance,
    ProcessStartDeclaration, ProcessStartOptions, ProcessStartOutcome, ProcessStartReceipt,
    ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTombstone,
    SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId, ScopeRef, ScopeStorageError, SessionId,
    SessionScope, SessionScopeId, StartCx, StartCxError, StartKey, StoreRealization, WaitKind,
    WaitState, artifact_referrer_ended, artifact_store_plugin_error, lifetime,
    load_process_execution_env, mint_process_id, process_child_session_id,
    process_runtime_session_ids, publish_process_execution_env,
};
pub use model::{ConsumerHold, SessionTurnOutcome};
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
    ParentEndApplication, apply_parent_end_plan, end_parent_scope, end_session_roots,
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
    ProcessToolIntents, ProcessWakeOutbox, ProjectionWatermark, SegmentStartMarker,
    WAKE_ENQUEUING_STALE_AFTER_MS, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryDisposition, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, reconcile_pruned_trigger_deliveries,
};
pub use scope_close::RegistryScopeClose;
pub use service::{ProcessService, ProcessToolVisibilityFilter, UnavailableProcessService};
pub use start_staging::{
    ArtifactReferrerPorts, ProcessStartStores, ReferrerAcquisition, RegisteredProcessStart,
    register_process_start,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::*;
pub use validation::{
    ProcessEventAppendPlan, ProcessRegistrationRefusal, ProcessStartPlan, ProcessTransition,
    ProcessTransitionPlan, abandoned_consumer_refusal, allocate_process_event_sequence,
    apply_process_event_projection, apply_process_status_projection, check_retained_start,
    fold_process_record, prepare_process_event_append, prepare_process_registration,
    prepare_process_start, prepare_process_transition, process_park_transitions,
    require_event_replay, validate_generic_process_event_append,
};

pub fn current_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}
pub use wake::{
    ProcessWakeDeliveryRequest, process_wake_delivery, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text,
};
