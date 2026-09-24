pub use lash_core_store::turn_input_vocabulary::*;
use lash_sansio::sync::MutexExt;
#[cfg(feature = "testing")]
pub mod assembly;
#[cfg(not(feature = "testing"))]
mod assembly;
mod builder;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::causal;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::causal;
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
pub use config_ops::{ApplyConfigPatch, SessionConfigPatch};
pub use effect::await_event_coordinator;
pub use effect::effect_replay_driver;
pub use effect::effect_replay_driver::{EffectCommitState, StoredChildArbitration};
pub use effect::promise_semantics;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::effect;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::effect;
mod claim_settlement;
#[doc(hidden)]
pub mod coalescing_scheduler;
mod environment;
mod error;
mod event_pump;
use lash_core_execution::runtime::host;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::in_memory_store;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::in_memory_store;
#[cfg(feature = "testing")]
pub use lash_core_store::input_normalization as io;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::input_normalization as io;
mod durable_queue;
mod lifecycle;
use claim_settlement::TurnClaimSettlement;
#[cfg(feature = "testing")]
pub mod logical_turn;
#[cfg(not(feature = "testing"))]
mod logical_turn;
pub(crate) mod native_substrate;
mod observation;
use lash_core_execution::runtime::process;
#[cfg(test)]
mod plugin_namespace_tests;
#[doc(hidden)]
pub mod process_permit;
use lash_core_store::queued_drain_policy;
pub use native_substrate::bounded_multiplicative_jitter;
pub(crate) use process_permit::DEFAULT_PROCESS_EXECUTION_CONCURRENCY;
pub use process_permit::{
    release_process_execution_permit_while, trigger_delivery_reconcile_scope,
};
pub mod scenario_contracts;
mod session_administration;
mod session_api;
#[cfg(feature = "testing")]
pub use lash_core_effect::session_execution_lease;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_effect::session_execution_lease;
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
pub use session_manager::RuntimeSessionServices;
#[cfg(any(test, feature = "testing"))]
pub use session_manager::append_receipt_mixed_usage_envelope_conformance;
#[cfg(any(test, feature = "testing"))]
pub use session_manager::append_usage_cancellation_exactly_once_conformance;
#[cfg(any(test, feature = "testing"))]
pub use session_manager::take_spawned_child_runtimes;
#[cfg(any(test, feature = "testing"))]
pub use session_manager::{
    PendingTokenLedgerEntry, StagedTokenLedger, record_reconciled_usage_shared,
    record_token_usage_shared, record_unreported_attempts_shared, stage_token_ledger_shared,
};
mod session_ops;
use lash_core_store::session_store_factory_types;
pub use session_store_factory_types::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, SessionStoreCreateRequest,
};
#[cfg(feature = "testing")]
pub mod state;
#[cfg(not(feature = "testing"))]
pub(crate) mod state;
#[cfg(test)]
pub(crate) mod tests;
mod tool_restore;
pub use tool_restore::ToolRestoreSite;
mod turn_boundary;
mod turn_commit_draft;
#[cfg(feature = "testing")]
pub use lash_core_execution::runtime::turn_control;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_execution::runtime::turn_control;
mod turn_driver;
use lash_core_store::turn_failure_evidence;
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
use lash_core_ids::worker_capacity;
#[cfg(feature = "testing")]
pub use lash_core_store::usage;
#[cfg(not(feature = "testing"))]
pub(crate) use lash_core_store::usage;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::llm::types::{
    LlmOutputPart, LlmProviderTraceEvent, LlmProviderTraceSender, LlmRequest, LlmResponse,
    LlmStreamEvent, LlmUsage, StreamBlockIdentity, StreamBlockKind,
};
use crate::plugin::{
    CheckpointHookContext, PrepareTurnRequest, SessionConfigChangedContext, SessionRelation,
};
use crate::sansio::{LlmCallError, Response};
use crate::session_model::{
    Message, MessageRole, Part, RuntimeSessionPolicy, SessionPolicy, SessionStreamEvent,
    TokenUsage, make_error_event, reassign_part_ids, shared_parts, transport_stream_events,
};
use crate::{
    CheckpointKind, PersistentRuntimeServices, PluginOperationInvokeError, PromptHookContext,
    RuntimeServices, Session, SessionCreateRequest, SessionError, SessionHandle, SessionSnapshot,
    TurnFinish, TurnOutcome, TurnStop,
};
use crate::{Effect, TurnMachine};

