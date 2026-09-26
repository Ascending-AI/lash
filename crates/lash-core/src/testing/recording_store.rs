//! A recording and fault-injecting decorator over any session store.
//!
//! [`RecordingStore`] wraps the store a test's backend hands out and adds
//! only what a test observes or forces: call counters and one-shot faults on
//! the operations the runtime suites steer. Every persistence semantic stays
//! the backend's own (ADR 0102): the decorator never answers a read or
//! applies a write itself, so a test that runs over it runs over the real
//! store.
//!
//! [`RecordingSessionStoreFactory`] decorates a backend's session catalog the
//! same way, wrapping every store it creates or reopens in a
//! [`RecordingStore`] and keeping the ones it created for the test to read.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_sansio::sync::MutexExt;

use crate::store::{
    PersistedSessionRead, RuntimeCommit, RuntimeCommitReceipt, RuntimePersistence,
    RuntimePersistenceDecorator, SessionExecutionLeaseAuthority, StoreError,
};
use crate::{SessionId, SessionStoreCreateRequest, SessionStoreFactory};

/// A session store with call counters and one-shot faults over the store it
/// wraps. See the module documentation.
pub struct RecordingStore {
    inner: Arc<dyn RuntimePersistence>,
    /// Runtime commits the wrapped store applied; an attempt it answered from
    /// an earlier commit's durable receipt is not one.
    pub runtime_commit_count: Mutex<usize>,
    /// Every runtime commit the wrapped store applied, in order: the committed
    /// bytes a test pins.
    runtime_commits: Mutex<Vec<RuntimeCommit>>,
    commit_attempt_count: AtomicUsize,
    load_session_count: AtomicUsize,
    load_session_head_meta_count: AtomicUsize,
    list_pending_queued_work_count: AtomicUsize,
    session_execution_lease_renewal_count: AtomicUsize,
    session_execution_lease_release_attempt_count: AtomicUsize,
    fail_next_runtime_commit: Mutex<Option<StoreError>>,
    inject_turn_cancel_before_next_runtime_commit: Mutex<Option<crate::TurnCancelRequest>>,
    fail_next_load_session_head_meta: AtomicBool,
    fail_load_session_on_call: Mutex<Option<usize>>,
    fail_next_session_execution_lease_renewal: Mutex<Option<StoreError>>,
    mutate_next_session_execution_lease_renewal: Mutex<Option<RenewalMutation>>,
    session_execution_lease_release_gate: Mutex<Option<Arc<SessionExecutionLeaseReleaseGate>>>,
    session_admission_count: AtomicUsize,
    abandoned_queued_work_claim_count: AtomicUsize,
    abandoned_turn_input_claim_count: AtomicUsize,
    claim_hook: Mutex<Option<ClaimHook>>,
    forged_head: Mutex<Option<crate::SessionHeadMeta>>,
    attachment_intents: Mutex<Vec<crate::store::AttachmentIntent>>,
}

/// A hook a test runs as the next claim reaches the store.
pub type ClaimHook = Arc<dyn Fn() + Send + Sync>;

/// Rewrites the lease a granted renewal answers with.
pub type RenewalMutation =
    Box<dyn FnOnce(crate::SessionExecutionLease) -> crate::SessionExecutionLease + Send>;

/// Suspends `release_session_execution_lease` before it reaches the wrapped
/// store, so a test can order a release against another operation or drop
/// the release future at exactly that point.
#[derive(Debug)]
pub struct SessionExecutionLeaseReleaseGate {
    entered: tokio::sync::Notify,
    admitted: tokio::sync::Semaphore,
}

impl SessionExecutionLeaseReleaseGate {
    fn new() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            admitted: tokio::sync::Semaphore::new(0),
        }
    }

    /// Wait until a release attempt has reached the gate.
    pub async fn wait_entered(&self) {
        self.entered.notified().await;
    }

    /// Let exactly one release attempt through.
    pub fn admit_one(&self) {
        self.admitted.add_permits(1);
    }

    async fn enter(&self) {
        self.entered.notify_one();
        self.admitted
            .acquire()
            .await
            .expect("lease release gate stays open")
            .forget();
    }
}

