//! Public in-memory `RuntimePersistence` + `SessionStoreFactory`.
//!
//! Explicitly-wired ephemeral storage for inline-tier hosts that run background
//! processes without durable backing: a `process` started in a turn (or by a
//! trigger) is executed by the lease-protected worker, which rebuilds its
//! session from the store factory — so even an in-memory host needs a factory.
//! This explicit opt-in has no silent in-memory default and holds the same `RuntimePersistence` contract as the
//! durable backend (verified by the `runtime_persistence` conformance suite).
use crate::facade_support::SessionGraphFacadeOps;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::{SessionStoreCreateRequest, SessionStoreFactory};
use crate::store::RuntimePersistence;
mod attachments;
mod checkpoints;
mod factory;
pub use factory::InMemorySessionStoreFactory;
mod maintenance;
mod queued_work;
mod reachability;
mod reads;
mod receipts;
mod session_binding;
mod session_execution_lease;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(any(test, feature = "testing"))]
mod testing_access;
mod turn_input;

use receipts::{RuntimeTurnCommitMap, RuntimeTurnCommitRecord};

#[derive(Clone)]
struct InMemoryQueuedBatch {
    batch: crate::QueuedWorkBatch,
    claim_id: Option<String>,
    claim_token: Option<String>,
    claim_owner: Option<crate::LeaseOwnerIdentity>,
    claim_fencing_token: u64,
    claim_session_lease_generation: u64,
}

#[derive(Clone, Default)]
struct InMemorySessionExecutionLease {
    owner: Option<crate::LeaseOwnerIdentity>,
    lease_token: Option<String>,
    fencing_token: u64,
    claimed_at_epoch_ms: u64,
    expires_at_epoch_ms: u64,
}

#[derive(Clone)]
struct InMemoryPendingTurnInput {
    input: crate::PendingTurnInput,
    claim_id: Option<String>,
    claim_token: Option<String>,
    claim_owner: Option<crate::LeaseOwnerIdentity>,
    claim_fencing_token: u64,
    claim_session_lease_generation: u64,
}

impl InMemorySessionExecutionLease {
    fn is_live(&self, now: u64) -> bool {
        self.lease_token.is_some() && self.expires_at_epoch_ms > now
    }
}

#[derive(Clone, Copy)]
enum InMemoryQueuedWorkClaimKind {
    LeadingSessionCommand,
    TurnWork {
        boundary: crate::QueuedWorkClaimBoundary,
        max_batches: usize,
    },
}

type InMemoryNodeAnchorRecord = (crate::BlobRef, crate::HydratedSessionCheckpoint, String);
type InMemoryNodeAnchors = Arc<Mutex<HashMap<String, InMemoryNodeAnchorRecord>>>;

#[cfg(any(test, feature = "testing"))]
pub type RawPendingTurnInputForTesting = (
    String,
    u64,
    crate::TurnInputState,
    Option<String>,
    u64,
    Option<u64>,
);

#[cfg(any(test, feature = "testing"))]
pub type RawQueuedWorkForTesting = (
    crate::QueuedWorkBatch,
    Option<String>,
    Option<crate::LeaseOwnerIdentity>,
    bool,
    u64,
    Option<u64>,
);

