use lash_core::facade_support::SessionGraphFacadeOps;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::runtime::{
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary, QueuedWorkItem,
};
use lash_core::store;
use lash_core::store::{PersistedSessionRead, RuntimeCommitReceipt, SessionHeadMeta};
use lash_core::{
    BlobRef, GcReport, LeaseOwnerIdentity, QueuedWorkStore, RuntimeCommit, RuntimePersistence,
    SelectedQueuedWorkClaimOutcome, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore, SessionGraph, SessionNodeRecord,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, StoreMaintenance, TurnInputStore,
    VacuumReport, facade_support::current_epoch_ms,
};

mod queued_work;
use queued_work::RuntimePerfQueuedBatch;

#[derive(Clone)]
struct RuntimePerfPendingTurnInput {
    input: lash_core::PendingTurnInput,
    claim_id: Option<String>,
    claim_token: Option<String>,
    claim_owner: Option<LeaseOwnerIdentity>,
    claim_fencing_token: u64,
    claim_session_lease_generation: u64,
}

impl RuntimePerfPendingTurnInput {
    fn claim_diagnostics(&self) -> Option<lash_core::PendingTurnInputClaimDiagnostics> {
        (self.claim_id.is_some() || matches!(self.input.state, lash_core::TurnInputState::Accepted))
            .then(|| lash_core::PendingTurnInputClaimDiagnostics {
                state: self.input.state,
                claim_id: self.claim_id.clone(),
                claim_owner: self.claim_owner.clone(),
                claim_session_lease_generation: self
                    .claim_token
                    .as_ref()
                    .map(|_| self.claim_session_lease_generation),
                claim_fencing_token: self.claim_fencing_token,
            })
    }

    fn clear_claim(&mut self) {
        self.claim_id = None;
        self.claim_token = None;
        self.claim_owner = None;
        self.claim_session_lease_generation = 0;
    }

    fn cancel_outcome(&mut self, claim_is_live: bool) -> lash_core::PendingTurnInputCancelOutcome {
        match self.input.state {
            lash_core::TurnInputState::Cancelled => {
                lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(self.input.clone())
            }
            lash_core::TurnInputState::Completed => {
                lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted(self.input.clone())
            }
            lash_core::TurnInputState::Accepted => {
                lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    input: self.input.clone(),
                    claim: self.claim_diagnostics(),
                }
            }
            lash_core::TurnInputState::PendingActive
            | lash_core::TurnInputState::DeferredNextTurn => {
                if self.claim_token.is_some() && claim_is_live {
                    lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        input: self.input.clone(),
                        claim: self.claim_diagnostics(),
                    }
                } else {
                    self.input.state = lash_core::TurnInputState::Cancelled;
                    self.clear_claim();
                    lash_core::PendingTurnInputCancelOutcome::Cancelled(self.input.clone())
                }
            }
        }
    }
}

fn find_pending_turn_input_index(
    pending: &[RuntimePerfPendingTurnInput],
    session_id: &str,
    target: &lash_core::PendingTurnInputCancelTarget,
) -> Option<usize> {
    pending.iter().position(|entry| {
        entry.input.session_id == session_id
            && match target {
                lash_core::PendingTurnInputCancelTarget::InputId(input_id) => {
                    entry.input.input_id == *input_id
                }
                lash_core::PendingTurnInputCancelTarget::SourceKey(source_key) => {
                    entry.input.source_key.as_deref() == Some(source_key.as_str())
                }
            }
    })
}

#[derive(Clone, Default)]
struct RuntimePerfSessionExecutionLease {
    owner: Option<LeaseOwnerIdentity>,
    executor_id: Option<String>,
    lease_token: Option<String>,
    fencing_token: u64,
    claimed_at_epoch_ms: u64,
    lease_term_ms: u64,
    expires_at_epoch_ms: u64,
}

impl RuntimePerfSessionExecutionLease {
    fn materialize(&self, session_id: &str) -> Option<SessionExecutionLease> {
        Some(SessionExecutionLease {
            session_id: session_id.to_string(),
            owner: self.owner.clone()?,
            executor_id: self.executor_id.clone()?,
            lease_token: self.lease_token.clone()?,
            fencing_token: self.fencing_token,
            claimed_at_epoch_ms: self.claimed_at_epoch_ms,
            lease_term_ms: self.lease_term_ms,
            expires_at_epoch_ms: self.expires_at_epoch_ms,
        })
    }
}

#[derive(Clone)]
enum RuntimePerfQueuedWorkClaimKind {
    LeadingSessionCommand,
    TurnWork {
        boundary: QueuedWorkClaimBoundary,
        policy: lash_core::QueuedWorkClaimPolicy,
    },
}

