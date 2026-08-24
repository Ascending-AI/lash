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
#[cfg(any(test, feature = "testing"))]
pub use testing_access::RawSessionExecutionLeaseRow;
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
    executor_id: Option<String>,
    lease_token: Option<String>,
    fencing_token: u64,
    claimed_at_epoch_ms: u64,
    lease_term_ms: u64,
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

/// The in-memory store's turn-input settlement predicate.
///
/// One predicate, two regimes: the claim fields only strengthen it. A claimed
/// settlement requires the row to still carry that claim; an unclaimed
/// settlement requires it to still be unclaimed and unsettled
/// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md) §5).
fn turn_input_settlement_matches(
    entry: &InMemoryPendingTurnInput,
    completed: &crate::TurnInputCompletion,
) -> bool {
    entry.input.session_id == completed.session_id
        && completed.input_ids.contains(&entry.input.input_id)
        && match completed.claim.as_ref() {
            Some(claim) => {
                entry.claim_id.as_deref() == Some(claim.claim_id.as_str())
                    && entry.claim_token.as_deref() == Some(claim.lease_token.as_str())
            }
            None => {
                entry.claim_id.is_none()
                    && !matches!(
                        entry.input.state,
                        crate::TurnInputState::Completed | crate::TurnInputState::Cancelled
                    )
            }
        }
}

impl InMemorySessionExecutionLease {
    fn is_live(&self, now: u64) -> bool {
        self.lease_token.is_some() && self.expires_at_epoch_ms > now
    }
}

#[derive(Clone)]
enum InMemoryQueuedWorkClaimKind {
    LeadingSessionCommand,
    TurnWork {
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    },
}

type InMemoryNodeAnchorRecord = (crate::BlobRef, crate::HydratedSessionCheckpoint, String);
type InMemoryNodeAnchors = Arc<Mutex<HashMap<String, InMemoryNodeAnchorRecord>>>;
/// Session id -> component blob refs its live checkpoint references.
pub(crate) type SharedCheckpointBlobRoots = Arc<Mutex<HashMap<String, HashSet<crate::BlobRef>>>>;
pub(crate) type SharedSessionCatalog = Arc<Mutex<HashMap<String, crate::SessionSummary>>>;

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

/// Factory-global attachment condemnation state keyed by digest. See
/// [`crate::AttachmentCondemnation`] for the state machine.
pub(crate) type SharedAttachmentCondemnations =
    Arc<Mutex<HashMap<crate::AttachmentId, AttachmentCondemnationPhase>>>;

