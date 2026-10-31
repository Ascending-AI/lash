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
    SESSION_COMMAND_APPLYING_PHASE, SESSION_COMMAND_COMMITTED_PHASE, SESSION_COMMAND_STAGED_PHASE,
};
mod compaction_base;
mod decoded_outcome;
mod owned_call;
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
/// The model start policy a host states (FIG-6034 §3.6) and its explicit
/// helpers.
pub use lash_core_execution::runtime::env_policy;
pub(crate) use lash_core_ids::clock;
#[cfg(feature = "testing")]
pub mod commit_admission;
#[cfg(not(feature = "testing"))]
mod commit_admission;
pub use commit_admission::run_head_advancing_commit_attempt;
mod config_ops;
mod config_transaction;
pub use config_transaction::ConfigTransactionSubmitError;
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
use lash_core_execution::runtime::host;
#[cfg(feature = "testing")]
pub use lash_core_store::input_normalization as io;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::input_normalization as io;
pub mod artifact_cleanup;
pub mod durable;
mod durable_queue;
mod lifecycle;
#[cfg(feature = "testing")]
pub mod logical_turn;
#[cfg(not(feature = "testing"))]
mod logical_turn;
pub mod obligations;
mod observation;
mod turn_config;
use lash_core_execution::runtime::process;
#[cfg(test)]
mod plugin_namespace_tests;
use lash_core_store::queued_drain_policy;
mod plugin_transition;
mod process_runtime;

pub mod scenario_contracts;
mod session_administration;
mod session_api;
mod session_carry;
pub mod session_delete;
use lash_core_store::session_catalog;
pub use session_administration::{SessionAdministration, SessionDeleteContext};
pub use session_catalog::*;
#[cfg(feature = "testing")]
pub mod session_manager;
#[cfg(not(feature = "testing"))]
mod session_manager;
#[doc(hidden)]
pub use lash_core_execution::runtime::process::ProcessStepTools;
pub use process_runtime::{EngineProcessRuntime, ProcessRuntimePorts, SessionTurnRuntime};
#[doc(hidden)]
pub use session_manager::RuntimeSessionServices;
mod session_environment;
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

use tokio_util::sync::CancellationToken;

use crate::CheckpointKind;
use crate::PersistentRuntimeServices;
use crate::PluginOperationInvokeError;
use crate::RuntimeServices;
use crate::Session;
use crate::SessionError;
use crate::SessionSnapshot;
use crate::TurnOutcome;
use crate::TurnStop;
use crate::llm::types::{
    LlmOutputPart, LlmProviderTraceDirection, LlmProviderTraceEvent, LlmProviderTraceSender,
    LlmRequest, LlmResponse, LlmStreamEvent, LlmUsage, StreamBlockIdentity,
};
use crate::plugin::{CheckpointHookContext, SessionConfigChangedContext, SessionRelation};
use crate::sansio::Response;
use crate::session_model::{
    Message, MessageRole, Part, RuntimeSessionPolicy, SessionPolicy, SessionStreamEvent,
    make_error_event, shared_parts,
};
use crate::{Effect, TurnMachine};
use lash_execution_contracts::session_types::SessionCreateRequest;
use lash_execution_contracts::session_types::SessionHandle;
use lash_protocol_types::LlmCallError;

use host::*;
use session_manager::*;
use turn_boundary::*;
use turn_commit_draft::*;
use turn_driver::*;

use assembly::{LlmStreamDebugState, LlmStreamState, fold_llm_stream_event};

