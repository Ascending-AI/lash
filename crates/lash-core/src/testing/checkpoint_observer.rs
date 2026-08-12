//! Store-factory decorator that observes real runtime-checkpoint commits, so a
//! harness can render durable-write lines from facts the backend accepted.

use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::runtime::{
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
};
use crate::store::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, PersistedSessionRead,
    RuntimeCommit, RuntimeCommitResult, RuntimePersistence, SessionCommitStore, StoreError,
};
use crate::{
    AttachmentId, BlobRef, CheckpointKind, ForkPoint, ForkSessionRequest, ForkSessionResult,
    GcReport, LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelResult, PendingTurnInputCancelTarget, PendingTurnInputDraft,
    SessionAdmission, SessionBinding, SessionExecutionLease, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore, SessionHeadMeta, SessionMeta,
    SessionStoreCreateRequest, SessionStoreFactory, StoreMaintenance, TurnInputApplication,
    TurnInputClaim, TurnInputStore, VacuumReport,
};
use serde::{Deserialize, Serialize};

/// Schema tag carried on every observed commit.
///
/// The value keeps its historical `lash.sim.` prefix because generated
/// simulation trace artifacts embed it; the observer itself is no longer
/// simulator-specific.
pub const CHECKPOINT_WRITE_EVENT_SCHEMA: &str = "lash.sim.checkpoint-write-event.v3";

/// One checkpoint component observed at the successful store-commit seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointComponentWrite {
    pub component: CheckpointComponent,
    pub kind: CheckpointComponentWriteKind,
}

/// Closed vocabulary of runtime-checkpoint components.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointComponent {
    TurnState,
    ToolState,
    PluginSnapshot,
    ExecutionState,
}

impl CheckpointComponent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnState => "turn_state",
            Self::ToolState => "tool_state",
            Self::PluginSnapshot => "plugin_snapshot",
            Self::ExecutionState => "execution_state",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CheckpointComponentWriteKind {
    /// Body present at the commit seam. `logical_bytes` is the size of the
    /// encoding-independent JSON projection used only for human comparison;
    /// it is not a backend's MessagePack/compressed byte count.
    Stored {
        #[serde(default, alias = "bytes", skip_serializing_if = "Option::is_none")]
        logical_bytes: Option<usize>,
    },
    UnchangedRef,
}

/// Typed token-accounting facts submitted by one runtime commit.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointUsageWrite {
    pub entries: usize,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

/// A successful runtime-state commit as observed by the simulator's store
/// wrapper. Component bodies are inspected before delegation, while the
/// resulting head revision is recorded only after the backend accepts them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointWriteEvent {
    pub schema: String,
    /// The real session id passed to the store commit.
    pub session_id: String,
    /// Optional generated-trace attribution for a separately executed contract
    /// proof. Ordinary generated runtime commits use `session_id` directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed_session_id: Option<String>,
    /// Boundary that caused a separately executed contract proof. Runtime-turn
    /// writes are linked by session plus turn instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause_boundary_id: Option<String>,
    pub commit_index: usize,
    pub turn_index: usize,
    pub revision_before: u64,
    pub revision_after: u64,
    pub usage: CheckpointUsageWrite,
    pub components: Vec<CheckpointComponentWrite>,
    /// Submitted rows plus the accepted raw/read projections observed after the
    /// commit. Simulation checkers fold these values without calling store or
    /// read-model implementation code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<CheckpointStateWrite>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointStateWrite {
    pub submitted_graph_append: serde_json::Value,
    pub submitted_turn_state: serde_json::Value,
    pub submitted_usage_rows: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_raw_rows: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_read_model: Option<serde_json::Value>,
}

impl CheckpointWriteEvent {
    pub fn has_unchanged_ref(&self) -> bool {
        self.components
            .iter()
            .any(|component| component.kind == CheckpointComponentWriteKind::UnchangedRef)
    }

    pub fn attributed_session(&self) -> &str {
        self.attributed_session_id
            .as_deref()
            .unwrap_or(&self.session_id)
    }
}

