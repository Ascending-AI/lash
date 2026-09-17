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

use crate::SessionStoreCreateRequest;
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
mod session_commit;
mod session_execution_lease;
use session_execution_lease::{HeldLeaseIdentity, InMemorySessionExecutionLease};
mod state_version;
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
#[cfg(any(test, feature = "testing"))]
mod testing_access;
#[cfg(any(test, feature = "testing"))]
pub use testing_access::RawSessionExecutionLeaseRow;
mod claim_hold;
mod turn_cancel_closure;
mod turn_input;
mod warnings;
use claim_hold::{ClaimHold, InMemoryClaimMint, InMemoryClaimRow, mint_in_memory_claim};

use receipts::{RuntimeTurnCommitMap, RuntimeTurnCommitRecord};

#[derive(Clone)]
pub struct InMemoryQueuedBatch {
    batch: crate::QueuedWorkBatch,
    claim: ClaimHold,
}

#[derive(Clone)]
pub struct InMemoryPendingTurnInput {
    input: crate::PendingTurnInput,
    claim: ClaimHold,
}

impl InMemoryClaimRow for InMemoryQueuedBatch {
    fn claim(&self) -> &ClaimHold {
        &self.claim
    }

    fn claim_mut(&mut self) -> &mut ClaimHold {
        &mut self.claim
    }
}

impl InMemoryClaimRow for InMemoryPendingTurnInput {
    fn claim(&self) -> &ClaimHold {
        &self.claim
    }

