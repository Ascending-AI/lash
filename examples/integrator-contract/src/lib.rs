//! External authorship proof. The only production dependency is lash.
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

struct Integrator {
    registry: Arc<dyn ProcessRegistry>,
}
mod storage;
pub mod stores;

#[lash::async_trait]
impl ToolProvider for Integrator {
    fn tool_manifests(
        &self,
        _config: &lash::tools::OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash::plugins::PluginError> {
        unreachable!("external signature witness")
    }
    fn resolve_manifest(
        &self,
        name: &str,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<ToolManifest> {
        unreachable!("external signature witness")
    }
    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<ToolManifest> {
        unreachable!("external signature witness")
    }
    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        unreachable!("external signature witness")
    }
    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
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
    async fn before_llm_call(
        &self,
        _ctx: ProtocolBeforeLlmCallContext,
        _request: &LlmRequest,
    ) -> Result<Option<ProtocolLlmCallAction>, PluginError> {
        unreachable!("external signature witness")
    }
    async fn prompt_facts(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<lash::plugins::ProtocolPromptFacts>, SessionError> {
        unreachable!("external signature witness")
    }
}

impl PluginFactory for Integrator {
    fn id(&self) -> &'static str {
        unreachable!("external signature witness")
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        unreachable!("external signature witness")
    }
}
impl Integrator {
    pub fn declare_configuration(
        &self,
        reg: &mut ConfigRegistrar,
    ) -> Result<(), ConfigRegistrationError> {
        reg.owner(IntegratorConfigOwner)
    }
}

impl Integrator {
    pub fn declaration() -> lash::plugins::PluginDeclaration {
        unreachable!("external signature witness")
    }
}
impl lash::plugins::PluginMetadata for Integrator {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        Self::declaration()
    }
}

struct IntegratorConfigOwner;

impl ConfigOwner for IntegratorConfigOwner {
    type Create = lash::messages::JsonValue;
    type Recorded = lash::messages::JsonValue;
    type Refusal = lash::messages::JsonValue;
    type RunOptions = lash::plugins::NoRunOptions;

    fn create(&self, input: Option<Self::Create>) -> Result<Option<Self::Recorded>, Self::Refusal> {
        unreachable!("external signature witness")
    }
    fn validate(
        &self,
        value: &Self::Recorded,
        base: Option<&Self::Recorded>,
        facts: &CandidateFacts<'_>,
    ) -> Result<(), Self::Refusal> {
        unreachable!("external signature witness")
    }

