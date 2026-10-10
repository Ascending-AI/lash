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

use lash::persistence::{ModuleArtifactCorruption, ModuleArtifactRefusal};
use lash::runtime::{ProcessCommand, ProcessListSelection};

use lash::persistence::{
    NonTerminalProcessPage, ProcessCancelReceipt, ProcessChange, ProcessClockRebind,
    ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessEventAppendRequest,
    ProcessEventLog, ProcessExecutionWriteAuthority, ProcessHandleView, ProcessLifecycle,
    ProcessListFilter, ProcessLiveReferenceView, ProcessObserverRegistry, ProcessOpScope,
    ProcessQuery, ProcessRecord, ProcessRegistrar, ProcessRegistry, ProcessRetention,
    ProcessStartOutcome, ProcessToolIntents,
};
use lash::process::{
    AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry, CausalRef,
    DeclaredProcessIdentity, HandleId, HostArtifactPin, HostArtifacts, Lifetime, LifetimeDecision,
    LifetimePolicy, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NoProcessWork, ObservedProcess,
    ObservedProcessEvent, ObservedWorkItem, ObservedWorkItemState, ParentEndPlan,
    ProcessAwaitOutput, ProcessChangeCursor, ProcessChangeHub, ProcessChangeSubscription,
    ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
    ProcessDefinitionValue, ProcessEngineKind, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventHistoryRetention, ProcessEventKind, ProcessEventLite, ProcessEventPage,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSink, ProcessEventsRead, ProcessExecutionContext, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExternalRef, ProcessHistoryContinuation, ProcessIdentity,
    ProcessInput, ProcessLifecycleFact, ProcessLineage, ProcessListMode, ProcessObserverBy,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance,
    ProcessPruneReport, ProcessRegistration, ProcessRegistryCursor, ProcessRuntimeHost,
    ProcessService, ProcessSessionDeleteReport, ProcessSignature, ProcessStartOptions,
    ProcessStartRequest, ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalWait,
    ProcessTombstone, ProcessToolVisibilityFilter, ProcessWorkObserver, ProcessWorkSnapshot,
    ProcessWorkSubstrate, ProcessWorkWiring, Processes, ProjectionWatermark, ScopeGrant, ScopeId,
    ScopeRef, SessionProcessAdmin, SessionScope, SessionScopeId, StartCx, StartCxError, WaitKind,
    WaitState, WatchedRegistry, lifetime, watch_process_registry,
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