    fn claim_mut(&mut self) -> &mut ClaimHold {
        &mut self.claim
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

type InMemoryNodeAnchorRecord = (crate::BlobRef, crate::HydratedSessionCheckpoint, SessionId);
type InMemoryNodeAnchors = Arc<Mutex<HashMap<crate::NodeId, InMemoryNodeAnchorRecord>>>;
/// Session id -> component blob refs its live checkpoint references.
pub(crate) type SharedCheckpointBlobRoots = Arc<Mutex<HashMap<SessionId, HashSet<crate::BlobRef>>>>;
pub(crate) type SharedSessionCatalog = Arc<Mutex<HashMap<SessionId, crate::SessionSummary>>>;

#[cfg(any(test, feature = "testing"))]
pub type RawPendingTurnInputForTesting = (
    crate::InputId,
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

/// The two condemnation phases. Absence from the map is the `Free` state.
/// A claim on `Condemned` gives one writer temporary ownership while it
/// restores the bytes; the phase itself remains until the write succeeds.
/// A completed physical delete removes the entry entirely — there is no
/// terminal phase, because condemnation already removed every manifest row and
/// adoption is gated on positive upload evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentCondemnationPhase {
    /// Claimed by a sweeper, no physical delete issued yet: a writer revokes it.
    Condemned {
        write_claim: Option<AttachmentWriteClaim>,
    },
    /// The physical delete is in flight: a writer must wait for its outcome.
    Deleting,
}

/// Association between one restoring attempt and its manifest intent. Host
/// recovery uses the session identity to remove exactly the abandoned attempt's
/// uncommitted row before releasing its claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentWriteClaim {
    pub(super) write_id: crate::AttachmentWriteToken,
    pub(super) session_id: SessionId,
}

pub struct InMemorySessionStore {
    pub clock: Arc<dyn crate::Clock>,
    /// Factory-lifetime authority for the reserved turn-cancellation promises.
    /// Factory-created stores expose the same resolver and binding identity on
    /// reopen. Standalone stores leave authority with their configured host.
    pub turn_cancellation_authority:
        Option<Arc<dyn lash_core_store::turn_control_binding::StoreTurnCancellationAuthority>>,
    /// Serializes every operation whose correctness depends on observing the
    /// session lease and mutating fenced runtime state atomically. Component
    /// mutexes still guard their data; this mutex supplies the transaction
    /// boundary and lock ordering that SQLite/Postgres provide natively.
    /// Poison recovery is deliberate under ADR 0054. These critical sections
    /// must remain host-code-free: read clocks and invoke any other dynamic
    /// host surface before acquiring this lock, then carry inert values in.
    pub write_transaction: Arc<Mutex<()>>,
    pub bound_session_id: Mutex<Option<SessionId>>,
    pub session_head_meta: Mutex<Option<crate::SessionHeadMeta>>,
    pub session_meta: Mutex<Option<crate::SessionMeta>>,
    /// Independently readable mutable-continuation generation beside binding metadata.
    pub session_state_version: Mutex<Option<u32>>,
    pub corrupt_session_payload_for_testing: std::sync::atomic::AtomicBool,
    pub session_graph: Mutex<crate::SessionGraph>,
    /// Shared leafless node catalog; never treated as a resident graph without a real leaf grafted
    /// first.
    pub global_session_graph: Arc<Mutex<crate::SessionGraph>>,
    pub global_node_owners: Arc<Mutex<HashMap<crate::NodeId, SessionId>>>,
    pub global_session_heads: Arc<Mutex<HashMap<SessionId, Option<crate::NodeId>>>>,
    pub node_anchors: InMemoryNodeAnchors,
    pub tombstoned_node_ids: Arc<Mutex<HashSet<crate::NodeId>>>,
    /// Permanent per-factory deletion ledger. Maintenance never prunes this:
    /// an id, once used and deleted in this store, must never be reused.
    pub deleted_session_ids: Arc<Mutex<HashSet<SessionId>>>,
    pub session_catalog: SharedSessionCatalog,
    pub checkpoint: Mutex<Option<crate::HydratedSessionCheckpoint>>,
    pub checkpoint_component_blobs: Arc<Mutex<HashMap<crate::BlobRef, Vec<u8>>>>,
    /// Factory-global reference edges from a session to the component blobs its
    /// *live* checkpoint holds. Edges, not counts (ADR 0067 §4): a commit
    /// replaces its session's edge set, a delete drops it, and
    /// `gc_unreachable` decides liveness by `NOT EXISTS` over the union. This
    /// is what lets the in-memory backend witness its own root set instead of
    /// reporting an unconditional empty sweep.
    pub checkpoint_blob_roots: SharedCheckpointBlobRoots,
    pub usage_deltas: Mutex<Vec<crate::store::RuntimeUsageDelta>>,
    pub runtime_commit_count: Mutex<usize>,
    pub runtime_turn_commits: Mutex<RuntimeTurnCommitMap>,
    pub session_execution_leases: Mutex<HashMap<SessionId, InMemorySessionExecutionLease>>,
    pub turn_cancellation_binding: Mutex<Option<(String, Option<crate::ExecutionScope>)>>,
    pub turn_cancel_closure_authorizations:
        Mutex<HashMap<TurnId, crate::TurnCancelClosureAuthorization>>,
    pub retired_turn_cancel_scopes: Arc<Mutex<HashSet<String>>>,
    pub queued_work: Mutex<Vec<InMemoryQueuedBatch>>,
    pub queued_work_next_seq: Mutex<u64>,
    /// Receiver-side sender allocation floor. This is a redelivery fence, not
    /// a consumption watermark: selected-batch settlement may be out of order.
    pub wake_redelivery_fences: Mutex<HashMap<(String, String), u64>>,
    pub pending_turn_inputs: Mutex<Vec<InMemoryPendingTurnInput>>,
    pub pending_turn_input_next_seq: Mutex<u64>,
    pub turn_cancel_requests: Mutex<HashMap<TurnId, InMemoryTurnCancelRequest>>,
    pub attachment_manifest: SharedAttachmentManifest,
    /// The attempt identity currently owning each manifest row, held beside the
    /// manifest rather than on the public entry projection: a host may observe
    /// *that* an upload completed, never present the fence identity that proves
    /// it. Shared factory-wide with the manifest it keys.
    pub attachment_write_ids: SharedAttachmentWriteIds,
    /// Per-digest attachment GC condemnation state, shared with every store the
    /// same factory owns because the digest is factory-global: the writer's
    /// intent insert and the sweeper's condemn CAS must meet here.
    pub attachment_condemnations: SharedAttachmentCondemnations,
    #[cfg(any(test, feature = "testing"))]
    pub claim_after_lease_validation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(any(test, feature = "testing"))]
    pub fail_next_exact_queue_claim: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    pub drop_next_list_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    pub drop_next_list_pending_queued_work_batch: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    pub list_pending_queued_work_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub load_session_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub load_session_head_meta_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub fail_next_load_session_head_meta: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    pub fail_load_session_on_call: Mutex<Option<usize>>,
    #[cfg(any(test, feature = "testing"))]
    pub checkpoint_probe_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub commit_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub fail_next_runtime_commit: Mutex<Option<crate::StoreError>>,
    #[cfg(any(test, feature = "testing"))]
    pub inject_turn_cancel_before_next_runtime_commit: Mutex<Option<crate::TurnCancelRequest>>,
    #[cfg(any(test, feature = "testing"))]
    pub fail_next_runtime_commit_after_first_mutation: Mutex<Option<crate::StoreError>>,
    #[cfg(any(test, feature = "testing"))]
    pub fail_next_session_execution_lease_renewal: Mutex<Option<crate::StoreError>>,
    #[cfg(any(test, feature = "testing"))]
    pub force_next_session_execution_lease_renewal_zero_match: std::sync::atomic::AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    pub next_session_execution_lease_renewal_response: Mutex<Option<crate::SessionExecutionLease>>,
    #[cfg(any(test, feature = "testing"))]
    pub session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub session_execution_lease_release_gate:
        Mutex<Option<Arc<test_support::SessionExecutionLeaseReleaseGate>>>,
    #[cfg(any(test, feature = "testing"))]
    pub session_execution_lease_release_attempt_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub raw_counter_defects: Mutex<HashMap<String, i64>>,
    #[cfg(any(test, feature = "testing"))]
    pub abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(any(test, feature = "testing"))]
    pub session_admission_count: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
