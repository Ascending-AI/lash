//! External authorship proof. The manifest has exactly one dependency, lash.
#![allow(unused_imports, unused_variables, dead_code)]

use lash::attachments::*;
use lash::direct::*;
use lash::durability::*;
use lash::observe::*;
use lash::persistence::queued_work::*;
use lash::persistence::*;
use lash::plugins::*;
use lash::process::*;
use lash::provider::*;
use lash::runtime::*;
use lash::tools::*;
use lash::triggers::*;
use lash::*;
use lash::{
    AwaitEventKey, AwaitEventWaitIdentity, CancellationToken, NodeId, ProcessId, Resolution,
    ResolveOutcome, SessionError, SessionId, TurnAttach, TurnId,
};
use std::collections::BTreeSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::result::Result;
use std::sync::{Arc, Weak};
use std::time::Instant;

struct Integrator;
mod storage;

#[lash::async_trait]
impl ToolProvider for Integrator {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        unreachable!("external signature witness")
    }
    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        unreachable!("external signature witness")
    }
    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        unreachable!("external signature witness")
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        unreachable!("external signature witness")
    }
    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        unreachable!("external signature witness")
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        unreachable!("external signature witness")
    }
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        unreachable!("external signature witness")
    }
    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProtocolSessionPlugin for Integrator {
    async fn initialize_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    async fn restore_session(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _state: ProtocolSessionRestoreView,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    async fn append_session_nodes(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _nodes: &[SessionAppendNode],
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    async fn apply_session_extension(
        &self,
        _extension: ProtocolSessionExtensionHandle,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    fn configure_runtime_on_materialize(
        &self,
        _ctx: ProtocolRuntimeContext<'_>,
        _materialization: ProtocolSessionMaterialization<'_>,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    fn apply_session_config_patch(
        &self,
        recorded: &ProtocolTurnOptions,
        plugin_options: &PluginOptions,
    ) -> Result<ProtocolTurnOptions, SessionError> {
        unreachable!("external signature witness")
    }
    async fn before_llm_call(
        &self,
        _ctx: ProtocolBeforeLlmCallContext,
        _request: &LlmRequest,
    ) -> Result<Option<ProtocolLlmCallAction>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn bound_variables_prompt(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<Arc<str>>, SessionError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl CodeExecutorPlugin for Integrator {
    async fn execute_code(
        &self,
        ctx: RuntimeExecutionContext<'_>,
        request: ExecRequest,
    ) -> Result<ExecResponse, SessionError> {
        unreachable!("external signature witness")
    }
    fn execution_state_dirty(&self) -> bool {
        unreachable!("external signature witness")
    }
    fn executable_generation(&self) -> Option<ExecutableGeneration> {
        unreachable!("external signature witness")
    }
    async fn snapshot_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<ExecutionStateSnapshot, SessionError> {
        unreachable!("external signature witness")
    }
    async fn frame_switch_carries(
        &self,
        ctx: ProtocolSessionContext<'_>,
        initial_nodes: &[SessionAppendNode],
    ) -> Result<Vec<ArtifactName>, SessionError> {
        unreachable!("external signature witness")
    }
    async fn probe_execution_state_capture(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    async fn hydrated_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<HydratedExecutionState>, SessionError> {
        unreachable!("external signature witness")
    }
    async fn acknowledge_execution_state_capture(&self) {
        unreachable!("external signature witness")
    }
    async fn abort_execution_state_capture(&self) {
        unreachable!("external signature witness")
    }
    async fn settle_code_execution(
        &self,
        _outcome: CodeExecutionOutcome,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
    async fn restore_execution_state(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _state: &HydratedExecutionState,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProtocolDriverPlugin for Integrator {
    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        unreachable!("external signature witness")
    }
    fn resolve_render(
        &self,
        _options: &ProtocolTurnOptions,
    ) -> Result<Option<RecordedRender>, String> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessEngine for Integrator {
    fn kind(&self) -> &'static str {
        unreachable!("external signature witness")
    }
    fn program_identity(
        &self,
        _payload: &lash::messages::JsonValue,
    ) -> Option<ExecutableGeneration> {
        unreachable!("external signature witness")
    }
    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        payload: lash::messages::JsonValue,
    ) -> Result<ProcessRunOutcome, ProcessInfraError> {
        unreachable!("external signature witness")
    }
    fn start_artifacts(
        &self,
        payload: &lash::messages::JsonValue,
    ) -> Result<Vec<ArtifactName>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn end_artifact_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn acquire_engine_artifact(
        &self,
        claim: &ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionRefusal> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl AwaitEventResolver for Integrator {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        unreachable!("external signature witness")
    }
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn await_event_key(
        &self,
        _scope: &ExecutionScope,
        _wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn resolve_await_event(
        &self,
        _key: &AwaitEventKey,
        _resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn peek_await_event(
        &self,
        _key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn await_await_event(
        &self,
        _key: &AwaitEventKey,
        _cancel: CancellationToken,
        _deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn revoke_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn cancel_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn retire_await_events_for_scope(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn await_event_scope_is_retired(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl EffectHost for Integrator {
    fn turn_control_binding_id(&self) -> String {
        unreachable!("external signature witness")
    }
    async fn retire_closed_root_waits(
        &self,
        _session_id: &SessionId,
        _root: &TurnId,
        _committed_turn: Option<&TurnId>,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn list_outstanding_await_event_keys(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        unreachable!("external signature witness")
    }
    fn turn_attach(&self) -> Option<Arc<dyn TurnAttach>> {
        unreachable!("external signature witness")
    }
    fn scoped<'run>(
        &'run self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        unreachable!("external signature witness")
    }
    fn scoped_static(
        &self,
        _admitted: AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        unreachable!("external signature witness")
    }
    fn scoped_for_group_child(
        &self,
        _admitted: AdmittedScope,
        _binding: GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        unreachable!("external signature witness")
    }
    fn route_handler_child_controller<'run>(
        &self,
        controller: ScopedEffectController<'run>,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        unreachable!("external signature witness")
    }
    fn install_tool_child_host(&self, candidate: Arc<ToolChildHost>) -> Option<Arc<ToolChildHost>> {
        unreachable!("external signature witness")
    }
    fn await_event_resolver(&self) -> &dyn AwaitEventResolver {
        unreachable!("external signature witness")
    }
    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a ScopedEffectController<'_>,
    ) -> Result<TurnControlBinding<'a>, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn prepare_tool_intent(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &ToolIntentIdentity,
        intent: ToolIntent,
    ) -> Result<ToolIntentPreparation, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &ToolIntentIdentity,
        _submitted: ToolIntent,
        outcome: ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn retire_effect_journal(
        &self,
        _retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn journal_replay(
        &self,
        journal: &EffectJournalIdentity,
    ) -> Result<JournalReplay, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    fn bind_process_registry(&self, _binding: ProcessRegistryBinding) {
        unreachable!("external signature witness")
    }
    async fn register_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn release_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl RuntimeEffectController for Integrator {
    fn owns_commit_backpressure(&self) -> bool {
        unreachable!("external signature witness")
    }
    fn wants_segment_boundary(&self, _progress: &SegmentProgress) -> Option<BoundaryReason> {
        unreachable!("external signature witness")
    }
    async fn observe_process_cancel(
        &self,
        lent_stop: &CancellationToken,
    ) -> Result<bool, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn observe_group_child_cancel(&self) -> Result<bool, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    fn group_child_cancel_watch(&self) -> Option<Arc<dyn GroupChildCancelWatch>> {
        unreachable!("external signature witness")
    }
    async fn record_process_drive_step(
        &self,
        name: String,
        step: ProcessDriveStep<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    fn group_child_scoped_controller(
        &self,
        _admitted: AdmittedScope,
        _binding: GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        unreachable!("external signature witness")
    }
    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: TurnCancelWait,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn commit_group_child_final(
        &self,
        commit: GroupChildFinalCommit,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
    async fn read_recorded_journal(
        &self,
        range: &RecordedKeyRange,
    ) -> Result<RecordedJournal, RuntimeEffectControllerError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessExecutionEnvStore for Integrator {
    async fn publish_process_execution_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn acquire_process_execution_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn get_process_execution_env(
        &self,
        env_ref: &ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessContinuationStore for Integrator {
    async fn put_segment_handover(
        &self,
        process_id: &ProcessId,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn get_segment_handover(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn latest_segment_handover(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn retire_segment_handovers_through(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn delete_segment_handovers(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn segment_start(
        &self,
        segment: &ProcessSegmentKey,
    ) -> Result<Option<SegmentStartMarker>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn mark_segment_started(
        &self,
        segment: &ProcessSegmentKey,
        marker: SegmentStartMarker,
    ) -> Result<SegmentStartMarker, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl AttachmentStore for Integrator {
    fn persistence(&self) -> AttachmentStorePersistence {
        unreachable!("external signature witness")
    }
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        unreachable!("external signature witness")
    }
    async fn get(&self, id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        unreachable!("external signature witness")
    }
    async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        unreachable!("external signature witness")
    }
    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        unreachable!("external signature witness")
    }
    async fn head(&self, id: &AttachmentId) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl LiveReplayStore for Integrator {
    fn prepare_publication(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<PreparedLiveReplayPublication, LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    fn publish_prepared(
        &self,
        prepared: PreparedLiveReplayPublication,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        unreachable!("external signature witness")
    }
    fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl TriggerStore for Integrator {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<TriggerEffectResult, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, PluginError> {
        unreachable!("external signature witness")
    }
    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<TriggerIngressReceipt, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_occurrences(
        &self,
        filter: TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &ProcessId,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_delivery_process_ids(&self) -> Result<Vec<ProcessId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_delivery_retention_candidates(
        &self,
    ) -> Result<Vec<TriggerDeliveryRetentionCandidate>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_session_owner_ids_for_retention(&self) -> Result<Vec<SessionId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn reconcile_trigger_retention(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> Result<TriggerRetentionReconciliationReport, PluginError> {
        unreachable!("external signature witness")
    }
    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[TriggerDeliveryRetentionCandidate],
    ) -> Result<usize, PluginError> {
        unreachable!("external signature witness")
    }
    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> TriggerOccurrenceReclamationResult {
        unreachable!("external signature witness")
    }
    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError> {
        unreachable!("external signature witness")
    }
    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> Result<usize, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessQuery for Integrator {
    async fn require_process_id(&self, process_id: &ProcessId) -> Result<ProcessId, PluginError> {
        unreachable!("external signature witness")
    }
    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn get_process_by_start_key(
        &self,
        start_key: &StartKey,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_non_terminal_processes_page(
        &self,
        limit: NonZeroUsize,
        continuation: Option<ProcessRegistryCursor>,
    ) -> Result<NonTerminalProcessPage, PluginError> {
        unreachable!("external signature witness")
    }
    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn live_reference_summary(&self) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_parked_processes(
        &self,
        query: &ProcessParkQuery,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn process_park_feed(
        &self,
        after: ParkFeedCursor,
        limit: NonZeroUsize,
    ) -> Result<ParkFeedPage<ProcessParkKey>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn summarize_parked_processes(&self) -> Result<ParkReport, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessRegistrar for Integrator {
    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn register_process_reporting_outcome(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRegistrationReceipt, PluginError> {
        unreachable!("external signature witness")
    }
    fn bind_effect_host(&self, effect_host: &Arc<dyn EffectHost>) {
        unreachable!("external signature witness")
    }
    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessObserverRegistry for Integrator {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn transfer_observers(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_live_observed_by(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn is_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<bool, PluginError> {
        unreachable!("external signature witness")
    }
    async fn observers_for_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<SessionId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn retarget_subscription(
        &self,
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessEventLog for Integrator {
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        unreachable!("external signature witness")
    }
    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        unreachable!("external signature witness")
    }
    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<Vec<ProcessEventAppendReceipt>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn event_page_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
        limit: NonZeroUsize,
        mode: ProcessEventQueryMode,
    ) -> Result<ProcessEventReadOutcome<ProcessEventPage>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn event_page(
        &self,
        process_id: &ProcessId,
        limit: NonZeroUsize,
        mode: ProcessEventQueryMode,
    ) -> Result<ProcessEventReadOutcome<ProcessEventPage>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        unreachable!("external signature witness")
    }
    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessLifecycle for Integrator {
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        unreachable!("external signature witness")
    }
    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        unreachable!("external signature witness")
    }
    async fn record_parent_end(&self, parent: &ScopeId) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn settle_terminal_publication(
        &self,
        process_id: &ProcessId,
    ) -> Result<bool, PluginError> {
        unreachable!("external signature witness")
    }
    async fn terminal_publication(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessTerminalPublication>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn get_parent_end_plan(
        &self,
        parent: &ScopeId,
    ) -> Result<Option<ParentEndPlan>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn get_parent_end_plan_by_key(
        &self,
        parent_kind: &str,
        parent_id: &str,
    ) -> Result<Option<ParentEndPlan>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_parent_end_children(
        &self,
        parent: &ScopeId,
        after: Option<&ProcessId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn settle_parent_end_plan(&self, parent: &ScopeId) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_unrecorded_opener_parents(
        &self,
        after: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ScopeId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError> {
        unreachable!("external signature witness")
    }
    async fn request_process_cancel(
        &self,
        process_id: &ProcessId,
        origin: CancelOrigin,
        requester: String,
        attribution: Option<RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn request_process_cancel_reporting_realization(
        &self,
        process_id: &ProcessId,
        origin: CancelOrigin,
        requester: String,
        attribution: Option<RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, StoreRealization), PluginError> {
        unreachable!("external signature witness")
    }
    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn park_process_with_authority(
        &self,
        process_id: &ProcessId,
        park: ProcessParkWrite,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
    async fn begin_parked_rerun_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessToolIntents for Integrator {
    async fn admit_tool_intent_submission(
        &self,
        submission: ToolIntentSubmissionRecord,
    ) -> Result<ToolIntentSubmissionAdmission, PluginError> {
        unreachable!("external signature witness")
    }
    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: ToolIntentExecutionOutcome,
    ) -> Result<ToolIntentSubmissionRecord, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessWakeOutbox for Integrator {
    fn wake_delivery_config(&self) -> WakeDeliveryConfig {
        unreachable!("external signature witness")
    }
    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<WakeDelivery>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_wake_deliveries(
        &self,
        state: Option<WakeDeliveryState>,
    ) -> Result<Vec<WakeDelivery>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn wake_delivery_report(&self) -> Result<WakeDeliveryReport, PluginError> {
        unreachable!("external signature witness")
    }
    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<WakeDeliveryClaimOutcome, PluginError> {
        unreachable!("external signature witness")
    }
    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: WakeDiscardReason,
    ) -> Result<WakeDeliveryClaimOutcome, PluginError> {
        unreachable!("external signature witness")
    }
    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<WakeDeliveryClaimOutcome, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessRetention for Integrator {
    async fn release_trigger_delivery_pin(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn list_trigger_delivery_pins(&self) -> Result<Vec<PinnedTriggerDelivery>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn TriggerStore>,
    ) -> Result<usize, PluginError> {
        unreachable!("external signature witness")
    }
    async fn compact_process_park_feed(&self, through: ParkFeedCursor) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError> {
        unreachable!("external signature witness")
    }
    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessClockRebind for Integrator {
    fn with_runtime_clock(
        &self,
        _clock: std::sync::Arc<dyn Clock>,
    ) -> Option<std::sync::Arc<dyn ProcessRegistry>> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl FleetFormatStore for Integrator {
    fn fleet_format(&self) -> FleetFormat {
        FleetFormat::current()
    }
}

#[test]
fn external_integrator_classes_compile_with_only_lash_dependency() {
    fn store<T: RuntimeStore + DeploymentStore>() {}
    fn effects<T: EffectHost + RuntimeEffectController + AwaitEventResolver>() {}
    fn tools<T: ToolProvider>() {}
    fn protocol<
        T: ProtocolSessionPlugin + CodeExecutorPlugin + ProtocolDriverPlugin + ProcessEngine,
    >() {
    }
    fn substrate<
        T: ProcessRegistry
            + TriggerStore
            + AttachmentStore
            + LiveReplayStore
            + ProcessExecutionEnvStore
            + ProcessContinuationStore,
    >() {
    }
    let manifest = include_str!("../Cargo.toml");
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("fixture dependencies")
        .trim();
    assert_eq!(
        dependencies.lines().count(),
        1,
        "the fixture must depend solely on lash"
    );
    assert!(dependencies.starts_with("lash = "));
    tools::<Integrator>();
    store::<Integrator>();
    effects::<Integrator>();
    protocol::<Integrator>();
    substrate::<Integrator>();
}