/// Shared sink used by every store handle created during one generated run.
///
/// This observes commits made through decorated `SessionStoreFactory` handles.
/// `DurableProcessWorker` task bodies currently construct a bare
/// `InMemorySessionStore` inside lash-core and are therefore explicitly outside
/// this collector's coverage; transcript consumers are warned at their emitter
/// boundary too.
#[derive(Clone, Debug, Default)]
pub struct CheckpointWriteCollector {
    state: Arc<Mutex<CheckpointWriteCollectorState>>,
    ref_only_mutation: Option<RefOnlyCommitMutation>,
}

#[derive(Debug, Default)]
struct CheckpointWriteCollectorState {
    events: Vec<CheckpointWriteEvent>,
    next_commit_by_session: BTreeMap<String, usize>,
    commit_budgets: BTreeMap<(String, u64), crate::testing::RuntimeCommitBudgetMeasurement>,
    latest_components_by_session:
        BTreeMap<String, BTreeMap<String, crate::CheckpointComponentDescriptor>>,
}

#[derive(Clone, Debug)]
struct RefOnlyCommitMutation {
    session_id: String,
    revision_before: u64,
}

impl CheckpointWriteCollector {
    /// Configure the regression-test mutation that reproduces the missing-body
    /// defect: when an updated component also carries its prior ref, drop the
    /// body before the backend sees the commit.
    ///
    /// This is the injected defect that proves a durable-write transcript can
    /// still discriminate a missing component body (ADR 0044's mutation rule).
    pub fn with_ref_only_mutation(session_id: impl Into<String>, revision_before: u64) -> Self {
        Self {
            state: Arc::default(),
            ref_only_mutation: Some(RefOnlyCommitMutation {
                session_id: session_id.into(),
                revision_before,
            }),
        }
    }

    pub fn events(&self) -> Vec<CheckpointWriteEvent> {
        let mut events = self.state.lock_recover().events.clone();
        events.sort_by(|left, right| {
            (
                left.attributed_session(),
                left.revision_before,
                left.revision_after,
                left.commit_index,
            )
                .cmp(&(
                    right.attributed_session(),
                    right.revision_before,
                    right.revision_after,
                    right.commit_index,
                ))
        });
        events
    }

    /// Return the exact pre-transaction budget measurement for an observed,
    /// successfully committed runtime write.
    pub fn runtime_commit_budget(
        &self,
        session_id: &str,
        revision_before: u64,
    ) -> Option<crate::testing::RuntimeCommitBudgetMeasurement> {
        self.state
            .lock_recover()
            .commit_budgets
            .get(&(session_id.to_string(), revision_before))
            .copied()
    }

    /// Record one observed commit, assigning its per-session commit index.
    ///
    /// Harnesses call this when re-attributing a commit that a separately
    /// executed proof produced; the decorator calls it for every commit it sees.
    pub fn push(&self, mut event: CheckpointWriteEvent) {
        let mut state = self.state.lock_recover();
        let session_id = event.attributed_session().to_string();
        let next = state.next_commit_by_session.entry(session_id).or_insert(0);
        *next += 1;
        event.commit_index = *next;
        state.events.push(event);
    }

    fn push_runtime_commit(
        &self,
        event: CheckpointWriteEvent,
        budget: crate::testing::RuntimeCommitBudgetMeasurement,
    ) {
        let key = (event.session_id.clone(), event.revision_before);
        self.push(event);
        self.state.lock_recover().commit_budgets.insert(key, budget);
    }

    fn apply_mutation(&self, commit: &mut RuntimeCommit) {
        let Some(mutation) = &self.ref_only_mutation else {
            return;
        };
        if commit.session_id != mutation.session_id
            || commit.expected_head_revision != mutation.revision_before
        {
            return;
        }
        let prior_components = self
            .state
            .lock_recover()
            .latest_components_by_session
            .get(&commit.session_id)
            .cloned()
            .unwrap_or_default();
        for (key, component) in &mut commit.checkpoint.components {
            if component.body().is_some()
                && let Some(descriptor) = prior_components.get(key).cloned()
            {
                *component = crate::HydratedCheckpointComponent::Unchanged { descriptor };
            }
        }
    }

