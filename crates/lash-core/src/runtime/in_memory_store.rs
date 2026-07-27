//! Public in-memory `RuntimePersistence` + `SessionStoreFactory`.
//!
//! Explicitly-wired ephemeral storage for inline-tier hosts that run background
//! processes without durable backing: a `process` started in a turn (or by a
//! trigger) is executed by the lease-protected worker, which rebuilds its
//! session from the store factory — so even an in-memory host needs a factory.
//! This is the named, opt-in counterpart to a durable factory; there is no
//! silent in-memory default. Holds the same `RuntimePersistence` contract as the
//! durable backend (verified by the `runtime_persistence` conformance suite).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::usage::merge_usage_delta_entries;
use super::{SessionStoreCreateRequest, SessionStoreFactory};
use crate::store::RuntimePersistence;

mod attachments;
mod factory;
mod maintenance;
mod queued_work;
mod refcounts;
#[cfg(test)]
mod test_support;
#[cfg(any(test, feature = "testing"))]
mod testing_access;
mod turn_input;

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

impl InMemoryPendingTurnInput {
    fn claim_diagnostics(&self) -> Option<crate::PendingTurnInputClaimDiagnostics> {
        (self.claim_id.is_some() || matches!(self.input.state, crate::TurnInputState::Accepted))
            .then(|| crate::PendingTurnInputClaimDiagnostics {
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

    fn cancel_outcome(&mut self, claim_is_live: bool) -> crate::PendingTurnInputCancelOutcome {
        match self.input.state {
            crate::TurnInputState::Cancelled => {
                crate::PendingTurnInputCancelOutcome::AlreadyCancelled(self.input.clone())
            }
            crate::TurnInputState::Completed => {
                crate::PendingTurnInputCancelOutcome::AlreadyCompleted(self.input.clone())
            }
            crate::TurnInputState::Accepted => {
                crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    input: self.input.clone(),
                    claim: self.claim_diagnostics(),
                }
            }
            crate::TurnInputState::PendingActive | crate::TurnInputState::DeferredNextTurn => {
                if self.claim_token.is_some() && claim_is_live {
                    crate::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        input: self.input.clone(),
                        claim: self.claim_diagnostics(),
                    }
                } else {
                    self.input.state = crate::TurnInputState::Cancelled;
                    self.clear_claim();
                    crate::PendingTurnInputCancelOutcome::Cancelled(self.input.clone())
                }
            }
        }
    }
}

fn find_pending_turn_input_index(
    pending: &[InMemoryPendingTurnInput],
    session_id: &str,
    target: &crate::PendingTurnInputCancelTarget,
) -> Option<usize> {
    pending.iter().position(|entry| {
        entry.input.session_id == session_id
            && match target {
                crate::PendingTurnInputCancelTarget::InputId(input_id) => {
                    entry.input.input_id == *input_id
                }
                crate::PendingTurnInputCancelTarget::SourceKey(source_key) => {
                    entry.input.source_key.as_deref() == Some(source_key.as_str())
                }
            }
    })
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
    node_anchors: Arc<Mutex<HashMap<String, (crate::BlobRef, crate::HydratedSessionCheckpoint)>>>,
    tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
    incoming_node_refs: Arc<Mutex<HashMap<String, i64>>>,
    pub(crate) checkpoint: Mutex<Option<crate::HydratedSessionCheckpoint>>,
    pub(crate) usage_deltas: Mutex<Vec<crate::TokenLedgerEntry>>,
    pub(crate) runtime_commit_count: Mutex<usize>,
    runtime_turn_commits: Mutex<RuntimeTurnCommitMap>,
    session_execution_leases: Mutex<HashMap<String, InMemorySessionExecutionLease>>,
    queued_work: Mutex<Vec<InMemoryQueuedBatch>>,
    queued_work_next_seq: Mutex<u64>,
    pending_turn_inputs: Mutex<Vec<InMemoryPendingTurnInput>>,
    pending_turn_input_next_seq: Mutex<u64>,
    attachment_manifest:
        Mutex<HashMap<(String, crate::AttachmentId), crate::AttachmentManifestEntry>>,
    #[cfg(test)]
    claim_after_lease_validation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    fail_next_exact_queue_claim: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    load_session_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    checkpoint_probe_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    commit_write_transaction_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_next_session_execution_lease_renewal: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize,
}

type RuntimeTurnCommitRecord = (String, crate::store::RuntimeCommitResult, u64);
type RuntimeTurnCommitMap = HashMap<(String, String), RuntimeTurnCommitRecord>;

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
        node_anchors: Arc<
            Mutex<HashMap<String, (crate::BlobRef, crate::HydratedSessionCheckpoint)>>,
        >,
        tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
        incoming_node_refs: Arc<Mutex<HashMap<String, i64>>>,
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
            incoming_node_refs,
            checkpoint: Mutex::new(None),
            usage_deltas: Mutex::new(Vec::new()),
            runtime_commit_count: Mutex::new(0),
            runtime_turn_commits: Mutex::new(std::collections::HashMap::new()),
            session_execution_leases: Mutex::new(HashMap::new()),
            queued_work: Mutex::new(Vec::new()),
            queued_work_next_seq: Mutex::new(0),
            pending_turn_inputs: Mutex::new(Vec::new()),
            pending_turn_input_next_seq: Mutex::new(0),
            attachment_manifest: Mutex::new(HashMap::new()),
            #[cfg(test)]
            claim_after_lease_validation_hook: Mutex::new(None),
            #[cfg(test)]
            fail_next_exact_queue_claim: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            load_session_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_probe_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            checkpoint_write_transaction_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            commit_write_transaction_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_session_execution_lease_renewal: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            session_execution_lease_renewal_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            abandoned_queued_work_claim_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            abandoned_turn_input_claim_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    fn run_claim_after_lease_validation_hook(&self) {
        let hook = self
            .claim_after_lease_validation_hook
            .lock()
            .expect("lock claim validation hook")
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_claim_after_lease_validation_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .claim_after_lease_validation_hook
            .lock()
            .expect("lock claim validation hook") = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_exact_queue_claim(&self) {
        self.fail_next_exact_queue_claim
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn verify_session_execution_lease(
        &self,
        session_id: &str,
        fence: &crate::SessionExecutionLeaseFence,
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
        completion: &crate::SessionExecutionLeaseCompletion,
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
    ) -> Result<crate::runtime::QueuedWorkClass, crate::store::StoreError> {
        batch.work_class().ok_or_else(|| {
            crate::store::StoreError::Backend(format!(
                "queued-work batch `{}` has mixed or empty payload classes",
                batch.batch_id
            ))
        })
    }

    fn enqueue_queued_work_in_memory(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> crate::QueuedWorkBatch {
        let mut queued = self.queued_work.lock().expect("lock queued work");
        if let Some(source_key) = batch.source_key.as_deref()
            && let Some(existing) = queued.iter().find(|entry| {
                entry.batch.session_id == batch.session_id
                    && entry.batch.source_key.as_deref() == Some(source_key)
            })
        {
            return existing.batch.clone();
        }
        let mut next_seq = self
            .queued_work_next_seq
            .lock()
            .expect("lock queued work seq");
        *next_seq = next_seq.saturating_add(1);
        let batch_id = format!("recording-qwb-{next_seq}");
        let stored = crate::QueuedWorkBatch {
            batch_id: batch_id.clone(),
            session_id: batch.session_id,
            enqueue_seq: *next_seq,
            source_key: batch.source_key,
            delivery_policy: batch.delivery_policy,
            slot_policy: batch.slot_policy,
            merge_key: batch.merge_key,
            available_at_ms: batch.available_at_ms,
            enqueued_at_ms: self.clock.timestamp_ms(),
            items: batch
                .payloads
                .into_iter()
                .enumerate()
                .map(|(index, payload)| crate::QueuedWorkItem {
                    item_id: format!("{batch_id}:item:{index}"),
                    payload,
                })
                .collect(),
        };
        queued.push(InMemoryQueuedBatch {
            batch: stored.clone(),
            claim_id: None,
            claim_token: None,
            claim_owner: None,
            claim_fencing_token: 0,
            claim_session_lease_generation: 0,
        });
        queued.sort_by_key(|entry| entry.batch.enqueue_seq);
        stored
    }

    fn claim_ready_queued_work_in_memory(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseFence,
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
        session_execution_lease: &crate::SessionExecutionLeaseFence,
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
        let claim_id = format!("recording-qwc:{}:{fencing_token}", first.enqueue_seq);
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
            batches,
        }))
    }

