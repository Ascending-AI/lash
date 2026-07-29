use std::sync::{Arc, Mutex};

use lash_core::runtime::{
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
};
use lash_core::store::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, PersistedSessionRead,
    RuntimeCommit, RuntimeCommitResult, RuntimePersistence, SessionCommitStore, StoreError,
};
use lash_core::{
    AttachmentId, BlobRef, CheckpointKind, ForkPoint, ForkSessionRequest, ForkSessionResult,
    GcReport, LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelResult, PendingTurnInputCancelTarget, PendingTurnInputDraft,
    SessionAdmission, SessionBinding, SessionExecutionLease, SessionExecutionLeaseClaimOutcome,
    SessionExecutionLeaseCompletion, SessionExecutionLeaseFence, SessionExecutionLeaseStore,
    SessionMeta, SessionStoreCreateRequest, SessionStoreFactory, StoreMaintenance,
    TurnInputApplication, TurnInputClaim, TurnInputStore, VacuumReport,
};
use serde::{Deserialize, Serialize};

pub const DURABLE_WRITE_EVENT_SCHEMA: &str = "lash.sim.durable-write-event.v1";

/// One checkpoint component observed at the successful store-commit seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableComponentWrite {
    pub component: String,
    pub kind: DurableComponentWriteKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DurableComponentWriteKind {
    Stored { bytes: usize },
    UnchangedRef,
}

/// A successful runtime-state commit as observed by the simulator's store
/// wrapper. Component bodies are inspected before delegation, while the
/// resulting head revision is recorded only after the backend accepts them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DurableWriteEvent {
    pub schema: String,
    pub session_id: String,
    pub commit_index: usize,
    pub turn_index: usize,
    pub revision_before: u64,
    pub revision_after: u64,
    pub components: Vec<DurableComponentWrite>,
}

impl DurableWriteEvent {
    pub fn has_unchanged_ref(&self) -> bool {
        self.components
            .iter()
            .any(|component| component.kind == DurableComponentWriteKind::UnchangedRef)
    }
}

/// Shared sink used by every store handle created during one generated run.
#[derive(Clone, Debug, Default)]
pub struct DurableWriteCollector {
    events: Arc<Mutex<Vec<DurableWriteEvent>>>,
    ref_only_mutation: Option<RefOnlyCommitMutation>,
}

#[derive(Clone, Debug)]
struct RefOnlyCommitMutation {
    session_id: String,
    revision_before: u64,
}

impl DurableWriteCollector {
    /// Configure the regression-test mutation that reproduces the missing-body
    /// defect: when an updated component also carries its prior ref, drop the
    /// body before the backend sees the commit.
    #[cfg(test)]
    pub fn with_ref_only_mutation(session_id: impl Into<String>, revision_before: u64) -> Self {
        Self {
            events: Arc::default(),
            ref_only_mutation: Some(RefOnlyCommitMutation {
                session_id: session_id.into(),
                revision_before,
            }),
        }
    }

    pub fn events(&self) -> Vec<DurableWriteEvent> {
        let mut events = self
            .events
            .lock()
            .expect("durable-write collector lock")
            .clone();
        events.sort_by(|left, right| {
            (
                left.session_id.as_str(),
                left.revision_before,
                left.revision_after,
                left.commit_index,
            )
                .cmp(&(
                    right.session_id.as_str(),
                    right.revision_before,
                    right.revision_after,
                    right.commit_index,
                ))
        });
        events
    }

    fn push(&self, mut event: DurableWriteEvent) {
        let mut events = self.events.lock().expect("durable-write collector lock");
        event.commit_index = events
            .iter()
            .filter(|recorded| recorded.session_id == event.session_id)
            .count()
            + 1;
        events.push(event);
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
        if commit.checkpoint.tool_state_ref.is_some() {
            commit.checkpoint.tool_state = None;
        }
        if commit.checkpoint.plugin_snapshot_ref.is_some() {
            commit.checkpoint.plugin_snapshot = None;
        }
        if commit.checkpoint.execution_state_ref.is_some() {
            commit.checkpoint.execution_state = None;
        }
    }
}

