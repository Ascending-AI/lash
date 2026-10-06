use crate::ActorContext;
pub use lash_core_store::turn_input_vocabulary::*;
#[cfg(feature = "testing")]
pub mod assembly;
#[cfg(not(feature = "testing"))]
mod assembly;
mod builder;
mod compact_context;
pub use compact_context::COMPACT_CONTEXT_COMMITTED_PHASE;
pub use host_commands::{
    PluginTaskCancelRequest, SESSION_COMMAND_APPLYING_PHASE, SESSION_COMMAND_COMMITTED_PHASE,
    SESSION_COMMAND_STAGED_PHASE, request_plugin_task_cancel,
};
mod compaction_base;
mod compaction_prompt;
mod decoded_outcome;
pub(crate) use decoded_outcome::DecodedEffectOutcome;
pub use lash_core_execution::runtime::attachment_delivery;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::causal;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::causal;
/// A command's journal suffix, which a language runtime's replay read parses
/// to classify a recorded command.
pub use lash_core_execution::runtime::causal::CommandSubKey;
/// The operation name a process sleep's replay key and durable effect summary
/// carry, which a language runtime's process sleep records against.
pub use lash_core_execution::runtime::causal::PROCESS_SLEEP_OPERATION;
pub(crate) use lash_core_ids::clock;
#[cfg(feature = "testing")]
pub mod commit_admission;
#[cfg(not(feature = "testing"))]
mod commit_admission;
pub use commit_admission::run_head_advancing_commit_attempt;
mod config_ops;
mod config_transaction;
pub use config_transaction::ConfigTransactionSubmitError;
pub use effect::await_event_identity;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::effect;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::effect;
#[doc(hidden)]
mod environment;
mod error;
mod frame_definition_carry;
mod frame_open;
mod host_commands;
mod observation_publisher;
mod turn_settlement;
use lash_core_execution::runtime::host;
#[cfg(feature = "testing")]
pub use lash_core_store::input_normalization as io;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::input_normalization as io;
pub mod artifact_cleanup;
pub mod durable;
mod durable_queue;
mod lifecycle;
pub mod process_start;
pub mod process_terminal;
pub mod recovery_lease;
pub mod shift;
pub mod trigger_delivery;
use turn_settlement::TurnIngressSettlement;
#[cfg(feature = "testing")]
pub mod logical_turn;
#[cfg(not(feature = "testing"))]
mod logical_turn;
mod observation;
use lash_core_execution::runtime::process;
#[cfg(test)]
mod plugin_namespace_tests;
use lash_core_store::queued_drain_policy;
mod plugin_transition;
mod process_runtime;
mod realization_runtime;
#[doc(hidden)]
pub use realization_runtime::realize_tool_intents;
mod run_start;
pub mod scenario_contracts;
mod session_administration;
mod session_api;
pub mod session_close;
pub mod session_delete;
use lash_core_store::session_catalog;
pub use session_administration::{
    SessionAdministration, SessionDeleteContext, SessionDeleteExecution,
};
pub use session_catalog::*;
#[cfg(feature = "testing")]
pub mod session_manager;
#[cfg(not(feature = "testing"))]
mod session_manager;
#[doc(hidden)]
pub use process_runtime::{ProcessRuntimeContext, ProcessRuntimePorts};
#[doc(hidden)]
pub use session_manager::RuntimeSessionServices;
#[cfg(any(test, feature = "testing"))]
pub use session_manager::take_spawned_child_runtimes;
mod session_ops;
use lash_core_store::session_store_factory_types;
pub use session_store_factory_types::{
    ForkSessionReceipt, ForkSessionRequest, RetainedRevision, Retention, SessionCreationHead,
    SessionStoreCreateRequest, Target,
};
#[cfg(feature = "testing")]
pub mod state;
#[cfg(not(feature = "testing"))]
pub(crate) mod state;
#[cfg(test)]
pub(crate) mod tests;
mod tool_restore;
mod tool_state_commands;
pub use tool_restore::ToolRestoreSite;
mod turn_boundary;
mod turn_commit_draft;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::turn_control;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::turn_control;
mod turn_driver;
mod turn_observer;
use lash_core_store::turn_failure_evidence;
use turn_observer::{Observation, TurnObserver};
mod turn_graph_editor;
pub use turn_failure_evidence::{
    ChargeSafetyRefusalEvidence, TurnFailureEvidence, TurnFailurePartialOutput,
    TurnFailureSettlement,
};
pub(crate) mod turn_input_ingress;
#[cfg(feature = "testing")]
pub mod turn_loop;
#[cfg(not(feature = "testing"))]
pub(crate) mod turn_loop;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::turn_queue;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::turn_queue;
#[cfg(feature = "testing")]
pub use lash_core_store::usage;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::usage;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::llm::types::{
    LlmOutputPart, LlmProviderTraceEvent, LlmProviderTraceSender, LlmRequest, LlmResponse,
    LlmStreamEvent, LlmUsage, StreamBlockIdentity, StreamBlockKind,
};
use crate::plugin::{CheckpointHookContext, SessionConfigChangedContext, SessionRelation};
use crate::sansio::{LlmCallError, Response};
use crate::session_model::{
    Message, MessageRole, Part, RuntimeSessionPolicy, SessionPolicy, SessionStreamEvent,
    TokenUsage, make_error_event, reassign_part_ids, shared_parts, transport_stream_events,
};
use crate::{
    CheckpointKind, PersistentRuntimeServices, PluginOperationInvokeError, RuntimeServices,
    Session, SessionCreateRequest, SessionError, SessionHandle, SessionSnapshot, TurnFinish,
    TurnOutcome, TurnStop,
};
use crate::{Effect, TurnMachine};