pub struct InMemoryTurnCancelRequest {
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
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_shared_history(
        clock: Arc<dyn crate::Clock>,
        turn_cancellation_authority: Option<
            Arc<dyn lash_core_store::turn_control_binding::StoreTurnCancellationAuthority>,
        >,
        write_transaction: Arc<Mutex<()>>,
        global_session_graph: Arc<Mutex<crate::SessionGraph>>,
        global_node_owners: Arc<Mutex<HashMap<crate::NodeId, SessionId>>>,
        global_session_heads: Arc<Mutex<HashMap<SessionId, Option<crate::NodeId>>>>,
        node_anchors: InMemoryNodeAnchors,
        checkpoint_component_blobs: Arc<Mutex<HashMap<crate::BlobRef, Vec<u8>>>>,
        checkpoint_blob_roots: SharedCheckpointBlobRoots,
        tombstoned_node_ids: Arc<Mutex<HashSet<crate::NodeId>>>,
        deleted_session_ids: Arc<Mutex<HashSet<SessionId>>>,
        session_catalog: SharedSessionCatalog,
        attachment_condemnations: SharedAttachmentCondemnations,
        attachment_manifest: SharedAttachmentManifest,
        retired_turn_cancel_scopes: Arc<Mutex<HashSet<String>>>,
        attachment_write_ids: SharedAttachmentWriteIds,
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
            attachment_write_ids,
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

    /// The generation and exact expiry of the session's currently-live
    /// execution lease, or `None` when no live lease holds the session.
    fn live_session_lease(&self, session_id: &SessionId, now: u64) -> Option<(u64, u64)> {
        let leases = self.session_execution_leases.lock_recover();
        leases
            .get(session_id)
            .filter(|lease| lease.is_live(now))
            .and_then(|lease| {
                lease
                    .held_fields()
                    .map(|fields| (lease.fencing_token, fields.expires_at_epoch_ms))
            })
    }

    /// The fencing token of the session's currently-live execution lease, or
    /// `None` when no live lease holds the session. A queued-work or turn-input
    /// claim is live for lease-less host callers exactly when the generation it
    /// pins equals this value (ADR 0029).
    fn live_session_lease_generation(&self, session_id: &SessionId, now: u64) -> Option<u64> {
        self.live_session_lease(session_id, now)
            .map(|(generation, _)| generation)
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
            // Released rows keep their fencing generation and zero their
            // timing columns, exactly like the durable `= 0` release writes.
            current.holder = None;
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
        current.holder = Some(HeldLeaseIdentity {
            owner: owner.clone(),
            executor_id: executor_id.to_string(),
            lease_token: lease_token.to_string(),
        });
        current.claimed_at_epoch_ms = now;
        current.lease_term_ms = lease_ttl_ms;
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
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
        let enqueue_seq = queued[selected_indices[0]].batch.enqueue_seq;
        let minted = mint_in_memory_claim(
            queued,
            InMemoryClaimMint {
                selected_indices: &selected_indices,
                enqueue_seq,
                dialect: crate::store::queued_work::ClaimIdDialect::RecordingQueuedWork,
                fencing_label: "queued_work_claim_fencing_token",
                session_id,
                owner,
                generation,
                now,
            },
        )?;
        let batches = selected_indices
            .iter()
            .map(|&index| queued[index].batch.clone())
            .collect();
        Ok(crate::QueuedWorkClaimOutcome::Claimed(
            crate::QueuedWorkClaim {
                session_id: SessionId::from(session_id.to_string()),
                claim_id: minted.claim_id,
                owner: owner.clone(),
                lease_token: minted.lease_token,
                fencing_token: minted.fencing_token,
                session_lease_generation: generation,
                data: crate::QueuedWorkClaimData {
                    batches,
                    abandon_restore_claim_id: minted.abandon_restore_claim_id,
                    abandon_restore_claim_token: minted
                        .abandon_restore_claim_token
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
                                &entry.input.state,
                                crate::TurnInputState::PendingActive(scope)
                                    | crate::TurnInputState::Accepted(scope)
                                    if scope.turn_id == *turn_id
                                        && scope.min_boundary.admits(*checkpoint)
                            )
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
        let enqueue_seq = pending[first_index].input.enqueue_seq;
        let minted = mint_in_memory_claim(
            pending,
            InMemoryClaimMint {
                selected_indices: &selected_indices,
                enqueue_seq,
                dialect: crate::store::queued_work::ClaimIdDialect::RecordingTurnInput,
                fencing_label: "turn_input_claim_fencing_token",
                session_id,
                owner,
                generation,
                now,
            },
        )?;
        let mut inputs = Vec::new();
        for index in selected_indices {
            let entry = &mut pending[index];
            if matches!(mode, crate::TurnInputClaimMode::ActiveTurn { .. })
                && let Some(accepted) = entry.input.state.accepted()
            {
                entry.input.state = accepted;
            }
            inputs.push(entry.input.clone());
        }
        Ok(Some(crate::TurnInputClaim {
            session_id: SessionId::from(session_id.to_string()),
            claim_id: minted.claim_id,
            owner: owner.clone(),
            lease_token: minted.lease_token,
            fencing_token: minted.fencing_token,
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
                        &entry.input.state,
                        crate::TurnInputState::PendingActive(scope)
                            | crate::TurnInputState::Accepted(scope)
                            if scope.turn_id == *turn_id
                                && scope.min_boundary.admits(checkpoint)
                    )
                    && (entry.claim.claimable_by(generation))
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

#[cfg(any(test, feature = "testing"))]
pub use factory::lineage_conformance_support::handles as in_memory_lineage_handles;

type SharedAttachmentManifest =
    Arc<Mutex<HashMap<(SessionId, crate::AttachmentId), crate::AttachmentManifestEntry>>>;

/// Attempt identity per manifest row, keyed exactly as the manifest is.
pub(crate) type SharedAttachmentWriteIds =
    Arc<Mutex<HashMap<(SessionId, crate::AttachmentId), crate::AttachmentWriteToken>>>;