/// The context for one step of a command run under `admitted`: the run's
/// own when it already serves that scope, else a rescope of it.
pub(crate) fn step_controller(
    controller: &ActorContext,
    admitted: crate::AdmittedScope,
) -> Result<ActorContext, RuntimeError> {
    if controller.execution_scope() == admitted.scope() {
        return Ok(controller.clone());
    }
    controller.rescope(admitted)
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn response_synthesized_from_aborted_stream(
    events: &[crate::llm::types::LlmStreamEvent],
) -> crate::llm::types::LlmResponse {
    use crate::llm::types::LlmUsage;
    use lash_core_llm::stream_assembly::LlmStreamAccumulator;

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
    response.parts =
        accumulator.response_parts(std::mem::take(&mut response.parts), "test-request");
    response
}
pub use builder::EmbeddedRuntimeBuilder;

pub use causal::{CommandReplayKey, command_invocation};
pub use clock::{Clock, ClockWallTime, SystemClock};
pub use durable_queue::{
    DurableSessionOps, EMPTY_HEAD_REVISION, HostInputAdmission, QueueWithdrawalObservation,
};
pub use effect::TurnCancelWait;
/// Presentation and attempt-stream vocabulary the effect
/// contracts below name.
pub use effect::{
    ATTEMPT_STREAM_BYTE_BUDGET, AttemptStream, AttemptStreamBuilder, AttemptStreamChannel,
    AttemptStreamEvent, AttemptStreamRecorder, AttemptStreamTruncation, CompactionBase,
    DecodedStreamEvent, PresentationBinding, ProcessDefinitionLocalExecution, ToolPresentation,
};
/// Runtime effect contracts, including local process execution capabilities.
pub use effect::{
    AdmittedDirectSend, AdmittedScope, AssistantResponseHookEvents, AssistantResponsePlan,
    AssistantStreamCapsule, AssistantStreamHookState, AwaitEventKey, AwaitEventWaitIdentity,
    CanonicalRuntimeEffectEnvelope, CausalRef, CheckpointAdmittedSet, CommandJournalGuard,
    EffectAddress, EffectJournalIdentity, EffectJournalRetirement, EffectOpener,
    EffectRetirementGate, ExecutionScope, ExternalCompletionError, JournalReplay, LlmRequestSpec,
    LlmStreamRecord, ProcessCommand, ProcessDriveStep, ProcessEffectOutcome, ProcessListSelection,
    ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation, RecordedKeyFence,
    RecordedKeyRange, RecordedKeys, RefusedWriteRange, Resolution, ResolveOutcome,
    RunAggregateWakePolicy, RuntimeAssistantResponseHooksOutcome, RuntimeAttribution,
    RuntimeDirectLlmOutcome, RuntimeEffectCommand, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    RuntimeInvocation, RuntimeLlmCallOutcome, RuntimeReplay, RuntimeReplayAttribution,
    RuntimeSleepOptions, RuntimeSubject, SegmentProgress, ServedOnly, ServedOnlyRange, SleepSpec,
    ToolAttemptEffectOutcome, ToolAttemptLaunch, TurnPrelude, validate_replayed_effect_envelope,
};
pub use environment::{ParkRefused, ParkedSession, RuntimeEnvironment, RuntimeEnvironmentBuilder};
pub(crate) use error::runtime_error_from_store_commit;
use error::session_commit_error;
pub use error::{
    ExecutableGeneration, ExecutableGenerationRefusal, OwnerRevisionConflict, RecordedRefusal,
    RuntimeError, RuntimeErrorCause, RuntimeErrorCode, SessionStateVersionRefusal,
    StoredDataCorruption, TurnFailureCause,
};
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    DataRetentionConfig, DeltaCoalescing, DeltaCoalescingError, EmbeddedRuntimeHost,
    ProcessRuntimeHost, RuntimeControlConfig, RuntimeDataLimits, RuntimeDurabilityConfig,
    RuntimeHostConfig, RuntimeProviderConfig,
};
use io::normalize_input_items;
pub use lash_core_execution::runtime::DirectCompletionClient;
pub use lash_core_execution::runtime::EffectOpenerError;
pub use lash_core_execution::runtime::work::{
    CommitAdmissionPolicy, CommitAdmissionPolicyError, DurableProcessWork, NoProcessWork,
    PollPacing, ProcessRegistryAwaiter, ProcessTerminalWait, ProcessWorkSubstrate,
    ProcessWorkWiring, RuntimePacingPolicy, WorkCadenceError, WorkCadencePolicy,
};
/// The trace handle a host config carries.
pub use lash_core_execution::runtime::{TraceEmitter, TraceRuntime};
pub use lash_kernel_value::observation::RunObservation;
pub use observation::InMemoryProcessReplayStore;
pub use observation::InMemoryProcessReplayStoreConfig;
pub use observation::LanguageExecutionObservation;
pub use observation::ParsedProcessObservationCursor;
pub use observation::ProcessDocumentIdentity;
pub use observation::ProcessEffectCoverage;
pub use observation::ProcessEffectEvidence;
pub use observation::ProcessEffectGapReason;
pub use observation::ProcessObservation;
pub use observation::ProcessObservationCursor;
pub use observation::ProcessObservationCursorError;
pub use observation::ProcessObservationEnd;
pub use observation::ProcessObservationEvent;
pub use observation::ProcessObservationEventPayload;
pub use observation::ProcessObservationGapCause;
pub use observation::ProcessObservationIdentity;
pub use observation::ProcessObservationReplacement;
pub use observation::ProcessReadView;
pub use observation::ProcessReplayEventDraft;
pub use observation::ProcessReplayGapReason;
pub use observation::ProcessReplayOutcome;
pub use observation::ProcessReplayPublishLimits;
pub use observation::ProcessReplayStore;
pub use observation::ProcessReplayStoreError;
pub use observation::ProcessReplaySubscribeOutcome;
pub use observation::ProcessReplaySubscription;
pub use observation::ProcessSequence;
pub use observation::ProcessWorkflowFoldError;
pub use observation::ProcessWorkflowLiveCoverage;
pub use observation::ProcessWorkflowOverlay;
pub use observation::ProcessWorkflowOverlayAccumulator;
pub use observation::ProcessWorkflowRan;
pub use observation::ProcessWorkflowSite;
pub use observation::ProcessWorkflowSiteState;
pub use observation::ProcessWorkflowWaitKind;
pub use observation::RetainedProcessView;
pub use observation::StepBodyStartedObservation;
pub use observation::commits_bridge;
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

