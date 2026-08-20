mod awaiter;
mod engine;
mod events;
mod materialization;
pub(crate) mod model;
#[cfg(test)]
mod model_filter_tests;
mod observation;
mod observer_intent;
mod op_scope;
mod references;
mod registry;
pub mod registry_transitions;
mod service;
#[cfg(any(test, feature = "testing"))]
mod testing;
#[cfg(test)]
mod tests;
mod time;
mod validation;
mod wake;

pub use awaiter::{
    ProcessAttach, ProcessAwaiter, ProcessChangeHub, ProcessEventSink, watch_process_registry,
    watch_process_registry_with_sink,
};
pub use engine::{
    PersistedSegmentHandover, ProcessEngine, ProcessEngineProcessContext, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext,
    ProcessEngineValidationContext, ProcessInfraError, ProcessRunOutcome, SegmentHandover,
};
pub use events::{
    AbandonEvidence, AbandonWriter, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessAwaitOutput,
    ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventSemantics, ProcessEventSemanticsSpec, ProcessEventType, ProcessTerminalSemantics,
    ProcessTerminalSpec, ProcessValueSelector, ProcessWake, ProcessWakeDelivery, ProcessWakeSpec,
    process_signal_event_type, process_signal_name_from_event_type, process_signal_wait_key,
    terminal_append_request, terminal_event_type_name, validate_process_signal_name,
};
pub use materialization::materialize_process_event_semantics;
pub use model::{
    AbandonRequest, InMemoryProcessExecutionEnvStore, ObserverInheritance,
    PROCESS_LEASE_SCHEMA_VERSION, ProcessCancelReceipt, ProcessChange, ProcessChangeCursor,
    ProcessCompletionOutcome, ProcessExecutionContext, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessHandleView, ProcessId, ProcessIdentity, ProcessInput, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessListFilter, ProcessListMode,
    ProcessObserverBy, ProcessOriginator, ProcessOutcome, ProcessProvenance, ProcessRecord,
    ProcessRegistration, ProcessSessionDeleteReport, ProcessSpawnProvenance, ProcessStartOptions,
    ProcessStartOutcome, ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter,
    ProcessTombstone, RecoveryContract, SessionId, SessionScope, SessionScopeId, WaitKind,
    WaitState, load_process_execution_env, persist_process_execution_env,
    process_runtime_session_ids,
};
pub use observation::{
    ObservedProcess, ObservedProcessEvent, ObservedWorkItem, ProcessWorkObserver,
    ProcessWorkSnapshot,
};
pub use observer_intent::{
    SessionObserverIntentSource, reconcile_session_process_observer_intents,
};
pub use op_scope::ProcessOpScope;
pub use references::ProcessLiveReferenceView;
#[cfg(any(test, feature = "testing"))]
pub(crate) use registry::reconcile_pruned_trigger_deliveries_interleaved;
pub use registry::{
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, ProcessContinuationStore, ProcessParentEndPlan,
    ProcessPruneReport, ProcessRegistry, ProcessWorklistCursor, ProcessWorklistPage,
    ProjectionWatermark, WAKE_ENQUEUING_STALE_AFTER_MS, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryReport, WakeDeliveryState,
    WakeDiscardReason, reconcile_pruned_trigger_deliveries,
};
pub use service::{ProcessService, ProcessToolVisibilityFilter, UnavailableProcessService};
#[cfg(any(test, feature = "testing"))]
pub use testing::*;
pub use time::{current_epoch_ms, epoch_ms_from_system_time, system_time_from_epoch_ms};
pub use validation::{
    ProcessEventAppendPlan, ProcessStartPlan, allocate_process_event_sequence,
    apply_process_event_projection, apply_process_status_projection, fold_process_record,
    prepare_process_event_append, prepare_process_registration, prepare_process_start,
    process_registration_fingerprint, require_event_replay, validate_generic_process_event_append,
};
pub use wake::{
    ProcessWakeDeliveryRequest, process_wake_delivery, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text,
};