impl RecordingStore {
    /// Record over `inner`, a store the test's backend opened.
    pub fn over(inner: Arc<dyn RuntimePersistence>) -> Self {
        Self {
            inner,
            runtime_commit_count: Mutex::new(0),
            runtime_commits: Mutex::new(Vec::new()),
            commit_attempt_count: AtomicUsize::new(0),
            load_session_count: AtomicUsize::new(0),
            load_session_head_meta_count: AtomicUsize::new(0),
            list_pending_queued_work_count: AtomicUsize::new(0),
            session_execution_lease_renewal_count: AtomicUsize::new(0),
            session_execution_lease_release_attempt_count: AtomicUsize::new(0),
            fail_next_runtime_commit: Mutex::new(None),
            inject_turn_cancel_before_next_runtime_commit: Mutex::new(None),
            fail_next_load_session_head_meta: AtomicBool::new(false),
            fail_load_session_on_call: Mutex::new(None),
            fail_next_session_execution_lease_renewal: Mutex::new(None),
            mutate_next_session_execution_lease_renewal: Mutex::new(None),
            session_execution_lease_release_gate: Mutex::new(None),
            session_admission_count: AtomicUsize::new(0),
            abandoned_queued_work_claim_count: AtomicUsize::new(0),
            abandoned_turn_input_claim_count: AtomicUsize::new(0),
            claim_hook: Mutex::new(None),
            forged_head: Mutex::new(None),
            attachment_intents: Mutex::new(Vec::new()),
        }
    }

    /// Every runtime commit the wrapped store applied, in commit order.
    pub fn runtime_commits(&self) -> Vec<RuntimeCommit> {
        self.runtime_commits.lock_recover().clone()
    }

    /// Every attachment write intent this store began, in order: the owner
    /// each write was attributed to when it started.
    pub fn attachment_intents(&self) -> Vec<crate::store::AttachmentIntent> {
        self.attachment_intents.lock_recover().clone()
    }

    /// Answer every later head read with `head` until a commit through this
    /// store lands: `load_session_head_meta` returns it, and `load_session`
    /// returns the stored session under its revision, config, current frame,
    /// checkpoint ref and leaf.
    ///
    /// This is a read-side double for a head the durable store cannot hold on
    /// its own — a changed checkpoint ref or leaf under an unchanged revision —
    /// so a test can drive the runtime's freshness decision on it. A head a
    /// real writer could produce belongs in a real commit instead
    /// ([`super::runtime_helpers::advance_session_head`]).
    pub fn forge_session_head(&self, head: crate::SessionHeadMeta) {
        *self.forged_head.lock_recover() = Some(head);
    }

    /// Session admissions (admit-and-bind) through this store.
    pub fn session_admission_count(&self) -> usize {
        self.session_admission_count.load(Ordering::SeqCst)
    }

    /// Queued-work and turn-input claims abandoned through this store.
    pub fn abandoned_claim_counts(&self) -> (usize, usize) {
        (
            self.abandoned_queued_work_claim_count
                .load(Ordering::SeqCst),
            self.abandoned_turn_input_claim_count.load(Ordering::SeqCst),
        )
    }

    /// Run `hook` as the next queued-work or turn-input claim reaches the
    /// store, before the wrapped store claims.
    pub fn set_claim_after_lease_validation_hook(&self, hook: ClaimHook) {
        *self.claim_hook.lock_recover() = Some(hook);
    }