use host::*;
use session_execution_lease::*;
use session_manager::*;
use turn_boundary::*;
use turn_commit_draft::*;
use turn_driver::*;

pub use crate::store::QueuedWorkClass;
use assembly::{
    LlmDebugText, LlmDebugToolCall, LlmStreamAccumulator, LlmStreamDebugState, LlmStreamEventLog,
    LlmStreamState, LlmStreamSummary, ReasoningPublicationState, TurnAssembler,
    fold_llm_stream_event,
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
#[cfg(test)]
#[allow(unused_imports)]
use assembly::{classify_output_state, sanitize_assistant_output};
pub use builder::EmbeddedRuntimeBuilder;
pub use causal::process_event_invocation;
pub use clock::{Clock, ClockWallTime, SystemClock};
pub use durable_queue::{DurableSessionOps, EMPTY_HEAD_REVISION};
/// Runtime effect contracts, including local process and trigger execution capabilities.
pub use effect::{
    AdmittedScope, AdmittedScopeError, AssistantResponseHookEvents, AwaitEventKey,
    AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, CanonicalRuntimeEffectEnvelope,
    CausalRef, CheckpointClaimSet, ChildDrainOutcome, CompletionKeyPreparation, DrainedChild,
    EffectAddress, EffectGroupDrainBudget, EffectGroupHandle, EffectGroupMembership, EffectHost,
    EffectJournalIdentity, EffectJournalRetirement, EffectJournaling, EffectOpener,
    EffectRetirementGate, ExecutionScope, ExternalCompletionError, GroupChildBinding,
    GroupDrainReport, GroupExecutors, GroupFinalizationReport, GroupOnlyFinalization,
    GroupSettlement, GroupWakePolicy, LlmRequestSpec, LoserPolicy, NativeEffectHost,
    NativeRuntimeEffectController, OpenerFinalizationSteps, ProcessCommand, ProcessEffectOutcome,
    ProcessLocalExecution, ProcessOutcomeObserver, ProcessTurnCancellation, QueuedLaneAcquisition,
    QueuedLaneAttempt, QueuedLaneGuard, QueuedLaneHolder, QueuedLaneProbe, RankedGroupSettlement,
    Resolution, ResolveOutcome, RuntimeAssistantResponseHooksOutcome, RuntimeAttribution,
    RuntimeAwaitEventOptions, RuntimeDirectLlmOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectInvocation, RuntimeEffectKind, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport, RuntimeEffectReplayTrace,
    RuntimeInvocation, RuntimeLlmCallOutcome, RuntimeReplay, RuntimeReplayAttribution,
    RuntimeSleepOptions, RuntimeSubject, ScopeBoundController, ScopedEffectController,
    SegmentProgress, SleepSpec, StoreEffectGroupClosing, StoreEffectGroupDrain,
    ToolAttemptEffectOutcome, ToolAttemptLaunch, ToolChildDriver, ToolIntentOutcomeSink,
    ToolIntentPreparation, ToolIntentSubmissionGuard, TriggerLocalExecution,
    TurnCancelClosureOwnerBinding, TurnCancellationAuthority, TurnControlAttachment,
    TurnControlAuthorityOwner, TurnControlBinding, TurnControlBindingId, TurnControlBindingIdError,
    UnsettledEffectGroup, concrete_turn_cancellation_authority, effect_groups_unsupported,
    refuse_unhonored_group_membership, turn_control_binding_id_for_scope,
    validate_replayed_effect_envelope,
};
#[cfg(feature = "testing")]
pub use effect::{RuntimeEffectControllerHandle, TurnCancelWait};
#[cfg(not(feature = "testing"))]
pub(crate) use effect::{RuntimeEffectControllerHandle, TurnCancelWait};
pub use environment::{ParkedSession, RuntimeEnvironment, RuntimeEnvironmentBuilder};
pub(crate) use error::runtime_error_from_store_commit;
use error::session_commit_error;
pub use error::{RuntimeError, RuntimeErrorCause, RuntimeErrorCode, TurnFailureCause};
pub use event_pump::drive_with_event_pump;
/// Embedded-host configuration and its public configuration sections.
pub use host::{
    DEFAULT_ENGINE_CHILD_MAX_ATTEMPTS, EmbeddedRuntimeHost, ProcessRuntimeHost,
    RuntimeControlConfig, RuntimeDurabilityConfig, RuntimeHostConfig, RuntimePromptConfig,
    RuntimeProviderConfig, RuntimeTracingConfig,
};
#[cfg(any(test, feature = "testing"))]
pub use in_memory_store::RawSessionExecutionLeaseRow;
pub use in_memory_store::{InMemorySessionStore, InMemorySessionStoreFactory};
use io::normalize_input_items;
pub use lash_core_execution::runtime::DirectCompletionClient;
pub use lash_core_execution::runtime::EffectOpenerError;
#[cfg(any(test, feature = "testing"))]
pub use native_substrate::QUEUED_WORK_MAX_TRANSIENT_ATTEMPTS;
pub use native_substrate::{
    DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY, QueuedWorkExecutionConcurrencyError,
    QueuedWorkRunError, QueuedWorkRunErrorClass, QueuedWorkRunHandle, QueuedWorkRunProgress,
    QueuedWorkRunRequest, QueuedWorkSlowWake, QueuedWorkWakeContended, QueuedWorkWakeFailure,
    QueuedWorkWakeOutcome,
};
pub use native_substrate::{
    NativeProcessAdmissionDriver, NativeProcessWork, NativeQueuedWork, NativeQueuedWorkConfigError,
    NativeSubstrateConfig, NativeSubstrateConfigError, NoQueuedWork, ProcessTerminalWait,
    ProcessWorkSubstrate, ProcessWorkWiring, QueuedWorkSubstrate, SessionDrainOutcome,
    SessionWorkTarget, WorkCadencePolicy, WorkerSweepPolicy,
};
pub use native_substrate::{WakeDeliveryDriveReport, WakeDeliveryDriver};
pub use observation::{
    InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig, LiveReplayEventDraft, LiveReplayGap,
    LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore, LiveReplayStoreError,
    LiveReplaySubscribeOutcome, LiveReplaySubscription, ObservationPluginServices,
    PreparedLiveReplayPublication, RuntimeHandle, RuntimeObservation, SessionCursor,
    SessionCursorError, SessionObservation, SessionObservationEvent,
    SessionObservationEventPayload, SessionObservationSubscription, SessionProcessEventKind,
    SessionQueueEventKind, SessionResume, SessionRevision,
};
#[cfg(any(test, feature = "testing"))]
pub use process::reconcile_pruned_trigger_deliveries_interleaved;
pub use process::registry_transitions;
pub use process::{
    AbandonEvidence, AbandonRequest, AbandonWriter, AdmittedProcessIdentity, ArtifactOwner,
    DEFAULT_WAKE_DELIVERY_EXPIRY_MS, DeclaredProcessIdentity, HandleId,
    InMemoryProcessExecutionEnvStore, ObservedProcess, ObservedProcessEvent,
    ObservedProcessEventLite, ObservedProcessEventPage, ObservedProcessEventReadOutcome,
    ObservedWorkItem, ObservedWorkItemState, OnParentEnd, PARENT_SCOPE_STORAGE_PAYLOAD_VERSION,
    PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
    PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION,
    PROCESS_LEASE_SCHEMA_VERSION, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ParentEndPlan, ParentScope,
    ParentScopeStorageError, PersistedSegmentHandover, ProcessArtifactCleanup,
    ProcessArtifactCleanupAck, ProcessAwaitOutput, ProcessCancelReceipt, ProcessChange,
    ProcessChangeCursor, ProcessChangeHub, ProcessClockRebind, ProcessCompletionAuthority,
    ProcessCompletionOutcome, ProcessContinuationStore, ProcessDefinitionRef,
    ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionValue,
    ProcessEffectNodeSummary, ProcessEffectOmissions, ProcessEffectOmittedCounts,
    ProcessEffectOutcomeClass, ProcessEffectSummary, ProcessEffectSummaryError,
    ProcessEffectSummaryOccurrence, ProcessEngine, ProcessEngineAdmission, ProcessEngineKind,
    ProcessEngineProcessContext, ProcessEngineRegistration, ProcessEngineRegistry,
    ProcessEngineRunContext, ProcessEngineRunGuard, ProcessEngineRuntimeContext, ProcessEvent,
    ProcessEventAppendPlan, ProcessEventAppendReceipt, ProcessEventAppendRequest,
    ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog, ProcessEventPage,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventPageToken,
    ProcessEventPageTokenStoreExt, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemantics, ProcessEventSemanticsSpec, ProcessEventSink,
    ProcessEventSinkRegistration, ProcessEventType, ProcessExecutionContext,
    ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExecutionEnvStore,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleView, ProcessId,
    ProcessIdentity, ProcessIncarnation, ProcessInfraError, ProcessInput, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessLeaseSchemaVersionError,
    ProcessLeases, ProcessLifecycle, ProcessLifecyclePolicy, ProcessListFilter, ProcessListMode,
    ProcessLiveReferenceView, ProcessObserverBy, ProcessObserverRegistry, ProcessOpScope,
    ProcessOriginator, ProcessOriginatorFilter, ProcessOutcome, ProcessProvenance,
    ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRef, ProcessRegistrar,
    ProcessRegistration, ProcessRegistrationDisposition, ProcessRegistrationOutcome,
    ProcessRegistrationProbe, ProcessRegistrationRefusal, ProcessRegistry, ProcessRegistryBinding,
    ProcessRetention, ProcessRunOutcome, ProcessScopeFenceHosts, ProcessService,
    ProcessSessionDeleteReport, ProcessSignature, ProcessSpawnProvenance, ProcessStartDeclaration,
    ProcessStartOptions, ProcessStartOutcome, ProcessStartPlan, ProcessStartRequest,
    ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalSemantics,
    ProcessTerminalSpec, ProcessTombstone, ProcessToolIntents, ProcessToolVisibilityFilter,
    ProcessTransition, ProcessTransitionPlan, ProcessValueSelector, ProcessWake,
    ProcessWakeDelivery, ProcessWakeDeliveryRequest, ProcessWakeOutbox, ProcessWakeSpec,
    ProcessWorkObserver, ProcessWorkSnapshot, ProcessWorklistCursor, ProcessWorklistPage,
    ProjectionWatermark, RecoveryContract, SegmentHandover, SessionId, SessionObserverIntentSource,
    SessionScope, SessionScopeId, StoreRealization, UnavailableProcessService,
    WAKE_ENQUEUING_STALE_AFTER_MS, WaitKind, WaitState, WakeDelivery, WakeDeliveryBlockedGroup,
    WakeDeliveryClaimOutcome, WakeDeliveryConfig, WakeDeliveryDisposition, WakeDeliveryReport,
    WakeDeliveryState, WakeDiscardReason, WatchedRegistry, allocate_process_event_sequence,
    apply_process_event_projection, apply_process_status_projection,
    artifact_destination_owner_retired_error, artifact_owner_is_permanently_retired,
    artifact_owner_retired_error, artifact_staging_edge_missing_error,
    artifact_staging_owner_edge_is_missing, artifact_store_plugin_error, current_epoch_ms,
    ensure_process_lease_schema_version, fold_process_record, load_process_execution_env,
    materialize_process_event_semantics, prepare_process_event_append,
    prepare_process_registration, prepare_process_start, prepare_process_transition,
    process_registration_fingerprint, process_runtime_session_ids, process_signal_await_key,
    process_signal_event_type, process_signal_name_from_event_type, process_signal_wait_key,
    process_wake_delivery, process_wake_input_from_event_payload, process_wake_turn_cause,
    process_wake_turn_text, publish_process_execution_env, reconcile_pruned_trigger_deliveries,
    reconcile_session_process_observer_intents, require_event_replay,
    settle_started_process_engine_artifacts, settle_started_process_execution_env,
    terminal_append_request, terminal_event_type_name, tool_failure_code,
    validate_generic_process_event_append, validate_process_signal_name, watch_process_registry,
    watch_process_registry_with_sink,
};
#[cfg(any(test, feature = "testing"))]
pub use process::{
    ConformanceProcessRegistry, EffectSummaryAppendFaults, PROCESS_REFUSAL_FIXTURE_PROCESS_ID,
    ProcessEventLogTestSupport, ProcessRegistryTestSupport, TestLocalProcessRegistry,
    TestProcessRegistryWriteExt, accepted_process_registration, fail_parent_end_once,
    refused_process_registrations,
};
pub use queued_drain_policy::default_queued_drain_policy;
pub use queued_drain_policy::{
    DrainMode, DrainModePolicy, QueuedDrainCandidate, QueuedDrainPolicy, QueuedDrainRequest,
    QueuedDrainSelection,
};
pub use scenario_contracts::{RUNTIME_SCENARIO_CONTRACTS, ScenarioContractSpec};
pub use state::{RuntimeCheckpointComponents, RuntimeSessionState};
use state::{append_session_nodes_to_state_with_clock, open_agent_frame_in_state_with_clock};
pub use turn_control::{
    TurnAddress, TurnAttach, TurnCancelAffectedInput, TurnCancelClosureAuthorization,
    TurnCancelClosureAuthorizationOutcome, TurnCancelClosureProposal, TurnCancelClosureSettlement,
    TurnCancelDisposition, TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode,
    TurnCancelOriginHint, TurnCancelOutcome, TurnCancelReceipt, TurnCancelRequest,
    TurnCancelRequestRecord, TurnCancellationEvidence, TurnTerminal, TurnWorkDriver,
};
#[cfg(feature = "testing")]
pub use turn_input_ingress::ingress_message_id;
#[cfg(not(feature = "testing"))]
pub use turn_input_ingress::ingress_message_id;
pub use turn_input_ingress::{
    AcceptedTurnInputDrive, AcceptedTurnInputRefusal, PendingTurnInput,
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
    PendingTurnInputClaimDiagnostics, PendingTurnInputDraft, PendingTurnInputRead,
    PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome, QueuedCheckpointTurnInput,
    TurnInputAcceptanceReceipt, TurnInputApplication, TurnInputCheckpointBoundary, TurnInputClaim,
    TurnInputClaimData, TurnInputClaimMode, TurnInputCompletion, TurnInputCompletionData,
    TurnInputIngress, TurnInputSettlementClaim, TurnInputState, TurnInputStateKind,
};
pub use turn_loop::ensure_durable_effect_input;
#[cfg(feature = "testing")]
pub use turn_queue::SessionCommandSettlement;
#[cfg(not(feature = "testing"))]
pub(crate) use turn_queue::SessionCommandSettlement;
pub(crate) use turn_queue::SessionCommandSettlementHandle;
pub use turn_queue::{
    DeliveryPolicy, PROCESS_WAKE_MERGE_KEY, ProcessWakeSource, QueuedCheckpointWork,
    QueuedTurnWork, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkBatchPayloads, QueuedWorkBatchingConfig, QueuedWorkClaim, QueuedWorkClaimBoundary,
    QueuedWorkClaimData, QueuedWorkClaimPolicy, QueuedWorkCompletion, QueuedWorkCompletionData,
    QueuedWorkEnqueueOutcome, QueuedWorkItem, QueuedWorkKind, QueuedWorkPayload, SessionCommand,
    SessionCommandPayload, SessionCommandReceipt, TurnWorkPayload, process_wake_batch_draft,
    process_wake_batch_draft_with_delivery_policy, process_wake_source_key,
};
pub use usage::{
    LedgerUsageDisposition, ReconciledUsageAttempt, SessionUsageReport, TokenLedgerEntry,
    UnreportedLedgerAttempt, UnreportedUsageAttempt, UsageDispositionError,
    UsageReconciliationReport, UsageReportRow, UsageTotals, diff_token_ledger, diff_usage_reports,
    outstanding_unreported_attempts,
};
use usage::{merge_ledger_entry_saturating, nonzero_usage};
pub use worker_capacity::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};