/// The two condemned phases. Absence from the map is the `Free` state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentCondemnationPhase {
    /// Claimed by a sweeper, no physical delete issued yet: a writer revokes it.
    Condemned,
    /// The physical delete is in flight: a writer must wait for the release.
    Deleting,
}

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
    pub(crate) bound_session_id: Mutex<Option<String>>,
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
    session_catalog: SharedSessionCatalog,
    pub(crate) checkpoint: Mutex<Option<crate::HydratedSessionCheckpoint>>,
    checkpoint_component_blobs: Arc<Mutex<HashMap<crate::BlobRef, Vec<u8>>>>,
    /// Factory-global reference edges from a session to the component blobs its
    /// *live* checkpoint holds. Edges, not counts (ADR 0067 §4): a commit
    /// replaces its session's edge set, a delete drops it, and
    /// `gc_unreachable` decides liveness by `NOT EXISTS` over the union. This
    /// is what lets the in-memory backend witness its own root set instead of
    /// reporting an unconditional empty sweep.
    pub(crate) checkpoint_blob_roots: SharedCheckpointBlobRoots,
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
    turn_cancel_requests: Mutex<HashMap<String, crate::TurnCancelRequestRecord>>,
    attachment_manifest:
        Mutex<HashMap<(String, crate::AttachmentId), crate::AttachmentManifestEntry>>,
    /// Per-digest attachment GC condemnation state, shared with every store the
    /// same factory owns because the digest is factory-global: the writer's
    /// intent insert and the sweeper's condemn CAS must meet here.
    pub(crate) attachment_condemnations: SharedAttachmentCondemnations,
    #[cfg(test)]
    claim_after_lease_validation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    fail_next_exact_queue_claim: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    list_pending_queued_work_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    load_session_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    load_session_head_meta_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_next_load_session_head_meta: std::sync::atomic::AtomicBool,
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
    force_next_session_execution_lease_renewal_zero_match: std::sync::atomic::AtomicBool,
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
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
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
        checkpoint_component_blobs: Arc<Mutex<HashMap<crate::BlobRef, Vec<u8>>>>,
        checkpoint_blob_roots: SharedCheckpointBlobRoots,
        tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
        deleted_session_ids: Arc<Mutex<HashSet<String>>>,
        session_catalog: SharedSessionCatalog,
        attachment_condemnations: SharedAttachmentCondemnations,
    ) -> Self {
        Self {
            clock,
            write_transaction,
            bound_session_id: Mutex::new(None),
            session_head_meta: Mutex::new(None),
            session_meta: Mutex::new(None),
            session_graph: Mutex::new(crate::SessionGraph::default()),
            global_session_graph,
            global_node_owners,
            global_session_heads,
            node_anchors,
            tombstoned_node_ids,
            deleted_session_ids,
            session_catalog,
            checkpoint: Mutex::new(None),
            checkpoint_component_blobs,
            checkpoint_blob_roots,
            usage_deltas: Mutex::new(Vec::new()),
            runtime_commit_count: Mutex::new(0),
            runtime_turn_commits: Mutex::new(std::collections::HashMap::new()),
            session_execution_leases: Mutex::new(HashMap::new()),
            queued_work: Mutex::new(Vec::new()),
            queued_work_next_seq: Mutex::new(0),
            wake_redelivery_fences: Mutex::new(HashMap::new()),
            pending_turn_inputs: Mutex::new(Vec::new()),
            pending_turn_input_next_seq: Mutex::new(0),
            turn_cancel_requests: Mutex::new(HashMap::new()),
            attachment_manifest: Mutex::new(HashMap::new()),
            attachment_condemnations,
            #[cfg(test)]
            claim_after_lease_validation_hook: Mutex::new(None),
            #[cfg(test)]
            fail_next_exact_queue_claim: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            list_pending_queued_work_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            load_session_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            load_session_head_meta_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_load_session_head_meta: std::sync::atomic::AtomicBool::new(false),
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
            force_next_session_execution_lease_renewal_zero_match:
                std::sync::atomic::AtomicBool::new(false),
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
                crate::store_backend_support::trace_session_execution_lease_refusal(
                    crate::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                    "token_scoped_release_did_not_match",
                    "in_memory_write_transaction",
                    completion,
                    crate::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                        current.and_then(|lease| lease.owner.as_ref()),
                        current.and_then(|lease| lease.executor_id.as_deref()),
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
            executor_id: current
                .executor_id
                .clone()
                .expect("live lease executor id set"),
            lease_token: current.lease_token.clone().expect("live lease token set"),
            fencing_token: current.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_epoch_ms,
            lease_term_ms: current.lease_term_ms,
            expires_at_epoch_ms: current.expires_at_epoch_ms,
        }
    }

    fn acquire_session_execution_lease_in_memory(
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        executor_id: &str,
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
        current.executor_id = Some(executor_id.to_string());
        current.lease_token = Some(lease_token.to_string());
        current.claimed_at_epoch_ms = now;
        current.lease_term_ms = lease_ttl_ms;
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
    ) -> Result<crate::QueuedWorkClaimOutcome, crate::store::StoreError> {
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
    ) -> Result<crate::QueuedWorkClaimOutcome, crate::store::StoreError> {
        let mut queued = self.queued_work.lock_recover();
        Self::claim_ready_queued_work_for_state(
            &mut queued,
            session_id,
            session_execution_lease,
            owner,
            kind,
            now,
        )
    }

    fn claim_ready_queued_work_for_state(
        queued: &mut [InMemoryQueuedBatch],
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        kind: InMemoryQueuedWorkClaimKind,
        now: u64,
    ) -> Result<crate::QueuedWorkClaimOutcome, crate::store::StoreError> {
        let max_batches = match &kind {
            InMemoryQueuedWorkClaimKind::LeadingSessionCommand => usize::MAX,
            InMemoryQueuedWorkClaimKind::TurnWork { policy, .. } => policy.max_rows,
        };
        if max_batches == 0 {
            return Ok(crate::QueuedWorkClaimOutcome::Refused(
                crate::QueuedWorkClaimRefusal::ZeroLimit,
            ));
        }
        // The fence is validated live, so its fencing token is the currently-live
        // session-lease generation. A row is claimable when it is unheld or its
        // pinned generation differs from ours; same-generation self-steal is
        // therefore unrepresentable (ADR 0029).
        let generation = session_execution_lease.fencing_token;
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
            // An exhausted lane and a lane whose next row is still deferred both
            // read as "no candidate" here, and only the first one is terminal
            // for a host: the deferred row is intact and drains on a later
            // attempt.
            let deferred_row_pending = queued.iter().any(|entry| {
                entry.batch.session_id == session_id
                    && entry.batch.available_at_ms > now
                    && claim_available(entry)
            });
            return Ok(crate::QueuedWorkClaimOutcome::Refused(
                if deferred_row_pending {
                    crate::QueuedWorkClaimRefusal::NotYetAvailable
                } else {
                    crate::QueuedWorkClaimRefusal::Empty
                },
            ));
        }
        let candidates = claimable_indices
            .iter()
            .map(|index| {
                let batch = &queued[*index].batch;
                Self::queued_batch_work_class(batch)?;
                Ok(crate::store::queued_work::ClaimCandidate::from_batch(
                    batch,
                    queued[*index].claim_fencing_token,
                    queued[*index].claim_id.clone(),
                ))
            })
            .collect::<Result<Vec<_>, crate::store::StoreError>>()?;
        let (selected_indices, refusal): (Vec<usize>, Option<crate::QueuedWorkClaimRefusal>) =
            match kind {
                InMemoryQueuedWorkClaimKind::LeadingSessionCommand => {
                    let selected_len =
                        crate::store::queued_work::select_leading_session_command(&candidates);
                    (
                        claimable_indices
                            .iter()
                            .copied()
                            .take(selected_len)
                            .collect(),
                        // Only the turn-work family's refusal reaches a host, and
                        // a successful selection has no refusal to report, so
                        // this family names one only when it took no rows.
                        (selected_len == 0).then_some(crate::QueuedWorkClaimRefusal::Empty),
                    )
                }
                InMemoryQueuedWorkClaimKind::TurnWork { boundary, policy } => {
                    let selection = crate::store::queued_work::select_turn_work_claim_indices(
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
            return Ok(crate::QueuedWorkClaimOutcome::Refused(
                refusal.unwrap_or(crate::QueuedWorkClaimRefusal::Empty),
            ));
        }
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
        let abandon_restore_claim_id = queued[first_index].claim_id.clone();
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
        Ok(crate::QueuedWorkClaimOutcome::Claimed(
            crate::QueuedWorkClaim {
                session_id: session_id.to_string(),
                claim_id,
                owner: owner.clone(),
                lease_token,
                fencing_token,
                session_lease_generation: generation,
                data: crate::QueuedWorkClaimData {
                    batches,
                    abandon_restore_claim_id,
                },
            },
        ))
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
        let mut pending = self.pending_turn_inputs.lock_recover();
        Self::claim_pending_turn_inputs_for_state(
            &mut pending,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            mode,
            now,
        )
    }

    fn claim_pending_turn_inputs_for_state(
        pending: &mut [InMemoryPendingTurnInput],
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

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<crate::SessionHeadMeta>, crate::StoreError> {
        #[cfg(test)]
        self.refuse_injected_counter_defect("session_head_revision")?;
        #[cfg(test)]
        self.load_session_head_meta_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        if self
            .fail_next_load_session_head_meta
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::StoreError::Backend(
                "injected load-session-head failure".to_string(),
            ));
        }
        Ok(self.session_head_meta.lock_recover().clone())
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, crate::store::StoreError> {
        if self.tombstoned_node_ids.lock_recover().contains(node_id) {
            return Ok(None);
        }
        if !self.node_visible_to_bound_session(node_id)? {
            return Ok(None);
        }
        let graph = self.global_session_graph.lock_recover();
        Ok(graph.find_node(node_id).cloned())
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitReceipt, crate::store::StoreError> {
        let planner = crate::store::RuntimeCommitPlanner::prepare(commit)?;
        let commit = planner.commit();
        let session_id = commit.session_id.clone();
        let transaction_now = self.clock.timestamp_ms();
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
        self.ensure_session_metadata_for_commit(commit)?;
        #[cfg(test)]
        self.fail_after_first_runtime_commit_mutation_if_requested(session_meta_before_commit)?;
        planner.validate_node_derivation()?;
        let key = (session_id.clone(), planner.operation_key().to_string());
        if let Some(stored) = self.runtime_turn_commits.lock_recover().get(&key).cloned() {
            let prior = crate::store::RuntimeCommitReceiptRecord {
                turn_commit_hash: stored.turn_commit_hash,
                result: stored.result,
                append_request_identity: stored.append_request_identity,
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
        let hydrated_checkpoint =
            checkpoints::resolve_components(&self.checkpoint_component_blobs, &commit.checkpoint)?;
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
            .append_request_identity
            .as_ref()
            .and_then(|identity| identity.requested_ancestor_node_id.as_deref())
            .is_none_or(|required| {
                self.session_graph
                    .lock_recover()
                    .active_path_contains(required)
            });
        let parent_node_facts = old_leaf_node_id
            .as_deref()
            .map(|leaf_node_id| {
                let resident = self.session_graph.lock_recover();
                let active_path = resident.active_path_nodes();
                let generation = active_path.len().checked_sub(1).ok_or_else(|| {
                    crate::StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "published leaf has an empty active path".to_string(),
                    }
                })? as u64;
                let frame_node_id = resident
                    .nearest_frame_node_id(Some(leaf_node_id))
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| crate::StoreError::MissingFrameOpenAncestor {
                        leaf_node_id: leaf_node_id.to_string(),
                    })?;
                Ok(crate::store::ParentNodeFacts {
                    node_id: leaf_node_id.to_string(),
                    generation,
                    frame_node_id,
                })
            })
            .transpose()?;
        let mut proposed = self.global_session_graph.lock_recover().clone();
        proposed.extend_node_records(commit.graph.nodes.iter().cloned());
        proposed.set_leaf_node_id(commit.graph.leaf_node_id().cloned());
        let plan = planner.plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: actual,
            old_leaf_node_id,
            requested_ancestor_is_active,
            occupied_node_ids,
            selected_leaf_is_live,
            has_live_nodes: has_existing_live_nodes,
            old_leaf_is_live,
            parent_node_facts,
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
                    .filter(|entry| turn_input_settlement_matches(entry, completed))
                    .count();
                if matches != completed.input_ids.len() {
                    let row_id = completed.input_ids.iter().find(|input_id| {
                        !pending.iter().any(|entry| {
                            entry.input.input_id == **input_id
                                && turn_input_settlement_matches(entry, completed)
                        })
                    });
                    let current = row_id.and_then(|input_id| {
                        pending.iter().find(|entry| {
                            entry.input.session_id == completed.session_id
                                && entry.input.input_id == *input_id
                        })
                    });
                    return Err(match completed.claim.as_ref() {
                        Some(claim) => crate::store::StoreError::TurnInputClaimSuperseded {
                            session_id: completed.session_id.clone(),
                            claim_id: claim.claim_id.clone(),
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
                        },
                        None => crate::store::StoreError::UnclaimedTurnInputSettlementSuperseded {
                            session_id: completed.session_id.clone(),
                            input_id: row_id
                                .cloned()
                                .unwrap_or_else(|| completed.input_ids.join(",")),
                            observed_state: current.map(|entry| {
                                entry.input.state.as_str().to_string().into_boxed_str()
                            }),
                            superseding_claim_id: current
                                .and_then(|entry| entry.claim_id.clone())
                                .map(String::into_boxed_str),
                        },
                    });
                }
            }
        }
        let manifest = hydrated_checkpoint.manifest()?;
        let checkpoint_bytes = rmp_serde::to_vec_named(&manifest).map_err(|error| {
            crate::store::StoreError::RecordEncodingFailed {
                record_kind: "in-memory checkpoint root".to_string(),
                message: error.to_string(),
            }
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
        let (staged_pending_turn_inputs, staged_turn_cancel_requests, turn_cancel_input_outcome) = {
            let mut pending = self.pending_turn_inputs.lock_recover().clone();
            let mut requests = self.turn_cancel_requests.lock_recover().clone();
            let mut outcome = crate::TurnCancelInputOutcome::default();
            for completed in &commit.completed_turn_input_claims {
                for entry in pending.iter_mut() {
                    if turn_input_settlement_matches(entry, completed) {
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
                        let disposition = requests
                            .get(turn_id)
                            .map_or(crate::TurnCancelDisposition::Defer, |record| {
                                record.request.undelivered
                            });
                        let affected = crate::TurnCancelAffectedInput {
                            input_id: entry.input.input_id.clone(),
                            payload: entry.input.input.clone(),
                            disposition,
                        };
                        match disposition {
                            crate::TurnCancelDisposition::Defer => {
                                entry.input.state = crate::TurnInputState::DeferredNextTurn;
                                entry.input.ingress = crate::TurnInputIngress::NextTurn;
                            }
                            crate::TurnCancelDisposition::Drop => {
                                entry.input.state = crate::TurnInputState::Cancelled;
                            }
                        }
                        entry.claim_id = None;
                        entry.claim_token = None;
                        entry.claim_owner = None;
                        entry.claim_session_lease_generation = 0;
                        if let Some(record) = requests.get_mut(turn_id) {
                            record
                                .outcome
                                .get_or_insert_with(crate::TurnCancelInputOutcome::default)
                                .affected_inputs
                                .push(affected.clone());
                        }
                        outcome.affected_inputs.push(affected);
                    }
                }
            }
            (pending, requests, outcome)
        };

        *self.queued_work.lock_recover() = staged_queued_work;
        *self.wake_redelivery_fences.lock_recover() = staged_wake_redelivery_fences;
        *self.queued_work_next_seq.lock_recover() = staged_queued_work_next_seq;
        *self.pending_turn_inputs.lock_recover() = staged_pending_turn_inputs;
        *self.turn_cancel_requests.lock_recover() = staged_turn_cancel_requests;
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
        // The write-transaction mutex still covers both this leaf publication
        // and the checkpoint-root replacement below. That is the in-memory
        // GC-safety equivalent of avoiding git's loose-object race: readers
        // can observe neither unreachable new leaves nor a root with missing
        // leaves.
        {
            let mut blobs = self.checkpoint_component_blobs.lock_recover();
            let mut roots = HashSet::new();
            for component in hydrated_checkpoint.components.values() {
                if let Some(blob_ref) = component.blob_ref().cloned() {
                    if let Some(body) = component.body().map(<[u8]>::to_vec) {
                        blobs.insert(blob_ref.clone(), body);
                    }
                    roots.insert(blob_ref);
                }
            }
            // This commit's checkpoint is the session's only live one, so its
            // component edges replace the superseded set wholesale.
            self.checkpoint_blob_roots
                .lock_recover()
                .insert(commit.session_id.clone(), roots);
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
        let head_revision = meta
            .as_ref()
            .expect("fresh commit publishes session head metadata")
            .head_revision;
        let relation = self
            .session_meta
            .lock_recover()
            .as_ref()
            .map(|meta| meta.relation.clone())
            .unwrap_or(crate::SessionRelation::Root);
        self.session_catalog
            .lock_recover()
            .entry(session_id.clone())
            .and_modify(|summary| {
                summary.last_commit_at_ms = Some(transaction_now);
                summary.head_revision = head_revision;
            })
            .or_insert_with(|| crate::SessionSummary {
                session_id: session_id.clone(),
                created_at_ms: transaction_now,
                last_commit_at_ms: Some(transaction_now),
                head_revision,
                relation: crate::SessionRelationKind::from_relation(&relation),
                parent_session_id: relation.parent_session_id().map(ToOwned::to_owned),
                deleted: false,
            });
        *self.runtime_commit_count.lock_recover() += 1;
        let mut result = plan.result(checkpoint_ref, manifest, staged_enqueued_queue_batches);
        result.turn_cancel_input_outcome = turn_cancel_input_outcome;
        let receipt = plan.receipt_write(&result);
        let stored_receipt = RuntimeTurnCommitRecord {
            turn_commit_hash: receipt.turn_commit_hash.to_string(),
            result: result.clone(),
            committed_at_ms: transaction_now,
            append_request_identity: receipt.append_request_identity.cloned(),
        };
        let mut runtime_turn_commits = self.runtime_turn_commits.lock_recover();
        runtime_turn_commits.insert(
            (session_id.clone(), receipt.operation_key.to_string()),
            stored_receipt.clone(),
        );
        if commit.turn_commit.operation.key == "session-command" {
            for batch_id in commit
                .completed_queue_claims
                .iter()
                .flat_map(|completion| &completion.batch_ids)
            {
                let marker = crate::store_backend_support::session_command_batch_completion_key(
                    &session_id,
                    batch_id,
                )?;
                runtime_turn_commits.insert(
                    (session_id.clone(), marker),
                    RuntimeTurnCommitRecord {
                        append_request_identity: None,
                        ..stored_receipt.clone()
                    },
                );
            }
        }
        drop(runtime_turn_commits);
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
        let mut bound = self.bound_session_id.lock_recover();
        if let Some(existing) = bound.as_ref() {
            if existing != &binding.session_id {
                return Err(crate::StoreError::SessionBindingMismatch {
                    bound_session_id: existing.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
        } else {
            *bound = Some(binding.session_id.clone());
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
            relation: binding.relation.clone(),
        });
        Ok(crate::SessionAdmission::Created)
    }

    async fn save_session_meta(
        &self,
        meta: crate::store::SessionMeta,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.replace_session_meta(meta.clone())?;
        if let Some(summary) = self
            .session_catalog
            .lock_recover()
            .get_mut(&meta.session_id)
        {
            summary.relation = crate::SessionRelationKind::from_relation(&meta.relation);
            summary.parent_session_id = meta.relation.parent_session_id().map(ToOwned::to_owned);
        }
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<crate::store::SessionMeta>, crate::store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}
