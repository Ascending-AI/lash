mod admission;
mod awaiter;
mod definition_ref;
mod engine;
mod events;
pub(crate) mod identity_projection;
mod lease_serde;
#[cfg(test)]
mod lease_serde_tests;
mod materialization;
pub(crate) mod model;
#[cfg(test)]
mod model_filter_tests;
mod observation;
mod observer_intent;
mod op_scope;
mod references;
mod registry;
mod registry_concerns;
mod registry_delegate;
pub mod registry_transitions;
mod service;
#[cfg(any(test, feature = "testing"))]
mod testing;
#[cfg(test)]
mod tests;
mod validation;
mod wake;

pub use admission::{
    ProcessAdmissionDeferred, ProcessAdmissionIntake, ProcessAdmissionReport, ProcessDrainDeferred,
    ProcessDrainReport, ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation,
    ProcessWorkerFault,
};
pub use awaiter::{
    ProcessChangeHub, ProcessEventSink, WatchedRegistry, watch_process_registry,
    watch_process_registry_with_sink,
};
pub use definition_ref::{
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
    ProcessDefinitionValue, ProcessEngineKind, ProcessSignature,
};
pub use engine::{
    AdmittedProcessIdentity, PersistedSegmentHandover, ProcessEngine, ProcessEngineAdmission,
    ProcessEngineProcessContext, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessInfraError,
    ProcessRunOutcome, SegmentHandover, settle_started_process_engine_artifacts,
};
pub use events::{
    AbandonEvidence, AbandonWriter, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessAwaitOutput,
    ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventPage, ProcessEventPageEvents,
    ProcessEventPageMore, ProcessEventPageToken, ProcessEventPageTokenStoreExt,
    ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventSemantics,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessTerminalSemantics, ProcessTerminalSpec,
    ProcessValueSelector, ProcessWake, ProcessWakeDelivery, ProcessWakeSpec,
    process_signal_await_key, process_signal_event_type, process_signal_name_from_event_type,
    process_signal_wait_key, terminal_append_request, terminal_event_type_name,
    validate_process_signal_name,
};
pub use materialization::materialize_process_event_semantics;
pub use model::{
    AbandonRequest, ArtifactOwner, DeclaredProcessIdentity, HandleId,
    InMemoryProcessExecutionEnvStore, ObserverInheritance, OnParentEnd,
    PARENT_SCOPE_STORAGE_PAYLOAD_VERSION, PROCESS_LEASE_SCHEMA_VERSION, ParentScope,
    ParentScopeStorageError, ProcessArtifactCleanup, ProcessArtifactCleanupAck,
    ProcessCancelReceipt, ProcessChange, ProcessChangeCursor, ProcessCompletionOutcome,
    ProcessExecutionContext, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessHandleView, ProcessId, ProcessIdentity, ProcessIncarnation, ProcessInput, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessLeaseSchemaVersionError,
    ProcessLifecyclePolicy, ProcessListFilter, ProcessListMode, ProcessObserverBy,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance, ProcessRecord,
    ProcessRef, ProcessRegistration, ProcessRegistrationDisposition, ProcessRegistrationOutcome,
    ProcessSessionDeleteReport, ProcessSpawnProvenance, ProcessStartDeclaration,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartRequest, ProcessStarted, ProcessStatus,
    ProcessStatusFilter, ProcessTombstone, RecoveryContract, SessionId, SessionScope,
    SessionScopeId, StoreRealization, WaitKind, WaitState,
    artifact_destination_owner_retired_error, artifact_owner_is_permanently_retired,
    artifact_owner_retired_error, artifact_staging_edge_missing_error,
    artifact_staging_owner_edge_is_missing, artifact_store_plugin_error,
    ensure_process_lease_schema_version, load_process_execution_env, process_runtime_session_ids,
    publish_process_execution_env, settle_started_process_execution_env,
};
pub use observation::{
    ObservedProcess, ObservedProcessEvent, ObservedProcessEventLite, ObservedProcessEventPage,
    ObservedProcessEventReadOutcome, ObservedWorkItem, ObservedWorkItemState, ProcessWorkObserver,
    ProcessWorkSnapshot,
};
pub use observer_intent::{
    SessionObserverIntentSource, reconcile_session_process_observer_intents,
};
pub use op_scope::ProcessOpScope;
pub use references::ProcessLiveReferenceView;
#[cfg(any(test, feature = "testing"))]
pub use registry::reconcile_pruned_trigger_deliveries_interleaved;
#[cfg(any(test, feature = "testing"))]
pub use registry::{
    ConformanceProcessRegistry, ProcessEventLogTestSupport, ProcessRegistryTestSupport,
};
pub use registry::{
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, ParentEndPlan, ProcessClockRebind, ProcessContinuationStore,
    ProcessEventLog, ProcessLeases, ProcessLifecycle, ProcessObserverRegistry, ProcessPruneReport,
    ProcessQuery, ProcessRegistrar, ProcessRegistrationProbe, ProcessRegistry,
    ProcessRegistryBinding, ProcessRetention, ProcessScopeFenceHosts, ProcessToolIntents,
    ProcessWakeOutbox, ProcessWorklistCursor, ProcessWorklistPage, ProjectionWatermark,
    WAKE_ENQUEUING_STALE_AFTER_MS, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryDisposition, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, reconcile_pruned_trigger_deliveries,
};
pub use service::{ProcessService, ProcessToolVisibilityFilter, UnavailableProcessService};
#[cfg(any(test, feature = "testing"))]
pub use testing::*;
pub use validation::{
    ProcessEventAppendPlan, ProcessRegistrationRefusal, ProcessStartPlan, ProcessTransition,
    ProcessTransitionPlan, allocate_process_event_sequence, apply_process_event_projection,
    apply_process_status_projection, fold_process_record, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_registration_fingerprint, require_event_replay, validate_generic_process_event_append,
};

pub fn current_epoch_ms() -> u64 {
    <crate::SystemClock as crate::ClockWallTime>::timestamp_ms(&crate::SystemClock)
}
pub use wake::{
    ProcessWakeDeliveryRequest, process_wake_delivery, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text,
};