// Turn-execution vocabulary. These types and the phase-probe trait carry no
// runtime machinery, so they live one layer down in `lash-core-llm` where the
// plugin, tool-provider and tool-dispatch layers can name them without
// reaching up into the runtime. Re-exported here at their original paths.
pub use lash_core_llm::turn_vocabulary::{
    AssistantOutput, OutputState, RuntimeNamedPhase, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
    TurnExecutionMetrics, TurnIssue, TurnIssueSeverity,
};

pub use lash_core_execution::runtime::{
    AgentFrameRun, AssembledTurn, CodeOutputRecord, EventSink, NOOP_EVENT_SINK,
    NOOP_TURN_ACTIVITY_SINK, NoopEventSink, NoopTurnActivitySink, ProtocolSessionExtension,
    ProtocolSessionExtensionHandle, RuntimeTurnPhaseProbeSlot, SessionStoreFactory,
    TerminationPolicy, TurnActivity, TurnActivitySink, TurnEvent,
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

/// Optional sinks and scoped effect controller passed to one of [`LashRuntime`]'s
/// turn-driving entry points (`stream_turn`,
/// `stream_turn_with_agent_frames`).
///
/// Event sinks default to no-op sinks.
/// Execution scope is explicit and required at every runtime boundary that can execute
/// nondeterministic work.
mod queued_run;
pub use queued_run::{QueuedEffectSource, QueuedTurnOptions};

pub struct TurnOptions<'a> {
    events: Option<&'a dyn EventSink>,
    turn_events: Option<&'a dyn TurnActivitySink>,
    scoped_effect_controller: ScopedEffectController<'a>,
    cancel: CancellationToken,
    local_cancel_origin: Option<TurnCancelOriginHint>,
}