use crate::store::ShiftFence;
use host::*;
use session_manager::*;
use turn_boundary::*;
use turn_commit_draft::*;
use turn_driver::*;

pub use crate::store::QueuedWorkClass;
use assembly::{
    LlmDebugText, LlmDebugToolCall, LlmStreamAccumulator, LlmStreamDebugState, LlmStreamEventLog,
    LlmStreamState, ReasoningPublicationState, fold_llm_stream_event,
};

#[cfg(any(test, feature = "testing"))]
pub(crate) fn response_synthesized_from_aborted_stream(
    events: &[crate::llm::types::LlmStreamEvent],
) -> crate::llm::types::LlmResponse {
    use crate::llm::types::LlmUsage;

    let mut accumulator = LlmStreamAccumulator::default();
    let mut usage = LlmUsage::default();
    for event in events {
        fold_llm_stream_event(&mut accumulator, &mut usage, event);
    }

    let mut response = crate::llm::types::LlmResponse {
        usage,
        terminal_reason: crate::llm::types::LlmTerminalReason::Stop,
        ..crate::llm::types::LlmResponse::default()
    };
    accumulator.apply_to_response(&mut response);
    response
}
pub use builder::EmbeddedRuntimeBuilder;
pub use causal::process_event_invocation;
pub use causal::{CommandReplayKey, command_invocation};
pub use clock::{Clock, ClockWallTime, SystemClock};
pub use durable_queue::{DurableSessionOps, EMPTY_HEAD_REVISION};
pub use effect::TurnCancelWait;
/// Presentation and attempt-stream vocabulary the effect
/// contracts below name.
pub use effect::{
    ATTEMPT_STREAM_BYTE_BUDGET, AdmittedHeadVerdict, AttemptStream, AttemptStreamBuilder,
    AttemptStreamChannel, AttemptStreamEvent, AttemptStreamRecorder, AttemptStreamTruncation,
    CompactionBase, DecodedStreamEvent, PresentationBinding, ProcessDefinitionLocalExecution,
    ToolAttemptCapture, ToolPresentation,
};
/// Runtime effect contracts, including local process and trigger execution capabilities.
pub use effect::{
    AdmittedScope, AssistantResponseHookEvents, AssistantResponsePlan, AssistantStreamHookState,
    AwaitEventKey, AwaitEventWaitIdentity, BoundaryReason, CanonicalRuntimeEffectEnvelope,
    CausalRef, CheckpointAdmittedSet, CommandJournalGuard, CompletionKeyPreparation, EffectAddress,
    EffectJournalIdentity, EffectJournalRetirement, EffectOpener, EffectRetirementGate,
    ExecutionScope, ExternalCompletionError, JournalReplay, LlmRequestSpec, LlmStreamRecord,
    ProcessCommand, ProcessDriveStep, ProcessEffectOutcome, ProcessListSelection,
    ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation, RecordedKeyFence,
    RecordedKeyRange, RecordedKeys, RefusedWriteRange, Resolution, ResolveOutcome,
    RunAggregateWakePolicy, RunRecordStep, RuntimeAssistantResponseHooksOutcome,
    RuntimeAttribution, RuntimeAwaitEventOptions, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace, RuntimeInvocation,
    RuntimeLlmCallOutcome, RuntimeReplay, RuntimeReplayAttribution, RuntimeSleepOptions,
    RuntimeSubject, SegmentProgress, ServedOnly, ServedOnlyRange, SleepSpec,
    ToolAttemptEffectOutcome, ToolAttemptLaunch, TriggerLocalExecution,
    TurnCancelClosureOwnerBinding, TurnCancellationAuthority, TurnControlAttachment,
    TurnControlBinding, TurnControlBindingId, TurnControlBindingIdError, TurnPrelude,
    TurnPreludeRef, TurnPreludeStore, turn_control_binding_id_for_scope,
    validate_replayed_effect_envelope,
};
pub use environment::{ParkRefused, ParkedSession, RuntimeEnvironment, RuntimeEnvironmentBuilder};
pub(crate) use error::runtime_error_from_store_commit;
use error::session_commit_error;
pub use error::{
    ExecutableGeneration, ExecutableGenerationRefusal, RuntimeError, RuntimeErrorCause,
    RuntimeErrorCode, SessionStateVersionRefusal, StoredDataCorruption, TurnFailureCause,
};
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    DeltaCoalescing, DeltaCoalescingError, EmbeddedRuntimeHost, ProcessRuntimeHost,
    RuntimeControlConfig, RuntimeDurabilityConfig, RuntimeHostConfig, RuntimeProviderConfig,
};
use io::normalize_input_items;
pub use lash_core_execution::runtime::DirectCompletionClient;
pub use lash_core_execution::runtime::EffectOpenerError;
pub use lash_core_execution::runtime::work::{
    DurableProcessWork, DurableSessionWork, NoProcessWork, NoSessionWork, ProcessRegistryAwaiter,
    ProcessTerminalWait, ProcessWorkSubstrate, ProcessWorkWiring, SessionShifts, SessionWorkEngine,
    WakeDeliveryDriveReport, WakeDeliveryDriver, WorkCadenceError, WorkCadencePolicy,
};
/// The trace handle a host config carries.
pub use lash_core_execution::runtime::{TraceEmitter, TraceRuntime};
pub use observation::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayEventDraft, LiveReplayGap,
    LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, LiveReplaySubscription, ObservationPluginServices,
    ParsedSessionCursor, RuntimeHandle, RuntimeObservation, SessionCursor, SessionCursorError,
    SessionObservation, SessionObservationEvent, SessionObservationEventPayload,
    SessionObservationSubscription, SessionProcessEventKind, SessionQueueEventKind, SessionResume,
    SessionRevision, WeakRuntimeHandle, load_durable_observation_head,
};
pub use observation_publisher::{ObservationSource, work_with_observations};
pub use process::ProcessChangeSubscription;
#[cfg(any(test, feature = "testing"))]
pub use process::reconcile_pruned_trigger_deliveries_interleaved;
pub use process::registry_transitions;
pub use process::{
    AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry,
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, DeclaredProcessIdentity, DefinitionAcquisition, EngineAction,
    EngineEvent, EngineState, EngineStateFormat, HandleId, HostWaitKind,
    InvalidProcessDefinitionId, InvalidStartKey, KeyName, Lifetime, LifetimeDecision,
    LifetimePolicy, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NonTerminalProcessPage, ObservedProcess,
    ObservedProcessEvent, ObservedProcessEventLite, ObservedProcessEventPage,
    ObservedProcessEventReadOutcome, ObservedWorkItem, ObservedWorkItemState,
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION,
    PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ParentEndPlan, PersistedSegmentHandover,
    PreparedProcessRegistration, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessChangeHub, ProcessClockRebind, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessContinuationStore, ProcessDefinition, ProcessDefinitionDraft,
    ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionStore,
    ProcessDefinitionStoredError, ProcessDefinitionTarget, ProcessDefinitionValue,
    ProcessEffectNodeReport, ProcessEffectOccurrence, ProcessEffectOmissions,
    ProcessEffectOmittedCounts, ProcessEffectOutcomeClass, ProcessEffectReport,
    ProcessEffectReportError, ProcessEngine, ProcessEngineAdmission, ProcessEngineKind,
    ProcessEngineProcessContext, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessEvent,
    ProcessEventAppendPlan, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog, ProcessEventPage,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventRelease, ProcessEventSemantics, ProcessEventSemanticsSpec, ProcessEventSink,
    ProcessEventSinkRegistration, ProcessEventType, ProcessExecutionContext,
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleView, ProcessId,
    ProcessIdMint, ProcessIdentity, ProcessInfraError, ProcessInput, ProcessLifecycle,
    ProcessLifecycleState, ProcessLineage, ProcessListFilter, ProcessListMode,
    ProcessLiveReferenceView, ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessOutcomeNotRetained,
    ProcessProvenance, ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRegistrar,
    ProcessRegistration, ProcessRegistrationOutcome, ProcessRegistrationReceipt,
    ProcessRegistrationRefusal, ProcessRegistry, ProcessRegistryCursor, ProcessResumeRefusal,
    ProcessRetention, ProcessRunOutcome, ProcessSegmentKey, ProcessService,
    ProcessSessionDeleteReport, ProcessSignal, ProcessSignalIdentity, ProcessSignalWaitBinding,
    ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration, ProcessStartOptions,
    ProcessStartOutcome, ProcessStartPlan, ProcessStartReceipt, ProcessStartRegistration,
    ProcessStartRequest, ProcessStartTarget, ProcessStarted, ProcessStatus, ProcessStatusFilter,
    ProcessTerminal, ProcessTerminalPublication, ProcessTerminalSemantics, ProcessTerminalSpec,
    ProcessTombstone, ProcessToolIntents, ProcessToolVisibilityFilter, ProcessTransition,
    ProcessTransitionPlan, ProcessValueSelector, ProcessWake, ProcessWakeDelivery,
    ProcessWakeDeliveryRequest, ProcessWakeOutbox, ProcessWakeSpec, ProcessWorkObserver,
    ProcessWorkSnapshot, ProjectionWatermark, RegistryScopeClose, ResolvedProcessDefinition,
    RetiredProcessStatus, SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId, ScopeRef,
    ScopeStorageError, SegmentHandover, SegmentHandoverCommit, SegmentStartMarker, SessionId,
    SessionObserverIntentSource, SessionScope, SessionScopeId, StartCx, StartCxError, StartKey,
    StepName, StepRequest, StoreRealization, TerminalProcessStatus, UnavailableProcessService,
    WAKE_ENQUEUING_STALE_AFTER_MS, WaitKind, WaitState, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryLifecycle, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, WakeId, WatchedRegistry, WeakProcessEngineRegistry,
    admitted_signal_wait, allocate_process_event_sequence, apply_process_event_projection,
    artifact_store_plugin_error, check_retained_start, current_epoch_ms, fold_process_record,
    lifetime, load_process_execution_env, materialize_process_event_semantics, mint_process_id,
    prepare_process_event_append, prepare_process_registration, prepare_process_start,
    prepare_process_transition, process_child_session_id, process_park_transitions,
    process_session_turn_id, process_signal_event_type, process_signal_name_from_event_type,
    process_signal_wait_key, process_wake_delivery, process_wake_input_from_event_payload,
    process_wake_turn_cause, process_wake_turn_text, publish_process_execution_env,
    reconcile_pruned_trigger_deliveries, reconcile_session_process_observer_intents,
    release_bound_trigger_delivery_pins, require_event_replay, terminal_append_request,
    terminal_event_type_name, tool_failure_code, validate_generic_process_event_append,
    validate_process_signal_name, watch_process_registry, watch_process_registry_with_sink,
};
#[cfg(any(test, feature = "testing"))]
pub use process::{
    ConformanceProcessRegistry, EffectSummaryAppendFaults, PROCESS_REFUSAL_FIXTURE_START_KEY,
    ProcessEventLogTestSupport, ProcessRegistryTestSupport, TestProcessRegistryWriteExt,
    accepted_process_registration, fail_parent_end_once, refused_process_registrations,
};
pub use process::{ConsumerHold, PinnedTriggerDelivery, SessionTurnOutcome, TriggerDeliveryPin};
pub use process::{DeclaredStartPhase, StartCancelDecision};
pub use process::{
    HostStartAdmission, ProcessStartStores, RegisteredProcessStart, SessionTurnAdmission,
    register_process_start,
};
pub use queued_drain_policy::default_queued_drain_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainFamily, QueuedDrainPolicy,
    QueuedDrainRequest, QueuedDrainSelection,
};
pub use scenario_contracts::{RUNTIME_SCENARIO_CONTRACTS, ScenarioContractSpec};
pub use state::{RuntimeCheckpointComponents, RuntimeSessionState};
use state::{append_session_nodes_to_state_with_clock, open_agent_frame_in_state_with_clock};
#[cfg(feature = "testing")]
pub use turn_boundary::{RecordedTurnAssembly, classify_output_state};
pub use turn_control::{
    LocalTurnStop, StopDeliveryGuard, TurnAddress, TurnAttach, TurnCancelAffectedInput,
    TurnCancelAffectedWake, TurnCancelClosureAuthorization, TurnCancelClosureAuthorizationOutcome,
    TurnCancelClosureProposal, TurnCancelClosureSettlement, TurnCancelGatePair,
    TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode, TurnCancelOutcome,
    TurnCancelReceipt, TurnCancelRequest, TurnCancelRequestRecord,
    TurnCancelUndeliveredInputPolicy, TurnCancellationEvidence, TurnTerminal, TurnWorkDriver,
    retry_cancel_watch,
};
#[cfg(feature = "testing")]
pub use turn_input_ingress::ingress_message_id;
#[cfg(not(feature = "testing"))]
pub use turn_input_ingress::ingress_message_id;
pub use turn_input_ingress::{
    AdmittedTurnInputs, PendingTurnInput, PendingTurnInputBatch, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget, PendingTurnInputDraft,
    PendingTurnInputRead, PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome,
    QueuedCheckpointTurnInput, TurnInputAcceptanceReceipt, TurnInputAdmissionMode,
    TurnInputApplication, TurnInputCheckpointBoundary, TurnInputCompletion,
    TurnInputCompletionData, TurnInputIngress, TurnInputState, TurnInputStateKind,
};
pub use turn_queue::SessionCommandSettlement;
pub(crate) use turn_queue::SessionCommandSettlementHandle;
pub use turn_queue::{
    AdmissionBoundary, AdmittedQueuedWork, CompactContextOutcome, DeliveryPolicy,
    OpenAgentFrameCommandOutcome, PROCESS_WAKE_MERGE_KEY, PluginOperationCommandOutcome,
    ProcessWakeSource, QueuedCheckpointWork, QueuedWorkAuthority, QueuedWorkBatch,
    QueuedWorkBatchDraft, QueuedWorkBatchingConfig, QueuedWorkCompletion, QueuedWorkEnqueueOutcome,
    QueuedWorkKind, QueuedWorkPayload, SessionCommand, SessionCommandOutcome,
    SessionCommandReceipt, TurnLaneAdmissionPolicy, process_wake_batch_draft,
    process_wake_batch_draft_with_delivery_policy, process_wake_source_key,
};
use usage::nonzero_usage;