    fn record_manifest(&self, session_id: &str, manifest: &crate::SessionCheckpoint) {
        self.state
            .lock_recover()
            .latest_components_by_session
            .insert(session_id.to_string(), manifest.components.clone());
    }
}

/// Test-support store factory decorator. It preserves the backend contract
/// exactly and adds observation only after a real commit succeeds, which is what
/// makes the resulting durable-write transcript lines real facts rather than
/// harness-constructed ones.
pub struct ObservedSessionStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    collector: CheckpointWriteCollector,
}

impl ObservedSessionStoreFactory {
    pub fn new(inner: Arc<dyn SessionStoreFactory>, collector: CheckpointWriteCollector) -> Self {
        Self { inner, collector }
    }

    fn wrap(&self, inner: Arc<dyn RuntimePersistence>) -> Arc<dyn RuntimePersistence> {
        Arc::new(ObservedSessionStore {
            inner,
            collector: self.collector.clone(),
        })
    }
}

/// Give conformance roles distinct outer handles over one in-memory substrate
/// without adding `Clone` or shared-field semantics to the production store.
#[cfg(test)]
pub(crate) fn fresh_runtime_persistence_handle(
    inner: Arc<dyn RuntimePersistence>,
) -> Arc<dyn RuntimePersistence> {
    Arc::new(ObservedSessionStore {
        inner,
        collector: CheckpointWriteCollector::default(),
    })
}

#[async_trait::async_trait]
impl SessionStoreFactory for ObservedSessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        Ok(self.wrap(self.inner.create_store(request).await?))
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        Ok(self
            .inner
            .open_existing_store(request)
            .await?
            .map(|store| self.wrap(store)))
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        self.inner.delete_session(session_id).await
    }

    async fn pin(&self, node_id: &str) -> Result<ForkPoint, StoreError> {
        self.inner.pin(node_id).await
    }

    async fn unpin(&self, node_id: &str) -> Result<(), StoreError> {
        self.inner.unpin(node_id).await
    }

    async fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError> {
        self.inner.fork_points().await
    }

    async fn fork_at(&self, request: &ForkSessionRequest) -> Result<ForkSessionResult, StoreError> {
        self.inner.fork_at(request).await
    }
}

#[async_trait::async_trait]
// The compile error should direct wrappers to implement the capability, not
// suggest replacing their factory with this concrete implementation.
#[diagnostic::do_not_recommend]
impl crate::AttachmentRootSet for ObservedSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<AttachmentId>, StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_attachment_ref(attachment_id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

struct ObservedSessionStore {
    inner: Arc<dyn RuntimePersistence>,
    collector: CheckpointWriteCollector,
}

impl AttachmentManifest for ObservedSessionStore {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        self.inner.record_intent(intent)
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        self.inner.commit_refs(session_id, attachment_ids)
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        self.inner.list_uncommitted(older_than_epoch_ms)
    }

    fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        self.inner
            .forget_aged_uncommitted_intents(intent_grace_cutoff_epoch_ms)
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_ref_for_id(attachment_id, intent_grace_cutoff_epoch_ms)
    }

    fn forget(&self, session_id: &str, attachment_id: &AttachmentId) -> Result<(), StoreError> {
        self.inner.forget(session_id, attachment_id)
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<bool, StoreError> {
        self.inner.holds_ref(session_id, attachment_id)
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        self.inner.list_all_refs()
    }
}

