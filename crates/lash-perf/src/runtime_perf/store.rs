//! Persistence-decorator measurements emitted by the runtime perf harness.
//!
//! Each `store.op.<name>.observed_micros` sample starts immediately before the
//! decorator delegates to the inner persistence implementation and ends when
//! that future returns, including errors. The bracket therefore includes any
//! pool or connection acquisition, backend I/O, and thread dispatch performed
//! by the inner implementation; it does not isolate any of those components.
//! Decorator-side commit sizing and node bookkeeping sit outside the bracket.
//! Queue-driver wake dispatch and claim scans do not pass through this
//! decorator at all and remain owned by the existing `wait.*` phase metrics.

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::store::{RuntimeCommitReceipt, RuntimePersistenceDecorator};
use lash_core::{
    RuntimeCommit, RuntimePersistence, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};

/// A deployment's session store plus the one counter the perf report cannot
/// obtain from the public persistence traits.
pub(crate) struct RuntimePerfStore {
    inner: Arc<dyn RuntimePersistence>,
    committed_node_ids: Arc<Mutex<HashSet<lash_core::NodeId>>>,
    metrics: Arc<RuntimePerfStoreMetrics>,
    measure_commit_bytes: bool,
}

impl RuntimePerfStore {
    pub(crate) fn graph_node_count(&self) -> usize {
        self.committed_node_ids.lock_recover().len()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimePerfCommitMeasurement {
    pub(crate) total_bytes: u64,
    pub(crate) checkpoint_bytes: u64,
    pub(crate) total_rows: u64,
    pub(crate) graph_rows: u64,
    pub(crate) checkpoint_components: u64,
}

#[derive(Default)]
pub(crate) struct RuntimePerfStoreMetrics {
    operations: Mutex<BTreeMap<String, RuntimePerfStoreOperationMeasurement>>,
    commits: Mutex<Vec<RuntimePerfCommitMeasurement>>,
    timings: Mutex<BTreeMap<String, RuntimePerfStoreTiming>>,
    pool_checkout_wait_nanos: Mutex<Vec<u64>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimePerfStoreTiming {
    pub(crate) calls: u64,
    pub(crate) total_micros: u64,
}

#[derive(Default)]
struct RuntimePerfStoreOperationMeasurement {
    calls: u64,
    observed_nanos: Vec<u64>,
}

struct RuntimePerfStoreCallObservation<'a> {
    metrics: &'a RuntimePerfStoreMetrics,
    operation: &'static str,
    started_at: Instant,
}

impl Drop for RuntimePerfStoreCallObservation<'_> {
    fn drop(&mut self) {
        let elapsed_nanos = self.started_at.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.metrics
            .operations
            .lock_recover()
            .entry(self.operation.to_string())
            .or_default()
            .observed_nanos
            .push(elapsed_nanos);
    }
}

impl RuntimePerfStoreMetrics {
    fn observe_call(&self, operation: &'static str) -> RuntimePerfStoreCallObservation<'_> {
        self.operations
            .lock_recover()
            .entry(operation.to_string())
            .or_default()
            .calls += 1;
        RuntimePerfStoreCallObservation {
            metrics: self,
            operation,
            started_at: Instant::now(),
        }
    }

    fn record_timing(&self, operation: &str, elapsed: Duration) {
        let mut timings = self.timings.lock_recover();
        let timing = timings.entry(operation.to_string()).or_default();
        timing.calls += 1;
        timing.total_micros = timing
            .total_micros
            .saturating_add(elapsed.as_micros().min(u128::from(u64::MAX)) as u64);
    }

    fn record_commit(&self, commit: &RuntimeCommit) {
        let Ok(budget) = lash_core::testing::measure_runtime_commit_budget(commit) else {
            return;
        };
        self.commits
            .lock_recover()
            .push(RuntimePerfCommitMeasurement {
                total_bytes: budget.total_bytes as u64,
                checkpoint_bytes: budget.checkpoint_bytes as u64,
                total_rows: budget.total_rows as u64,
                graph_rows: budget.graph_rows as u64,
                checkpoint_components: commit.checkpoint.components.len() as u64,
            });
    }

    pub(crate) fn call_counters(&self) -> BTreeMap<String, u64> {
        let operations = self.operations.lock_recover();
        let mut counters = operations
            .iter()
            .map(|(operation, measurement)| (format!("store_calls.{operation}"), measurement.calls))
            .collect::<BTreeMap<_, _>>();
        counters.insert(
            "store_calls.total".to_string(),
            operations
                .values()
                .map(|measurement| measurement.calls)
                .sum(),
        );
        for (operation, measurement) in operations.iter() {
            let family = format!("store.op.{operation}.observed_micros");
            counters.insert(format!("{family}.count"), measurement.calls);
            counters.insert(
                format!("{family}.total"),
                measurement.observed_nanos.iter().sum::<u64>() / 1_000,
            );
        }
        counters
    }

    pub(crate) fn observed_latency_samples(&self) -> BTreeMap<String, Vec<f64>> {
        self.operations
            .lock_recover()
            .iter()
            .map(|(operation, measurement)| {
                (
                    format!("store.op.{operation}.observed_micros"),
                    measurement
                        .observed_nanos
                        .iter()
                        .map(|nanos| *nanos as f64 / 1_000.0)
                        .collect(),
                )
            })
            .collect()
    }

    pub(crate) fn record_pool_checkout_waits(&self, samples: Vec<u64>) {
        self.pool_checkout_wait_nanos.lock_recover().extend(samples);
    }

    pub(crate) fn pool_checkout_wait_samples_ms(&self) -> Vec<f64> {
        self.pool_checkout_wait_nanos
            .lock_recover()
            .iter()
            .map(|nanos| *nanos as f64 / 1_000_000.0)
            .collect()
    }

    pub(crate) fn commit_measurements(&self) -> Vec<RuntimePerfCommitMeasurement> {
        self.commits.lock_recover().clone()
    }

    pub(crate) fn timing_snapshot(&self) -> BTreeMap<String, RuntimePerfStoreTiming> {
        self.timings.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for RuntimePerfStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        if self.measure_commit_bytes {
            self.metrics.record_commit(&commit);
        }
        let node_ids = commit
            .graph
            .nodes()
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let observation = self.metrics.observe_call("commit_runtime_state");
        let started = observation.started_at;
        let receipt = self.inner.commit_runtime_state(commit).await;
        self.metrics
            .record_timing("store_transaction", started.elapsed());
        drop(observation);
        let receipt = receipt?;
        self.committed_node_ids.lock_recover().extend(node_ids);
        Ok(receipt)
    }

    async fn load_session(
        &self,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, StoreError> {
        let _observation = self.metrics.observe_call("load_session");
        self.inner.load_session().await
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, StoreError> {
        let _observation = self.metrics.observe_call("load_session_head_meta");
        self.inner.load_session_head_meta().await
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, StoreError> {
        let _observation = self.metrics.observe_call("load_node");
        self.inner.load_node(node_id).await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, StoreError> {
        let _observation = self.metrics.observe_call("admit_and_bind_session");
        self.inner.admit_and_bind_session(binding).await
    }

    async fn save_session_meta(&self, meta: lash_core::SessionMeta) -> Result<(), StoreError> {
        let _observation = self.metrics.observe_call("save_session_meta");
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<lash_core::SessionMeta>, StoreError> {
        let _observation = self.metrics.observe_call("load_session_meta");
        self.inner.load_session_meta().await
    }

    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        let observation = self.metrics.observe_call("enqueue_pending_turn_input");
        let started = observation.started_at;
        let result = self.inner.enqueue_pending_turn_input(input).await;
        self.metrics
            .record_timing("queue_enqueue", started.elapsed());
        drop(observation);
        result
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::WorkClaim<lash_core::runtime::TurnInputClaimData>>, StoreError>
    {
        let observation = self.metrics.observe_call("claim_next_turn_inputs");
        let started = observation.started_at;
        let result = self
            .inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await;
        self.metrics.record_timing("claim_scan", started.elapsed());
        drop(observation);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<lash_core::WorkClaim<lash_core::runtime::TurnInputClaimData>>,
            Option<lash_core::WorkClaim<lash_core::runtime::QueuedWorkClaimData>>,
        ),
        StoreError,
    > {
        let observation = self.metrics.observe_call("claim_checkpoint_work");
        let started = observation.started_at;
        let result = self
            .inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await;
        self.metrics.record_timing("claim_scan", started.elapsed());
        drop(observation);
        result
    }

    async fn try_claim_session_execution_lease(
        &self,
        session_id: &SessionId,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, StoreError> {
        let _observation = self
            .metrics
            .observe_call("try_claim_session_execution_lease");
        self.inner
            .try_claim_session_execution_lease(session_id, owner, executor_id, lease_ttl_ms)
            .await
    }

    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &SessionId,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, StoreError> {
        let _observation = self
            .metrics
            .observe_call("try_claim_session_execution_lease_with_token");
        self.inner
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                executor_id,
                claim_nonce,
                lease_ttl_ms,
            )
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLease, StoreError> {
        let _observation = self.metrics.observe_call("renew_session_execution_lease");
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let _observation = self.metrics.observe_call("release_session_execution_lease");
        self.inner.release_session_execution_lease(completion).await
    }
}

/// A deployment's session catalog, decorated: every store it opens is a
/// [`RuntimePerfStore`] feeding one metrics sink.
///
/// Stores of one session share their committed-node set, so a session's
/// node count survives the runtime reopening its store.
#[derive(Clone)]
pub(crate) struct RuntimePerfStoreFactory {
    inner: Arc<dyn SessionStoreFactory>,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<RuntimePerfStore>>>>,
    metrics: Arc<RuntimePerfStoreMetrics>,
    measure_commit_bytes: bool,
}

impl RuntimePerfStoreFactory {
    pub(crate) fn decorating(inner: Arc<dyn SessionStoreFactory>) -> Self {
        Self::decorating_with_commit_measurement(inner, true)
    }

    pub(crate) fn decorating_without_commit_measurement(
        inner: Arc<dyn SessionStoreFactory>,
    ) -> Self {
        Self::decorating_with_commit_measurement(inner, false)
    }

    fn decorating_with_commit_measurement(
        inner: Arc<dyn SessionStoreFactory>,
        measure_commit_bytes: bool,
    ) -> Self {
        Self {
            inner,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(RuntimePerfStoreMetrics::default()),
            measure_commit_bytes,
        }
    }

    pub(crate) fn metrics(&self) -> Arc<RuntimePerfStoreMetrics> {
        Arc::clone(&self.metrics)
    }

    /// The decorated store this factory last opened for `session_id`.
    pub(crate) fn session_store(&self, session_id: &SessionId) -> Option<Arc<RuntimePerfStore>> {
        self.sessions.lock_recover().get(session_id).cloned()
    }

    /// Create (or reopen) the root session `session_id` and return its
    /// decorated store.
    pub(crate) async fn root_store(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<RuntimePerfStore>, StoreError> {
        let request = SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        let store = self.inner.create_store(&request).await?;
        Ok(self.wrap(session_id, store))
    }

    fn wrap(
        &self,
        session_id: &SessionId,
        inner: Arc<dyn RuntimePersistence>,
    ) -> Arc<RuntimePerfStore> {
        let mut sessions = self.sessions.lock_recover();
        let committed_node_ids = sessions
            .get(session_id)
            .map(|store| Arc::clone(&store.committed_node_ids))
            .unwrap_or_default();
        let store = Arc::new(RuntimePerfStore {
            inner,
            committed_node_ids,
            metrics: Arc::clone(&self.metrics),
            measure_commit_bytes: self.measure_commit_bytes,
        });
        sessions.insert(session_id.clone(), Arc::clone(&store));
        store
    }
}

#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for RuntimePerfStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for RuntimePerfStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        let store = self.inner.create_store(request).await?;
        Ok(self.wrap(&request.session_id, store) as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        let store = self.inner.open_existing_store(request).await?;
        Ok(store.map(|store| self.wrap(&request.session_id, store) as Arc<dyn RuntimePersistence>))
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        let store = self.inner.open_existing_store_by_id(session_id).await?;
        Ok(store.map(|store| self.wrap(session_id, store) as Arc<dyn RuntimePersistence>))
    }

    // The unbound store has no session id to key a `RuntimePerfStore` under;
    // it binds on its first admitted session.
    async fn open_unbound_store(&self) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.open_unbound_store().await
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core::SessionReadView>, StoreError> {
        self.inner.read_session(session_id).await
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

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    async fn list_sessions(
        &self,
        filter: &lash_core::SessionListFilter,
    ) -> Result<Vec<lash_core::SessionSummary>, StoreError> {
        SessionStoreFactory::list_sessions(self.inner.as_ref(), filter).await
    }

    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash_core::store::UnsettledTurnCounts, StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core::store::TurnParkQuery,
    ) -> Result<Vec<lash_core::store::TurnPark>, StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: lash_core::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>, StoreError> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash_core::SessionId,
        root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash_core::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::store::ControlIntent>, StoreError> {
        self.inner.list_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::ParkFeedCursor,
    ) -> Result<(), StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for RuntimePerfStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, StoreError> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<(), StoreError> {
        self.inner.acknowledge_intent(id, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: lash_core::store::ControlIntentId,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::ControlIntent, StoreError> {
        self.inner
            .record_intent_failure(id, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, StoreError> {
        self.inner.load_intent(id).await
    }
}

#[cfg(test)]
mod tests;
