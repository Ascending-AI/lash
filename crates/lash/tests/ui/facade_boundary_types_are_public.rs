use std::sync::Arc;

use async_trait::async_trait;
use lash::direct::{
    AttachmentSource, DirectLlmClient, DirectLlmError, DirectLlmResult, DirectRequest, NonNegativeFiniteF64,
    NonNegativeFiniteF64Error, GenerationOptions, LlmEventSender, LlmOutputPart, LlmUsage,
};
use lash::durability::RuntimeHostConfig;
use lash::messages::MessageRole;
use lash::persistence::{
    CheckpointKind, GcReport, GraphCommitDelta, LeaseOwnerIdentity, OperationId,
    PersistedSessionRead, PendingTurnInputDraft, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkStore, RuntimeCommit, RuntimeCommitResult,
    RuntimePersistence, RuntimeSessionState, RuntimeTurnCommitStamp, SessionCheckpoint,
    SessionCommitStore, SessionExecutionLease, SessionExecutionLeaseClaimOutcome,
    SessionExecutionLeaseCompletion, SessionExecutionLeaseFence, SessionExecutionLeaseStore,
    SessionMeta, SessionNodeRecord, SessionReadScope, StoreError, StoreMaintenance, TurnInputClaim,
    TurnInputCheckpointBoundary, TurnInputIngress, TurnInputState, TurnInputStore, VacuumReport,
    load_persisted_session_state,
    load_persisted_session_state_active_path,
};
use lash::usage::{TokenLedgerEntry, TokenUsage};
use lash::plugins::{
    AfterToolCallHook, BeforeToolCallHook, CompactionContext, ContextCompaction, ContextCompactor,
    ContextError, PluginDirective, PluginHost, PluginSpec, PluginSpecBuilder, PluginSpecFactory,
    ToolCallHookContext, ToolCatalogContribution, ToolResultHookContext,
};
use lash::provider::{
    ProviderRateLimitPolicy, ProviderReliability, ProviderRetryPolicy,
};
use lash::runtime::AdvancedLashCoreBuilder;
use lash::tools::{ToolActivation, ToolCallRecord, ToolOutputContract};
use lash::turn::{AssistantOutput, TurnIssue};
use lash::{ModelLimits, ModelSpec};

struct FacadeStore;

lash_core::impl_noop_attachment_manifest!(FacadeStore);

#[async_trait]
impl SessionCommitStore for FacadeStore {
    async fn load_session(
        &self,
        _scope: SessionReadScope,
    ) -> Result<Option<PersistedSessionRead>, StoreError> {
        Ok(None)
    }

    async fn load_node(&self, _node_id: &str) -> Result<Option<SessionNodeRecord>, StoreError> {
        Ok(None)
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        let manifest = SessionCheckpoint::new(
            commit.checkpoint.turn_state,
            commit.checkpoint.tool_state_ref,
            commit.checkpoint.plugin_snapshot_ref,
            commit.checkpoint.plugin_snapshot_revision,
            commit.checkpoint.execution_state_ref,
        );
        Ok(RuntimeCommitResult {
            head_revision: commit.expected_head_revision.unwrap_or_default() + 1,
            checkpoint_ref: "checkpoint".to_string().into(),
            manifest,
            realization_digest: String::new(),
            agent_frames: Vec::new(),
            current_agent_frame_id: String::new(),
            enqueued_queue_batches: Vec::new(),
            turn_input_applications: Vec::new(),
        })
    }

    async fn save_session_meta(&self, _meta: SessionMeta) -> Result<(), StoreError> {
        Ok(())
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        Ok(None)
    }
}