pub struct InMemorySessionStore {
    clock: Arc<dyn crate::Clock>,
    /// Serializes every operation whose correctness depends on observing the
    /// session lease and mutating fenced runtime state atomically. Component
    /// mutexes still guard their data; this mutex supplies the transaction
    /// boundary and lock ordering that SQLite/Postgres provide natively.
    write_transaction: Arc<Mutex<()>>,
    pub(crate) session_head_meta: Mutex<Option<crate::SessionHeadMeta>>,
    pub(crate) session_meta: Mutex<Option<crate::SessionMeta>>,
    pub(crate) session_graph: Mutex<crate::SessionGraph>,
    global_session_graph: Arc<Mutex<crate::SessionGraph>>,
    global_node_owners: Arc<Mutex<HashMap<String, String>>>,
    global_session_heads: Arc<Mutex<HashMap<String, Option<String>>>>,
    node_anchors: InMemoryNodeAnchors,
    tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
    /// Permanent per-factory deletion ledger. Maintenance never prunes this:
    /// an id, once used and deleted in this store, must never be reused.
    deleted_session_ids: Arc<Mutex<HashSet<String>>>,
    pub(crate) checkpoint: Mutex<Option<crate::HydratedSessionCheckpoint>>,
    tool_state_blobs: Mutex<HashMap<crate::BlobRef, crate::ToolState>>,
    plugin_snapshot_blobs: Mutex<HashMap<crate::BlobRef, crate::PluginSessionSnapshot>>,
    execution_state_blobs: Mutex<HashMap<crate::BlobRef, Vec<u8>>>,
    pub(crate) usage_deltas: Mutex<Vec<crate::store::RuntimeUsageDelta>>,
    pub(crate) runtime_commit_count: Mutex<usize>,
    runtime_turn_commits: Mutex<RuntimeTurnCommitMap>,
    session_execution_leases: Mutex<HashMap<String, InMemorySessionExecutionLease>>,
    queued_work: Mutex<Vec<InMemoryQueuedBatch>>,
    queued_work_next_seq: Mutex<u64>,
    /// Receiver-side sender allocation floor. This is a redelivery fence, not
    /// a consumption watermark: selected-batch settlement may be out of order.
    wake_redelivery_fences: Mutex<HashMap<(String, String), u64>>,
    pending_turn_inputs: Mutex<Vec<InMemoryPendingTurnInput>>,
    pending_turn_input_next_seq: Mutex<u64>,
    attachment_manifest:
        Mutex<HashMap<(String, crate::AttachmentId), crate::AttachmentManifestEntry>>,
    #[cfg(test)]
    claim_after_lease_validation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    fail_next_exact_queue_claim: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    load_session_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_load_session_on_call: Mutex<Option<usize>>,
    #[cfg(test)]
    checkpoint_probe_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    commit_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_next_runtime_commit: Mutex<Option<crate::StoreError>>,
    #[cfg(test)]
    fail_next_runtime_commit_after_first_mutation: Mutex<Option<crate::StoreError>>,
    #[cfg(test)]
    fail_next_session_execution_lease_renewal: Mutex<Option<crate::StoreError>>,
    #[cfg(test)]
    session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    session_execution_lease_release_gate:
        Mutex<Option<Arc<test_support::SessionExecutionLeaseReleaseGate>>>,
    #[cfg(test)]
    session_execution_lease_release_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) session_admission_count: std::sync::atomic::AtomicUsize,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::SystemClock))
    }

    /// Return the active durable graph rows without constructing a session
    /// read model.
    ///
    /// Differential persistence tests use this testing-only seam because a
    /// [`crate::SessionGraph`] read model indexes duplicate node ids and would
    /// hide malformed durable rows.
    ///
    pub fn with_clock(clock: Arc<dyn crate::Clock>) -> Self {
        Self::with_shared_history(
            clock,
            Arc::new(Mutex::new(())),
            Arc::new(Mutex::new(crate::SessionGraph::default())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(HashSet::new())),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_shared_history(
        clock: Arc<dyn crate::Clock>,
        write_transaction: Arc<Mutex<()>>,
        global_session_graph: Arc<Mutex<crate::SessionGraph>>,
        global_node_owners: Arc<Mutex<HashMap<String, String>>>,
        global_session_heads: Arc<Mutex<HashMap<String, Option<String>>>>,
        node_anchors: InMemoryNodeAnchors,
        tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
        deleted_session_ids: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self {
            clock,
            write_transaction,
            session_head_meta: Mutex::new(None),
            session_meta: Mutex::new(None),
            session_graph: Mutex::new(crate::SessionGraph::default()),
            global_session_graph,
            global_node_owners,
            global_session_heads,
            node_anchors,
            tombstoned_node_ids,
            deleted_session_ids,
            checkpoint: Mutex::new(None),
            tool_state_blobs: Mutex::new(HashMap::new()),
            plugin_snapshot_blobs: Mutex::new(HashMap::new()),
            execution_state_blobs: Mutex::new(HashMap::new()),
            usage_deltas: Mutex::new(Vec::new()),
            runtime_commit_count: Mutex::new(0),
            runtime_turn_commits: Mutex::new(std::collections::HashMap::new()),
            session_execution_leases: Mutex::new(HashMap::new()),
            queued_work: Mutex::new(Vec::new()),
            queued_work_next_seq: Mutex::new(0),
            wake_redelivery_fences: Mutex::new(HashMap::new()),
            pending_turn_inputs: Mutex::new(Vec::new()),
            pending_turn_input_next_seq: Mutex::new(0),
            attachment_manifest: Mutex::new(HashMap::new()),
            #[cfg(test)]
            claim_after_lease_validation_hook: Mutex::new(None),
            #[cfg(test)]
            fail_next_exact_queue_claim: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            load_session_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_load_session_on_call: Mutex::new(None),
            #[cfg(test)]
            checkpoint_probe_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            commit_write_transaction_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_runtime_commit: Mutex::new(None),
            #[cfg(test)]
            fail_next_runtime_commit_after_first_mutation: Mutex::new(None),
            #[cfg(test)]
            fail_next_session_execution_lease_renewal: Mutex::new(None),
            #[cfg(test)]
            session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            session_execution_lease_release_gate: Mutex::new(None),
            #[cfg(test)]
            session_execution_lease_release_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            session_admission_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn verify_session_execution_lease(
        &self,
        session_id: &str,
        fence: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<(), crate::store::StoreError> {
        if fence.session_id != session_id {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: session_id.to_string(),
            });
        }
        let now = self.clock.timestamp_ms();
        let leases = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases");
        let Some(current) = leases.get(&fence.session_id) else {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if current
            .owner
            .as_ref()
            .is_some_and(|owner| owner.same_incarnation(&fence.owner))
            && current.lease_token.as_deref() == Some(fence.lease_token.as_str())
            && current.fencing_token == fence.fencing_token
            && current.expires_at_epoch_ms > now
        {
            Ok(())
        } else {
            Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            })
        }
    }

    /// The fencing token of the session's currently-live execution lease, or
    /// `None` when no live lease holds the session. A queued-work or turn-input
    /// claim is live for lease-less host callers exactly when the generation it
    /// pins equals this value (ADR 0029).
    fn live_session_lease_generation(&self, session_id: &str, now: u64) -> Option<u64> {
        let leases = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases");
        leases
            .get(session_id)
            .filter(|lease| lease.is_live(now))
            .map(|lease| lease.fencing_token)
    }

    fn release_session_execution_lease_in_memory(
        &self,
        completion: &crate::SessionExecutionLeaseAuthority,
    ) {
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases");
        if let Some(current) = leases.get_mut(&completion.session_id)
            && current
                .owner
                .as_ref()
                .is_some_and(|owner| owner.same_incarnation(&completion.owner))
            && current.lease_token.as_deref() == Some(completion.lease_token.as_str())
            && current.fencing_token == completion.fencing_token
        {
            current.owner = None;
            current.lease_token = None;
            current.claimed_at_epoch_ms = 0;
            current.expires_at_epoch_ms = 0;
        }
    }

    fn in_memory_session_execution_lease(
        session_id: &str,
        current: &InMemorySessionExecutionLease,
    ) -> crate::SessionExecutionLease {
        crate::SessionExecutionLease {
            session_id: session_id.to_string(),
            owner: current.owner.clone().expect("live lease owner set"),
            lease_token: current.lease_token.clone().expect("live lease token set"),
            fencing_token: current.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_epoch_ms,
            expires_at_epoch_ms: current.expires_at_epoch_ms,
        }
    }

    fn acquire_session_execution_lease_in_memory(
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        current: &mut InMemorySessionExecutionLease,
        now: u64,
        lease_ttl_ms: u64,
    ) -> crate::SessionExecutionLease {
        current.fencing_token = current.fencing_token.saturating_add(1);
        current.owner = Some(owner.clone());
        current.lease_token = Some(format!(
            "{}:{}:{}:{now}:{}",
            session_id, owner.owner_id, owner.incarnation_id, current.fencing_token
        ));
        current.claimed_at_epoch_ms = now;
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
        Self::in_memory_session_execution_lease(session_id, current)
    }

    fn queued_batch_work_class(
        batch: &crate::QueuedWorkBatch,
    ) -> Result<crate::store::QueuedWorkClass, crate::store::StoreError> {
        batch.work_class().ok_or_else(|| {
            crate::store::StoreError::Backend(format!(
                "queued-work batch `{}` has mixed or empty payload classes",
                batch.batch_id
            ))
        })
    }

    fn claim_ready_queued_work_in_memory(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        kind: InMemoryQueuedWorkClaimKind,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        self.claim_ready_queued_work_after_lease_validation(
            session_id,
            session_execution_lease,
            owner,
            kind,
        )
    }

    fn claim_ready_queued_work_after_lease_validation(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        kind: InMemoryQueuedWorkClaimKind,
    ) -> Result<Option<crate::QueuedWorkClaim>, crate::store::StoreError> {
        let max_batches = match kind {
            InMemoryQueuedWorkClaimKind::LeadingSessionCommand => 1,
            InMemoryQueuedWorkClaimKind::TurnWork { max_batches, .. } => max_batches,
        };
        if max_batches == 0 {
            return Ok(None);
        }
        // The fence is validated live, so its fencing token is the currently-live
        // session-lease generation. A row is claimable when it is unheld or its
        // pinned generation differs from ours; same-generation self-steal is
        // therefore unrepresentable (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let now = self.clock.timestamp_ms();
        let mut queued = self.queued_work.lock().expect("lock queued work");
        queued.sort_by_key(|entry| entry.batch.enqueue_seq);
        let claim_available = |entry: &InMemoryQueuedBatch| {
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
            return Ok(None);
        }
        let candidates = claimable_indices
            .iter()
            .map(|index| {
                let batch = &queued[*index].batch;
                Ok(crate::store::queued_work::ClaimCandidate {
                    enqueue_seq: batch.enqueue_seq,
                    claim_fencing_token: queued[*index].claim_fencing_token,
                    work_class: Self::queued_batch_work_class(batch)?,
                    delivery_policy: batch.delivery_policy,
                    slot_policy: batch.slot_policy,
                    merge_key: batch.merge_key.clone(),
                })
            })
            .collect::<Result<Vec<_>, crate::store::StoreError>>()?;
        let selected_len = match kind {
            InMemoryQueuedWorkClaimKind::LeadingSessionCommand => {
                crate::store::queued_work::select_leading_session_command(&candidates)
            }
            InMemoryQueuedWorkClaimKind::TurnWork {
                boundary,
                max_batches,
            } => crate::store::queued_work::select_turn_work_claim_prefix(
                &candidates,
                boundary,
                max_batches,
            ),
        };
        if selected_len == 0 {
            return Ok(None);
        }
        let first_index = claimable_indices[0];
        let first = queued[first_index].batch.clone();
        let fencing_token = queued[first_index].claim_fencing_token.saturating_add(1);
        let claim_id = crate::store::queued_work::derive_claim_id(
            crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
            first.enqueue_seq,
            fencing_token,
        );
        let lease_token = format!(
            "{}:{}:{}:{claim_id}:{now}",
            session_id, owner.owner_id, owner.incarnation_id
        );
        let mut batches = Vec::new();
        for index in claimable_indices.into_iter().take(selected_len) {
            let entry = &mut queued[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = entry.claim_fencing_token.saturating_add(1);
            entry.claim_session_lease_generation = generation;
            batches.push(entry.batch.clone());
        }
        Ok(Some(crate::QueuedWorkClaim {
            session_id: session_id.to_string(),
            claim_id,
            owner: owner.clone(),
            lease_token,
            fencing_token,
            session_lease_generation: generation,
            data: crate::QueuedWorkClaimData { batches },
        }))
    }

    fn claim_pending_turn_inputs_in_memory(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
        mode: crate::TurnInputClaimMode,
    ) -> Result<Option<crate::TurnInputClaim>, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.verify_session_execution_lease(session_id, session_execution_lease)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        self.claim_pending_turn_inputs_after_lease_validation(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            mode,
        )
    }

    fn claim_pending_turn_inputs_after_lease_validation(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
        mode: crate::TurnInputClaimMode,
    ) -> Result<Option<crate::TurnInputClaim>, crate::store::StoreError> {
        if max_inputs == 0 {
            return Ok(None);
        }
        // Validated-live fence: its fencing token is the currently-live
        // session-lease generation. Rows pinned to it are our own live claims;
        // rows pinned to any other generation (or unheld) are claimable
        // (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let now = self.clock.timestamp_ms();
        let mut pending = self
            .pending_turn_inputs
            .lock()
            .expect("lock pending turn input");
        pending.sort_by_key(|entry| entry.input.enqueue_seq);
        let claim_available = |entry: &InMemoryPendingTurnInput| {
            entry.claim_token.is_none() || entry.claim_session_lease_generation != generation
        };
        let selected_indices = pending
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.input.session_id == session_id
                    && claim_available(entry)
                    && match &mode {
                        crate::TurnInputClaimMode::ActiveTurn {
                            turn_id,
                            checkpoint,
                        } => {
                            matches!(
                                entry.input.state,
                                crate::TurnInputState::PendingActive
                                    | crate::TurnInputState::Accepted
                            ) && entry
                                .input
                                .ingress
                                .active_turn_id()
                                .is_some_and(|active| active == turn_id.as_str())
                                && entry.input.ingress.admits_checkpoint(*checkpoint)
                        }
                        crate::TurnInputClaimMode::NextTurn => {
                            entry.input.state.is_next_turn_pending()
                        }
                    }
            })
            .map(|(index, _)| index)
            .take(max_inputs)
            .collect::<Vec<_>>();
        let Some(first_index) = selected_indices.first().copied() else {
            return Ok(None);
        };
        let fencing_token = pending[first_index].claim_fencing_token.saturating_add(1);
        let claim_id = crate::store::queued_work::derive_claim_id(
            crate::store::queued_work::ClaimIdDialect::RecordingTurnInput,
            pending[first_index].input.enqueue_seq,
            fencing_token,
        );
        let lease_token = format!(
            "{}:{}:{}:{claim_id}:{now}",
            session_id, owner.owner_id, owner.incarnation_id
        );
        let mut inputs = Vec::new();
        for index in selected_indices {
            let entry = &mut pending[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = entry.claim_fencing_token.saturating_add(1);
            entry.claim_session_lease_generation = generation;
            if matches!(mode, crate::TurnInputClaimMode::ActiveTurn { .. }) {
                entry.input.state = crate::TurnInputState::Accepted;
            }
            inputs.push(entry.input.clone());
        }
        Ok(Some(crate::TurnInputClaim {
            session_id: session_id.to_string(),
            claim_id,
            owner: owner.clone(),
            lease_token,
            fencing_token,
            session_lease_generation: generation,
            data: crate::TurnInputClaimData {
                mode,
                inputs,
                applications: Vec::new(),
            },
        }))
    }

    fn checkpoint_work_pending_in_memory(
        &self,
        session_id: &str,
        generation: u64,
        turn_id: &str,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<bool, crate::store::StoreError> {
        let has_turn_input = max_inputs > 0
            && self
                .pending_turn_inputs
                .lock()
                .expect("lock pending turn input")
                .iter()
                .any(|entry| {
                    entry.input.session_id == session_id
                        && matches!(
                            entry.input.state,
                            crate::TurnInputState::PendingActive | crate::TurnInputState::Accepted
                        )
                        && (entry.claim_token.is_none()
                            || entry.claim_session_lease_generation != generation)
                        && entry
                            .input
                            .ingress
                            .active_turn_id()
                            .is_some_and(|active| active == turn_id)
                        && entry.input.ingress.admits_checkpoint(checkpoint)
                });
        if has_turn_input || max_batches == 0 {
            return Ok(has_turn_input);
        }

        let now = self.clock.timestamp_ms();
        let queued = self.queued_work.lock().expect("lock queued work");
        let first_ready = queued
            .iter()
            .filter(|entry| {
                entry.batch.session_id == session_id
                    && entry.batch.available_at_ms <= now
                    && (entry.claim_token.is_none()
                        || entry.claim_session_lease_generation != generation)
            })
            .min_by_key(|entry| entry.batch.enqueue_seq);
        first_ready
            .map(|entry| {
                Self::queued_batch_work_class(&entry.batch).map(|class| {
                    class == crate::store::QueuedWorkClass::TurnWork
                        && entry.batch.delivery_policy
                            == crate::DeliveryPolicy::EarliestSafeBoundary
                })
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::store::SessionCommitStore for InMemorySessionStore {
    async fn load_session(
        &self,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::store::StoreError> {
        #[cfg(test)]
        let load_call = self
            .load_session_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        #[cfg(test)]
        if self
            .fail_load_session_on_call
            .lock()
            .expect("lock load-session failure injection")
            .is_some_and(|call| call == load_call)
        {
            self.fail_load_session_on_call
                .lock()
                .expect("lock load-session failure injection")
                .take();
            return Err(crate::StoreError::Backend(
                "injected load-session failure".to_string(),
            ));
        }
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory read snapshot");
        let Some(meta) = self.session_head_meta.lock().expect("lock store").clone() else {
            return Ok(None);
        };
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .clone();
        let global_graph = self
            .global_session_graph
            .lock()
            .expect("lock global graph")
            .clone();
        let mut graph = global_graph;
        graph.set_leaf_node_id(meta.leaf_node_id.clone());
        let mut graph = graph.trim_to_active_path();
        if !tombstoned.is_empty() {
            let leaf_node_id = graph
                .leaf_node_id
                .clone()
                .filter(|leaf| !tombstoned.contains(leaf));
            graph = crate::SessionGraph::from_nodes(
                graph
                    .nodes
                    .iter()
                    .filter(|node| !tombstoned.contains(&node.node_id))
                    .cloned()
                    .collect(),
                leaf_node_id,
            );
        }
        Ok(Some(crate::store::PersistedSessionRead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            config: meta.config,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            checkpoint_ref: meta.checkpoint_ref,
            checkpoint: self.checkpoint.lock().expect("lock checkpoint").clone(),
            token_ledger: crate::store::merge_token_ledger_entries_checked(
                self.usage_deltas
                    .lock()
                    .expect("lock usage deltas")
                    .iter()
                    .map(|delta| delta.entry.clone())
                    .collect(),
            )?,
        }))
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, crate::store::StoreError> {
        if self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .contains(node_id)
        {
            return Ok(None);
        }
        if !self.node_visible_to_bound_session(node_id) {
            return Ok(None);
        }
        let graph = self.global_session_graph.lock().expect("lock global graph");
        Ok(graph.find_node(node_id).cloned())
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitResult, crate::store::StoreError> {
        commit.validate_budget()?;
        commit.validate_operation_session()?;
        let session_id = commit.session_id.clone();
        let turn_commit_hash = commit.turn_commit_hash()?;
        let turn_input_applications = commit.turn_input_applications();
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| crate::session_graph::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        #[cfg(test)]
        self.commit_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if let Some(error) = self
            .fail_next_runtime_commit
            .lock()
            .expect("lock next runtime commit failure")
            .take()
        {
            return Err(error);
        }
        let mut meta = self.session_head_meta.lock().expect("lock store");
        let actual = meta.as_ref().map_or(0, |meta| meta.head_revision);
        if let Some(bound) = meta.as_ref().map(|meta| meta.session_id.clone())
            && bound != commit.session_id
        {
            return Err(crate::store::StoreError::SessionBindingMismatch {
                bound_session_id: bound,
                attempted_session_id: commit.session_id,
            });
        }
        if let Some(batch) = commit
            .enqueued_queue_batches
            .iter()
            .find(|batch| batch.session_id != commit.session_id)
        {
            return Err(crate::store::StoreError::SessionBindingMismatch {
                bound_session_id: commit.session_id.clone(),
                attempted_session_id: batch.session_id.clone(),
            });
        }
        for batch in &commit.enqueued_queue_batches {
            batch
                .validate_process_wake_source()
                .map_err(crate::store::StoreError::Backend)?;
        }
        #[cfg(test)]
        let session_meta_before_commit =
            self.session_meta.lock().expect("lock session meta").clone();
        self.ensure_session_metadata_for_commit(&commit)?;
        #[cfg(test)]
        self.fail_after_first_runtime_commit_mutation_if_requested(session_meta_before_commit)?;
        commit.validate_node_derivation()?;
        let completed = &commit.turn_commit;
        let operation_key = completed.operation.storage_key()?;
        let key = (session_id.clone(), operation_key.clone());
        if let Some(stored) = self
            .runtime_turn_commits
            .lock()
            .expect("lock runtime turn commits")
            .get(&key)
            .cloned()
        {
            let result = receipts::replay_existing_runtime_commit(
                stored,
                &turn_commit_hash,
                completed,
                session_id,
                operation_key,
            )?;
            if let Some(completion) = commit.release_session_execution_lease.as_ref() {
                self.release_session_execution_lease_in_memory(completion);
            }
            return Ok(result);
        }
        receipts::enforce_fresh_append_ancestor(&self.session_graph, completed)?;
        let expected = commit.expected_head_revision;
        if expected != actual {
            return Err(crate::store::StoreError::HeadRevisionConflict {
                expected: commit.expected_head_revision,
                actual,
            });
        }
        let (tool_state_ref, tool_state) = checkpoints::resolve_component(
            &self.tool_state_blobs,
            "tool-state",
            commit.checkpoint.tool_state.as_ref(),
            commit.checkpoint.tool_state_ref.as_ref(),
        )?;
        let (plugin_snapshot_ref, plugin_snapshot) = checkpoints::resolve_component(
            &self.plugin_snapshot_blobs,
            "plugin-snapshot",
            commit.checkpoint.plugin_snapshot.as_ref(),
            commit.checkpoint.plugin_snapshot_ref.as_ref(),
        )?;
        let (execution_state_ref, execution_state) = checkpoints::resolve_component(
            &self.execution_state_blobs,
            "execution-state",
            commit.checkpoint.execution_state.as_ref(),
            commit.checkpoint.execution_state_ref.as_ref(),
        )?;
        let hydrated_checkpoint = crate::HydratedSessionCheckpoint {
            turn_state: commit.checkpoint.turn_state.clone(),
            tool_state_ref,
            tool_state,
            plugin_snapshot_ref,
            plugin_snapshot,
            plugin_snapshot_revision: commit.checkpoint.plugin_snapshot_revision,
            execution_state_ref,
            execution_state,
        };
        commit.validate_append_node_ids_unique()?;
        commit.graph.validate_append_topology()?;
        let incoming_nodes = commit.graph.nodes.as_slice();
        let mut global_node_owners = self
            .global_node_owners
            .lock()
            .expect("lock global in-memory node ids");
        if let Some(node) = incoming_nodes
            .iter()
            .find(|node| global_node_owners.contains_key(&node.node_id))
        {
            return Err(crate::store::StoreError::NodeIdCollision {
                node_id: node.node_id.clone(),
            });
        }
        let graph = self.global_session_graph.lock().expect("lock global graph");
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        if let Some(node) = incoming_nodes.iter().find(|node| {
            graph.find_node(&node.node_id).is_some() || tombstoned.contains(&node.node_id)
        }) {
            return Err(crate::store::StoreError::NodeIdCollision {
                node_id: node.node_id.clone(),
            });
        }
        drop(graph);
        drop(tombstoned);
        let (has_existing_live_nodes, existing_leaf_is_live) = {
            let graph = self.global_session_graph.lock().expect("lock global graph");
            let tombstoned = self
                .tombstoned_node_ids
                .lock()
                .expect("lock tombstoned nodes");
            let has_existing_live_nodes = meta
                .as_ref()
                .and_then(|head| head.leaf_node_id.as_ref())
                .is_some_and(|node_id| !tombstoned.contains(node_id));
            let existing_leaf_is_live = commit.graph.leaf_node_id().is_some_and(|leaf_node_id| {
                !tombstoned.contains(leaf_node_id) && graph.find_node(leaf_node_id).is_some()
            });
            (has_existing_live_nodes, existing_leaf_is_live)
        };
        match commit.graph.leaf_node_id() {
            Some(leaf_node_id)
                if !commit
                    .graph
                    .appended_nodes()
                    .any(|node| &node.node_id == leaf_node_id)
                    && !existing_leaf_is_live =>
            {
                return Err(crate::store::StoreError::InvalidGraphLeaf {
                    leaf_node_id: Some(leaf_node_id.clone()),
                });
            }
            None if commit.graph.appended_nodes().next().is_some() || has_existing_live_nodes => {
                return Err(crate::store::StoreError::InvalidGraphLeaf { leaf_node_id: None });
            }
            _ => {}
        }
        let old_leaf_node_id = meta.as_ref().and_then(|head| head.leaf_node_id.clone());
        match commit.graph.nodes.first() {
            None if commit.graph.leaf_node_id != old_leaf_node_id => {
                return Err(crate::StoreError::InvalidGraphLeaf {
                    leaf_node_id: commit.graph.leaf_node_id.clone(),
                });
            }
            Some(first) if first.parent_node_id.as_ref() != old_leaf_node_id.as_ref() => {
                return Err(crate::StoreError::InvalidGraphParent {
                    node_id: first.node_id.clone(),
                    expected: old_leaf_node_id,
                    actual: first.parent_node_id.clone(),
                });
            }
            _ => {}
        }
        {
            let mut proposed = self
                .global_session_graph
                .lock()
                .expect("lock global graph")
                .clone();
            proposed.extend_node_records(commit.graph.nodes.iter().cloned());
            proposed.set_leaf_node_id(commit.graph.leaf_node_id().cloned());
            if let Some(leaf_node_id) = proposed.leaf_node_id.as_deref() {
                let derived = proposed
                    .nearest_frame_node_id(Some(leaf_node_id))
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| crate::store::StoreError::MissingFrameOpenAncestor {
                        leaf_node_id: leaf_node_id.to_string(),
                    })?;
                if commit.current_frame_node_id.as_deref() != Some(derived.as_str()) {
                    return Err(crate::store::StoreError::Backend(format!(
                        "current_frame_node_id {:?} does not match derived frame `{derived}`",
                        commit.current_frame_node_id
                    )));
                }
            } else if commit.current_frame_node_id.is_some() {
                return Err(crate::store::StoreError::InvalidGraphLeaf { leaf_node_id: None });
            }
        }
        let (staged_tombstoned_node_ids, staged_session_heads) = {
            let mut proposed = self
                .global_session_graph
                .lock()
                .expect("lock global graph")
                .clone();
            proposed.extend_node_records(commit.graph.nodes.iter().cloned());
            let new_leaf_node_id = commit.graph.leaf_node_id().cloned();
            proposed.set_leaf_node_id(new_leaf_node_id.clone());
            let old_leaf_node_id = meta.as_ref().and_then(|head| head.leaf_node_id.clone());
            let mut tombstoned = self
                .tombstoned_node_ids
                .lock()
                .expect("lock tombstoned nodes")
                .clone();
            let mut session_heads = self
                .global_session_heads
                .lock()
                .expect("lock global session heads")
                .clone();
            session_heads.insert(commit.session_id.clone(), new_leaf_node_id.clone());
            let anchored_node_ids = self
                .node_anchors
                .lock()
                .expect("lock node anchors")
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            for node in incoming_nodes {
                if let Some(parent_node_id) = &node.parent_node_id
                    && (proposed.find_node(parent_node_id).is_none()
                        || tombstoned.contains(parent_node_id))
                {
                    return Err(crate::store::StoreError::InvalidGraphParent {
                        node_id: node.node_id.clone(),
                        expected: node.parent_node_id.clone(),
                        actual: None,
                    });
                }
            }
            let mut live_child_counts = Self::live_child_counts(&proposed, &tombstoned);
            if old_leaf_node_id != new_leaf_node_id
                && let Some(old_leaf_node_id) = old_leaf_node_id
            {
                Self::reclaim_unreachable_ancestry(
                    &proposed,
                    &mut live_child_counts,
                    &mut tombstoned,
                    &old_leaf_node_id,
                    &session_heads,
                    &anchored_node_ids,
                );
            }
            (tombstoned, session_heads)
        };
        {
            let queued = self.queued_work.lock().expect("lock queued work");
            for completed in &commit.completed_queue_claims {
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
                    let row_id = completed.batch_ids.iter().find(|batch_id| {
                        !queued.iter().any(|entry| {
                            entry.batch.session_id == completed.session_id
                                && entry.batch.batch_id == **batch_id
                                && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                                && entry.claim_token.as_deref()
                                    == Some(completed.lease_token.as_str())
                        })
                    });
                    let current = row_id.and_then(|batch_id| {
                        queued.iter().find(|entry| {
                            entry.batch.session_id == completed.session_id
                                && entry.batch.batch_id == *batch_id
                        })
                    });
                    return Err(crate::store::StoreError::QueuedWorkClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
                        row_id: row_id.cloned().map(String::into_boxed_str),
                        superseding_claim_id: current
                            .and_then(|entry| entry.claim_id.clone())
                            .map(String::into_boxed_str),
                        superseding_session_lease_generation: current.and_then(|entry| {
                            entry
                                .claim_token
                                .as_ref()
                                .map(|_| Box::new(entry.claim_session_lease_generation))
                        }),
                    });
                }
            }
        }
        {
            let pending = self
                .pending_turn_inputs
                .lock()
                .expect("lock pending turn input");
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
                    let row_id = completed.input_ids.iter().find(|input_id| {
                        !pending.iter().any(|entry| {
                            entry.input.session_id == completed.session_id
                                && entry.input.input_id == **input_id
                                && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                                && entry.claim_token.as_deref()
                                    == Some(completed.lease_token.as_str())
                        })
                    });
                    let current = row_id.and_then(|input_id| {
                        pending.iter().find(|entry| {
                            entry.input.session_id == completed.session_id
                                && entry.input.input_id == *input_id
                        })
                    });
                    return Err(crate::store::StoreError::TurnInputClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
                        row_id: row_id.cloned().map(String::into_boxed_str),
                        superseding_claim_id: current
                            .and_then(|entry| entry.claim_id.clone())
                            .map(String::into_boxed_str),
                        superseding_session_lease_generation: current.and_then(|entry| {
                            entry
                                .claim_token
                                .as_ref()
                                .map(|_| Box::new(entry.claim_session_lease_generation))
                        }),
                    });
                }
            }
        }
        let manifest = crate::store::SessionCheckpoint::new(
            hydrated_checkpoint.turn_state.clone(),
            hydrated_checkpoint.tool_state_ref.clone(),
            hydrated_checkpoint.plugin_snapshot_ref.clone(),
            hydrated_checkpoint.plugin_snapshot_revision,
            hydrated_checkpoint.execution_state_ref.clone(),
        );
        let checkpoint_bytes = rmp_serde::to_vec_named(&manifest).map_err(|err| {
            crate::store::StoreError::Backend(format!(
                "failed to encode in-memory checkpoint manifest: {err}"
            ))
        })?;
        let checkpoint_ref = crate::BlobRef(crate::stable_hash::sha256_hex(&checkpoint_bytes));
        let operation_key = commit.turn_commit.operation.storage_key()?;
        let (
            staged_queued_work,
            staged_wake_redelivery_fences,
            staged_queued_work_next_seq,
            staged_enqueued_queue_batches,
        ) = {
            let mut queued = self.queued_work.lock().expect("lock queued work").clone();
            let mut fences = self
                .wake_redelivery_fences
                .lock()
                .expect("lock wake redelivery fences")
                .clone();
            let mut next_seq = *self
                .queued_work_next_seq
                .lock()
                .expect("lock queued work seq");
            for completed in &commit.completed_queue_claims {
                for entry in queued.iter().filter(|entry| {
                    entry.batch.session_id == completed.session_id
                        && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                        && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                        && completed.batch_ids.contains(&entry.batch.batch_id)
                }) {
                    if let Some((process_id, sequence)) =
                        entry
                            .batch
                            .items
                            .iter()
                            .find_map(|item| match &item.payload {
                                crate::QueuedWorkPayload::ProcessWake { wake } => {
                                    Some((wake.process_id.clone(), wake.sequence))
                                }
                                _ => None,
                            })
                    {
                        fences
                            .entry((entry.batch.session_id.clone(), process_id))
                            .and_modify(|allocation_floor| {
                                *allocation_floor = (*allocation_floor).max(sequence);
                            })
                            .or_insert(sequence);
                    }
                }
                queued.retain(|entry| {
                    !(entry.batch.session_id == completed.session_id
                        && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                        && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                        && completed.batch_ids.contains(&entry.batch.batch_id))
                });
            }
            let enqueued_at_ms = self.clock.timestamp_ms();
            let enqueued = commit
                .enqueued_queue_batches
                .iter()
                .cloned()
                .map(|batch| {
                    Self::enqueue_queued_work_for_state(
                        &mut queued,
                        &fences,
                        &mut next_seq,
                        batch,
                        enqueued_at_ms,
                    )
                    .map(crate::QueuedWorkEnqueueOutcome::into_batch)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (queued, fences, next_seq, enqueued)
        };
        let staged_pending_turn_inputs = {
            let mut pending = self
                .pending_turn_inputs
                .lock()
                .expect("lock pending turn input")
                .clone();
            for completed in &commit.completed_turn_input_claims {
                for entry in pending.iter_mut() {
                    if entry.input.session_id == completed.session_id
                        && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                        && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                        && completed.input_ids.contains(&entry.input.input_id)
                    {
                        entry.input.state = crate::TurnInputState::Completed;
                        entry.clear_claim();
                    }
                }
            }
            if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
                for entry in pending.iter_mut() {
                    if entry.input.session_id == commit.session_id
                        && entry.input.state == crate::TurnInputState::PendingActive
                        && entry
                            .input
                            .ingress
                            .active_turn_id()
                            .is_some_and(|active| active == turn_id)
                    {
                        entry.input.state = crate::TurnInputState::DeferredNextTurn;
                        entry.input.ingress = crate::TurnInputIngress::NextTurn;
                        entry.claim_id = None;
                        entry.claim_token = None;
                        entry.claim_owner = None;
                        entry.claim_session_lease_generation = 0;
                    }
                }
            }
            pending
        };

        *self.queued_work.lock().expect("lock queued work") = staged_queued_work;
        *self
            .wake_redelivery_fences
            .lock()
            .expect("lock wake redelivery fences") = staged_wake_redelivery_fences;
        *self
            .queued_work_next_seq
            .lock()
            .expect("lock queued work seq") = staged_queued_work_next_seq;
        *self
            .pending_turn_inputs
            .lock()
            .expect("lock pending turn input") = staged_pending_turn_inputs;
        let mut global_graph = self.global_session_graph.lock().expect("lock global graph");
        global_graph.extend_node_records(commit.graph.nodes.iter().cloned());
        let leaf_node_id = commit.graph.leaf_node_id.clone();
        let mut resident_graph = global_graph.clone();
        resident_graph.set_leaf_node_id(leaf_node_id.clone());
        resident_graph = resident_graph.trim_to_active_path();
        *self.session_graph.lock().expect("lock graph") = resident_graph;
        drop(global_graph);
        *self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes") = staged_tombstoned_node_ids;
        *self
            .global_session_heads
            .lock()
            .expect("lock global session heads") = staged_session_heads;
        for node in incoming_nodes {
            global_node_owners.insert(node.node_id.clone(), commit.session_id.clone());
        }
        drop(global_node_owners);
        {
            let mut usage_deltas = self.usage_deltas.lock().expect("lock usage deltas");
            for delta in &commit.usage_deltas {
                if !usage_deltas
                    .iter()
                    .any(|stored| stored.identity == delta.identity)
                {
                    usage_deltas.push(delta.clone());
                }
            }
        }
        if let (Some(blob_ref), Some(body)) = (
            hydrated_checkpoint.tool_state_ref.clone(),
            hydrated_checkpoint.tool_state.clone(),
        ) {
            self.tool_state_blobs
                .lock()
                .expect("lock in-memory tool-state blobs")
                .insert(blob_ref, body);
        }
        if let (Some(blob_ref), Some(body)) = (
            hydrated_checkpoint.plugin_snapshot_ref.clone(),
            hydrated_checkpoint.plugin_snapshot.clone(),
        ) {
            self.plugin_snapshot_blobs
                .lock()
                .expect("lock in-memory plugin-snapshot blobs")
                .insert(blob_ref, body);
        }
        if let (Some(blob_ref), Some(body)) = (
            hydrated_checkpoint.execution_state_ref.clone(),
            hydrated_checkpoint.execution_state.clone(),
        ) {
            self.execution_state_blobs
                .lock()
                .expect("lock in-memory execution-state blobs")
                .insert(blob_ref, body);
        }
        *self.checkpoint.lock().expect("lock checkpoint") = Some(hydrated_checkpoint);
        self.commit_attachment_refs_in_memory(&commit.session_id, &commit.committed_attachment_ids);
        self.commit_turn_attachment_intents(&commit.session_id, &commit.turn_commit);
        let head_revision = actual + 1;
        *meta = Some(crate::SessionHeadMeta::assemble(
            crate::SessionHeadPayload {
                schema_version: crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: commit.session_id,
                config: commit.config,
                current_frame_node_id: commit.current_frame_node_id,
            },
            head_revision,
            Some(checkpoint_ref.clone()),
            leaf_node_id.clone(),
        ));
        *self
            .runtime_commit_count
            .lock()
            .expect("lock runtime commit count") += 1;
        let result = crate::store::RuntimeCommitResult {
            head_revision,
            checkpoint_ref,
            manifest,
            committed_leaf_node_id: leaf_node_id,
            realized_node_timestamps,
            committed_usage_delta_identities: commit
                .usage_deltas
                .iter()
                .map(|delta| delta.identity.clone())
                .collect(),
            enqueued_queue_batches: staged_enqueued_queue_batches,
            turn_input_applications,
            receipt_replayed: false,
        };
        self.runtime_turn_commits
            .lock()
            .expect("lock runtime turn commits")
            .insert(
                (session_id, operation_key),
                RuntimeTurnCommitRecord {
                    turn_commit_hash,
                    result: result.clone(),
                    committed_at_ms: self.clock.timestamp_ms(),
                    request_identity_hash: commit.turn_commit.request_identity_hash.clone(),
                    requested_node_count: commit.turn_commit.requested_node_count,
                    _requested_ancestor_node_id: commit
                        .turn_commit
                        .requested_ancestor_node_id
                        .clone(),
                    identity_encoding_version: commit.turn_commit.identity_encoding_version,
                },
            );
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            self.release_session_execution_lease_in_memory(completion);
        }
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &crate::SessionBinding,
    ) -> Result<crate::SessionAdmission, crate::StoreError> {
        #[cfg(test)]
        self.session_admission_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        binding.validate()?;
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        if self
            .deleted_session_ids
            .lock()
            .expect("lock deleted session ids")
            .contains(&binding.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: binding.session_id.clone(),
            });
        }
        let mut durable = self.session_meta.lock().expect("lock session meta");
        if let Some(meta) = durable.as_ref() {
            if meta.session_id != binding.session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: meta.session_id.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
            return Ok(crate::SessionAdmission::Rebound);
        }
        *durable = Some(crate::SessionMeta {
            session_id: binding.session_id.clone(),
            session_name: binding.session_id.clone(),
            created_at: self.clock.timestamp_rfc3339(),
            model: binding.model_id.clone(),
            cwd: binding.cwd.clone(),
            relation: binding.relation.clone(),
        });
        Ok(crate::SessionAdmission::Created)
    }

    async fn save_session_meta(
        &self,
        meta: crate::store::SessionMeta,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.replace_session_meta(meta)
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<crate::store::SessionMeta>, crate::store::StoreError> {
        Ok(self.session_meta.lock().expect("lock session meta").clone())
    }
}
