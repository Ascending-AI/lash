pub use lash_core_store::turn_input_vocabulary::*;
use lash_protocol_types::TurnId;
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
#[cfg(any(test, feature = "testing"))]
pub use deployment_store_decorator::DeploymentOp;
pub use deployment_store_decorator::DeploymentStoreDecorator;
use lash_execution_contracts::deployment_decorator as deployment_store_decorator;
pub use turn_lane::{head_input, head_input_run, input_run};
mod vocabulary;
pub use vocabulary::*;

/// The trace handle a host config carries ([`RuntimeHostConfig::tracing`]).
pub use crate::trace::{TraceEmitter, TraceRuntime};

pub use causal::tool_retry_sleep_invocation;
pub use causal::{CommandReplayKey, command_invocation};
pub use clock::{Clock, ClockWallTime, SystemClock};
/// Runtime effect contracts, including local process execution capabilities.
pub use effect::AdmittedDirectSend;
pub use effect::AdmittedScope;
pub use effect::AssistantResponseHookEvents;
pub use effect::AssistantResponsePlan;
pub use effect::AssistantStreamCapsule;
pub use effect::AssistantStreamHookState;
pub use effect::AwaitEventKey;
pub use effect::AwaitEventWaitIdentity;
pub use effect::CanonicalRuntimeEffectEnvelope;
pub use effect::CausalRef;
pub use effect::CheckpointAdmittedSet;
pub use effect::CommandJournalGuard;
pub use effect::EffectAddress;
pub use effect::EffectJournalIdentity;
pub use effect::EffectJournalRetirement;
pub use effect::EffectOpener;
pub use effect::EffectRetirementGate;
pub use effect::ExecutionScope;
pub use effect::ExternalCompletionError;
pub use effect::JournalReplay;
pub use effect::LlmRequestSpec;
pub use effect::LlmStreamRecord;
pub use effect::PresentationBinding;
pub use effect::ProcessCommand;
pub use effect::ProcessDriveStep;
pub use effect::ProcessEffectOutcome;
pub use effect::ProcessListSelection;
pub use effect::ProcessLocalExecution;
pub use effect::ProcessOutcomeObserver;
pub use effect::ProcessTurnCancellation;
pub use effect::RecordedKeyFence;
pub use effect::RecordedKeyRange;
pub use effect::RecordedKeys;
pub use effect::RefusedWriteRange;
pub use effect::Resolution;
pub use effect::ResolveOutcome;
pub use effect::RunAggregateWakePolicy;
pub use effect::RuntimeAssistantResponseHooksOutcome;
pub use effect::RuntimeAttribution;
pub use effect::RuntimeDirectLlmOutcome;
pub use effect::RuntimeEffectCommand;
pub use effect::RuntimeEffectControllerError;
pub use effect::RuntimeEffectEnvelope;
pub use effect::RuntimeEffectInvocation;
pub use effect::RuntimeEffectKind;
pub use effect::RuntimeEffectLocalExecutor;
pub use effect::RuntimeEffectOutcome;
pub use effect::RuntimeEffectReplayMismatchReport;
pub use effect::RuntimeEffectReplayTrace;
pub use effect::RuntimeInvocation;
pub use effect::RuntimeLlmCallOutcome;
pub use effect::RuntimeReplay;
pub use effect::RuntimeReplayAttribution;
pub use effect::RuntimeSleepOptions;
pub use effect::RuntimeSubject;
pub use effect::SegmentProgress;
pub use effect::ServedOnly;
pub use effect::ServedOnlyRange;
pub use effect::SleepSpec;
pub use effect::ToolAttemptEffectOutcome;
pub use effect::ToolAttemptLaunch;
pub use effect::TurnCancelWait;
pub use effect::TurnPrelude;

pub use effect::validate_replayed_effect_envelope;
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    DataRetentionConfig, DeltaCoalescing, DeltaCoalescingError, EmbeddedRuntimeHost,
    ProcessRuntimeHost, RuntimeControlConfig, RuntimeDataLimits, RuntimeDurabilityConfig,
    RuntimeHostConfig, RuntimeProviderConfig,
};
pub use process::ProcessChangeSubscription;
pub use process::env_policy;