// Turn-execution vocabulary. These types and the phase-probe trait carry no
// runtime machinery, so they live one layer down in `lash-core-llm` where the
// plugin, tool-provider and tool-dispatch layers can name them without
// reaching up into the runtime. Re-exported here at their original paths.
pub use lash_core_llm::turn_vocabulary::{
    AssistantOutput, OutputState, TurnExecutionMetrics, TurnIssue, TurnIssueSeverity,
};

pub use lash_core_execution::runtime::{
    AgentFrameRun, AssembledTurn, CodeOutputRecord, DeploymentStore, DeploymentStoreDecorator,
    EventSink, NOOP_EVENT_SINK, NOOP_TURN_ACTIVITY_SINK, NoopEventSink, NoopTurnActivitySink,
    ProtocolSessionExtension, TerminationPolicy, TurnActivity, TurnActivitySink, TurnEvent,
    admit_session_state_generation, admit_session_view, live_session_view,
    park_turn_refused_by_generation, session_is_live,
};

mod normalized_item {
    pub use lash_core_store::input_normalization::NormalizedItem;
}

// The relocated `runtime::tests` binaries name this type; the `testing` feature
// is the seam that lets them, and the non-testing public surface is unchanged.
#[cfg(feature = "testing")]
pub use normalized_item::NormalizedItem;
#[cfg(not(feature = "testing"))]
pub(crate) use normalized_item::NormalizedItem;