    fn run_claim_hook(&self) {
        let hook = self.claim_hook.lock_recover().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Suspend every later `release_session_execution_lease` at the gate this
    /// returns, until the gate admits it.
    pub fn gate_session_execution_lease_release(&self) -> Arc<SessionExecutionLeaseReleaseGate> {
        let gate = Arc::new(SessionExecutionLeaseReleaseGate::new());
        *self.session_execution_lease_release_gate.lock_recover() = Some(Arc::clone(&gate));
        gate
    }

    /// The store this one records over.
    pub fn inner(&self) -> &Arc<dyn RuntimePersistence> {
        &self.inner
    }

    /// Runtime commits attempted through this store, accepted or not.
    pub fn commit_write_transaction_count(&self) -> usize {
        self.commit_attempt_count.load(Ordering::SeqCst)
    }

    pub fn load_session_count(&self) -> usize {
        self.load_session_count.load(Ordering::SeqCst)
    }

    pub fn load_session_head_meta_count(&self) -> usize {
        self.load_session_head_meta_count.load(Ordering::SeqCst)
    }

    pub fn list_pending_queued_work_count(&self) -> usize {
        self.list_pending_queued_work_count.load(Ordering::SeqCst)
    }

    pub fn session_execution_lease_renewal_count(&self) -> usize {
        self.session_execution_lease_renewal_count
            .load(Ordering::SeqCst)
    }

    pub fn session_execution_lease_release_attempt_count(&self) -> usize {
        self.session_execution_lease_release_attempt_count
            .load(Ordering::SeqCst)
    }

    /// Refuse the next runtime commit with `error`, before the wrapped store
    /// sees it.
    pub fn fail_next_runtime_commit(&self, error: StoreError) {
        *self.fail_next_runtime_commit.lock_recover() = Some(error);
    }

    /// Record `request` on the wrapped store immediately before the next
    /// runtime commit reaches it: a cancel that races the commit and lands
    /// first.
    pub fn inject_turn_cancel_before_next_runtime_commit(&self, request: crate::TurnCancelRequest) {
        *self
            .inject_turn_cancel_before_next_runtime_commit
            .lock_recover() = Some(request);
    }

    pub fn fail_next_load_session_head_meta(&self) {
        self.fail_next_load_session_head_meta
            .store(true, Ordering::SeqCst);
    }

    /// Fail the `call`-th `load_session` (1-based, counting every call).
    pub fn fail_load_session_on_call(&self, call: usize) {
        *self.fail_load_session_on_call.lock_recover() = Some(call);
    }

    /// Inject a transient renewal rejection (the lease stays durably ours).
    pub fn fail_next_session_execution_lease_renewal(&self) {
        self.fail_next_session_execution_lease_renewal_with(StoreError::Backend(
            "injected session execution lease renewal rejection".to_string(),
        ));
    }

    /// Answer the next granted renewal with the wrapped store's own renewed
    /// lease rewritten by `mutation`: the store has extended its row, and the
    /// caller receives a response the lease guard must judge.
    pub fn mutate_next_session_execution_lease_renewal(
        &self,
        mutation: impl FnOnce(crate::SessionExecutionLease) -> crate::SessionExecutionLease
        + Send
        + 'static,
    ) {
        *self
            .mutate_next_session_execution_lease_renewal
            .lock_recover() = Some(Box::new(mutation));
    }

    /// Inject a specific renewal rejection. Transient errors and a definitive
    /// `SessionExecutionLeaseExpired` mean different things to a lease guard.
    pub fn fail_next_session_execution_lease_renewal_with(&self, error: StoreError) {
        *self
            .fail_next_session_execution_lease_renewal
            .lock_recover() = Some(error);
    }
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for RecordingStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        let call = self.load_session_count.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut fail_on = self.fail_load_session_on_call.lock_recover();
            if fail_on.is_some_and(|fail_on| fail_on == call) {
                fail_on.take();
                return Err(StoreError::Backend(
                    "injected load-session failure".to_string(),
                ));
            }
        }
        let read = self.inner.load_session().await?;
        let forged = self.forged_head.lock_recover().clone();
        let Some(head) = forged else {
            return Ok(read);
        };
        let Some(mut read) = read else {
            return Ok(None);
        };
        read.head_revision = head.head_revision;
        read.config = head.config.clone();
        read.current_frame_node_id = head.current_frame_node_id.clone();
        read.checkpoint_ref = head.checkpoint_ref.clone();
        if read.graph.leaf_node_id != head.leaf_node_id {
            read.graph = crate::SessionGraph::from_shared_nodes(
                read.graph.nodes.clone(),
                head.leaf_node_id.clone(),
            )
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        Ok(Some(read))
    }

    async fn load_session_head_meta(&self) -> Result<Option<crate::SessionHeadMeta>, StoreError> {
        self.load_session_head_meta_count
            .fetch_add(1, Ordering::SeqCst);
        if self
            .fail_next_load_session_head_meta
            .swap(false, Ordering::SeqCst)
        {
            return Err(StoreError::Backend(
                "injected load-session-head failure".to_string(),
            ));
        }
        let forged = self.forged_head.lock_recover().clone();
        match forged {
            Some(head) => Ok(Some(head)),
            None => self.inner.load_session_head_meta().await,
        }
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        self.commit_attempt_count.fetch_add(1, Ordering::SeqCst);
        let injected_failure = self.fail_next_runtime_commit.lock_recover().take();
        if let Some(error) = injected_failure {
            return Err(error);
        }
        let injected_cancel = self
            .inject_turn_cancel_before_next_runtime_commit
            .lock_recover()
            .take();
        if let Some(request) = injected_cancel {
            self.inner.record_turn_cancel_request(request).await?;
        }
        let applied = commit.clone();
        let receipt = self.inner.commit_runtime_state(commit).await?;
        self.forged_head.lock_recover().take();
        if !receipt.receipt_replayed {
            *self.runtime_commit_count.lock_recover() += 1;
            self.runtime_commits.lock_recover().push(applied);
        }
        Ok(receipt)
    }