#[async_trait::async_trait]
impl SessionCommitStore for ObservedSessionStore {
    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        self.inner.load_session().await
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.inner.load_session_head_meta().await
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        mut commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        self.collector.apply_mutation(&mut commit);
        let mut event = checkpoint_write_event(&commit);
        let budget = crate::testing::measure_runtime_commit_budget(&commit)?;
        let result = self.inner.commit_runtime_state(commit).await?;
        self.collector
            .record_manifest(&event.session_id, &result.manifest);
        if let Some(state) = event.state.as_mut()
            && let Some(accepted) = self.inner.load_session().await?
        {
            let read_model = accepted.current_frame_node_id.as_deref().map_or_else(
                || accepted.graph.read_model(),
                |frame_node_id| accepted.graph.read_model_for_frame(frame_node_id),
            );
            state.accepted_raw_rows = Some(serde_json::json!({
                "graph_nodes": accepted.graph.nodes,
                "graph_leaf_node_id": accepted.graph.leaf_node_id,
                "turn_state": accepted.checkpoint.as_ref().map(|checkpoint| &checkpoint.turn_state),
                "token_ledger": accepted.token_ledger,
            }));
            state.accepted_read_model = Some(serde_json::json!({
                "graph_node_count": accepted.graph.nodes.len(),
                "messages": read_model.messages.as_ref(),
                "token_usage": accepted.checkpoint.as_ref().map(|checkpoint| &checkpoint.turn_state.token_usage),
            }));
        }
        self.collector.push_runtime_commit(
            CheckpointWriteEvent {
                revision_after: result.head_revision,
                ..event
            },
            budget,
        );
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionAdmission, StoreError> {
        self.inner.admit_and_bind_session(binding).await
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        self.inner.load_session_meta().await
    }
}

