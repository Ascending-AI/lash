//! Public in-memory `RuntimePersistence` + `SessionStoreFactory`.
//!
//! Explicitly-wired ephemeral storage for native-substrate hosts that run background
//! processes without durable backing: a `process` started in a turn (or by a
//! trigger) is executed by the lease-protected worker, which rebuilds its
//! session from the store factory — so even an in-memory host needs a factory.
//! This explicit opt-in has no silent in-memory default and holds the same `RuntimePersistence` contract as the
//! durable backend (verified by the `runtime_persistence` conformance suite).
use crate::SessionId;
use crate::TurnId;
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
mod retention;
mod session_binding;
mod session_execution_lease;
use session_execution_lease::{InMemorySessionExecutionLease, Lease};
mod state_version;
#[cfg(any(test, feature = "testing"))]
pub(crate) mod test_support;
#[cfg(any(test, feature = "testing"))]
mod testing_access;
#[cfg(any(test, feature = "testing"))]
pub use testing_access::RawSessionExecutionLeaseRow;
mod claim_hold;
mod turn_cancel_closure;
mod turn_input;
mod warnings;
use claim_hold::ClaimHold;

use receipts::{RuntimeTurnCommitMap, RuntimeTurnCommitRecord};

#[derive(Clone)]
struct InMemoryQueuedBatch {
    batch: crate::QueuedWorkBatch,
    claim: ClaimHold,
}

#[derive(Clone)]
struct InMemoryPendingTurnInput {
    input: crate::PendingTurnInput,
    claim: ClaimHold,
}

#[derive(Clone)]
enum InMemoryQueuedWorkClaimKind {
    LeadingSessionCommand,
    TurnWork {
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    },
}

type InMemoryNodeAnchorRecord = (crate::BlobRef, crate::HydratedSessionCheckpoint, SessionId);
type InMemoryNodeAnchors = Arc<Mutex<HashMap<String, InMemoryNodeAnchorRecord>>>;
/// Session id -> component blob refs its live checkpoint references.
pub(crate) type SharedCheckpointBlobRoots = Arc<Mutex<HashMap<SessionId, HashSet<crate::BlobRef>>>>;
pub(crate) type SharedSessionCatalog = Arc<Mutex<HashMap<SessionId, crate::SessionSummary>>>;

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

/// The three condemnation phases. Absence from the map is the `Free` state.
/// A claim on `Condemned` or `Reclaimed` gives one writer temporary ownership
/// while it restores the bytes; the phase itself remains durable until success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentCondemnationPhase {
    /// Claimed by a sweeper, no physical delete issued yet: a writer revokes it.
    Condemned {
        write_claim: Option<AttachmentWriteClaim>,
    },
    /// The physical delete is in flight: a writer must wait for its outcome.
    Deleting,
    /// The physical delete succeeded: adoption refuses until a fresh put.
    Reclaimed {
        write_claim: Option<AttachmentWriteClaim>,
    },
}

/// Durable association between one restoring attempt and its manifest intent.
/// Host recovery uses the session identity to remove exactly the abandoned
/// attempt's uncommitted row before releasing its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttachmentWriteClaim {
    pub(super) token: crate::AttachmentWriteToken,
    pub(super) session_id: SessionId,
}