impl<'a> TurnOptions<'a> {
    pub fn new(
        cancel: CancellationToken,
        scoped_effect_controller: ScopedEffectController<'a>,
    ) -> Self {
        Self {
            events: None,
            turn_events: None,
            scoped_effect_controller,
            cancel,
            local_cancel_origin: None,
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

    pub fn with_local_cancel_origin_hint(mut self, hint: TurnCancelOriginHint) -> Self {
        self.local_cancel_origin = Some(hint);
        self
    }

    pub(crate) fn local_cancel_origin_hint(&self) -> Option<TurnCancelOriginHint> {
        self.local_cancel_origin.clone()
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

    pub(crate) fn scoped_effect_controller(&self) -> ScopedEffectController<'a> {
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
    pub state: RuntimeSessionState,
    pub runtime_lease_owner: crate::LeaseOwnerIdentity,
    pub runtime_lease_executor_id: String,
    pub(crate) queued_run: Option<Box<crate::store::QueuedRunAdmission>>,
    /// Session-scoped token cost ledger. Shared by ALL
    /// `RuntimeSessionServices` instances created from this runtime
    /// (both per-turn and async maintenance). Entries accumulate here
    /// and are drained into `state.token_ledger` at turn-commit time.
    pub shared_token_ledger: Arc<std::sync::Mutex<Vec<session_manager::PendingTokenLedgerEntry>>>,
    pub process_sync_needed: Arc<AtomicBool>,
    pub turn_phase_probe: Option<Arc<dyn RuntimeTurnPhaseProbe>>,
    /// How far this handle's resident session has travelled with the durable
    /// one: validity of live plugin/protocol state, whether this handle loaded
    /// the graph itself, cross-process staleness, and the lease and turn its
    /// last commit ran under. Its reload and invalidation rules are methods on
    /// [`ResidentSessionContinuity`].
    pub resident_session: ResidentSessionContinuity,
    /// Materialization resolved protocol facts that must be durable before queued work may
    /// reconstruct this session in another runtime.
    pub materialized_protocol_config_dirty: bool,
    /// The report from the most recent persisted-tool-state install on this
    /// runtime — the open that built it, or the latest host restore, persisted
    /// state install or resident re-sync. This is how the report reaches a
    /// host on the paths that have no return value to give it (FIG-3367); the
    /// facade reads it as `LashSession::tool_restore_report()`.
    pub tool_restore_report: Option<crate::ToolRestoreReport>,
    /// Attempts whose usage never arrived after an abort or failure, not yet
    /// reconciled (FIG-2765). Runtime-resident: persisted holes live in the
    /// ledger's unreported rows; this is the attribution a later
    /// [`LashRuntime::reconcile_unreported_usage`] needs.
    pub unreported_usage_attempts: Vec<UnreportedUsageAttempt>,
    /// Claim ids of the journaled initial drive set the running direct turn
    /// replays (ADR 0069 §6). Such a claim is exempt from the
    /// recovered-settlement drop: if its rows were reclaimed while the turn was
    /// down, another driver answered them, so the turn cedes at commit instead
    /// of committing the same words without a settlement.
    pub(crate) journaled_drive_claims: std::collections::BTreeSet<String>,
}

#[cfg(any(test, feature = "testing"))]
pub use in_memory_store::in_memory_lineage_handles;