pub use lash_core_store::process_env::ProcessComponentConfig;
pub use lash_core_store::process_env::ProcessComponentEntry;
pub use lash_core_store::process_env::ProcessComponentId;
pub use lash_core_store::process_env::ProcessComponentIdError;
pub use lash_core_store::process_env::ProcessConfigKey;
pub use lash_core_store::process_env::ProcessEnv;
pub use lash_core_store::process_env::ProcessEnvBuilder;
pub use lash_core_store::process_env::ProcessEnvRefusal;
pub use lash_core_store::process_env::ProcessLimits;
pub use lash_core_store::process_env::ProcessModel;
pub use lash_core_store::process_identity::InvalidStartKey;
pub use lash_core_store::process_identity::ProcessEnvRef;
pub use lash_core_store::process_identity::ProcessIdMint;
pub use lash_protocol_types::InvalidProcessDefinitionId;
pub use lash_protocol_types::ProcessDefinitionId;
pub use lash_protocol_types::ProcessId;
pub use lash_protocol_types::handle::HandleId;
pub use process::EnclosingProcess;
pub use process::HoldEnvError;
pub use process::ProcessChangeHub;
pub use process::ProcessEngineRegistration;
pub use process::ProcessEngineRegistry;
pub use process::ProcessEnvChoice;
pub use process::ProcessEnvCx;
pub use process::ProcessEnvPolicy;
pub use process::ProcessEnvPolicyError;
pub use process::ProcessEnvPorts;
pub use process::ProcessEnvSelection;
pub use process::ProcessEventSink;
pub use process::ProcessOpScope;
pub use process::ProcessService;
pub use process::ProcessStartChoice;

pub use process::ProcessToolVisibilityFilter;
pub use process::ResolvedInlineStart;
pub use process::ResolvedStartTarget;

pub use lash_core_store::process_identity::StartKey;
pub use lash_core_store::scope_identity::SCOPE_STORAGE_PAYLOAD_VERSION;
pub use lash_core_store::scope_identity::ScopeId;
pub use lash_core_store::scope_identity::ScopeStorageError;
pub use lash_protocol_types::SessionId;
pub use process::SessionObserverIntentSource;

pub use process::WatchedRegistry;
pub use process::WeakProcessEngineRegistry;
pub use process::current_epoch_ms;
pub use process::hold_process_env;

pub use lash_core_store::process_identity::mint_process_id;
pub use process::reconcile_session_process_observer_intents;
pub use process::watch_process_registry;

#[cfg(any(test, feature = "testing"))]
pub use process::ProcessRegistryFaults;
pub use process::{
    ArtifactReferrerPorts, HostStartAdmission, PreparedProcessStart, ProcessStartStores,
    ReferrerAcquisition, RegisteredProcessStart, SessionTurnAdmission, StagedProcessStart,
    StartPhaseClock, StartStaging, is_start_operation, register_process_start, stage_process_start,
    stage_store_local_start, start_operation_journal,
};

pub use process::request_process_successor;

pub use lash_core_store::session_state::RuntimeCheckpointComponents;
pub use lash_core_store::session_state::RuntimeSessionAuthority;
pub use lash_core_store::session_state::RuntimeSessionState;
pub use queued_drain_policy::default_queued_drain_policy;
pub(crate) use queued_drain_policy::shared_drain_mode_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainFamily, QueuedDrainPolicy,
    QueuedDrainRequest, QueuedDrainSelection,
};
pub use session_catalog::*;
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
    ExecutableGeneration, ExecutableGenerationRefusal, OwnerRevisionConflict, RecordedRefusal,
    RuntimeError, RuntimeErrorCause, RuntimeErrorCode, SessionStateVersionRefusal,
    StoredDataCorruption, TurnFailureCause,
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