/// Optional sinks and scoped effect controller for a turn the kernel executes in
/// process: a child session's turn, and a test's turn on the engine's calls
/// (`testing::TestTurnExecution`).
///
/// Event sinks default to no-op sinks.
/// Execution scope is explicit and required at every runtime boundary that can execute
/// nondeterministic work.
mod queued_options;
pub use queued_options::{QueuedEffectSource, QueuedTurnOptions};

pub struct TurnOptions<'a> {
    events: Option<&'a dyn EventSink>,
    turn_events: Option<&'a dyn TurnActivitySink>,
    scoped_effect_controller: ActorContext,
    local_stop: LocalTurnStop,
}

impl<'a> TurnOptions<'a> {
    /// `cancel` is a host-local stop lever for the turn: firing it asks the
    /// turn to stop now, delivered as a durable request on the turn's gate
    /// (see [`LocalTurnStop`]). The shift itself never reads it.
    pub fn new(cancel: CancellationToken, scoped_effect_controller: ActorContext) -> Self {
        Self {
            events: None,
            turn_events: None,
            scoped_effect_controller,
            local_stop: LocalTurnStop::from_token(cancel, None),
        }
    }

    pub fn with_events(mut self, events: &'a dyn EventSink) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_turn_events(mut self, turn_events: &'a dyn TurnActivitySink) -> Self {
        self.turn_events = Some(turn_events);
        self
    }