pub struct InMemorySessionStore {
    clock: Arc<dyn crate::Clock>,
    /// Factory-lifetime authority for the reserved turn-cancellation promises.
    /// Factory-created stores expose the same resolver and binding identity on
    /// reopen. Standalone stores leave authority with their configured host.
    turn_cancellation_authority: Option<crate::TurnCancellationAuthority>,
    /// Serializes every operation whose correctness depends on observing the
    /// session lease and mutating fenced runtime state atomically. Component
    /// mutexes still guard their data; this mutex supplies the transaction
    /// boundary and lock ordering that SQLite/Postgres provide natively.
    /// Poison recovery is deliberate under ADR 0054. These critical sections
    /// must remain host-code-free: read clocks and invoke any other dynamic
    /// host surface before acquiring this lock, then carry inert values in.
    write_transaction: Arc<Mutex<()>>,
    pub(crate) bound_session_id: Mutex<Option<SessionId>>,
    pub(crate) session_head_meta: Mutex<Option<crate::SessionHeadMeta>>,
    pub(crate) session_meta: Mutex<Option<crate::SessionMeta>>,
    /// Independently readable mutable-continuation generation beside binding metadata.
    pub(crate) session_state_version: Mutex<Option<u32>>,
    corrupt_session_payload_for_testing: std::sync::atomic::AtomicBool,
    pub(crate) session_graph: Mutex<crate::SessionGraph>,
    /// Shared leafless node catalog; never treated as a resident graph without a real leaf grafted
    /// first.
    global_session_graph: Arc<Mutex<crate::SessionGraph>>,
    global_node_owners: Arc<Mutex<HashMap<String, SessionId>>>,
    global_session_heads: Arc<Mutex<HashMap<SessionId, Option<String>>>>,
    node_anchors: InMemoryNodeAnchors,
    tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
    /// Permanent per-factory deletion ledger. Maintenance never prunes this:
    /// an id, once used and deleted in this store, must never be reused.
    deleted_session_ids: Arc<Mutex<HashSet<SessionId>>>,
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
    session_execution_leases: Mutex<HashMap<SessionId, InMemorySessionExecutionLease>>,
    turn_cancellation_binding: Mutex<Option<(String, Option<crate::ExecutionScope>)>>,
    turn_cancel_closure_authorizations:
        Mutex<HashMap<TurnId, crate::TurnCancelClosureAuthorization>>,
    retired_turn_cancel_scopes: Arc<Mutex<HashSet<String>>>,
    queued_work: Mutex<Vec<InMemoryQueuedBatch>>,
    queued_work_next_seq: Mutex<u64>,
    /// Receiver-side sender allocation floor. This is a redelivery fence, not
    /// a consumption watermark: selected-batch settlement may be out of order.
    wake_redelivery_fences: Mutex<HashMap<(String, String), u64>>,
    pending_turn_inputs: Mutex<Vec<InMemoryPendingTurnInput>>,
    pending_turn_input_next_seq: Mutex<u64>,
    turn_cancel_requests: Mutex<HashMap<TurnId, InMemoryTurnCancelRequest>>,
    attachment_manifest: SharedAttachmentManifest,
    /// Per-digest attachment GC condemnation state, shared with every store the
    /// same factory owns because the digest is factory-global: the writer's
    /// intent insert and the sweeper's condemn CAS must meet here.
    pub(crate) attachment_condemnations: SharedAttachmentCondemnations,
    #[cfg(any(test, feature = "testing"))]
    claim_after_lease_validation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(any(test, feature = "testing"))]
    fail_next_exact_queue_claim: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    list_pending_queued_work_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    load_session_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    load_session_head_meta_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    fail_next_load_session_head_meta: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    fail_load_session_on_call: Mutex<Option<usize>>,
    #[cfg(any(test, feature = "testing"))]
    checkpoint_probe_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    commit_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    fail_next_runtime_commit: Mutex<Option<crate::StoreError>>,
    #[cfg(any(test, feature = "testing"))]
    inject_turn_cancel_before_next_runtime_commit: Mutex<Option<crate::TurnCancelRequest>>,
    #[cfg(any(test, feature = "testing"))]
    fail_next_runtime_commit_after_first_mutation: Mutex<Option<crate::StoreError>>,
    #[cfg(any(test, feature = "testing"))]
    fail_next_session_execution_lease_renewal: Mutex<Option<crate::StoreError>>,
    #[cfg(any(test, feature = "testing"))]
    force_next_session_execution_lease_renewal_zero_match: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    next_session_execution_lease_renewal_response: Mutex<Option<crate::SessionExecutionLease>>,
    #[cfg(any(test, feature = "testing"))]
    session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    session_execution_lease_release_gate:
        Mutex<Option<Arc<test_support::SessionExecutionLeaseReleaseGate>>>,
    #[cfg(any(test, feature = "testing"))]
    session_execution_lease_release_attempt_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    raw_counter_defects: Mutex<HashMap<String, i64>>,
    #[cfg(any(test, feature = "testing"))]
    abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub(crate) session_admission_count: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