/// Simulator-only store factory decorator. It preserves the backend contract
/// exactly and adds observation only after a real commit succeeds.
pub struct ObservedSessionStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    collector: DurableWriteCollector,
}

impl ObservedSessionStoreFactory {
    pub fn new(inner: Arc<dyn SessionStoreFactory>, collector: DurableWriteCollector) -> Self {
        Self { inner, collector }
    }

    fn wrap(&self, inner: Arc<dyn RuntimePersistence>) -> Arc<dyn RuntimePersistence> {
        Arc::new(ObservedSessionStore {
            inner,
            collector: self.collector.clone(),
        })
    }
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
    collector: DurableWriteCollector,
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

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        mut commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        self.collector.apply_mutation(&mut commit);
        let event = durable_write_event(&commit);
        let result = self.inner.commit_runtime_state(commit).await?;
        self.collector.push(DurableWriteEvent {
            revision_after: result.head_revision,
            ..event
        });
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

fn durable_write_event(commit: &RuntimeCommit) -> DurableWriteEvent {
    let checkpoint = &commit.checkpoint;
    let mut components = vec![DurableComponentWrite {
        component: "turn_state".to_string(),
        kind: DurableComponentWriteKind::Stored {
            bytes: encoded_len(&checkpoint.turn_state),
        },
    }];
    record_component(
        &mut components,
        "tool_state",
        checkpoint.tool_state_ref.as_ref(),
        checkpoint.tool_state.as_ref().map(encoded_len),
    );
    record_component(
        &mut components,
        "plugin_snapshot",
        checkpoint.plugin_snapshot_ref.as_ref(),
        checkpoint.plugin_snapshot.as_ref().map(encoded_len),
    );
    record_component(
        &mut components,
        "execution_state",
        checkpoint.execution_state_ref.as_ref(),
        checkpoint.execution_state.as_ref().map(Vec::len),
    );
    DurableWriteEvent {
        schema: DURABLE_WRITE_EVENT_SCHEMA.to_string(),
        session_id: commit.session_id.clone(),
        commit_index: 0,
        turn_index: commit.checkpoint.turn_state.turn_index,
        revision_before: commit.expected_head_revision,
        revision_after: 0,
        components,
    }
}

fn encoded_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value)
        .expect("checkpoint components serialize before store commit")
        .len()
}

fn record_component(
    components: &mut Vec<DurableComponentWrite>,
    name: &str,
    component_ref: Option<&BlobRef>,
    body_bytes: Option<usize>,
) {
    let kind = if let Some(bytes) = body_bytes {
        Some(DurableComponentWriteKind::Stored { bytes })
    } else if component_ref.is_some() {
        Some(DurableComponentWriteKind::UnchangedRef)
    } else {
        None
    };
    if let Some(kind) = kind {
        components.push(DurableComponentWrite {
            component: name.to_string(),
            kind,
        });
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
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
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
        session_execution_lease: &SessionExecutionLeaseFence,
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
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        self.inner
            .try_claim_session_execution_lease(session_id, owner, lease_ttl_ms)
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseCompletion,
    ) -> Result<(), StoreError> {
        self.inner.release_session_execution_lease(completion).await
    }
}

#[async_trait::async_trait]
impl lash_core::QueuedWorkStore for ObservedSessionStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        self.inner.enqueue_queued_work(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.inner
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        max_batches: usize,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.inner
            .claim_ready_queued_work(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                max_batches,
            )
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<(Option<TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        self.inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                max_batches,
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        self.inner
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                batch_ids,
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
    async fn vacuum(&self) -> Result<VacuumReport, StoreError> {
        self.inner.vacuum().await
    }

    async fn gc_unreachable(&self) -> Result<GcReport, StoreError> {
        self.inner.gc_unreachable().await
    }
}