    /// Replaces the host-local stop lever with `stop`, which also carries the
    /// origin a forwarded stop records and its `AfterStep` request.
    pub fn with_local_stop(mut self, stop: LocalTurnStop) -> Self {
        self.local_stop = stop;
        self
    }

    pub(crate) fn local_stop(&self) -> &LocalTurnStop {
        &self.local_stop
    }

    pub(crate) fn events_or_noop(&self) -> &'a dyn EventSink {
        self.events.unwrap_or(&NOOP_EVENT_SINK)
    }

    pub(crate) fn turn_events_or_noop(&self) -> &'a dyn TurnActivitySink {
        self.turn_events.unwrap_or(&NOOP_TURN_ACTIVITY_SINK)
    }

    pub(crate) fn execution_scope_id(&self) -> &str {
        self.scoped_effect_controller.scope_id()
    }

    pub(crate) fn scoped_effect_controller(&self) -> ActorContext {
        self.scoped_effect_controller.clone()
    }
}

enum RuntimeStreamEvent {
    Session(SessionStreamEvent),
    Turn(TurnActivity),
}

pub(in crate::runtime) use turn_loop::ResidentSessionContinuity;
#[cfg(feature = "testing")]
pub use turn_loop::ResidentSessionState;
#[cfg(not(feature = "testing"))]
pub(crate) use turn_loop::ResidentSessionState;