    async fn begin_attachment_write(
        &self,
        intent: crate::store::AttachmentIntent,
    ) -> Result<crate::store::AttachmentWriteFence, StoreError> {
        self.attachment_intents.lock_recover().push(intent.clone());
        self.inner.begin_attachment_write(intent).await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &crate::SessionBinding,
    ) -> Result<crate::store::SessionAdmission, StoreError> {
        self.session_admission_count.fetch_add(1, Ordering::SeqCst);
        self.inner.admit_and_bind_session(binding).await
    }

    async fn select_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        scope: &crate::ExecutionScope,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &crate::PersistedSessionConfig,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::store::SelectedQueuedRun, StoreError> {
        self.run_claim_hook();
        self.inner
            .select_queued_run(fence, scope, owner, max_inputs, configuration, policy)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::QueuedWorkClaimOutcome, StoreError> {
        self.run_claim_hook();
        self.inner
            .claim_ready_queued_work(session_id, session_execution_lease, owner, boundary, policy)
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>,
            Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>,
        ),
        StoreError,
    > {
        self.run_claim_hook();
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
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[crate::BatchId],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError> {
        self.run_claim_hook();
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

    async fn claim_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        self.run_claim_hook();
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
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        self.run_claim_hook();
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>,
    ) -> Result<(), StoreError> {
        self.abandoned_queued_work_claim_count
            .fetch_add(1, Ordering::SeqCst);
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>],
    ) -> Result<(), StoreError> {
        self.abandoned_queued_work_claim_count
            .fetch_add(claims.len(), Ordering::SeqCst);
        self.inner.abandon_queued_work_claims(claims).await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>,
    ) -> Result<(), StoreError> {
        self.abandoned_turn_input_claim_count
            .fetch_add(1, Ordering::SeqCst);
        self.inner.abandon_turn_input_claim(claim).await
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>],
    ) -> Result<(), StoreError> {
        self.abandoned_turn_input_claim_count
            .fetch_add(claims.len(), Ordering::SeqCst);
        self.inner.abandon_turn_input_claims(claims).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        self.list_pending_queued_work_count
            .fetch_add(1, Ordering::SeqCst);
        self.inner.list_pending_queued_work(session_id).await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<crate::SessionExecutionLease, StoreError> {
        // Counted once the renewal has its answer, in the same poll that
        // returns it: a test that waits for the count sees a finished renewal.
        let injected = self
            .fail_next_session_execution_lease_renewal
            .lock_recover()
            .take();
        let renewal = match injected {
            Some(error) => Err(error),
            None => {
                let renewed = self
                    .inner
                    .renew_session_execution_lease(fence, lease_ttl_ms)
                    .await;
                let mutation = self
                    .mutate_next_session_execution_lease_renewal
                    .lock_recover()
                    .take();
                match (renewed, mutation) {
                    (Ok(lease), Some(mutation)) => Ok(mutation(lease)),
                    (renewed, _) => renewed,
                }
            }
        };
        self.session_execution_lease_renewal_count
            .fetch_add(1, Ordering::SeqCst);
        renewal
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let gate = self
            .session_execution_lease_release_gate
            .lock_recover()
            .clone();
        if let Some(gate) = gate {
            gate.enter().await;
        }
        self.session_execution_lease_release_attempt_count
            .fetch_add(1, Ordering::SeqCst);
        self.inner.release_session_execution_lease(completion).await
    }
}

/// A session catalog that wraps every store of the catalog it decorates in a
/// [`RecordingStore`], and keeps the ones it created. See the module
/// documentation.
///
/// A session's first create or open is wrapped once and every later open of
/// the same session hands back that wrapper, so a fault armed on
/// [`Self::store_for`] reaches whichever runtime holds the session.
#[derive(Clone)]
pub struct RecordingSessionStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    stores: Arc<Mutex<WrappedStores>>,
}

