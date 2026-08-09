//! Public in-memory `RuntimePersistence` + `SessionStoreFactory`.
//!
//! Explicitly-wired ephemeral storage for inline-tier hosts that run background
//! processes without durable backing: a `process` started in a turn (or by a
//! trigger) is executed by the lease-protected worker, which rebuilds its
//! session from the store factory — so even an in-memory host needs a factory.
//! This explicit opt-in has no silent in-memory default and holds the same `RuntimePersistence` contract as the
//! durable backend (verified by the `runtime_persistence` conformance suite).
use crate::facade_support::SessionGraphFacadeOps;
use lash_sansio::sync::MutexExt;

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
    /// Poison recovery is deliberate under ADR 0054. These critical sections
    /// must remain host-code-free: read clocks and invoke any other dynamic
    /// host surface before acquiring this lock, then carry inert values in.
    write_transaction: Arc<Mutex<()>>,
    pub(crate) session_head_meta: Mutex<Option<crate::SessionHeadMeta>>,
    pub(crate) session_meta: Mutex<Option<crate::SessionMeta>>,
    pub(crate) session_graph: Mutex<crate::SessionGraph>,
    /// Shared leafless node catalog; never treated as a resident graph without a real leaf grafted
    /// first.
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
    next_session_execution_lease_renewal_response: Mutex<Option<crate::SessionExecutionLease>>,
    #[cfg(test)]
    session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    session_execution_lease_release_gate:
        Mutex<Option<Arc<test_support::SessionExecutionLeaseReleaseGate>>>,
    #[cfg(test)]
    session_execution_lease_release_attempt_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    raw_counter_defects: Mutex<HashMap<String, i64>>,
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
            next_session_execution_lease_renewal_response: Mutex::new(None),
            #[cfg(test)]
            session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            session_execution_lease_release_gate: Mutex::new(None),
            #[cfg(test)]
            session_execution_lease_release_attempt_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            raw_counter_defects: Mutex::new(HashMap::new()),
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
        now: u64,
    ) -> Result<(), crate::store::StoreError> {
        let leases = self.session_execution_leases.lock_recover();
        crate::store::session_execution_lease::require_current_session_execution_lease(
            session_id,
            leases.get(session_id).map(|current| {
                crate::store::session_execution_lease::SessionExecutionLeaseFenceFacts {
                    owner: current.owner.as_ref(),
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
    /// claim is live for lease-less host callers exactly when the generation it
    /// pins equals this value (ADR 0029).
    fn live_session_lease_generation(&self, session_id: &str, now: u64) -> Option<u64> {
        let leases = self.session_execution_leases.lock_recover();
        leases
            .get(session_id)
            .filter(|lease| lease.is_live(now))
            .map(|lease| lease.fencing_token)
    }

    fn release_session_execution_lease_in_memory(
        &self,
        completion: &crate::SessionExecutionLeaseAuthority,
        trace_refusal: bool,
    ) -> bool {
        let mut leases = self.session_execution_leases.lock_recover();
        if let Some(current) = leases.get_mut(&completion.session_id)
            && current
                .owner
                .as_ref()
                .is_some_and(|owner| owner.same_incarnation(&completion.owner))
            && current.lease_token.as_deref() == Some(completion.lease_token.as_str())
        {
            current.owner = None;
            current.lease_token = None;
            current.claimed_at_epoch_ms = 0;
            current.expires_at_epoch_ms = 0;
            true
        } else {
            if trace_refusal {
                let current = leases.get(&completion.session_id);
                crate::store_backend_support::trace_session_execution_lease_refusal(
                    crate::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                    "token_scoped_release_did_not_match",
                    "in_memory_write_transaction",
                    completion,
                    crate::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                        current.and_then(|lease| lease.owner.as_ref()),
                        current.and_then(|lease| lease.lease_token.as_deref()),
                    ),
                );
            }
            false
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
        lease_token: &str,
        current: &mut InMemorySessionExecutionLease,
        now: u64,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLease, crate::StoreError> {
        current.fencing_token = crate::StoreError::checked_monotonic_increment(
            "session_execution_lease_fencing_token",
            current.fencing_token,
        )?;
        current.owner = Some(owner.clone());
        current.lease_token = Some(lease_token.to_string());
        current.claimed_at_epoch_ms = now;
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
        Ok(Self::in_memory_session_execution_lease(session_id, current))
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
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        self.claim_ready_queued_work_after_lease_validation(
            session_id,
            session_execution_lease,
            owner,
            kind,
            now,
        )
    }

    fn claim_ready_queued_work_after_lease_validation(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        kind: InMemoryQueuedWorkClaimKind,
        now: u64,
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
        let mut queued = self.queued_work.lock_recover();
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
        let selected_indices = claimable_indices
            .into_iter()
            .take(selected_len)
            .collect::<Vec<_>>();
        let next_fencing_tokens = selected_indices
            .iter()
            .map(|index| {
                crate::StoreError::checked_monotonic_increment(
                    "queued_work_claim_fencing_token",
                    queued[*index].claim_fencing_token,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let first_index = selected_indices[0];
        let first = queued[first_index].batch.clone();
        let fencing_token = next_fencing_tokens[0];
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
        for (index, next_fencing_token) in selected_indices.into_iter().zip(next_fencing_tokens) {
            let entry = &mut queued[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = next_fencing_token;
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
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(test)]
        self.run_claim_after_lease_validation_hook();
        self.claim_pending_turn_inputs_after_lease_validation(
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            mode,
            now,
        )
    }

    fn claim_pending_turn_inputs_after_lease_validation(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
        mode: crate::TurnInputClaimMode,
        now: u64,
    ) -> Result<Option<crate::TurnInputClaim>, crate::store::StoreError> {
        if max_inputs == 0 {
            return Ok(None);
        }
        // Validated-live fence: its fencing token is the currently-live
        // session-lease generation. Rows pinned to it are our own live claims;
        // rows pinned to any other generation (or unheld) are claimable
        // (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let mut pending = self.pending_turn_inputs.lock_recover();
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
        let next_fencing_tokens = selected_indices
            .iter()
            .map(|index| {
                crate::StoreError::checked_monotonic_increment(
                    "turn_input_claim_fencing_token",
                    pending[*index].claim_fencing_token,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let fencing_token = next_fencing_tokens[0];
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
        for (index, next_fencing_token) in selected_indices.into_iter().zip(next_fencing_tokens) {
            let entry = &mut pending[index];
            entry.claim_id = Some(claim_id.clone());
            entry.claim_token = Some(lease_token.clone());
            entry.claim_owner = Some(owner.clone());
            entry.claim_fencing_token = next_fencing_token;
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
            && self.pending_turn_inputs.lock_recover().iter().any(|entry| {
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
        let queued = self.queued_work.lock_recover();
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
        self.refuse_injected_counter_defect("session_head_revision")?;
        #[cfg(test)]
        let load_call = self
            .load_session_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        #[cfg(test)]
        if self
            .fail_load_session_on_call
            .lock_recover()
            .is_some_and(|call| call == load_call)
        {
            self.fail_load_session_on_call.lock_recover().take();
            return Err(crate::StoreError::Backend(
                "injected load-session failure".to_string(),
            ));
        }
        let _transaction = self.write_transaction.lock_recover();
        let Some(meta) = self.session_head_meta.lock_recover().clone() else {
            return Ok(None);
        };
        let tombstoned = self.tombstoned_node_ids.lock_recover().clone();
        let global_graph = self.global_session_graph.lock_recover().clone();
        let mut graph = global_graph;
        graph.set_leaf_node_id(meta.leaf_node_id.clone());
        let map_graph_corruption =
            |error: crate::StoreError| crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: error.to_string(),
            };
        let mut graph = graph
            .try_trim_to_active_path()
            .map_err(map_graph_corruption)?;
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
            )
            .map_err(map_graph_corruption)?;
        }
        graph
            .validate_resident_integrity()
            .map_err(map_graph_corruption)?;
        Ok(Some(crate::store::PersistedSessionRead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            config: meta.config,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            checkpoint_ref: meta.checkpoint_ref,
            checkpoint: self.checkpoint.lock_recover().clone(),
            token_ledger: crate::store::merge_token_ledger_entries_checked(
                self.usage_deltas
                    .lock_recover()
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
        if self.tombstoned_node_ids.lock_recover().contains(node_id) {
            return Ok(None);
        }
        if !self.node_visible_to_bound_session(node_id) {
            return Ok(None);
        }
        let graph = self.global_session_graph.lock_recover();
        Ok(graph.find_node(node_id).cloned())
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitResult, crate::store::StoreError> {
        let planner = crate::store::RuntimeCommitPlanner::prepare(commit)?;
        let commit = planner.commit();
        let session_id = commit.session_id.clone();
        let transaction_now = self.clock.timestamp_ms();
        let transaction_created_at = self.clock.timestamp_rfc3339();
        let _transaction = self.write_transaction.lock_recover();
        #[cfg(test)]
        self.commit_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.ensure_session_not_deleted(&session_id)?;
        if let Some(fence) = commit.session_execution_lease_fence.as_ref() {
            // This check-then-act read is atomic under the coarse write lock;
            // that serialization is intentional for the development backend.
            self.verify_session_execution_lease(&session_id, fence, transaction_now)?;
        }
        #[cfg(test)]
        if let Some(error) = self.fail_next_runtime_commit.lock_recover().take() {
            return Err(error);
        }
        let mut meta = self.session_head_meta.lock_recover();
        let actual = meta.as_ref().map_or(0, |meta| meta.head_revision);
        planner.validate_session_binding(meta.as_ref().map(|meta| meta.session_id.as_str()))?;
        #[cfg(test)]
        let session_meta_before_commit = self.session_meta.lock_recover().clone();
        self.ensure_session_metadata_for_commit(commit, &transaction_created_at)?;
        #[cfg(test)]
        self.fail_after_first_runtime_commit_mutation_if_requested(session_meta_before_commit)?;
        planner.validate_node_derivation()?;
        let key = (session_id.clone(), planner.operation_key().to_string());
        if let Some(stored) = self.runtime_turn_commits.lock_recover().get(&key).cloned() {
            let stored_count = stored
                .requested_node_count
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    crate::store::StoreError::Backend(
                        "stored append requested-node count does not fit u64".to_string(),
                    )
                })?;
            let prior = crate::store::RuntimeCommitReceiptRecord {
                turn_commit_hash: stored.turn_commit_hash,
                result: stored.result,
                request_identity_hash: stored.request_identity_hash,
                identity_encoding_version: stored.identity_encoding_version,
                requested_node_count: stored_count,
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
        let incoming_nodes = commit.graph.nodes.as_slice();
        let mut global_node_owners = self.global_node_owners.lock_recover();
        let graph = self.global_session_graph.lock_recover();
        let tombstoned = self.tombstoned_node_ids.lock_recover();
        let occupied_node_ids = incoming_nodes
            .iter()
            .filter(|node| {
                global_node_owners.contains_key(&node.node_id)
                    || graph.find_node(&node.node_id).is_some()
                    || tombstoned.contains(&node.node_id)
            })
            .map(|node| node.node_id.clone())
            .collect();
        drop(graph);
        drop(tombstoned);
        let (has_existing_live_nodes, selected_leaf_is_live, old_leaf_is_live) = {
            let graph = self.global_session_graph.lock_recover();
            let tombstoned = self.tombstoned_node_ids.lock_recover();
            let has_existing_live_nodes = global_node_owners.iter().any(|(node_id, owner)| {
                owner == &commit.session_id && !tombstoned.contains(node_id)
            });
            let selected_leaf_is_live = commit.graph.leaf_node_id().is_some_and(|leaf_node_id| {
                !tombstoned.contains(leaf_node_id) && graph.find_node(leaf_node_id).is_some()
            });
            let old_leaf_is_live = meta
                .as_ref()
                .and_then(|head| head.leaf_node_id.as_deref())
                .is_none_or(|leaf_node_id| {
                    !tombstoned.contains(leaf_node_id) && graph.find_node(leaf_node_id).is_some()
                });
            (
                has_existing_live_nodes,
                selected_leaf_is_live,
                old_leaf_is_live,
            )
        };
        let old_leaf_node_id = meta.as_ref().and_then(|head| head.leaf_node_id.clone());
        let requested_ancestor_is_active = commit
            .turn_commit
            .requested_ancestor_node_id
            .as_deref()
            .is_none_or(|required| {
                self.session_graph
                    .lock_recover()
                    .active_path_contains(required)
            });
        let mut proposed = self.global_session_graph.lock_recover().clone();
        proposed.extend_node_records(commit.graph.nodes.iter().cloned());
        proposed.set_leaf_node_id(commit.graph.leaf_node_id().cloned());
        let derived_frame_node_id = match proposed.leaf_node_id.as_deref() {
            Some(leaf_node_id) => proposed
                .nearest_frame_node_id(Some(leaf_node_id))
                .map(ToOwned::to_owned),
            None => None,
        };
        let plan = planner.plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: actual,
            old_leaf_node_id,
            requested_ancestor_is_active,
            occupied_node_ids,
            selected_leaf_is_live,
            has_live_nodes: has_existing_live_nodes,
            old_leaf_is_live,
            derived_frame_node_id,
        })?;
        let (staged_tombstoned_node_ids, staged_session_heads) = {
            let new_leaf_node_id = commit.graph.leaf_node_id().cloned();
            let mut tombstoned = self.tombstoned_node_ids.lock_recover().clone();
            let mut session_heads = self.global_session_heads.lock_recover().clone();
            session_heads.insert(commit.session_id.clone(), new_leaf_node_id.clone());
            let anchored_node_ids = self
                .node_anchors
                .lock_recover()
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            let mut live_child_counts = Self::live_child_counts(&proposed, &tombstoned);
            if plan.head_changed()
                && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
            {
                Self::reclaim_unreachable_ancestry(
                    &proposed,
                    &mut live_child_counts,
                    &mut tombstoned,
                    old_leaf_node_id,
                    &session_heads,
                    &anchored_node_ids,
                );
            }
            (tombstoned, session_heads)
        };
        {
            let queued = self.queued_work.lock_recover();
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
        let (
            staged_queued_work,
            staged_wake_redelivery_fences,
            staged_queued_work_next_seq,
            staged_enqueued_queue_batches,
        ) = {
            let mut queued = self.queued_work.lock_recover().clone();
            let mut fences = self.wake_redelivery_fences.lock_recover().clone();
            let mut next_seq = *self.queued_work_next_seq.lock_recover();
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
                        transaction_now,
                    )
                    .map(crate::QueuedWorkEnqueueOutcome::into_batch)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (queued, fences, next_seq, enqueued)
        };
        let staged_pending_turn_inputs = {
            let mut pending = self.pending_turn_inputs.lock_recover().clone();
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

        *self.queued_work.lock_recover() = staged_queued_work;
        *self.wake_redelivery_fences.lock_recover() = staged_wake_redelivery_fences;
        *self.queued_work_next_seq.lock_recover() = staged_queued_work_next_seq;
        *self.pending_turn_inputs.lock_recover() = staged_pending_turn_inputs;
        let mut global_graph = self.global_session_graph.lock_recover();
        global_graph.extend_node_records(commit.graph.nodes.iter().cloned());
        let leaf_node_id = commit.graph.leaf_node_id.clone();
        let mut resident_graph = global_graph.clone();
        resident_graph.set_leaf_node_id(leaf_node_id.clone());
        resident_graph = resident_graph.trim_to_active_path();
        *self.session_graph.lock_recover() = resident_graph;
        drop(global_graph);
        *self.tombstoned_node_ids.lock_recover() = staged_tombstoned_node_ids;
        *self.global_session_heads.lock_recover() = staged_session_heads;
        for node in incoming_nodes {
            global_node_owners.insert(node.node_id.clone(), commit.session_id.clone());
        }
        drop(global_node_owners);
        {
            let mut usage_deltas = self.usage_deltas.lock_recover();
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
            self.tool_state_blobs.lock_recover().insert(blob_ref, body);
        }
        if let (Some(blob_ref), Some(body)) = (
            hydrated_checkpoint.plugin_snapshot_ref.clone(),
            hydrated_checkpoint.plugin_snapshot.clone(),
        ) {
            self.plugin_snapshot_blobs
                .lock_recover()
                .insert(blob_ref, body);
        }
        if let (Some(blob_ref), Some(body)) = (
            hydrated_checkpoint.execution_state_ref.clone(),
            hydrated_checkpoint.execution_state.clone(),
        ) {
            self.execution_state_blobs
                .lock_recover()
                .insert(blob_ref, body);
        }
        *self.checkpoint.lock_recover() = Some(hydrated_checkpoint);
        self.commit_attachment_refs_in_memory(
            &commit.session_id,
            &commit.committed_attachment_ids,
            transaction_now,
        );
        self.commit_turn_attachment_intents(
            &commit.session_id,
            &commit.turn_commit,
            transaction_now,
        );
        *meta = Some(plan.head_meta(checkpoint_ref.clone()));
        *self.runtime_commit_count.lock_recover() += 1;
        let result = plan.result(checkpoint_ref, manifest, staged_enqueued_queue_batches);
        let receipt = plan.receipt_write(&result);
        self.runtime_turn_commits.lock_recover().insert(
            (session_id, receipt.operation_key.to_string()),
            RuntimeTurnCommitRecord {
                turn_commit_hash: receipt.turn_commit_hash.to_string(),
                result: result.clone(),
                committed_at_ms: transaction_now,
                request_identity_hash: receipt.request_identity_hash.map(str::to_string),
                requested_node_count: receipt.requested_node_count,
                _requested_ancestor_node_id: receipt.requested_ancestor_node_id.map(str::to_string),
                identity_encoding_version: receipt.identity_encoding_version,
            },
        );
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            let _release_was_current =
                self.release_session_execution_lease_in_memory(completion, false);
            // FIG-884: head CAS is commit authority; release is ancillary.
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
        let created_at = self.clock.timestamp_rfc3339();
        let _transaction = self.write_transaction.lock_recover();
        if self
            .deleted_session_ids
            .lock_recover()
            .contains(&binding.session_id)
        {
            return Err(crate::StoreError::SessionDeleted {
                session_id: binding.session_id.clone(),
            });
        }
        let mut durable = self.session_meta.lock_recover();
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
            created_at,
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
        let _transaction = self.write_transaction.lock_recover();
        self.replace_session_meta(meta)
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<crate::store::SessionMeta>, crate::store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}