#[derive(Default)]
pub(crate) struct RuntimePerfStore {
    next_blob_id: AtomicU64,
    queued_work_next_seq: AtomicU64,
    session_head_meta: Mutex<Option<SessionHeadMeta>>,
    session_graph: Mutex<SessionGraph>,
    usage_deltas: Mutex<Vec<store::RuntimeUsageDelta>>,
    session_meta: Mutex<Option<store::SessionMeta>>,
    runtime_turn_commits: Mutex<HashMap<(String, String), (String, RuntimeCommitReceipt)>>,
    session_execution_leases: Mutex<HashMap<String, RuntimePerfSessionExecutionLease>>,
    queued_work: Mutex<Vec<RuntimePerfQueuedBatch>>,
    pending_turn_input_next_seq: AtomicU64,
    pending_turn_inputs: Mutex<Vec<RuntimePerfPendingTurnInput>>,
}

impl RuntimePerfStore {
    pub(crate) fn graph_node_count(&self) -> usize {
        self.session_graph.lock_recover().nodes.len()
    }

    fn enqueue_queued_work_in_memory(&self, batch: QueuedWorkBatchDraft) -> QueuedWorkBatch {
        let mut queued = self.queued_work.lock_recover();
        if let Some(source_key) = batch.source_key.as_deref()
            && let Some(existing) = queued.iter().find(|entry| {
                entry.batch.session_id == batch.session_id
                    && entry.batch.source_key.as_deref() == Some(source_key)
            })
        {
            return existing.batch.clone();
        }
        let enqueue_seq = self.queued_work_next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let batch_id = format!("perf-qwb-{enqueue_seq}");
        let kind = batch.kind();
        let stored = QueuedWorkBatch {
            batch_id: batch_id.clone(),
            session_id: batch.session_id,
            enqueue_seq,
            source_key: batch.source_key,
            delivery_policy: batch.delivery_policy,
            kind,
            authority: batch.authority,
            merge_key: batch.merge_key,
            available_at_ms: batch.available_at_ms,
            enqueued_at_ms: current_epoch_ms(),
            items: batch
                .payloads
                .into_iter()
                .enumerate()
                .map(|(index, payload)| QueuedWorkItem {
                    item_id: format!("{batch_id}:item:{index}"),
                    payload,
                })
                .collect(),
        };
        queued.push(RuntimePerfQueuedBatch {
            batch: stored.clone(),
            claim_id: None,
            claim_token: None,
            claim_owner: None,
            claim_fencing_token: 0,
            claim_session_lease_generation: 0,
        });
        stored
    }

    fn verify_session_execution_lease(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let now = current_epoch_ms();
        let leases = self.session_execution_leases.lock_recover();
        lash_core::store_backend_support::require_current_session_execution_lease(
            session_id,
            leases.get(session_id).map(|current| {
                lash_core::store_backend_support::SessionExecutionLeaseFenceFacts {
                    owner: current.owner.as_ref(),
                    executor_id: current.executor_id.as_deref(),
                    lease_token: current.lease_token.as_deref(),
                    fencing_token: current.fencing_token,
                    expires_at_epoch_ms: current.expires_at_epoch_ms,
                }
            }),
            fence,
            now,
        )
    }

    /// The fencing token of the session's currently-live execution lease, or
    /// `None` when no live lease holds the session. A queued-work or turn-input
    /// claim is live for lease-less hosts when its generation equals this value (ADR 0029).
    fn live_session_lease_generation(&self, session_id: &str, now: u64) -> Option<u64> {
        let leases = self.session_execution_leases.lock_recover();
        leases
            .get(session_id)
            .filter(|lease| lease.lease_token.is_some() && lease.expires_at_epoch_ms > now)
            .map(|lease| lease.fencing_token)
    }