struct InMemoryTurnCancelRequest {
    record: crate::TurnCancelRequestRecord,
    intent_revision: u64,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        warnings::process_owner_death_degraded("InMemorySessionStore::new");
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
        warnings::process_owner_death_degraded("InMemorySessionStore::with_clock");
        Self::with_shared_history(
            clock,
            Some(factory::turn_cancellation_authority()),
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
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_shared_history(
        clock: Arc<dyn crate::Clock>,
        turn_cancellation_authority: Option<crate::TurnCancellationAuthority>,
        write_transaction: Arc<Mutex<()>>,
        global_session_graph: Arc<Mutex<crate::SessionGraph>>,
        global_node_owners: Arc<Mutex<HashMap<String, SessionId>>>,
        global_session_heads: Arc<Mutex<HashMap<SessionId, Option<String>>>>,
        node_anchors: InMemoryNodeAnchors,
        checkpoint_component_blobs: Arc<Mutex<HashMap<crate::BlobRef, Vec<u8>>>>,
        checkpoint_blob_roots: SharedCheckpointBlobRoots,
        tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
        deleted_session_ids: Arc<Mutex<HashSet<SessionId>>>,
        session_catalog: SharedSessionCatalog,
        attachment_condemnations: SharedAttachmentCondemnations,
        attachment_manifest: SharedAttachmentManifest,
        retired_turn_cancel_scopes: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        warnings::process_owner_death_degraded("InMemorySessionStore::with_shared_history");
        Self {
            clock,
            turn_cancellation_authority,
            write_transaction,
            bound_session_id: Mutex::new(None),
            session_head_meta: Mutex::new(None),
            session_meta: Mutex::new(None),
            session_state_version: Mutex::new(Some(crate::store::CURRENT_SESSION_STATE_VERSION)),
            corrupt_session_payload_for_testing: std::sync::atomic::AtomicBool::new(false),
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
            turn_cancellation_binding: Mutex::new(None),
            turn_cancel_closure_authorizations: Mutex::new(HashMap::new()),
            retired_turn_cancel_scopes,
            queued_work: Mutex::new(Vec::new()),
            queued_work_next_seq: Mutex::new(0),
            wake_redelivery_fences: Mutex::new(HashMap::new()),
            pending_turn_inputs: Mutex::new(Vec::new()),
            pending_turn_input_next_seq: Mutex::new(0),
            turn_cancel_requests: Mutex::new(HashMap::new()),
            attachment_manifest,
            attachment_condemnations,
            #[cfg(any(test, feature = "testing"))]
            claim_after_lease_validation_hook: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            fail_next_exact_queue_claim: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            list_pending_queued_work_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            load_session_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            load_session_head_meta_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            fail_next_load_session_head_meta: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            fail_load_session_on_call: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            checkpoint_probe_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            commit_write_transaction_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            fail_next_runtime_commit: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            inject_turn_cancel_before_next_runtime_commit: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            fail_next_runtime_commit_after_first_mutation: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            fail_next_session_execution_lease_renewal: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            force_next_session_execution_lease_renewal_zero_match:
                std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(test, feature = "testing"))]
            next_session_execution_lease_renewal_response: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            session_execution_lease_release_gate: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            session_execution_lease_release_attempt_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            raw_counter_defects: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "testing"))]
            abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(any(test, feature = "testing"))]
            session_admission_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn verify_session_execution_lease(
        &self,
        session_id: &SessionId,
        fence: &crate::SessionExecutionLeaseAuthority,
        now: u64,
    ) -> Result<(), crate::store::StoreError> {
        let leases = self.session_execution_leases.lock_recover();
        crate::store::session_execution_lease::require_current_session_execution_lease(
            session_id,
            leases
                .get(session_id)
                .map(InMemorySessionExecutionLease::fence_facts),
            fence,
            now,
        )
    }

    /// The fencing token of the session's currently-live execution lease, or
    /// `None` when no live lease holds the session. A queued-work or turn-input
    /// claim is live for lease-less host callers exactly when the generation it
    /// pins equals this value (ADR 0029).
    fn live_session_lease_generation(&self, session_id: &SessionId, now: u64) -> Option<u64> {
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
            && current.is_held_by(&completion.owner, &completion.executor_id)
            && current.lease_token_matches(&completion.lease_token)
        {
            current.lease = Lease::Free;
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
                        current.and_then(|lease| lease.held_fields().map(|fields| fields.owner)),
                        current
                            .and_then(|lease| lease.held_fields().map(|fields| fields.executor_id)),
                        current
                            .and_then(|lease| lease.held_fields().map(|fields| fields.lease_token)),
                    ),
                );
            }
            false
        }
    }

    fn in_memory_session_execution_lease(
        session_id: &SessionId,
        current: &InMemorySessionExecutionLease,
    ) -> crate::SessionExecutionLease {
        match current.held_fields() {
            Some(fields) => crate::SessionExecutionLease {
                session_id: SessionId::from(session_id.to_string()),
                owner: fields.owner.clone(),
                executor_id: fields.executor_id.to_string(),
                lease_token: fields.lease_token.to_string(),
                fencing_token: current.fencing_token,
                claimed_at_epoch_ms: fields.claimed_at_epoch_ms,
                lease_term_ms: fields.lease_term_ms,
                expires_at_epoch_ms: fields.expires_at_epoch_ms,
            },
            None => unreachable!("free session execution lease has no public projection"),
        }
    }

    fn acquire_session_execution_lease_in_memory(
        session_id: &SessionId,
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
        current.lease = Lease::Held {
            owner: owner.clone(),
            executor_id: executor_id.to_string(),
            lease_token: lease_token.to_string(),
            claimed_at_epoch_ms: now,
            lease_term_ms: lease_ttl_ms,
            expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
        };
        Ok(Self::in_memory_session_execution_lease(session_id, current))
    }

    fn claim_ready_queued_work_in_memory(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        kind: InMemoryQueuedWorkClaimKind,
    ) -> Result<crate::QueuedWorkClaimOutcome, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(any(test, feature = "testing"))]
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
        session_id: &SessionId,
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
        session_id: &SessionId,
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
        let claim_available = |entry: &InMemoryQueuedBatch| entry.claim.claimable_by(generation);
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
                crate::store::queued_work::ClaimCandidate::from_batch(
                    batch,
                    queued[*index].claim.fencing_token,
                    queued[*index].claim.id(),
                    queued[*index].claim.token(),
                )
            })
            .collect::<Vec<_>>();
        let selected_indices: Vec<usize> = match kind {
            InMemoryQueuedWorkClaimKind::LeadingSessionCommand => {
                let selected_len =
                    crate::store::queued_work::select_leading_session_command(&candidates);
                if selected_len == 0 {
                    return Ok(crate::QueuedWorkClaimOutcome::Refused(
                        crate::QueuedWorkClaimRefusal::Empty,
                    ));
                }
                claimable_indices
                    .iter()
                    .copied()
                    .take(selected_len)
                    .collect()
            }
            InMemoryQueuedWorkClaimKind::TurnWork { boundary, policy } => {
                match crate::store::queued_work::select_turn_work_claim_indices(
                    &candidates,
                    boundary,
                    &policy,
                    now,
                )? {
                    crate::store::TurnWorkClaimSelection::Selected { indices } => indices
                        .into_iter()
                        .map(|candidate_index| claimable_indices[candidate_index])
                        .collect(),
                    crate::store::TurnWorkClaimSelection::Refused { reason } => {
                        return Ok(crate::QueuedWorkClaimOutcome::Refused(reason));
                    }
                }
            }
        };
        let next_fencing_tokens = selected_indices
            .iter()
            .map(|index| {
                crate::StoreError::checked_monotonic_increment(
                    "queued_work_claim_fencing_token",
                    queued[*index].claim.fencing_token,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let first_index = selected_indices[0];
        let first = queued[first_index].batch.clone();
        let abandon_restore_claim_id = queued[first_index].claim.id();
        let abandon_restore_claim_token = queued[first_index].claim.token();
        let fencing_token = next_fencing_tokens[0];
        let claim_id = crate::store::queued_work::derive_claim_id(
            crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
            first.enqueue_seq,
            fencing_token,
        );
        let lease_token =
            crate::store::queued_work::derive_claim_lease_token(session_id, owner, &claim_id, now);
        let mut batches = Vec::new();
        for (index, next_fencing_token) in selected_indices.into_iter().zip(next_fencing_tokens) {
            let entry = &mut queued[index];
            entry.claim.acquire(
                claim_id.clone(),
                lease_token.clone(),
                owner.clone(),
                generation,
                next_fencing_token,
            );
            batches.push(entry.batch.clone());
        }
        Ok(crate::QueuedWorkClaimOutcome::Claimed(
            crate::QueuedWorkClaim {
                session_id: SessionId::from(session_id.to_string()),
                claim_id,
                owner: owner.clone(),
                lease_token,
                fencing_token,
                session_lease_generation: generation,
                data: crate::QueuedWorkClaimData {
                    batches,
                    abandon_restore_claim_id,
                    abandon_restore_claim_token: abandon_restore_claim_token
                        .map(String::into_boxed_str),
                },
            },
        ))
    }

    fn claim_pending_turn_inputs_in_memory(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
        mode: crate::TurnInputClaimMode,
    ) -> Result<Option<crate::TurnInputClaim>, crate::store::StoreError> {
        let now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        self.verify_session_execution_lease(session_id, session_execution_lease, now)?;
        #[cfg(any(test, feature = "testing"))]
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
        session_id: &SessionId,
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
        session_id: &SessionId,
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
        let claim_available =
            |entry: &InMemoryPendingTurnInput| entry.claim.claimable_by(generation);
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
                    pending[*index].claim.fencing_token,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let fencing_token = next_fencing_tokens[0];
        let claim_id = crate::store::queued_work::derive_claim_id(
            crate::store::queued_work::ClaimIdDialect::RecordingTurnInput,
            pending[first_index].input.enqueue_seq,
            fencing_token,
        );
        let lease_token =
            crate::store::queued_work::derive_claim_lease_token(session_id, owner, &claim_id, now);
        let mut inputs = Vec::new();
        for (index, next_fencing_token) in selected_indices.into_iter().zip(next_fencing_tokens) {
            let entry = &mut pending[index];
            entry.claim.acquire(
                claim_id.clone(),
                lease_token.clone(),
                owner.clone(),
                generation,
                next_fencing_token,
            );
            if matches!(mode, crate::TurnInputClaimMode::ActiveTurn { .. }) {
                entry.input.state = crate::TurnInputState::Accepted;
            }
            inputs.push(entry.input.clone());
        }
        Ok(Some(crate::TurnInputClaim {
            session_id: SessionId::from(session_id.to_string()),
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
        session_id: &SessionId,
        generation: u64,
        turn_id: &TurnId,
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
                    && (entry.claim.claimable_by(generation))
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
                    && (entry.claim.claimable_by(generation))
            })
            .min_by_key(|entry| entry.batch.enqueue_seq);
        Ok(first_ready.is_some_and(|entry| {
            entry.batch.work_class() == crate::store::QueuedWorkClass::TurnWork
                && entry.batch.delivery_policy == crate::DeliveryPolicy::EarliestSafeBoundary
        }))
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        warnings::process_owner_death_degraded("InMemorySessionStore::default");
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::store::SessionCommitStore for InMemorySessionStore {
    async fn read_session_state_version(&self) -> Result<u32, crate::StoreError> {
        self.read_session_state_version_in_memory()
    }

    async fn admit_session_state(
        &self,
        lease: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<crate::store::SessionStateAdmission, crate::StoreError> {
        self.admit_session_state_in_memory(lease)
    }

    async fn load_session(
        &self,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::store::StoreError> {
        self.guard_session_payload_in_memory()?;
        #[cfg(any(test, feature = "testing"))]
        self.refuse_injected_counter_defect("session_head_revision")?;
        #[cfg(any(test, feature = "testing"))]
        let load_call = self
            .load_session_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        #[cfg(any(test, feature = "testing"))]
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
        let map_graph_corruption =
            |error: crate::StoreError| crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: error.to_string(),
            };
        let mut graph =
            crate::SessionGraph::from_nodes(global_graph.nodes.clone(), meta.leaf_node_id.clone())
                .map_err(map_graph_corruption)?
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
        let mut turn_failure_settlements = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .filter_map(|((owner_session_id, turn_id), record)| {
                (owner_session_id == meta.session_id && !record.result.failure_evidence.is_empty())
                    .then(|| {
                        (
                            record.committed_at_ms,
                            crate::TurnFailureSettlement {
                                turn_id: turn_id.clone(),
                                evidence: record.result.failure_evidence.clone(),
                            },
                        )
                    })
            })
            .collect::<Vec<_>>();
        turn_failure_settlements.sort_by(|(left_at, left), (right_at, right)| {
            left_at
                .cmp(right_at)
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        let turn_failure_settlements = turn_failure_settlements
            .into_iter()
            .map(|(_, settlement)| settlement)
            .collect();
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
            turn_failure_settlements,
        }))
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<crate::SessionHeadMeta>, crate::StoreError> {
        self.read_session_state_version().await?;
        #[cfg(any(test, feature = "testing"))]
        self.refuse_injected_counter_defect("session_head_revision")?;
        #[cfg(any(test, feature = "testing"))]
        self.load_session_head_meta_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(any(test, feature = "testing"))]
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

    /// FIG-653: fork-lineage visibility is graph membership, not authorization.
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
        #[cfg(any(test, feature = "testing"))]
        self.commit_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.ensure_session_not_deleted(&session_id)?;
        turn_cancel_closure::verify_pre_replay_fence(self, commit, transaction_now)?;
        #[cfg(any(test, feature = "testing"))]
        if let Some(error) = self.fail_next_runtime_commit.lock_recover().take() {
            return Err(error);
        }
        #[cfg(any(test, feature = "testing"))]
        if let Some(request) = self
            .inject_turn_cancel_before_next_runtime_commit
            .lock_recover()
            .take()
        {
            debug_assert_eq!(request.address.session_id, commit.session_id);
            let mut requests = self.turn_cancel_requests.lock_recover();
            match requests.get_mut(&request.address.turn_id) {
                Some(stored) if request.mode.is_stronger_than(stored.record.request.mode) => {
                    stored.intent_revision = crate::StoreError::checked_monotonic_increment(
                        "turn_cancel_intent_revision",
                        stored.intent_revision,
                    )?;
                }
                Some(_) => {}
                None => {
                    requests.insert(
                        request.address.turn_id.clone(),
                        InMemoryTurnCancelRequest {
                            record: crate::TurnCancelRequestRecord {
                                request,
                                outcome: None,
                            },
                            intent_revision: 1,
                        },
                    );
                }
            }
        }
        let session_meta_before_commit = self.session_meta.lock_recover().clone();
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
        turn_cancel_closure::validate_after_receipt_miss(self, commit, transaction_now)?;
        if let (Some(turn_id), Some(observed)) = (
            commit.interrupted_turn_input_turn_id.as_ref(),
            commit.interrupted_turn_cancel_intent.as_ref(),
        ) {
            let requests = self.turn_cancel_requests.lock_recover();
            if turn_input::snapshot(&requests, turn_id) != *observed {
                return Err(crate::StoreError::TurnCancelIntentChanged {
                    session_id: commit.session_id.clone(),
                    turn_id: turn_id.clone(),
                });
            }
        }
        // Receipt replay and the cancellation predicate are adjudicated before
        // even session binding metadata can be materialized by a fresh commit.
        self.ensure_session_metadata_for_commit(commit)?;
        let mut meta = self.session_head_meta.lock_recover();
        let actual = meta.as_ref().map_or(0, |meta| meta.head_revision);
        #[cfg(any(test, feature = "testing"))]
        self.fail_after_first_runtime_commit_mutation_if_requested(
            session_meta_before_commit.clone(),
        )?;
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
        let (has_existing_live_nodes, selected_leaf_is_live, published_leaf) = {
            let graph = self.global_session_graph.lock_recover();
            let tombstoned = self.tombstoned_node_ids.lock_recover();
            let has_existing_live_nodes = global_node_owners.iter().any(|(node_id, owner)| {
                owner == commit.session_id && !tombstoned.contains(node_id)
            });
            let selected_leaf_is_live = commit.graph.leaf_node_id().is_some_and(|leaf_node_id| {
                !tombstoned.contains(leaf_node_id) && graph.find_node(leaf_node_id).is_some()
            });
            let published_leaf = match meta.as_ref().and_then(|head| head.leaf_node_id.as_ref()) {
                None => crate::store::PublishedLeafFacts::Absent,
                Some(leaf_node_id)
                    if tombstoned.contains(leaf_node_id)
                        || graph.find_node(leaf_node_id).is_none() =>
                {
                    crate::store::PublishedLeafFacts::Retired {
                        node_id: leaf_node_id.clone(),
                    }
                }
                Some(leaf_node_id) => {
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
                    crate::store::PublishedLeafFacts::Live(crate::store::ParentNodeFacts {
                        node_id: leaf_node_id.to_string(),
                        generation,
                        frame_node_id,
                    })
                }
            };
            (
                has_existing_live_nodes,
                selected_leaf_is_live,
                published_leaf,
            )
        };
        let requested_ancestor_is_active = match &commit.turn_commit.append_request_identity {
            crate::AppendRequestIdentity::Append {
                requested_ancestor_node_id: Some(required),
                ..
            } => self
                .session_graph
                .lock_recover()
                .active_path_contains(required),
            crate::AppendRequestIdentity::PlainCommit
            | crate::AppendRequestIdentity::Append {
                requested_ancestor_node_id: None,
                ..
            }
            | crate::AppendRequestIdentity::SemanticBoundary { .. } => true,
        };
        let plan = planner.plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: actual,
            published_leaf,
            requested_ancestor_is_active,
            occupied_node_ids,
            selected_leaf_is_live,
            has_live_nodes: has_existing_live_nodes,
        })?;
        let mut proposed = self.global_session_graph.lock_recover().clone();
        proposed.apply_append(&commit.graph)?;
        let (staged_tombstoned_node_ids, staged_session_heads) = {
            let new_leaf_node_id = commit.graph.leaf_node_id().cloned();
            let mut tombstoned = self.tombstoned_node_ids.lock_recover().clone();
            let mut session_heads = self.global_session_heads.lock_recover().clone();
            session_heads.insert(
                SessionId::from(commit.session_id.clone().to_string()),
                new_leaf_node_id.clone(),
            );
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
                if let Some((row_id, current)) = turn_input::settlement_mismatch(
                    &queued,
                    &completed.batch_ids,
                    &completed.session_id,
                    |entry| (&entry.batch.session_id, &entry.batch.batch_id),
                    |entry| {
                        entry.batch.session_id == completed.session_id
                            && entry
                                .claim
                                .owned_by(&completed.claim_id, &completed.lease_token)
                            && completed.batch_ids.contains(&entry.batch.batch_id)
                    },
                ) {
                    return Err(crate::store::StoreError::QueuedWorkClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
                        row_id: row_id.cloned().map(String::into_boxed_str),
                        superseding_claim_id: current
                            .and_then(|entry| entry.claim.id())
                            .map(String::into_boxed_str),
                        superseding_session_lease_generation: current
                            .and_then(|entry| entry.claim.diagnostic_generation().map(Box::new)),
                    });
                }
            }
        }
        {
            let pending = self.pending_turn_inputs.lock_recover();
            for completed in &commit.completed_turn_input_claims {
                if let Some((row_id, current)) = turn_input::settlement_mismatch(
                    &pending,
                    &completed.input_ids,
                    &completed.session_id,
                    |entry| (&entry.input.session_id, &entry.input.input_id),
                    |entry| turn_input::settlement_matches(entry, completed),
                ) {
                    return Err(match completed.claim.as_ref() {
                        Some(claim) => crate::store::StoreError::TurnInputClaimSuperseded {
                            session_id: completed.session_id.clone(),
                            claim_id: claim.claim_id.clone(),
                            row_id: row_id.cloned().map(String::into_boxed_str),
                            superseding_claim_id: current
                                .and_then(|entry| entry.claim.id())
                                .map(String::into_boxed_str),
                            superseding_session_lease_generation: current.and_then(|entry| {
                                entry.claim.diagnostic_generation().map(Box::new)
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
                                .and_then(|entry| entry.claim.id())
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
        let checkpoint_ref = crate::BlobRef::for_content(&checkpoint_bytes);
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
                        && entry
                            .claim
                            .owned_by(&completed.claim_id, &completed.lease_token)
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
                            .entry((
                                entry.batch.session_id.clone().to_string(),
                                process_id.to_string(),
                            ))
                            .and_modify(|allocation_floor| {
                                *allocation_floor = (*allocation_floor).max(sequence);
                            })
                            .or_insert(sequence);
                    }
                }
                queued.retain(|entry| {
                    !(entry.batch.session_id == completed.session_id
                        && entry
                            .claim
                            .owned_by(&completed.claim_id, &completed.lease_token)
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
                    if turn_input::settlement_matches(entry, completed) {
                        entry.input.state = crate::TurnInputState::Completed;
                        entry.clear_claim();
                    }
                }
            }
            if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
                let cancellation = commit.interrupted_turn_input_cancellation.as_ref();
                let disposition = cancellation
                    .map_or(crate::TurnCancelDisposition::Defer, |evidence| {
                        evidence.undelivered
                    });
                if let Some(evidence) = commit
                    .turn_cancel_closure_settlement
                    .as_ref()
                    .and_then(crate::TurnCancelClosureSettlement::base_cancellation)
                {
                    turn_input::reconcile_authenticated_turn_cancel_winner(
                        &mut requests,
                        &crate::TurnAddress::new(&commit.session_id, turn_id),
                        evidence,
                    )?;
                }
                for entry in pending.iter_mut() {
                    if entry.input.session_id == commit.session_id
                        && entry.input.state == crate::TurnInputState::PendingActive
                        && entry
                            .input
                            .ingress
                            .active_turn_id()
                            .is_some_and(|active| active == turn_id)
                    {
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
                        entry.claim.release();
                        if cancellation.is_some()
                            && let Some(record) = requests.get_mut(turn_id)
                        {
                            record
                                .record
                                .outcome
                                .get_or_insert_with(crate::TurnCancelInputOutcome::default)
                                .affected_inputs
                                .push(affected.clone());
                        }
                        if cancellation.is_some() {
                            outcome.affected_inputs.push(affected);
                        }
                    }
                }
            }
            (pending, requests, outcome)
        };

        // Refuse an armed attachment delete before publishing staged boundary
        // state. The same factory transaction excludes attachment GC.
        self.commit_attachment_refs_in_memory(
            &commit.session_id,
            &commit.committed_attachment_ids,
            transaction_now,
        )?;
        *self.queued_work.lock_recover() = staged_queued_work;
        *self.wake_redelivery_fences.lock_recover() = staged_wake_redelivery_fences;
        *self.queued_work_next_seq.lock_recover() = staged_queued_work_next_seq;
        *self.pending_turn_inputs.lock_recover() = staged_pending_turn_inputs;
        *self.turn_cancel_requests.lock_recover() = staged_turn_cancel_requests;
        let resident_graph = proposed.trim_to_active_path();
        let mut global_graph = self.global_session_graph.lock_recover();
        *global_graph = proposed;
        *self.session_graph.lock_recover() = resident_graph;
        drop(global_graph);
        *self.tombstoned_node_ids.lock_recover() = staged_tombstoned_node_ids;
        *self.global_session_heads.lock_recover() = staged_session_heads;
        for node in incoming_nodes {
            global_node_owners.insert(
                node.node_id.clone(),
                SessionId::from(commit.session_id.clone().to_string()),
            );
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
                .insert(commit.session_id.clone().clone(), roots);
        }
        *self.checkpoint.lock_recover() = Some(hydrated_checkpoint);
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
        let durable_relation = session_meta_before_commit.map(|meta| meta.relation);
        self.session_catalog
            .lock_recover()
            .entry(SessionId::from(session_id.clone().to_string()))
            .and_modify(|summary| {
                summary.last_commit_at_ms = Some(transaction_now);
                summary.head_revision = head_revision;
                summary.durable_relation = durable_relation.clone();
            })
            .or_insert_with(|| crate::SessionSummary {
                session_id: session_id.clone(),
                created_at_ms: transaction_now,
                last_commit_at_ms: Some(transaction_now),
                head_revision,
                relation: durable_relation
                    .as_ref()
                    .map(crate::SessionRelationKind::from_relation)
                    .unwrap_or(crate::SessionRelationKind::Root),
                durable_relation: durable_relation.clone(),
                parent_session_id: durable_relation
                    .as_ref()
                    .and_then(crate::SessionRelation::parent_session_id)
                    .map(ToOwned::to_owned)
                    .map(Into::into),
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
            append_request_identity: receipt.append_request_identity.clone(),
        };
        let mut runtime_turn_commits = self.runtime_turn_commits.lock_recover();
        runtime_turn_commits.insert(
            (
                session_id.clone().clone(),
                receipt.operation_key.to_string(),
            ),
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
                    (session_id.clone().clone(), marker),
                    RuntimeTurnCommitRecord {
                        append_request_identity: crate::AppendRequestIdentity::PlainCommit,
                        ..stored_receipt.clone()
                    },
                );
            }
        }
        drop(runtime_turn_commits);
        turn_cancel_closure::consume(self, commit);
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
        self.admit_and_bind_session_in_memory(binding)
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
            summary.durable_relation = Some(meta.relation.clone());
            summary.parent_session_id = meta
                .relation
                .parent_session_id()
                .map(ToOwned::to_owned)
                .map(Into::into);
        }
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<crate::store::SessionMeta>, crate::store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}

#[cfg(any(test, feature = "testing"))]
pub use factory::lineage_conformance_support::handles as in_memory_lineage_handles;

type SharedAttachmentManifest =
    Arc<Mutex<HashMap<(SessionId, crate::AttachmentId), crate::AttachmentManifestEntry>>>;
