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

use lash::persistence::{
    ModuleArtifactAstRefusal, ModuleArtifactCorruption, ModuleArtifactGeneration,
    ModuleArtifactRefusal,
};
use lash::runtime::{ProcessCommand, ProcessListSelection};

use lash::process::{
    AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry, CausalRef,
    DeclaredProcessIdentity, HandleId, HostArtifactPin, HostArtifacts, Lifetime, LifetimeDecision,
    LifetimePolicy, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NoProcessWork, NonTerminalProcessPage,
    ObservedProcess, ObservedProcessEvent, ObservedWorkItem, ObservedWorkItemState, ParentEndPlan,
    ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange, ProcessChangeCursor, ProcessChangeHub,
    ProcessChangeSubscription, ProcessClockRebind, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessDefinitionRef, ProcessDefinitionRefusal,
    ProcessDefinitionResolution, ProcessDefinitionValue, ProcessEngineKind, ProcessEvent,
    ProcessEventAppendReceipt, ProcessEventAppendRequest, ProcessEventHistoryRetention,
    ProcessEventKind, ProcessEventLite, ProcessEventLog, ProcessEventPage, ProcessEventPageEvents,
    ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventSink,
    ProcessEventsRead, ProcessExecutionContext, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleView,
    ProcessHistoryContinuation, ProcessIdentity, ProcessInput, ProcessLifecycle,
    ProcessLifecycleFact, ProcessLineage, ProcessListFilter, ProcessListMode,
    ProcessLiveReferenceView, ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance,
    ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRegistrar, ProcessRegistration,
    ProcessRegistry, ProcessRegistryCursor, ProcessRetention, ProcessRuntimeHost, ProcessService,
    ProcessSessionDeleteReport, ProcessSignature, ProcessStartOptions, ProcessStartOutcome,
    ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalWait,
    ProcessTombstone, ProcessToolIntents, ProcessToolVisibilityFilter, ProcessWorkObserver,
    ProcessWorkSnapshot, ProcessWorkSubstrate, ProcessWorkWiring, Processes, ProjectionWatermark,
    ScopeGrant, ScopeId, ScopeRef, SessionProcessAdmin, SessionScope, SessionScopeId, StartCx,
    StartCxError, WaitKind, WaitState, WatchedRegistry, lifetime, watch_process_registry,
};
// FIG-4656: the lifecycle state a record holds, the outcome a terminal state
// owns, the statuses derived from them, and the start target a request names.
use lash::process::{
    ProcessLifecycleState, ProcessOutcomeNotRetained, ProcessStartRegistration, ProcessStartTarget,
    ProcessTerminal, RetiredProcessStatus, TerminalProcessStatus,
};

fn paged_events_signature_is_public(processes: &Processes, from: ProcessHistoryContinuation) {
    let _future = processes.events(
        from,
        std::num::NonZeroUsize::new(64).unwrap(),
        ProcessEventQueryMode::Lite,
    );
}

fn process_change_subscription_signature_is_public(
    hub: &ProcessChangeHub,
    process_id: &lash::ProcessId,
) -> ProcessChangeSubscription {
    hub.subscribe(process_id)
}

fn main() {}