#[async_trait]
impl SessionExecutionLeaseStore for FacadeStore {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        Ok(SessionExecutionLeaseClaimOutcome::Acquired(
            SessionExecutionLease {
                session_id: session_id.to_string(),
                owner: owner.clone(),
                lease_token: "facade-token".to_string(),
                fencing_token: 1,
                claimed_at_epoch_ms: 0,
                expires_at_epoch_ms: lease_ttl_ms,
            },
        ))
    }

    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        _observed_holder: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        Ok(SessionExecutionLeaseClaimOutcome::Acquired(
            SessionExecutionLease {
                session_id: session_id.to_string(),
                owner: owner.clone(),
                lease_token: "facade-token".to_string(),
                fencing_token: 1,
                claimed_at_epoch_ms: 0,
                expires_at_epoch_ms: lease_ttl_ms,
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        Ok(SessionExecutionLease {
            session_id: fence.session_id.clone(),
            owner: fence.owner.clone(),
            lease_token: fence.lease_token.clone(),
            fencing_token: fence.fencing_token,
            claimed_at_epoch_ms: 0,
            expires_at_epoch_ms: lease_ttl_ms,
        })
    }

    async fn release_session_execution_lease(
        &self,
        _completion: &SessionExecutionLeaseCompletion,
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

// Compile-only store: these segments exist to prove every capability trait
// (and its signature vocabulary) is nameable through the facade.
#[async_trait]
impl TurnInputStore for FacadeStore {
    async fn enqueue_pending_turn_input(
        &self,
        _input: PendingTurnInputDraft,
    ) -> Result<lash::PendingTurnInput, StoreError> {
        unreachable!("compile-only facade store")
    }

    async fn list_pending_turn_inputs(
        &self,
        _session_id: &str,
    ) -> Result<Vec<lash::PendingTurnInput>, StoreError> {
        Ok(Vec::new())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        _session_id: &str,
        _targets: &[lash::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash::PendingTurnInputCancelResult>, StoreError> {
        unreachable!("compile-only facade store")
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        _session_id: &str,
        _anchor: &lash::PendingTurnInputCancelTarget,
    ) -> Result<lash::PendingTurnInputSuffixCancelOutcome, StoreError> {
        unreachable!("compile-only facade store")
    }

    async fn claim_active_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &SessionExecutionLeaseFence,
        _owner: &LeaseOwnerIdentity,
        _turn_id: &str,
        _checkpoint: CheckpointKind,
        _max_inputs: usize,
    ) -> Result<Option<TurnInputClaim>, StoreError> {
        Ok(None)
    }

    async fn claim_next_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &SessionExecutionLeaseFence,
        _owner: &LeaseOwnerIdentity,
        _max_inputs: usize,
    ) -> Result<Option<TurnInputClaim>, StoreError> {
        Ok(None)
    }

    async fn abandon_turn_input_claim(&self, _claim: &TurnInputClaim) -> Result<(), StoreError> {
        Ok(())
    }
}

#[async_trait]
impl QueuedWorkStore for FacadeStore {
    async fn enqueue_queued_work(
        &self,
        _batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        unreachable!("compile-only facade store")
    }

    async fn claim_leading_ready_session_command(
        &self,
        _session_id: &str,
        _session_execution_lease: &SessionExecutionLeaseFence,
        _owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        Ok(None)
    }

    async fn claim_ready_queued_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &SessionExecutionLeaseFence,
        _owner: &LeaseOwnerIdentity,
        _boundary: QueuedWorkClaimBoundary,
        _max_batches: usize,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        Ok(None)
    }

    async fn claim_checkpoint_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &SessionExecutionLeaseFence,
        _owner: &LeaseOwnerIdentity,
        _turn_id: &str,
        _checkpoint: CheckpointKind,
        _max_inputs: usize,
        _max_batches: usize,
    ) -> Result<(Option<TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        Ok((None, None))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        _session_id: &str,
        _session_execution_lease: &SessionExecutionLeaseFence,
        _owner: &LeaseOwnerIdentity,
        _boundary: QueuedWorkClaimBoundary,
        _batch_ids: &[String],
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        Ok(None)
    }

    async fn abandon_queued_work_claim(&self, _claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        _session_id: &str,
        _batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        Ok(None)
    }

    async fn list_queued_work(&self, _session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        Ok(Vec::new())
    }

    async fn list_pending_queued_work(
        &self,
        _session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl StoreMaintenance for FacadeStore {
    async fn tombstone_nodes(&self, _ids: &[String]) -> Result<(), StoreError> {
        Ok(())
    }

    async fn vacuum(&self) -> Result<VacuumReport, StoreError> {
        Ok(VacuumReport::default())
    }

    async fn gc_unreachable(&self) -> Result<GcReport, StoreError> {
        Ok(GcReport::default())
    }
}

fn persistence_types_are_nameable(
    graph: GraphCommitDelta,
    ledger: Vec<TokenLedgerEntry>,
) -> RuntimeCommit {
    RuntimeCommit {
        session_id: "facade".to_string(),
        expected_head_revision: Some(0),
        session_execution_lease: None,
        release_session_execution_lease: None,
        config: Default::default(),
        agent_frames: Vec::new(),
        current_agent_frame_id: String::new(),
        graph,
        checkpoint: Default::default(),
        usage_deltas: ledger,
        turn_commit: Some(RuntimeTurnCommitStamp::new(
            "facade",
            OperationId::turn("facade", "turn", "final"),
            "sha256:facade",
        )),
        completed_queue_claims: Vec::new(),
        completed_turn_input_claims: Vec::new(),
        enqueued_queue_batches: Vec::new(),
        interrupted_turn_input_turn_id: None,
        committed_attachment_ids: Vec::new(),
    }
}

fn plugin_types_are_nameable() -> PluginHost {
    let before: BeforeToolCallHook = Arc::new(|ctx: ToolCallHookContext| {
        Box::pin(async move { Ok(vec![PluginDirective::ReplaceToolArgs { args: ctx.args }]) })
    });
    let after: AfterToolCallHook = Arc::new(|ctx: ToolResultHookContext| {
        Box::pin(async move { Ok(vec![PluginDirective::short_circuit(ctx.result)]) })
    });
    let builder: PluginSpecBuilder = Arc::new(move |_ctx| {
        Ok(PluginSpec::new()
            .with_before_tool_call(Arc::clone(&before))
            .with_after_tool_call(Arc::clone(&after)))
    });
    PluginHost::new(vec![Arc::new(PluginSpecFactory::new("facade", builder))])
}

struct FacadeCompactor;

#[async_trait]
impl ContextCompactor for FacadeCompactor {
    fn id(&self) -> &'static str {
        "facade.compactor"
    }

    async fn compact(
        &self,
        _ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        Ok(Some(ContextCompaction::default()))
    }
}

fn context_compactor_types_are_nameable() -> PluginSpec {
    PluginSpec::new().with_context_compactor(10, Arc::new(FacadeCompactor))
}

async fn direct_response_type_is_nameable(
    client: &mut DirectLlmClient,
    request: DirectRequest,
) -> Result<DirectLlmResult, DirectLlmError> {
    client.complete(request).await
}

fn direct_payload_types_are_nameable(
    attachment: AttachmentSource,
    event_sender: LlmEventSender,
    output: LlmOutputPart,
    usage: LlmUsage,
    token_usage: TokenUsage,
) {
    let _ = (attachment, event_sender, output, usage, token_usage);
}

fn generation_option_types_are_nameable(
    mut request: DirectRequest,
    temperature: f64,
) -> Result<GenerationOptions, NonNegativeFiniteF64Error> {
    request.generation.temperature = Some(NonNegativeFiniteF64::new(temperature)?);
    request.generation.seed = Some(1);
    Ok(request.generation)
}

fn advanced_builder_accepts_runtime_host_config(
    builder: AdvancedLashCoreBuilder,
    config: RuntimeHostConfig,
) -> AdvancedLashCoreBuilder {
    builder.runtime_host_config(config)
}

fn tool_contract_types_are_nameable(
    activation: ToolActivation,
    record: ToolCallRecord,
    contract: ToolOutputContract,
) {
    let _ = (activation, record, contract);
}

fn tool_catalog_types_are_nameable(contribution: ToolCatalogContribution) {
    let _ = contribution;
}

fn message_role_type_is_nameable(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Event => "event",
    }
}

fn turn_result_detail_types_are_nameable(output: AssistantOutput, issue: TurnIssue) {
    let _ = (output, issue);
}

fn provider_reliability_types_are_nameable(
    reliability: ProviderReliability,
    retry: ProviderRetryPolicy,
    rate_limits: ProviderRateLimitPolicy,
) {
    let _ = (reliability, retry, rate_limits);
}

fn model_spec_types_are_nameable(spec: ModelSpec, limits: ModelLimits) {
    let _ = (spec, limits);
}

fn cancellation_token_is_at_root(token: lash::CancellationToken, session: &lash::LashSession) {
    token.cancel();
    let _: usize = session.cancel_running_turns();
}

fn turn_input_ingress_types_are_nameable(
    ingress: TurnInputIngress,
    boundary: TurnInputCheckpointBoundary,
    state: TurnInputState,
) {
    let _ = (ingress, boundary, state);
}

async fn queued_work_wait_is_nameable(session: &lash::LashSession) -> lash::Result<()> {
    session.await_queued_work_batch("qwb:batch").await
}

async fn pending_turn_input_cancel_facade_is_nameable(
    session: &lash::LashSession,
    target: lash::PendingTurnInputCancelTarget,
) -> lash::Result<()> {
    let _: Vec<lash::PendingTurnInputCancelResult> = session
        .cancel_pending_turn_inputs(vec![target.clone()])
        .await?;
    let _: lash::PendingTurnInputSuffixCancelOutcome =
        session.cancel_pending_turn_input_suffix(target).await?;
    Ok(())
}

fn observation_types_are_homed_in_observe(
    cursor: lash::observe::SessionCursor,
    observation: lash::observe::SessionObservation,
    resume: lash::observe::SessionResume,
    revision: lash::observe::SessionRevision,
) {
    let _ = (cursor, observation, resume, revision);
}

fn trigger_types_are_homed_in_triggers(
    event: lash::triggers::TriggerEvent,
    report: lash::triggers::TriggerEmitReport,
    registration: lash::triggers::TriggerRegistration,
    source_type: lash::triggers::TriggerEventType,
    filter: lash::triggers::TriggerSubscriptionFilter,
    target: lash::triggers::TriggerTargetSummary,
) {
    let _ = (event, report, registration, source_type, filter, target);
    let _ = lash::triggers::empty_trigger_source_key("ui.button.pressed");
}

async fn persistence_load_helpers_are_nameable(
    store: &dyn RuntimePersistence,
) -> Result<Option<RuntimeSessionState>, StoreError> {
    let _ = load_persisted_session_state_active_path(store, None).await?;
    load_persisted_session_state(store).await
}

// Types that appear in facade public signatures must have a reachable facade
// home (no bare `lash_core::` leak). See lib.rs contract: "Every public name
// has exactly one home."
#[allow(clippy::too_many_arguments)]
fn leaked_signature_types_are_homed(
    execution: lash::ExecutionSummary,
    message: lash::messages::Message,
    tool_id: lash::tools::ToolId,
    create_request: lash::SessionCreateRequest,
    start_point: lash::SessionStartPoint,
    plugin_options: lash::plugins::PluginOptions,
    provenance: lash::process::ProcessProvenance,
    outcome: lash::TurnOutcome,
    finish: lash::TurnFinish,
    stop: lash::TurnStop,
    cause: lash::TurnCause,
    subscription: lash::triggers::TriggerSubscriptionRecord,
    replay_store: lash::observe::InMemoryLiveReplayStore,
) {
    let _ = (
        execution,
        message,
        tool_id,
        create_request,
        start_point,
        plugin_options,
        provenance,
        outcome,
        finish,
        stop,
        cause,
        subscription,
        replay_store,
    );
}

fn assert_store_object(_: Arc<dyn RuntimePersistence>) {}

fn main() {
    assert_store_object(Arc::new(FacadeStore));
    let _ = persistence_types_are_nameable(
        GraphCommitDelta::Unchanged { leaf_node_id: None },
        Vec::new(),
    );
    let _ = plugin_types_are_nameable();
    let _ = context_compactor_types_are_nameable();
    let _ = direct_response_type_is_nameable;
    let _ = direct_payload_types_are_nameable;
    let _ = generation_option_types_are_nameable;
    let _ = advanced_builder_accepts_runtime_host_config;
    let _ = tool_contract_types_are_nameable;
    let _ = tool_catalog_types_are_nameable;
    let _ = message_role_type_is_nameable;
    let _ = turn_result_detail_types_are_nameable;
    let _ = provider_reliability_types_are_nameable;
    let _ = model_spec_types_are_nameable;
    let _ = persistence_load_helpers_are_nameable;
    let _ = observation_types_are_homed_in_observe;
    let _ = trigger_types_are_homed_in_triggers;
    let _ = cancellation_token_is_at_root;
    let _ = queued_work_wait_is_nameable;
    let _ = pending_turn_input_cancel_facade_is_nameable;
    let _ = leaked_signature_types_are_homed;
}