/// Runtime session orchestration over host-supplied services and policy.
pub struct LashRuntime {
    pub session: Option<Session>,
    pub host: RuntimeHost,
    pub services: RuntimeServices,
    /// The resident runtime state. Private so every write either mutates it
    /// in place inside the runtime or goes through
    /// [`Self::install_resident_state`] / [`Self::install_resolved_run`],
    /// which publish the resident authority to the live plugin session
    /// (FIG-4024). Read it through [`Self::state`].
    state: RuntimeSessionState,
    pub runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub runtime_lease_executor_id: String,
    pub process_sync_needed: Arc<AtomicBool>,
    pub turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    /// How far this handle's resident session has travelled with the durable
    /// one: validity of live plugin/protocol state, whether this handle loaded
    /// the graph itself, cross-process staleness, and the lease and turn its
    /// last commit ran under. Its reload and invalidation rules are methods on
    /// [`ResidentSessionContinuity`].
    pub resident_session: ResidentSessionContinuity,
    /// The report of the latest persisted-tool-state install on this runtime
    /// that no turn has reported yet: the run transition that built its
    /// capabilities, or a later host restore, persisted-state install or
    /// resident re-sync. The next turn takes it and reports it as
    /// `TurnEvent::ToolRestoreReported` (FIG-3367, FIG-5134).
    pub tool_restore_report: Option<crate::ToolRestoreReport>,
    /// Whether the running direct turn replays the journaled initial shift
    /// set (ADR 0069 §6). A superseded one cedes the turn at commit under
    /// any generation: if its rows were reclaimed while the turn was down,
    /// another shift answered them, so committing would answer them twice.
    /// Set while an engine executions one admitted run as an attempt of its own
    /// ([`execute_admitted_run`](crate::shift::execute_admitted_run)): the engine retries
    /// that attempt on a live fault, under the same run (FIG-3897). The
    /// attempt's guard lowers it when the attempt returns or the engine
    /// drops it, discarding a dropped attempt's residue (FIG-3984).
    pub(crate) engine_retries_run: bool,
    /// The turn index the running direct turn's admission recorded
    /// (FIG-3682). The accept phase sets it after it adopted the head the
    /// turn was admitted on; the prepare phase takes it, so the admitted
    /// physical turn is addressed under the recorded index and never re-reads
    /// the head a replay's live store may have moved past.
    pub(crate) admitted_turn_index: Option<usize>,
    /// The admitted run this runtime is running, with the fence its seal
    /// raised (FIG-3600 S7): its commits present the fence, and the commit of
    /// its final physical turn writes its terminal evidence.
    pub(crate) shift_run: Option<Box<crate::runtime::shift::RunExecution>>,
}

#[doc(hidden)]
pub use lash_core_execution::runtime::RuntimeTurnPhaseProbeSlot;
/// Explicitly unstable internal instrumentation, outside the promised API.
#[doc(hidden)]
pub use lash_core_llm::turn_vocabulary::{
    RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
};