/// The per-session wrappers a [`RecordingSessionStoreFactory`] handed out.
type WrappedStores = Vec<(SessionId, Arc<RecordingStore>)>;

impl RecordingSessionStoreFactory {
    /// Record over `inner`, a backend's session catalog.
    pub fn over(inner: Arc<dyn SessionStoreFactory>) -> Self {
        Self {
            inner,
            stores: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every store this catalog wrapped, in first-seen order.
    pub fn stores(&self) -> Vec<Arc<RecordingStore>> {
        self.stores
            .lock_recover()
            .iter()
            .map(|(_, store)| Arc::clone(store))
            .collect()
    }

    /// The wrapper over `session_id`'s store, once the session was created or
    /// opened through this catalog.
    pub fn store_for(&self, session_id: &SessionId) -> Option<Arc<RecordingStore>> {
        self.stores
            .lock_recover()
            .iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, store)| Arc::clone(store))
    }

    fn record(
        &self,
        session_id: &SessionId,
        store: Arc<dyn RuntimePersistence>,
    ) -> Arc<dyn RuntimePersistence> {
        let mut stores = self.stores.lock_recover();
        if let Some((_, recorded)) = stores.iter().find(|(id, _)| id == session_id) {
            return Arc::clone(recorded) as Arc<dyn RuntimePersistence>;
        }
        let recorded = Arc::new(RecordingStore::over(store));
        stores.push((session_id.clone(), Arc::clone(&recorded)));
        recorded
    }
}

#[async_trait::async_trait]
#[diagnostic::do_not_recommend]
impl crate::AttachmentRootSet for RecordingSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for RecordingSessionStoreFactory {
    fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>) {
        self.inner.bind_effect_host(effect_host);
    }

    fn bind_artifact_stores(
        &self,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        process_engines: crate::ProcessEngineRegistry,
    ) {
        self.inner
            .bind_artifact_stores(process_env_store, process_engines);
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError> {
        self.inner
            .pending_turn_cancel_closure_pins(session_id)
            .await
    }

    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &crate::ExecutionScope,
    ) -> Result<(), StoreError> {
        self.inner.retire_turn_cancel_closure_scope(scope).await
    }

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        self.inner
            .has_claimable_queued_work(request, now_epoch_ms)
            .await
    }

    async fn reclaim_retained_evidence(
        &self,
        bound: crate::store::RetentionBound,
    ) -> crate::store::MaintenanceResult<crate::store::RetentionReport> {
        self.inner.reclaim_retained_evidence(bound).await
    }

    async fn count_unsettled_turns(&self) -> Result<crate::store::UnsettledTurnCounts, StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &crate::store::TurnParkQuery,
    ) -> Result<Vec<crate::store::TurnPark>, StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: crate::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::store::ParkFeedPage<crate::store::TurnParkTarget>, StoreError> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: crate::store::ParkFeedCursor,
    ) -> Result<(), StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }

    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        let store = self.inner.create_store(request).await?;
        Ok(self.record(&request.session_id, store))
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        Ok(self
            .inner
            .open_existing_store(request)
            .await?
            .map(|store| self.record(&request.session_id, store)))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        Ok(self
            .inner
            .open_existing_store_by_id(session_id)
            .await?
            .map(|store| self.record(session_id, store)))
    }

    // The unbound store has no session id to record under; the caller that
    // wants it recorded wraps it, as the storage-only twins do.
    async fn open_unbound_store(&self) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.open_unbound_store().await
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::SessionReadView>, StoreError> {
        self.inner.read_session(session_id).await
    }

    async fn list_sessions(
        &self,
        filter: &crate::SessionListFilter,
    ) -> Result<Vec<crate::SessionSummary>, StoreError> {
        self.inner.list_sessions(filter).await
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    async fn pin(&self, node_id: &str) -> Result<crate::ForkPoint, StoreError> {
        self.inner.pin(node_id).await
    }

    async fn unpin(&self, node_id: &str) -> Result<(), StoreError> {
        self.inner.unpin(node_id).await
    }

    async fn fork_points(&self) -> Result<Vec<crate::ForkPoint>, StoreError> {
        self.inner.fork_points().await
    }

    async fn fork_at(
        &self,
        request: &crate::ForkSessionRequest,
    ) -> Result<crate::ForkSessionReceipt, StoreError> {
        self.inner.fork_at(request).await
    }
}