    fn release_session_execution_lease_in_memory(
        &self,
        completion: &SessionExecutionLeaseAuthority,
        trace_refusal: bool,
    ) -> bool {
        let mut leases = self.session_execution_leases.lock_recover();
        if let Some(current) = leases.get_mut(&completion.session_id)
            && current.owner.as_ref() == Some(&completion.owner)
            && current.executor_id.as_deref() == Some(completion.executor_id.as_str())
            && current.lease_token.as_deref() == Some(completion.lease_token.as_str())
        {
            current.owner = None;
            current.executor_id = None;
            current.lease_token = None;
            current.claimed_at_epoch_ms = 0;
            current.lease_term_ms = 0;
            current.expires_at_epoch_ms = 0;
            true
        } else {
            if trace_refusal {
                let current = leases.get(&completion.session_id);
                lash_core::store_backend_support::trace_session_execution_lease_refusal(
                    lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                    "token_scoped_release_did_not_match",
                    "perf_store_lock",
                    completion,
                    lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                        current.and_then(|lease| lease.owner.as_ref()),
                        current.and_then(|lease| lease.executor_id.as_deref()),
                        current.and_then(|lease| lease.lease_token.as_deref()),
                    ),
                );
            }
            false
        }
    }

    fn queued_batch_work_class(
        batch: &QueuedWorkBatch,
    ) -> Result<lash_core::store::QueuedWorkClass, StoreError> {
        batch.work_class().ok_or_else(|| {
            StoreError::Backend(format!(
                "queued-work batch `{}` has mixed or empty payload classes",
                batch.batch_id
            ))
        })
    }

    fn claim_ready_queued_work_perf(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        kind: RuntimePerfQueuedWorkClaimKind,
    ) -> Result<lash_core::QueuedWorkClaimOutcome, StoreError> {
        let max_batches = match &kind {
            RuntimePerfQueuedWorkClaimKind::LeadingSessionCommand => 1,
            RuntimePerfQueuedWorkClaimKind::TurnWork { policy, .. } => policy.max_rows,
        };
        if max_batches == 0 {
            return Ok(lash_core::QueuedWorkClaimOutcome::Refused(
                lash_core::QueuedWorkClaimRefusal::ZeroLimit,
            ));
        }
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        // The fence is validated live, so its fencing token is the currently-live
        // session-lease generation. A row is claimable when it is unheld or its
        // pinned generation differs from ours; same-generation self-steal is
        // therefore unrepresentable (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let now = current_epoch_ms();
        let mut queued = self.queued_work.lock_recover();
        let claim_available = |entry: &RuntimePerfQueuedBatch| {
            entry.claim_token.is_none() || entry.claim_session_lease_generation != generation
        };
        let claimable_indices = queued
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.batch.session_id == session_id
                    && entry.batch.available_at_ms <= now
                    && claim_available(entry)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if claimable_indices.is_empty() {
            // A lane with a still-deferred row is not an exhausted lane: the
            // work is intact and drains on a later attempt.
            let deferred_row_pending = queued.iter().any(|entry| {
                entry.batch.session_id == session_id
                    && entry.batch.available_at_ms > now
                    && claim_available(entry)
            });
            return Ok(lash_core::QueuedWorkClaimOutcome::Refused(
                if deferred_row_pending {
                    lash_core::QueuedWorkClaimRefusal::NotYetAvailable
                } else {
                    lash_core::QueuedWorkClaimRefusal::Empty
                },
            ));
        }
        let candidates = claimable_indices
            .iter()
            .map(|index| {
                let batch = &queued[*index].batch;
                Self::queued_batch_work_class(batch)?;
                Ok(store::queued_work::ClaimCandidate::from_batch(
                    batch,
                    queued[*index].claim_fencing_token,
                    queued[*index].claim_id.clone(),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let (selected_indices, refusal): (Vec<usize>, Option<lash_core::QueuedWorkClaimRefusal>) =
            match kind {
                RuntimePerfQueuedWorkClaimKind::LeadingSessionCommand => {
                    let selected_len =
                        store::queued_work::select_leading_session_command(&candidates);
                    (
                        claimable_indices
                            .iter()
                            .copied()
                            .take(selected_len)
                            .collect(),
                        // A successful selection has no refusal to report.
                        (selected_len == 0).then_some(lash_core::QueuedWorkClaimRefusal::Empty),
                    )
                }
                RuntimePerfQueuedWorkClaimKind::TurnWork { boundary, policy } => {
                    let selection = store::queued_work::select_turn_work_claim_indices(
                        &candidates,
                        boundary,
                        &policy,
                        now,
                    )?;
                    (
                        selection
                            .indices
                            .into_iter()
                            .map(|candidate_index| claimable_indices[candidate_index])
                            .collect(),
                        selection.refusal,
                    )
                }
            };
        if selected_indices.is_empty() {
            return Ok(lash_core::QueuedWorkClaimOutcome::Refused(
                refusal.unwrap_or(lash_core::QueuedWorkClaimRefusal::Empty),
            ));
        }
        let first_index = selected_indices[0];
        let first = queued[first_index].batch.clone();
        let abandon_restore_claim_id = queued[first_index].claim_id.clone();
        let fencing_token = queued[first_index].claim_fencing_token.saturating_add(1);
        let claim_id = store::queued_work::derive_claim_id(
            store::queued_work::ClaimIdDialect::PerformanceQueuedWork,
            first.enqueue_seq,
            fencing_token,
        );
        let lease_token = format!(
            "{session_id}:{}:{}:{claim_id}:{now}",
            owner.owner_id, owner.incarnation_id
        );
        let mut batches = Vec::new();
        for index in selected_indices {
            let entry = &mut queued[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = entry.claim_fencing_token.saturating_add(1);
            entry.claim_session_lease_generation = generation;
            batches.push(entry.batch.clone());
        }
        Ok(lash_core::QueuedWorkClaimOutcome::Claimed(
            QueuedWorkClaim {
                session_id: session_id.to_string(),
                claim_id,
                owner: owner.clone(),
                lease_token,
                fencing_token,
                session_lease_generation: generation,
                data: lash_core::store_backend_support::queued_work_claim_data(
                    batches,
                    abandon_restore_claim_id,
                ),
            },
        ))
    }

    fn claim_pending_turn_inputs_perf(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
        mode: lash_core::TurnInputClaimMode,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        if max_inputs == 0 {
            return Ok(None);
        }
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        // Validated-live fence: its fencing token is the currently-live
        // session-lease generation. Rows pinned to it are our own live claims;
        // rows pinned to any other generation (or unheld) are claimable
        // (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let now = current_epoch_ms();
        let mut pending = self.pending_turn_inputs.lock_recover();
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        let selected_indices = pending
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.input.session_id == session_id
                    && (entry.claim_token.is_none()
                        || entry.claim_session_lease_generation != generation)
            })
            .filter(|(_, entry)| match &mode {
                lash_core::TurnInputClaimMode::ActiveTurn {
                    turn_id,
                    checkpoint,
                } => {
                    matches!(
                        entry.input.state,
                        lash_core::TurnInputState::PendingActive
                            | lash_core::TurnInputState::Accepted
                    ) && entry
                        .input
                        .ingress
                        .active_turn_id()
                        .is_some_and(|active| active == turn_id.as_str())
                        && entry.input.ingress.admits_checkpoint(*checkpoint)
                }
                lash_core::TurnInputClaimMode::NextTurn => entry.input.state.is_next_turn_pending(),
            })
            .map(|(index, _)| index)
            .take(max_inputs)
            .collect::<Vec<_>>();
        let Some(first_index) = selected_indices.first().copied() else {
            return Ok(None);
        };
        let first = pending[first_index].input.clone();
        let fencing_token = pending[first_index].claim_fencing_token.saturating_add(1);
        let claim_id = store::queued_work::derive_claim_id(
            store::queued_work::ClaimIdDialect::PerformanceTurnInput,
            first.enqueue_seq,
            fencing_token,
        );
        let lease_token = format!(
            "{session_id}:{}:{}:{claim_id}:{now}",
            owner.owner_id, owner.incarnation_id
        );
        let state_after_claim = match mode {
            lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
            lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        let mut inputs = Vec::with_capacity(selected_indices.len());
        for index in selected_indices {
            let entry = &mut pending[index];
            entry.input.state = state_after_claim;
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = entry.claim_fencing_token.saturating_add(1);
            entry.claim_session_lease_generation = generation;
            inputs.push(entry.input.clone());
        }
        Ok(Some(lash_core::TurnInputClaim {
            session_id: session_id.to_string(),
            claim_id,
            owner: owner.clone(),
            lease_token,
            fencing_token,
            session_lease_generation: generation,
            data: lash_core::runtime::TurnInputClaimData {
                mode,
                inputs,
                applications: Vec::new(),
            },
        }))
    }
}

#[derive(Clone)]
pub(crate) struct RuntimePerfStoreFactory {
    pub(crate) store: Arc<RuntimePerfStore>,
    child_stores: Arc<Mutex<HashMap<String, Arc<RuntimePerfStore>>>>,
}

impl RuntimePerfStoreFactory {
    pub(crate) fn new(store: Arc<RuntimePerfStore>) -> Self {
        Self {
            store,
            child_stores: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

// RuntimePerfStore deliberately uses the no-op attachment manifest, so this
// benchmark factory explicitly owns no attachment roots.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for RuntimePerfStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, StoreError> {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for RuntimePerfStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        if request.parent_session_id().is_none() {
            return Ok(Arc::clone(&self.store) as Arc<dyn RuntimePersistence>);
        }
        let mut stores = self.child_stores.lock_recover();
        let store = stores
            .entry(request.session_id.clone())
            .or_insert_with(|| Arc::new(RuntimePerfStore::default()));
        Ok(Arc::clone(store) as Arc<dyn RuntimePersistence>)
    }

    // The perf harness never deletes: stores are retained for the whole run and
    // no tombstone is recorded.
    async fn session_was_deleted(&self, _session_id: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

lash_core::impl_noop_attachment_manifest!(RuntimePerfStore);

#[async_trait::async_trait]
impl SessionCommitStore for RuntimePerfStore {
    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, store::StoreError> {
        binding.validate()?;
        let mut meta = self.session_meta.lock_recover();
        if let Some(meta) = meta.as_ref() {
            if meta.session_id != binding.session_id {
                return Err(StoreError::SessionBindingMismatch {
                    bound_session_id: meta.session_id.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
            return Ok(lash_core::SessionAdmission::Rebound);
        }
        *meta = Some(store::SessionMeta {
            session_id: binding.session_id.clone(),
            relation: binding.relation.clone(),
        });
        Ok(lash_core::SessionAdmission::Created)
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, store::StoreError> {
        let Some(meta) = self.session_head_meta.lock_recover().clone() else {
            return Ok(None);
        };
        let graph = self.session_graph.lock_recover().trim_to_active_path();
        Ok(Some(PersistedSessionRead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            config: meta.config,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            checkpoint_ref: meta.checkpoint_ref,
            checkpoint: None,
            token_ledger: self
                .usage_deltas
                .lock_recover()
                .iter()
                .map(|delta| delta.entry.clone())
                .collect(),
        }))
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, store::StoreError> {
        Ok(self.session_head_meta.lock_recover().clone())
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<SessionNodeRecord>, store::StoreError> {
        Ok(self
            .session_graph
            .lock_recover()
            .find_node(node_id)
            .cloned())
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, store::StoreError> {
        let planner = store::RuntimeCommitPlanner::prepare(commit)?;
        let commit = planner.commit();
        let session_id = &commit.session_id;
        if let Some(fence) = commit.session_execution_lease_fence.as_ref() {
            // The measurement backend intentionally uses a coarse in-memory
            // check-then-act fence rather than modeling a production database.
            self.verify_session_execution_lease(session_id, fence)?;
        }
        let mut meta_guard = self.session_head_meta.lock_recover();
        planner
            .validate_session_binding(meta_guard.as_ref().map(|meta| meta.session_id.as_str()))?;
        planner.validate_node_derivation()?;
        let actual = meta_guard.as_ref().map_or(0, |meta| meta.head_revision);
        let key = (session_id.clone(), planner.operation_key().to_string());
        if let Some((stored_hash, result)) =
            self.runtime_turn_commits.lock_recover().get(&key).cloned()
        {
            let prior = store::RuntimeCommitReceiptRecord {
                turn_commit_hash: stored_hash,
                result,
                request_identity_hash: None,
                identity_encoding_version: None,
                requested_node_count: None,
            };
            let replay = planner
                .decide_receipt(Some(prior))?
                .expect("an existing receipt must produce replay or an error");
            if let Some(completion) = replay.release_session_execution_lease() {
                let _release_was_current =
                    self.release_session_execution_lease_in_memory(completion, false);
                // FIG-884: ancillary stale release must never veto a replayed commit.
            }
            return Ok(replay.into_result());
        }
        let graph_snapshot = self.session_graph.lock_recover().clone();
        let requested_ancestor_is_active = commit
            .turn_commit
            .requested_ancestor_node_id
            .as_deref()
            .is_none_or(|required| graph_snapshot.active_path_contains(required));
        let occupied_node_ids = commit
            .graph
            .nodes
            .iter()
            .filter(|node| graph_snapshot.find_node(&node.node_id).is_some())
            .map(|node| node.node_id.clone())
            .collect();
        let selected_leaf_is_live = commit
            .graph
            .leaf_node_id()
            .is_some_and(|leaf| graph_snapshot.find_node(leaf).is_some());
        let old_leaf_node_id = meta_guard
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone());
        let has_live_nodes = old_leaf_node_id.is_some();
        let old_leaf_is_live = old_leaf_node_id
            .as_deref()
            .is_none_or(|leaf| graph_snapshot.find_node(leaf).is_some());
        let parent_node_facts = old_leaf_node_id
            .as_deref()
            .map(|leaf_node_id| {
                let generation = graph_snapshot
                    .active_path_nodes()
                    .len()
                    .checked_sub(1)
                    .ok_or_else(|| StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "published leaf has an empty active path".to_string(),
                    })? as u64;
                let frame_node_id = graph_snapshot
                    .nearest_frame_node_id(Some(leaf_node_id))
                    .map(str::to_string)
                    .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
                        leaf_node_id: leaf_node_id.to_string(),
                    })?;
                Ok(store::ParentNodeFacts {
                    node_id: leaf_node_id.to_string(),
                    generation,
                    frame_node_id,
                })
            })
            .transpose()?;
        let plan = planner.plan(store::FreshRuntimeCommitFacts {
            actual_head_revision: actual,
            old_leaf_node_id,
            requested_ancestor_is_active,
            occupied_node_ids,
            selected_leaf_is_live,
            has_live_nodes,
            old_leaf_is_live,
            parent_node_facts,
        })?;
        {
            let pending = self.pending_turn_inputs.lock_recover();
            for completed in &commit.completed_turn_input_claims {
                let matches = pending
                    .iter()
                    .filter(|entry| {
                        entry.input.session_id == completed.session_id
                            && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                            && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                            && completed.input_ids.contains(&entry.input.input_id)
                    })
                    .count();
                if matches != completed.input_ids.len() {
                    return Err(StoreError::TurnInputClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
                        row_id: None,
                        superseding_claim_id: None,
                        superseding_session_lease_generation: None,
                    });
                }
            }
        }
        let mut graph = self.session_graph.lock_recover();
        graph.extend_node_records(commit.graph.nodes.iter().cloned());
        let leaf_node_id = commit.graph.leaf_node_id.clone();
        graph.set_leaf_node_id(leaf_node_id.clone());
        if !commit.usage_deltas.is_empty() {
            let mut stored_usage = self.usage_deltas.lock_recover();
            for delta in &commit.usage_deltas {
                if !stored_usage
                    .iter()
                    .any(|stored| stored.identity == delta.identity)
                {
                    stored_usage.push(delta.clone());
                }
            }
        }
        for completed in &commit.completed_queue_claims {
            let mut queued = self.queued_work.lock_recover();
            let matches = queued
                .iter()
                .filter(|entry| {
                    entry.batch.session_id == completed.session_id
                        && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                        && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                        && completed.batch_ids.contains(&entry.batch.batch_id)
                })
                .count();
            if matches != completed.batch_ids.len() {
                return Err(StoreError::QueuedWorkClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                    row_id: None,
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                });
            }
            queued.retain(|entry| {
                !(entry.batch.session_id == completed.session_id
                    && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                    && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                    && completed.batch_ids.contains(&entry.batch.batch_id))
            });
        }
        if !commit.completed_turn_input_claims.is_empty()
            || commit.interrupted_turn_input_turn_id.is_some()
        {
            let mut pending = self.pending_turn_inputs.lock_recover();
            for completed in &commit.completed_turn_input_claims {
                for entry in pending.iter_mut() {
                    if entry.input.session_id == completed.session_id
                        && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                        && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                        && completed.input_ids.contains(&entry.input.input_id)
                    {
                        entry.input.state = lash_core::TurnInputState::Completed;
                        entry.clear_claim();
                    }
                }
            }
            if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
                for entry in pending.iter_mut() {
                    if entry.input.session_id == *session_id
                        && entry.input.state == lash_core::TurnInputState::PendingActive
                        && entry.input.ingress.active_turn_id() == Some(turn_id)
                    {
                        entry.input.ingress = lash_core::TurnInputIngress::NextTurn;
                        entry.input.state = lash_core::TurnInputState::DeferredNextTurn;
                        entry.claim_id = None;
                        entry.claim_token = None;
                        entry.claim_owner = None;
                        entry.claim_session_lease_generation = 0;
                    }
                }
            }
        }
        let manifest = commit.checkpoint.manifest()?;
        drop(graph);
        let id = self.next_blob_id.fetch_add(1, Ordering::Relaxed);
        let checkpoint_ref = BlobRef(format!("perf-checkpoint-{id}"));
        *meta_guard = Some(plan.head_meta(checkpoint_ref.clone()));
        let result = plan.result(
            checkpoint_ref,
            manifest,
            commit
                .enqueued_queue_batches
                .iter()
                .cloned()
                .map(|batch| self.enqueue_queued_work_in_memory(batch))
                .collect(),
        );
        let receipt = plan.receipt_write(&result);
        self.runtime_turn_commits.lock_recover().insert(
            (session_id.clone(), receipt.operation_key.to_string()),
            (receipt.turn_commit_hash.to_string(), result.clone()),
        );
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            let _release_was_current =
                self.release_session_execution_lease_in_memory(completion, false);
            // FIG-884: head CAS is commit authority; release is ancillary.
        }
        Ok(result)
    }

    async fn save_session_meta(&self, meta: store::SessionMeta) -> Result<(), store::StoreError> {
        *self.session_meta.lock_recover() = Some(meta);
        Ok(())
    }

    async fn load_session_meta(&self) -> Result<Option<store::SessionMeta>, store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for RuntimePerfStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let lease_token = claim_nonce.as_str();
        let now = current_epoch_ms();
        let mut leases = self.session_execution_leases.lock_recover();
        let current = leases.entry(session_id.to_string()).or_default();
        if current.lease_token.is_some() && current.expires_at_epoch_ms > now {
            if current
                .owner
                .as_ref()
                .is_some_and(|current_owner| current_owner.same_incarnation(owner))
                && current.executor_id.as_deref() == Some(executor_id)
            {
                if current.lease_token.as_deref() != Some(lease_token) {
                    current.lease_token = Some(lease_token.to_string());
                }
                current.lease_term_ms = lease_ttl_ms;
                current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
                return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                    SessionExecutionLeaseAcquisition::fresh(
                        current.materialize(session_id).expect("live lease set"),
                    ),
                ));
            }
            return Ok(SessionExecutionLeaseClaimOutcome::Busy {
                holder: current.materialize(session_id).expect("live lease set"),
            });
        }
        // Read the lapsed holder before overwriting it. The claim is the only
        // atomic moment a takeover is observable, and the displaced runner is
        // usually why the lease lapsed, so it cannot be relied on to report it.
        // A double that skips this silently disables the takeover event for
        // whatever it stands in for.
        let displaced = current
            .owner
            .clone()
            .zip(current.executor_id.clone())
            .filter(|(previous, previous_executor_id)| {
                !previous.same_incarnation(owner) || previous_executor_id != executor_id
            })
            .map(|(previous, previous_executor_id)| {
                (
                    previous,
                    previous_executor_id,
                    current.fencing_token,
                    current.expires_at_epoch_ms,
                )
            });
        current.fencing_token = current.fencing_token.saturating_add(1);
        current.owner = Some(owner.clone());
        current.executor_id = Some(executor_id.to_string());
        current.lease_token = Some(lease_token.to_string());
        current.claimed_at_epoch_ms = now;
        current.lease_term_ms = lease_ttl_ms;
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
        let lease = current.materialize(session_id).expect("claimed lease set");
        // FIG-1573: no orphan repair here - a takeover does not prove the
        // previous turn is gone (cold recovery resumes it under the same turn
        // id). The runtime owns the repair.
        Ok(SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, previous_executor_id, generation, expired_at_epoch_ms)) => {
                    SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        previous_executor_id,
                        generation,
                        expired_at_epoch_ms,
                    )
                }
                None => SessionExecutionLeaseAcquisition::fresh(lease),
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let now = current_epoch_ms();
        let mut leases = self.session_execution_leases.lock_recover();
        let Some(current) = leases.get_mut(&fence.session_id) else {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if current.owner.as_ref() != Some(&fence.owner)
            || current.executor_id.as_deref() != Some(fence.executor_id.as_str())
            || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
        {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "perf_store_lock",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.owner.as_ref(),
                    current.executor_id.as_deref(),
                    current.lease_token.as_deref(),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseRenewalRefused {
                session_id: fence.session_id.clone(),
            });
        }
        if current.expires_at_epoch_ms <= now {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        current.lease_term_ms = lease_ttl_ms;
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
        Ok(current
            .materialize(&fence.session_id)
            .expect("renewed lease remains materialized"))
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        if self.release_session_execution_lease_in_memory(completion, true) {
            Ok(())
        } else {
            Err(StoreError::SessionExecutionLeaseReleaseRefused {
                session_id: completion.session_id.clone(),
            })
        }
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError> {
        let leases = self.session_execution_leases.lock_recover();
        Ok(leases
            .get(session_id)
            .and_then(|current| current.materialize(session_id)))
    }
}