pub use process::EnclosingProcess;

pub use lash_protocol_types::handle::HandleId;
pub use process::HoldEnvError;

pub use lash_core_store::process_identity::InvalidStartKey;
pub use lash_protocol_types::InvalidProcessDefinitionId;

pub use process::ProcessChangeHub;

pub use lash_core_store::process_env::ProcessComponentConfig;
pub use lash_core_store::process_env::ProcessComponentEntry;
pub use lash_core_store::process_env::ProcessConfigKey;
pub use lash_kernel_value::providers::ProcessComponentId;
pub use lash_kernel_value::providers::ProcessComponentIdError;

pub use lash_protocol_types::ProcessDefinitionId;

pub use lash_core_store::process_env::ProcessEnv;
pub use lash_core_store::process_env::ProcessEnvBuilder;
pub use lash_core_store::process_env::ProcessEnvRefusal;
pub use lash_core_store::process_identity::ProcessEnvRef;
pub use process::ProcessEngineRegistration;
pub use process::ProcessEngineRegistry;
pub use process::ProcessEnvChoice;
pub use process::ProcessEnvCx;
pub use process::ProcessEnvPolicy;
pub use process::ProcessEnvPolicyError;
pub use process::ProcessEnvPorts;
pub use process::ProcessEnvSelection;

pub use process::ProcessEventSink;

pub use lash_core_store::process_identity::ProcessIdMint;
pub use lash_vocabulary::identity::ProcessId;

pub use lash_core_store::process_env::ProcessLimits;

pub use lash_core_store::process_env::ProcessModel;

pub use process::ProcessOpScope;

pub use process::ProcessService;

pub use process::ProcessStartChoice;

pub use process::ProcessToolVisibilityFilter;

pub use process::ResolvedInlineStart;

pub use process::ResolvedStartTarget;

pub use lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION;

pub use lash_core_store::scope_identity::ScopeId;

pub use lash_core_store::scope_identity::ScopeStorageError;
pub use lash_kernel_value::identity::SessionId;
pub use process::SessionObserverIntentSource;

pub use process::StagedProcessStart;

pub use lash_core_store::process_identity::StartKey;

pub use process::WatchedRegistry;
pub use process::WeakProcessEngineRegistry;

pub use process::current_epoch_ms;

pub use process::hold_process_env;

pub use lash_core_store::process_identity::mint_process_id;