    fn claim_pending_turn_inputs_in_memory(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseFence,
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
        session_execution_lease: &crate::SessionExecutionLeaseFence,
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
                            entry.input.state == crate::TurnInputState::PendingActive
                                && entry
                                    .input
                                    .ingress
                                    .active_turn_id()
                                    .is_some_and(|active| active == turn_id)
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
        let claim_id = format!(
            "recording-tic:{}:{fencing_token}",
            pending[first_index].input.enqueue_seq
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
            mode,
            inputs,
            applications: Vec::new(),
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
                        && entry.input.state == crate::TurnInputState::PendingActive
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
                    class == crate::runtime::QueuedWorkClass::TurnWork
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
        scope: crate::store::SessionReadScope,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::store::StoreError> {
        #[cfg(test)]
        self.load_session_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let requested_leaf = match scope {
            crate::store::SessionReadScope::FullGraph => meta.leaf_node_id.clone(),
            crate::store::SessionReadScope::ActivePath { leaf_node_id } => {
                leaf_node_id.or_else(|| meta.leaf_node_id.clone())
            }
        };
        let mut graph = self
            .global_session_graph
            .lock()
            .expect("lock global graph")
            .clone();
        graph.set_leaf_node_id(requested_leaf);
        graph = graph.trim_to_active_path();
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
            token_ledger: merge_usage_delta_entries(
                self.usage_deltas.lock().expect("lock usage deltas").clone(),
            ),
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
        Ok(self
            .global_session_graph
            .lock()
            .expect("lock global graph")
            .find_node(node_id)
            .cloned())
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitResult, crate::store::StoreError> {
        commit.validate_budget()?;
        commit.validate_operation_session()?;
        let turn_input_applications = commit.turn_input_applications();
        let realization_digest = crate::store::graph_realization_digest(&commit.graph);
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| crate::store::RealizedNodeTimestamp {
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
        if let Some(completed) = &commit.turn_commit {
            let operation_key = completed.operation.storage_key()?;
            let key = (completed.session_id.clone(), operation_key.clone());
            if let Some((stored_hash, result, _committed_at_ms)) = self
                .runtime_turn_commits
                .lock()
                .expect("lock runtime turn commits")
                .get(&key)
                .cloned()
            {
                if stored_hash == completed.turn_commit_hash {
                    if let Some(completion) = commit.release_session_execution_lease.as_ref() {
                        self.release_session_execution_lease_in_memory(completion);
                    }
                    return Ok(result);
                }
                return Err(crate::store::StoreError::RuntimeTurnCommitConflict {
                    session_id: completed.session_id.clone(),
                    turn_id: operation_key,
                });
            }
        }
        let expected = commit.expected_head_revision;
        if expected != actual {
            return Err(crate::store::StoreError::HeadRevisionConflict {
                expected: commit.expected_head_revision,
                actual,
            });
        }
        commit.validate_node_derivation()?;
        commit.validate_append_node_ids_unique()?;
        commit.graph.validate_append_topology()?;
        let incoming_nodes = match &commit.graph {
            crate::store::GraphCommitDelta::Unchanged { .. } => &[][..],
            crate::store::GraphCommitDelta::Append { nodes, .. } => nodes.as_slice(),
        };
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
        if let crate::store::GraphCommitDelta::Append { nodes, .. } = &commit.graph {
            let graph = self.global_session_graph.lock().expect("lock global graph");
            let tombstoned = self
                .tombstoned_node_ids
                .lock()
                .expect("lock tombstoned nodes");
            if let Some(node) = nodes.iter().find(|node| {
                graph.find_node(&node.node_id).is_some() || tombstoned.contains(&node.node_id)
            }) {
                return Err(crate::store::StoreError::NodeIdCollision {
                    node_id: node.node_id.clone(),
                });
            }
        }
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
        match &commit.graph {
            crate::store::GraphCommitDelta::Unchanged { leaf_node_id }
                if leaf_node_id != &old_leaf_node_id =>
            {
                return Err(crate::StoreError::InvalidGraphLeaf {
                    leaf_node_id: leaf_node_id.clone(),
                });
            }
            crate::store::GraphCommitDelta::Append { nodes, .. }
                if !nodes.is_empty()
                    && nodes.first().and_then(|node| node.parent_node_id.as_ref())
                        != old_leaf_node_id.as_ref() =>
            {
                return Err(crate::StoreError::InvalidGraphParent {
                    node_id: nodes
                        .first()
                        .map(|node| node.node_id.clone())
                        .unwrap_or_default(),
                    expected: old_leaf_node_id,
                    actual: nodes.first().and_then(|node| node.parent_node_id.clone()),
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
            if let crate::store::GraphCommitDelta::Append { nodes, .. } = &commit.graph {
                proposed.extend_node_records(nodes.iter().cloned());
            }
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
        let (staged_incoming_node_refs, staged_tombstoned_node_ids, staged_session_heads) = {
            let mut proposed = self
                .global_session_graph
                .lock()
                .expect("lock global graph")
                .clone();
            if let crate::store::GraphCommitDelta::Append { nodes, .. } = &commit.graph {
                proposed.extend_node_records(nodes.iter().cloned());
            }
            let new_leaf_node_id = commit.graph.leaf_node_id().cloned();
            proposed.set_leaf_node_id(new_leaf_node_id.clone());
            let old_leaf_node_id = meta.as_ref().and_then(|head| head.leaf_node_id.clone());
            let mut counts = self
                .incoming_node_refs
                .lock()
                .expect("lock incoming node refs")
                .clone();
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
                counts.insert(node.node_id.clone(), 0);
            }
            for node in incoming_nodes {
                if let Some(parent_node_id) = &node.parent_node_id {
                    if proposed.find_node(parent_node_id).is_none()
                        || tombstoned.contains(parent_node_id)
                    {
                        return Err(crate::store::StoreError::InvalidGraphParent {
                            node_id: node.node_id.clone(),
                            expected: node.parent_node_id.clone(),
                            actual: None,
                        });
                    }
                    *counts.entry(parent_node_id.clone()).or_default() += 1;
                }
            }
            if old_leaf_node_id != new_leaf_node_id {
                if let Some(new_leaf_node_id) = &new_leaf_node_id {
                    *counts.entry(new_leaf_node_id.clone()).or_default() += 1;
                }
                if let Some(old_leaf_node_id) = old_leaf_node_id {
                    Self::decrement_node_reference(
                        &proposed,
                        &mut counts,
                        &mut tombstoned,
                        &old_leaf_node_id,
                        &session_heads,
                        &anchored_node_ids,
                    )?;
                }
            }
            (counts, tombstoned, session_heads)
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
                    return Err(crate::store::StoreError::QueuedWorkClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
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
                    return Err(crate::store::StoreError::TurnInputClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
                    });
                }
            }
        }
        {
            let mut queued = self.queued_work.lock().expect("lock queued work");
            for completed in &commit.completed_queue_claims {
                queued.retain(|entry| {
                    !(entry.batch.session_id == completed.session_id
                        && entry.claim_id.as_deref() == Some(completed.claim_id.as_str())
                        && entry.claim_token.as_deref() == Some(completed.lease_token.as_str())
                        && completed.batch_ids.contains(&entry.batch.batch_id))
                });
            }
        }
        {
            let mut pending = self
                .pending_turn_inputs
                .lock()
                .expect("lock pending turn input");
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
        }
        if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
            let mut pending = self
                .pending_turn_inputs
                .lock()
                .expect("lock pending turn input");
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
        let mut global_graph = self.global_session_graph.lock().expect("lock global graph");
        let leaf_node_id = match &commit.graph {
            crate::store::GraphCommitDelta::Unchanged { leaf_node_id } => leaf_node_id.clone(),
            crate::store::GraphCommitDelta::Append {
                nodes,
                leaf_node_id,
            } => {
                global_graph.extend_node_records(nodes.iter().cloned());
                leaf_node_id.clone()
            }
        };
        let mut resident_graph = global_graph.clone();
        resident_graph.set_leaf_node_id(leaf_node_id.clone());
        resident_graph = resident_graph.trim_to_active_path();
        let graph_node_count = resident_graph.nodes.len();
        *self.session_graph.lock().expect("lock graph") = resident_graph;
        drop(global_graph);
        *self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs") = staged_incoming_node_refs;
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
        self.usage_deltas
            .lock()
            .expect("lock usage deltas")
            .extend(commit.usage_deltas.iter().cloned());
        let checkpoint_ref = crate::BlobRef(format!("recording-checkpoint-{}", actual + 1));
        let manifest = crate::store::SessionCheckpoint::new(
            commit.checkpoint.turn_state.clone(),
            commit.checkpoint.tool_state_ref.clone(),
            commit.checkpoint.plugin_snapshot_ref.clone(),
            commit.checkpoint.plugin_snapshot_revision,
            commit.checkpoint.execution_state_ref.clone(),
        );
        *self.checkpoint.lock().expect("lock checkpoint") = Some(commit.checkpoint);
        crate::AttachmentManifest::commit_refs(
            self,
            &commit.session_id,
            &commit.committed_attachment_ids,
        )?;
        if let Some(completed) = &commit.turn_commit {
            self.commit_turn_attachment_intents(completed);
        }
        let head_revision = actual + 1;
        *meta = Some(crate::SessionHeadMeta {
            schema_version: crate::store::SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: commit.session_id,
            head_revision,
            config: commit.config,
            current_frame_node_id: commit.current_frame_node_id,
            checkpoint_ref: Some(checkpoint_ref.clone()),
            leaf_node_id,
            graph_node_count,
            token_ledger: Vec::new(),
        });
        *self
            .runtime_commit_count
            .lock()
            .expect("lock runtime commit count") += 1;
        let result = crate::store::RuntimeCommitResult {
            head_revision,
            checkpoint_ref,
            manifest,
            realization_digest,
            realized_node_timestamps,
            enqueued_queue_batches: commit
                .enqueued_queue_batches
                .into_iter()
                .map(|batch| self.enqueue_queued_work_in_memory(batch))
                .collect(),
            turn_input_applications,
        };
        if let Some(completed) = &commit.turn_commit {
            let operation_key = completed.operation.storage_key()?;
            self.runtime_turn_commits
                .lock()
                .expect("lock runtime turn commits")
                .insert(
                    (completed.session_id.clone(), operation_key),
                    (
                        completed.turn_commit_hash.clone(),
                        result.clone(),
                        self.clock.timestamp_ms(),
                    ),
                );
        }
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            self.release_session_execution_lease_in_memory(completion);
        }
        Ok(result)
    }

    async fn save_session_meta(
        &self,
        meta: crate::store::SessionMeta,
    ) -> Result<(), crate::store::StoreError> {
        *self.session_meta.lock().expect("lock session meta") = Some(meta);
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<crate::store::SessionMeta>, crate::store::StoreError> {
        Ok(self.session_meta.lock().expect("lock session meta").clone())
    }
}

#[async_trait::async_trait]
impl crate::store::SessionExecutionLeaseStore for InMemorySessionStore {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLeaseClaimOutcome, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let now = self.clock.timestamp_ms();
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases");
        let current = leases.entry(session_id.to_string()).or_default();
        if current.is_live(now) {
            if current
                .owner
                .as_ref()
                .is_some_and(|current_owner| current_owner.same_incarnation(owner))
            {
                current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
                return Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
                    Self::in_memory_session_execution_lease(session_id, current),
                ));
            }
            return Ok(crate::SessionExecutionLeaseClaimOutcome::Busy {
                holder: Self::in_memory_session_execution_lease(session_id, current),
            });
        }
        Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
            Self::acquire_session_execution_lease_in_memory(
                session_id,
                owner,
                current,
                now,
                lease_ttl_ms,
            ),
        ))
    }

    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &crate::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLeaseClaimOutcome, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let now = self.clock.timestamp_ms();
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases");
        let current = leases.entry(session_id.to_string()).or_default();
        if !current.is_live(now) {
            return Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
                Self::acquire_session_execution_lease_in_memory(
                    session_id,
                    owner,
                    current,
                    now,
                    lease_ttl_ms,
                ),
            ));
        }
        let holder = Self::in_memory_session_execution_lease(session_id, current);
        if observed_holder.session_id == session_id
            && holder.owner.same_incarnation(&observed_holder.owner)
            && holder.lease_token == observed_holder.lease_token
            && holder.fencing_token == observed_holder.fencing_token
            && holder.owner.is_definitely_dead_for_claimant(owner)
        {
            return Ok(crate::SessionExecutionLeaseClaimOutcome::Acquired(
                Self::acquire_session_execution_lease_in_memory(
                    session_id,
                    owner,
                    current,
                    now,
                    lease_ttl_ms,
                ),
            ));
        }
        Ok(crate::SessionExecutionLeaseClaimOutcome::Busy { holder })
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &crate::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLease, crate::store::StoreError> {
        #[cfg(test)]
        {
            self.session_execution_lease_renewal_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .fail_next_session_execution_lease_renewal
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(crate::store::StoreError::Backend(
                    "injected session execution lease renewal rejection".to_string(),
                ));
            }
        }
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let now = self.clock.timestamp_ms();
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("lock session execution leases");
        let Some(current) = leases.get_mut(&fence.session_id) else {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !current
            .owner
            .as_ref()
            .is_some_and(|owner| owner.same_incarnation(&fence.owner))
            || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
            || current.fencing_token != fence.fencing_token
            || current.expires_at_epoch_ms <= now
        {
            return Err(crate::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        current.expires_at_epoch_ms = now.saturating_add(lease_ttl_ms);
        Ok(crate::SessionExecutionLease {
            session_id: fence.session_id.clone(),
            owner: fence.owner.clone(),
            lease_token: fence.lease_token.clone(),
            fencing_token: fence.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_epoch_ms,
            expires_at_epoch_ms: current.expires_at_epoch_ms,
        })
    }

    async fn release_session_execution_lease(
        &self,
        completion: &crate::SessionExecutionLeaseCompletion,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.release_session_execution_lease_in_memory(completion);
        Ok(())
    }
}

/// Test-only introspection: call counters and a head-meta seeder used by the
/// lash-core runtime tests. Compiled only under `cfg(test)`, so the shipped
/// embedding surface carries none of it.
#[cfg(test)]
impl InMemorySessionStore {
    pub(crate) async fn save_session_head_meta(&self, meta: crate::SessionHeadMeta) {
        *self.session_head_meta.lock().expect("lock store") = Some(meta);
    }

    pub(crate) fn load_session_count(&self) -> usize {
        self.load_session_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn checkpoint_claim_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// Session-id-keyed factory: the same in-memory store is returned for a given
/// session across opens (so a worker rebuild sees the session's state), and a
/// fresh store is created on first use.
#[derive(Clone)]
pub struct InMemorySessionStoreFactory {
    clock: Arc<dyn crate::Clock>,
    stores: Arc<Mutex<HashMap<String, Arc<InMemorySessionStore>>>>,
    write_transaction: Arc<Mutex<()>>,
    global_session_graph: Arc<Mutex<crate::SessionGraph>>,
    global_node_owners: Arc<Mutex<HashMap<String, String>>>,
    global_session_heads: Arc<Mutex<HashMap<String, Option<String>>>>,
    node_anchors: Arc<Mutex<HashMap<String, (crate::BlobRef, crate::HydratedSessionCheckpoint)>>>,
    tombstoned_node_ids: Arc<Mutex<HashSet<String>>>,
    incoming_node_refs: Arc<Mutex<HashMap<String, i64>>>,
}

impl InMemorySessionStoreFactory {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::SystemClock))
    }

    pub fn with_clock(clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            clock,
            stores: Arc::new(Mutex::new(HashMap::new())),
            write_transaction: Arc::new(Mutex::new(())),
            global_session_graph: Arc::new(Mutex::new(crate::SessionGraph::default())),
            global_node_owners: Arc::new(Mutex::new(HashMap::new())),
            global_session_heads: Arc::new(Mutex::new(HashMap::new())),
            node_anchors: Arc::new(Mutex::new(HashMap::new())),
            tombstoned_node_ids: Arc::new(Mutex::new(HashSet::new())),
            incoming_node_refs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySessionStoreFactory {
    fn default() -> Self {
        Self::new()
    }
}