#[async_trait::async_trait]
impl TurnInputStore for RuntimePerfStore {
    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        let mut pending = self.pending_turn_inputs.lock_recover();
        if let Some(source_key) = draft.source_key.as_deref()
            && let Some(existing) = pending.iter().find(|entry| {
                entry.input.session_id == draft.session_id
                    && entry.input.source_key.as_deref() == Some(source_key)
            })
        {
            if !draft
                .submitted_content_matches(&existing.input)
                .map_err(|err| {
                    StoreError::Backend(format!(
                        "failed to compare pending turn input submission: {err}"
                    ))
                })?
            {
                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                    session_id: draft.session_id.clone(),
                    source_key: source_key.to_string(),
                    existing_input_id: existing.input.input_id.clone(),
                });
            }
            return Ok(existing.input.clone());
        }
        let enqueue_seq = self
            .pending_turn_input_next_seq
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let input_id = draft
            .input_id
            .unwrap_or_else(|| format!("perf-ti-{enqueue_seq}"));
        let state = match draft.ingress {
            lash_core::TurnInputIngress::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputIngress::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        let stored = lash_core::PendingTurnInput {
            input_id,
            session_id: draft.session_id,
            enqueue_seq,
            source_key: draft.source_key,
            ingress: draft.ingress,
            state,
            enqueued_at_ms: current_epoch_ms(),
            input: draft.input,
        };
        pending.push(RuntimePerfPendingTurnInput {
            input: stored.clone(),
            claim_id: None,
            claim_token: None,
            claim_owner: None,
            claim_fencing_token: 0,
            claim_session_lease_generation: 0,
        });
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        Ok(stored)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::PendingTurnInput>, StoreError> {
        let now = current_epoch_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut inputs = self
            .pending_turn_inputs
            .lock_recover()
            .iter()
            .filter(|entry| {
                entry.input.session_id == session_id
                    && matches!(
                        entry.input.state,
                        lash_core::TurnInputState::PendingActive
                            | lash_core::TurnInputState::DeferredNextTurn
                    )
                    && (entry.claim_token.is_none()
                        || live_generation != Some(entry.claim_session_lease_generation))
            })
            .map(|entry| entry.input.clone())
            .collect::<Vec<_>>();
        inputs.sort_by_key(|input| input.enqueue_seq);
        Ok(inputs)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::TurnInputApplication>, StoreError> {
        let mut commits = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .filter(|((stored_session_id, _), _)| stored_session_id == session_id)
            .map(|((_, turn_id), (_, result))| {
                (
                    result.head_revision,
                    turn_id.clone(),
                    result.turn_input_applications.clone(),
                )
            })
            .collect::<Vec<_>>();
        commits.sort_by(|left, right| (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str())));
        Ok(commits
            .into_iter()
            .flat_map(|(_, _, applications)| applications)
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelReceipt>, StoreError> {
        let now = current_epoch_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome = match find_pending_turn_input_index(&pending, session_id, target) {
                Some(index) => {
                    let claim_is_live =
                        live_generation == Some(pending[index].claim_session_lease_generation);
                    pending[index].cancel_outcome(claim_is_live)
                }
                None => lash_core::PendingTurnInputCancelOutcome::NotFound,
            };
            results.push(lash_core::PendingTurnInputCancelReceipt {
                target: target.clone(),
                outcome,
            });
        }
        Ok(results)
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let now = current_epoch_ms();
        let live_generation = self.live_session_lease_generation(session_id, now);
        let mut pending = self.pending_turn_inputs.lock_recover();
        let Some(anchor_seq) = find_pending_turn_input_index(&pending, session_id, anchor)
            .map(|index| pending[index].input.enqueue_seq)
        else {
            return Ok(
                lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound {
                    anchor: anchor.clone(),
                },
            );
        };
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        let outcomes = pending
            .iter_mut()
            .filter(|entry| entry.input.session_id == session_id)
            .filter(|entry| entry.input.enqueue_seq >= anchor_seq)
            .map(|entry| {
                let claim_is_live = live_generation == Some(entry.claim_session_lease_generation);
                entry.cancel_outcome(claim_is_live)
            })
            .collect::<Vec<_>>();
        Ok(lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes {
            anchor: anchor.clone(),
            outcomes,
        })
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        self.claim_pending_turn_inputs_perf(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.clone(),
                checkpoint,
            },
        )
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        self.claim_pending_turn_inputs_perf(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::NextTurn,
        )
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::TurnInputClaim,
    ) -> Result<(), StoreError> {
        let mut pending = self.pending_turn_inputs.lock_recover();
        for entry in pending.iter_mut() {
            if entry.input.session_id == claim.session_id
                && entry.claim_id.as_deref() == Some(claim.claim_id.as_str())
                && entry.claim_token.as_deref() == Some(claim.lease_token.as_str())
            {
                if matches!(entry.input.state, lash_core::TurnInputState::Accepted) {
                    entry.input.state = match claim.mode {
                        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                            lash_core::TurnInputState::PendingActive
                        }
                        lash_core::TurnInputClaimMode::NextTurn => {
                            lash_core::TurnInputState::DeferredNextTurn
                        }
                    };
                }
                entry.claim_id = None;
                entry.claim_token = None;
                entry.claim_owner = None;
                entry.claim_session_lease_generation = 0;
            }
        }
        Ok(())
    }

    /// Re-defer active-turn-scoped inputs whose pinned turn can no longer commit
    /// (FIG-1573), sharing the row rule with every other backend.
    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> Result<usize, StoreError> {
        // Same order as the claim path: validate the fence, then hold the rows.
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut repaired = 0usize;
        for entry in pending.iter_mut() {
            if entry.input.session_id != session_id {
                continue;
            }
            if !lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
                scope,
                session_execution_lease.fencing_token,
                entry.input.state,
                &entry.input.ingress,
                entry.claim_token.is_some(),
                entry.claim_session_lease_generation,
            ) {
                continue;
            }
            entry.input.state = lash_core::TurnInputState::DeferredNextTurn;
            entry.input.ingress = lash_core::TurnInputIngress::NextTurn;
            entry.claim_id = None;
            entry.claim_token = None;
            entry.claim_owner = None;
            entry.claim_session_lease_generation = 0;
            repaired += 1;
        }
        Ok(repaired)
    }
}

#[async_trait::async_trait]
impl StoreMaintenance for RuntimePerfStore {
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        _session_id: &str,
    ) -> Result<bool, store::StoreError> {
        Err(unsupported_maintenance(
            "seed_session_trigger_manifest_ref_for_testing",
        ))
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        _session_id: &str,
    ) -> Result<Vec<(String, String)>, store::StoreError> {
        Err(unsupported_maintenance(
            "raw_session_owned_artifact_refs_for_testing",
        ))
    }

    /// The perf store implements no reclamation, and says so: an unimplemented
    /// lever fails rather than answering with an empty sweep it did not perform
    /// (ADR 0067 §4).
    async fn vacuum(&self) -> store::MaintenanceResult<VacuumReport> {
        Err(store::MaintenanceFailure::failed_before_any_work(
            unsupported_maintenance("vacuum"),
        ))
    }

    async fn gc_unreachable(&self) -> store::MaintenanceResult<GcReport> {
        Err(store::MaintenanceFailure::failed_before_any_work(
            unsupported_maintenance("gc_unreachable"),
        ))
    }
}

fn unsupported_maintenance(operation: &'static str) -> StoreError {
    StoreError::UnsupportedStoreOperation { operation }
}

#[cfg(test)]
mod tests;