fn checkpoint_write_event(commit: &RuntimeCommit) -> CheckpointWriteEvent {
    let checkpoint = &commit.checkpoint;
    let mut components = vec![CheckpointComponentWrite {
        component: CheckpointComponent::TurnState,
        kind: CheckpointComponentWriteKind::Stored {
            logical_bytes: checkpoint_encoded_len(&checkpoint.turn_state).ok(),
        },
    }];
    record_component(
        &mut components,
        CheckpointComponent::ToolState,
        checkpoint.component_ref(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT),
        checkpoint
            .component_body(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
            .map(|body| Some(body.len())),
    );
    record_component(
        &mut components,
        CheckpointComponent::PluginSnapshot,
        checkpoint.component_ref(crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT),
        checkpoint
            .component_body(crate::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT)
            .map(|body| Some(body.len())),
    );
    // Execution state is an opaque `Vec<u8>` the engine owns, so it is recorded
    // as written without a size. Measuring it the way typed components are
    // measured would serialize the bytes as a JSON decimal array, which reports
    // roughly 3.5x the real length and — because digits per byte depend on the
    // byte's value — shifts whenever an embedded identifier changes. That made
    // transcripts flaky on cosmetic churn while staying blind to real payload
    // differences, since genuinely different execution states round to the same
    // rendered size. `lash-sim`'s contract support omits it for the same reason.
    record_component(
        &mut components,
        CheckpointComponent::ExecutionState,
        checkpoint.component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT),
        checkpoint
            .component_body(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .map(|_| None),
    );
    CheckpointWriteEvent {
        schema: CHECKPOINT_WRITE_EVENT_SCHEMA.to_string(),
        session_id: commit.session_id.clone(),
        attributed_session_id: None,
        cause_boundary_id: None,
        commit_index: 0,
        turn_index: commit.checkpoint.turn_state.turn_index,
        revision_before: commit.expected_head_revision,
        revision_after: 0,
        usage: checkpoint_usage_write(commit),
        components,
        state: Some(CheckpointStateWrite {
            submitted_graph_append: serde_json::to_value(&commit.graph)
                .expect("runtime graph append is serializable"),
            submitted_turn_state: serde_json::to_value(&checkpoint.turn_state)
                .expect("runtime turn state is serializable"),
            submitted_usage_rows: serde_json::to_value(
                commit
                    .usage_deltas
                    .iter()
                    .map(|delta| &delta.entry)
                    .collect::<Vec<_>>(),
            )
            .expect("runtime usage rows are serializable"),
            accepted_raw_rows: None,
            accepted_read_model: None,
        }),
    }
}

fn checkpoint_usage_write(commit: &RuntimeCommit) -> CheckpointUsageWrite {
    let mut usage = CheckpointUsageWrite {
        entries: commit.usage_deltas.len(),
        ..CheckpointUsageWrite::default()
    };
    for delta in &commit.usage_deltas {
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(delta.entry.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(delta.entry.usage.output_tokens);
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(delta.entry.usage.cache_read_input_tokens);
        usage.cache_write_input_tokens = usage
            .cache_write_input_tokens
            .saturating_add(delta.entry.usage.cache_write_input_tokens);
        usage.reasoning_output_tokens = usage
            .reasoning_output_tokens
            .saturating_add(delta.entry.usage.reasoning_output_tokens);
    }
    usage
}

fn checkpoint_encoded_len(value: &impl Serialize) -> Result<usize, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(value).map(|bytes| bytes.len())
}

/// `logical_bytes` presence marks the component as written this commit; the
/// inner value is its rendered size, which callers omit for opaque blobs.
fn record_component(
    components: &mut Vec<CheckpointComponentWrite>,
    component: CheckpointComponent,
    component_ref: Option<&BlobRef>,
    logical_bytes: Option<Option<usize>>,
) {
    let kind = if let Some(logical_bytes) = logical_bytes {
        Some(CheckpointComponentWriteKind::Stored { logical_bytes })
    } else if component_ref.is_some() {
        Some(CheckpointComponentWriteKind::UnchangedRef)
    } else {
        None
    };
    if let Some(kind) = kind {
        components.push(CheckpointComponentWrite { component, kind });
    }
}

#[async_trait::async_trait]
impl TurnInputStore for ObservedSessionStore {
    async fn enqueue_pending_turn_input(
        &self,
        input: PendingTurnInputDraft,
    ) -> Result<PendingTurnInput, StoreError> {
        self.inner.enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<PendingTurnInput>, StoreError> {
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<TurnInputApplication>, StoreError> {
        self.inner.list_turn_input_applications(session_id).await
    }

    async fn cancel_pending_turn_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<PendingTurnInputCancelOutcome, StoreError> {
        self.inner
            .cancel_pending_turn_input(session_id, input_id)
            .await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[PendingTurnInputCancelTarget],
    ) -> Result<Vec<PendingTurnInputCancelResult>, StoreError> {
        self.inner
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<TurnInputClaim>, StoreError> {
        self.inner
            .claim_active_turn_inputs(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
            )
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<TurnInputClaim>, StoreError> {
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_turn_input_claim(&self, claim: &TurnInputClaim) -> Result<(), StoreError> {
        self.inner.abandon_turn_input_claim(claim).await
    }

    async fn abandon_turn_input_claims(&self, claims: &[TurnInputClaim]) -> Result<(), StoreError> {
        self.inner.abandon_turn_input_claims(claims).await
    }
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for ObservedSessionStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        claim_nonce: &crate::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        self.inner
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                claim_nonce,
                lease_ttl_ms,
            )
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        self.inner.release_session_execution_lease(completion).await
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError> {
        self.inner.get_session_execution_lease(session_id).await
    }
}

#[async_trait::async_trait]
impl crate::QueuedWorkStore for ObservedSessionStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        self.inner.enqueue_queued_work(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.inner
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.inner
            .claim_ready_queued_work(session_id, session_execution_lease, owner, boundary, policy)
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<(Option<TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        self.inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError> {
        self.inner
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                batch_ids,
                policy,
            )
            .await
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        self.inner.abandon_queued_work_claims(claims).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        self.inner
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn list_queued_work(&self, session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.inner.list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.inner.list_pending_queued_work(session_id).await
    }
}

#[async_trait::async_trait]
impl StoreMaintenance for ObservedSessionStore {
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError> {
        self.inner
            .seed_session_trigger_manifest_ref_for_testing(session_id)
            .await
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.inner
            .raw_session_owned_artifact_refs_for_testing(session_id)
            .await
    }

    async fn vacuum(&self) -> Result<VacuumReport, StoreError> {
        self.inner.vacuum().await
    }

    async fn gc_unreachable(&self) -> Result<GcReport, StoreError> {
        self.inner.gc_unreachable().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn committed_transcript_golden_carries_recorded_usage() {
        use crate::testing::behavior_transcript::{Actor, Entry, Transcript, Usage};

        let collector = CheckpointWriteCollector::default();
        let factory = ObservedSessionStoreFactory::new(
            Arc::new(crate::facade_support::InMemorySessionStoreFactory::new()),
            collector.clone(),
        );
        let store = factory
            .create_store(&SessionStoreCreateRequest {
                session_id: "observed-usage".to_string(),
                relation: crate::SessionRelation::Root,
                policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            })
            .await
            .expect("create observed store");
        let pending = Arc::new(Mutex::new(Vec::new()));
        let recorded = crate::TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_input_tokens: 3,
            cache_write_input_tokens: 2,
            reasoning_output_tokens: 4,
        };
        crate::runtime::record_token_usage_shared(&pending, "turn", "model-a", &recorded);
        let operation = crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("observed-usage-turn"),
            "append-session-nodes",
        );
        let staged = crate::runtime::stage_token_ledger_shared(&pending, &operation)
            .expect("stage recorded usage");
        let mut state = crate::RuntimeSessionState {
            session_id: "observed-usage".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let (commit, _) = RuntimeCommit::persisted_state_with_operation_and_staged_usage(
            &mut state,
            staged.deltas(),
            operation,
        )
        .expect("bind staged usage to commit envelope");
        let result = store
            .commit_runtime_state(commit)
            .await
            .expect("commit recorded usage");
        staged
            .confirm_identities(&result.committed_usage_delta_identities)
            .expect("confirm committed usage identities");

        let write = collector.events().pop().expect("observed accepted commit");
        let mut transcript = Transcript::new();
        transcript.record(Entry::commit(
            Actor::session(write.session_id),
            write.revision_before,
            write.revision_after,
            Usage::new(
                write.usage.entries,
                write.usage.input_tokens,
                write.usage.output_tokens,
                write.usage.cache_read_input_tokens,
                write.usage.cache_write_input_tokens,
                write.usage.reasoning_output_tokens,
            ),
        ));
        insta::assert_snapshot!(transcript.render(), @r#"
        session-001  commit    checkpoint.commit       rev=0->1
        session-001              usage                 entries=1 input=11 output=7 cache_read=3 cache_write=2 reasoning=4 total=23
        "#);
    }

    #[test]
    fn commit_observer_projects_typed_usage_buckets() {
        let state = crate::RuntimeSessionState {
            session_id: "observed-usage".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let commit = RuntimeCommit::persisted_state_for_test(
            &state,
            &[
                crate::TokenLedgerEntry {
                    source: "turn".to_string(),
                    model: "model-a".to_string(),
                    usage: crate::TokenUsage {
                        input_tokens: 11,
                        output_tokens: 7,
                        cache_read_input_tokens: 3,
                        cache_write_input_tokens: 2,
                        reasoning_output_tokens: 4,
                    },
                },
                crate::TokenLedgerEntry {
                    source: "child".to_string(),
                    model: "model-b".to_string(),
                    usage: crate::TokenUsage {
                        input_tokens: 5,
                        output_tokens: 6,
                        cache_read_input_tokens: 1,
                        cache_write_input_tokens: 0,
                        reasoning_output_tokens: 2,
                    },
                },
            ],
        );
        assert_eq!(
            checkpoint_usage_write(&commit),
            CheckpointUsageWrite {
                entries: 2,
                input_tokens: 16,
                output_tokens: 13,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 2,
                reasoning_output_tokens: 6,
            }
        );
    }

    #[test]
    fn logical_size_failure_degrades_to_unknown_stored_size() {
        struct AlwaysFails;

        impl serde::Serialize for AlwaysFails {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("fixture refuses encoding"))
            }
        }

        let size = checkpoint_encoded_len(&AlwaysFails);
        assert!(size.is_err(), "fixture must refuse MessagePack encoding");

        let mut components = Vec::new();
        record_component(
            &mut components,
            CheckpointComponent::ToolState,
            None,
            Some(size.ok()),
        );
        assert_eq!(
            components,
            vec![CheckpointComponentWrite {
                component: CheckpointComponent::ToolState,
                kind: CheckpointComponentWriteKind::Stored {
                    logical_bytes: None,
                },
            }]
        );
    }
}
