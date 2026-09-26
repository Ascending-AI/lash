// FIG-2801 / FIG-2990: `lash::process` is the embedder-facing half of the
// processes-are-values surface, and nothing in the workspace consumes it by
// this path. A `pub use` that is re-homed to another facade module, narrowed
// behind a feature, or dropped therefore leaves `cargo check --workspace`
// green while every out-of-tree embedder breaks. Naming each export here makes
// the move a compile error in the seal job, which is the only place the facade
// is compiled as a dependent would see it.
//
// The imports are the assertion: an unresolved path fails this fixture. They
// are deliberately unused, so the lint is allowed rather than worked around
// with a hundred throwaway bindings.
#![allow(unused_imports)]

use lash::process::{
    AbandonEvidence, AbandonRequest, AbandonWriter, AdmittedProcessIdentity, ArtifactOwner,
    CausalRef, DeclaredProcessIdentity, HandleId, NativeProcessWork, ObservedProcess,
    ObservedProcessEvent, ObservedWorkItem, ObservedWorkItemState, OnParentEnd, ParentEndPlan,
    ParentScope, ProcessAdmissionDeferred, ProcessAdmissionIntake, ProcessAdmissionReport,
    ProcessArtifactCleanupAck, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessChangeHub, ProcessClockRebind, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessContinuationStore, ProcessCursor, ProcessCursorError,
    ProcessCursorReference, ProcessDefinitionRef, ProcessDefinitionRefusal,
    ProcessDefinitionResolution, ProcessDefinitionValue, ProcessDurableCompleteness,
    ProcessDurableSnapshot, ProcessEngineKind, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest, ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog,
    ProcessEventPage, ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode,
    ProcessEventReadOutcome, ProcessEventSemantics, ProcessEventSemanticsSpec, ProcessEventSink,
    ProcessEventType, ProcessEventsFrom, ProcessEventsRead, ProcessExecutionConcurrencyError,
    ProcessExecutionContext, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleView, ProcessIdentity,
    ProcessInput, ProcessLease, ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessLeases,
    ProcessLifecycle, ProcessLifecyclePolicy, ProcessListFilter, ProcessListMode,
    ProcessLiveReferenceView, ProcessObservationItem, ProcessObservationSnapshot,
    ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope, ProcessOriginator,
    ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance, ProcessPruneReport, ProcessQuery,
    ProcessRecord, ProcessRegistrar, ProcessRegistration, ProcessRegistry, ProcessRetention,
    ProcessRuntimeHost, ProcessService, ProcessSessionDeleteReport, ProcessSignature,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartRequest, ProcessStarted, ProcessStatus,
    ProcessStatusFilter, ProcessTerminalSemantics, ProcessTerminalSpec, ProcessTerminalWait,
    ProcessTombstone, ProcessToolIntents, ProcessToolVisibilityFilter, ProcessValueSelector,
    ProcessWake, ProcessWakeDelivery, ProcessWakeOutbox, ProcessWakeSpec, ProcessWorkObserver,
    ProcessWorkSnapshot, ProcessWorkSubstrate, ProcessWorkWiring, ProcessWorkerFault,
    ProcessWorklistCursor, ProcessWorklistPage, Processes, ProjectionWatermark, RecoveryContract,
    SessionProcessAdmin, SessionScope, SessionScopeId, WaitKind, WaitState, WakeDelivery,
    WakeDeliveryBlockedGroup, WakeDeliveryClaimOutcome, WakeDeliveryConfig,
    WakeDeliveryDisposition, WakeDeliveryDriveReport, WakeDeliveryDriver, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, WatchedRegistry, process_wake_source_key,
    publish_process_execution_env, watch_process_registry, watch_process_registry_with_sink,
};

fn paged_events_signature_is_public(processes: &Processes, cursor: ProcessCursor) {
    let _future = processes.events(
        ProcessEventsFrom::After(cursor),
        std::num::NonZeroUsize::new(64).unwrap(),
        ProcessEventQueryMode::Lite,
    );
}

fn main() {}