    fn apply_run_options(
        &self,
        recorded: &Self::Recorded,
        _options: Self::RunOptions,
    ) -> std::result::Result<Self::Recorded, Self::Refusal> {
        Ok(recorded.clone())
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
    async fn capture_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _purpose: ExecutionGraphPurpose,
    ) -> Result<ExecutionGraphCapture, SessionError> {
        unreachable!("external signature witness")
    }
    async fn frame_switch_carries(
        &self,
        ctx: ProtocolSessionContext<'_>,
        successor: &lash::plugins::FrameNodeId,
        initial_nodes: &[SessionAppendNode],
    ) -> Result<Vec<ArtifactName>, SessionError> {
        unreachable!("external signature witness")
    }
    async fn hydrated_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
    ) -> Result<Option<HydratedExecutionGraph>, SessionError> {
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
    async fn restore_execution_graph(
        &self,
        _ctx: ProtocolSessionContext<'_>,
        _graph: &HydratedExecutionGraph,
        _purpose: ExecutionGraphPurpose,
    ) -> Result<(), SessionError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProtocolDriverPlugin for Integrator {
    fn attachment_omission_text(
        &self,
        _notice: &lash::plugins::AttachmentMaterializationNotice,
    ) -> String {
        unreachable!("external signature witness")
    }

    fn build_preamble(&self, input: ProtocolBuildInput) -> TurnDriverPreamble {
        unreachable!("external signature witness")
    }
    fn resolve_render(
        &self,
        _options: &ProtocolTurnOptions,
    ) -> Result<Option<RecordedRender>, lash::plugins::RenderFault> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessEngine for Integrator {
    fn settled_ran(
        &self,
        _kind: &lash::plugins::EngineStepKind,
        _output: &lash::plugins::SettledOutput,
    ) -> lash::plugins::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash::plugins::ProcessTraceClass {
        lash::plugins::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &lash::messages::JsonValue,
    ) -> Result<lash::process::PerformedEffects, lash::process::ProcessEnvRefusal> {
        Ok(lash::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash::process::ProcessSignature,
        _args: &lash::messages::JsonMap<String, lash::messages::JsonValue>,
        _mode: lash::process::ArgsMode,
    ) -> std::result::Result<(), lash::process::ArgsMismatch> {
        unreachable!("external signature witness")
    }

    fn kind(&self) -> &'static str {
        unreachable!("external signature witness")
    }
    fn program_identity(
        &self,
        _payload: &lash::messages::JsonValue,
    ) -> Option<ExecutableGeneration> {
        unreachable!("external signature witness")
    }
    fn state_format(&self) -> EngineStateFormat {
        unreachable!("external signature witness")
    }
    fn cancel_grace(&self) -> std::time::Duration {
        unreachable!("external signature witness")
    }
    fn check_settings(
        &self,
        payload: &lash::messages::JsonValue,
        settings: Option<&lash::process::EngineSettings>,
    ) -> Result<(), PluginError> {
        unreachable!("external signature witness")
    }
    fn default_settings(
        &self,
        payload: &lash::messages::JsonValue,
    ) -> Result<Option<lash::process::EngineSettings>, PluginError> {
        unreachable!("external signature witness")
    }
    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
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
    ) -> Result<ProcessDefinitionResolution, lash::process::ProcessDefinitionError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessEnvStore for Integrator {
    async fn publish_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &ProcessEnvRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn acquire_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &ProcessEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        unreachable!("external signature witness")
    }
    async fn get_process_env(
        &self,
        env_ref: &ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
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
    async fn get(
        &self,
        id: &AttachmentId,
        max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
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

/// A live tail an external store builds its subscription from.
struct ExternalLiveTail;

impl Stream for ExternalLiveTail {
    type Item = Result<Arc<SessionObservationEvent>, LiveReplayStoreError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(None)
    }
}

#[lash::async_trait]
impl LiveReplayStore for Integrator {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    async fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    async fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        Ok(LiveReplaySubscribeOutcome::Subscribed(
            LiveReplaySubscription::new(Vec::new(), ExternalLiveTail),
        ))
    }
    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        unreachable!("external signature witness")
    }

    fn tail_cursor(&self, session_id: &SessionId) -> SessionCursor {
        unreachable!("external signature witness")
    }
    async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    async fn invalidate_all(&self) -> Result<(), LiveReplayStoreError> {
        unreachable!("external signature witness")
    }
    async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
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

    async fn list_processes_page(
        &self,
        filter: &ProcessListFilter,
        limit: NonZeroUsize,
        continuation: Option<lash::process::ProcessRosterCursor>,
    ) -> Result<lash::persistence::ProcessRosterRecords, PluginError> {
        unreachable!("external signature witness")
    }
    async fn process_change_bounds(
        &self,
    ) -> Result<lash::process::ProcessChangeBounds, PluginError> {
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
}

#[lash::async_trait]
impl ProcessRegistrar for Integrator {
    async fn prepare_process_registration(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<PreparedProcessRegistration, PluginError> {
        unreachable!("external signature witness")
    }
    async fn commit_process_registration(
        &self,
        prepared: PreparedProcessRegistration,
    ) -> Result<ProcessRegistrationReceipt, PluginError> {
        unreachable!("external signature witness")
    }

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
    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<ProcessSessionDeleteReport, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessEventLog for Integrator {
    async fn event_by_replay_key(
        &self,
        process_id: &ProcessId,
        replay_key: &str,
        through_sequence: u64,
    ) -> Result<ProcessEventReadOutcome<Option<ProcessEvent>>, PluginError> {
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
    async fn get_parent_end_plan(
        &self,
        parent: &ScopeId,
    ) -> Result<Option<ParentEndPlan>, PluginError> {
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
    ) -> Result<StoreTransition<ToolIntentSubmissionRecord>, PluginError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ProcessRetention for Integrator {
    async fn release_process_events(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<ProcessEventRelease, PluginError> {
        self.registry
            .release_process_events(process_id, through)
            .await
    }
    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
    ) -> Result<usize, PluginError> {
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

/// The facade alone supplies both the registry implementation contract above
/// and the row constructors an external store uses for its lifecycle fold.
#[test]
fn a_store_author_constructs_its_process_row_through_the_facade() {
    let process_id = lash::ProcessId::fixture("external-store-row");
    let registration = lash::testing::held_engine_registration(
        Default::default(),
        ProcessProvenance::host(),
        Lifetime::Detached,
    );
    let row = ProcessRecord::from_registration(registration, process_id.clone());
    assert_eq!(row.id, process_id);
    assert_eq!(row.status(), ProcessStatus::Running);
    assert_eq!(row.last_event_sequence, 0);
}