pub use process::reconcile_session_process_observer_intents;

pub use process::watch_process_registry;

pub use process::{DeclaredStartPhase, StartCancelDecision};
pub use process::{
    HostStartAdmission, PreparedProcessStart, ProcessStartStores, RegisteredProcessStart,
    SessionTurnAdmission, StartPhaseClock, StartStaging, is_start_operation,
    register_process_start, stage_process_start, start_operation_journal,
};
pub use queued_drain_policy::default_queued_drain_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainFamily, QueuedDrainPolicy,
    QueuedDrainRequest, QueuedDrainSelection,
};
pub use scenario_contracts::{RUNTIME_SCENARIO_CONTRACTS, ScenarioContractSpec};
pub use state::{RuntimeCheckpointComponents, RuntimeSessionAuthority, RuntimeSessionState};
use state::{append_session_nodes_to_state_with_clock, open_agent_frame_in_state_with_clock};
pub use turn_control::{
    OperationCancelOutcome, QueueWithdrawalPublisher, TurnAddress, TurnAttach,
    TurnCancelAffectedInput, TurnCancelInputOutcome, TurnCancelMode, TurnCancelOutcome,
    TurnCancelReceipt, TurnCancelRequest, TurnCancelUndeliveredInputPolicy,
    TurnCancellationEvidence, TurnTerminal, TurnWorkDriver,
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
    OpenAgentFrameCommandOutcome, PluginOperationCommandOutcome, QueuedWorkAuthority,
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkBatchingConfig, QueuedWorkCompletion,
    QueuedWorkEnqueueOutcome, QueuedWorkPayload, SessionCommand, SessionCommandOutcome,
    SessionCommandReceipt, TurnLaneAdmissionPolicy,
};
use usage::nonzero_usage;

// Turn-execution vocabulary. These types and the phase-probe trait carry no
// runtime machinery, so they live one layer down in `lash-core-llm` where the
// plugin, tool-provider and tool-dispatch layers can name them without
// reaching up into the runtime. Re-exported here at their original paths.
pub use lash_core_llm::turn_vocabulary::{TurnExecutionMetrics, TurnIssue, TurnIssueSeverity};

pub use lash_core_execution::runtime::CodeOutputRecord;

pub use lash_core_execution::runtime::DeploymentStoreDecorator;
pub use lash_core_execution::runtime::EventSink;
pub use lash_core_execution::runtime::NOOP_EVENT_SINK;
pub use lash_core_execution::runtime::NOOP_TURN_ACTIVITY_SINK;
pub use lash_core_execution::runtime::NoopEventSink;
pub use lash_core_execution::runtime::NoopTurnActivitySink;
pub use lash_core_execution::runtime::ProtocolSessionExtension;
pub use lash_core_execution::runtime::ToolIntentActivityOutcome;
pub use lash_core_execution::runtime::ToolIntentRealizedRef;
pub use lash_core_execution::runtime::TurnActivity;
pub use lash_core_execution::runtime::TurnActivitySink;
pub use lash_core_execution::runtime::TurnEvent;

mod normalized_item {
    pub use lash_core_store::input_normalization::NormalizedItem;
}

// The relocated `runtime::tests` binaries name this type; the `testing` feature
// is the seam that lets them, and the non-testing public surface is unchanged.
#[cfg(feature = "testing")]
pub use normalized_item::NormalizedItem;
#[cfg(not(feature = "testing"))]
pub(crate) use normalized_item::NormalizedItem;

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
    plugin_bindings: Option<Arc<crate::plugin::ServiceBindings>>,
    environment: Option<Arc<session_environment::SessionEnvironment>>,
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
    /// The session actor's page reader, set by the durable actor every
    /// activation of this runtime under a claim (the epoch changes per
    /// claim): a stored-state check reads the head's pages through it
    /// (FIG-6418).
    page_reader: Option<crate::plugin::SessionPageReader>,
}

#[doc(hidden)]
pub use lash_core_execution::runtime::RuntimeTurnPhaseProbeSlot;
/// Explicitly unstable internal instrumentation, outside the promised API.
#[doc(hidden)]
pub use lash_core_llm::turn_vocabulary::{
    RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
};
