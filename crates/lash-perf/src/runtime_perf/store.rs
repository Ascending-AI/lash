use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::store::{RuntimeCommitReceipt, RuntimePersistenceDecorator};
use lash_core::{
    RuntimeCommit, RuntimePersistence, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};

/// Runtime persistence with the production in-memory semantics plus the one
/// counter the perf report cannot obtain from the public persistence traits.
pub(crate) struct RuntimePerfStore {
    inner: Arc<dyn RuntimePersistence>,
    committed_node_ids: Mutex<HashSet<String>>,
    metrics: Arc<RuntimePerfStoreMetrics>,
    measure_commit_bytes: bool,
}

impl RuntimePerfStore {
    fn wrap(
        inner: Arc<dyn RuntimePersistence>,
        metrics: Arc<RuntimePerfStoreMetrics>,
        measure_commit_bytes: bool,
    ) -> Self {
        Self {
            inner,
            committed_node_ids: Mutex::new(HashSet::new()),
            metrics,
            measure_commit_bytes,
        }
    }

    pub(crate) fn graph_node_count(&self) -> usize {
        self.committed_node_ids.lock_recover().len()
    }

    pub(crate) fn metrics(&self) -> Arc<RuntimePerfStoreMetrics> {
        Arc::clone(&self.metrics)
    }
}

impl Default for RuntimePerfStore {
    fn default() -> Self {
        Self::wrap(
            Arc::new(lash_core::facade_support::InMemorySessionStore::default()),
            Arc::new(RuntimePerfStoreMetrics::default()),
            false,
        )
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
    calls: Mutex<BTreeMap<String, u64>>,
    commits: Mutex<Vec<RuntimePerfCommitMeasurement>>,
    timings: Mutex<BTreeMap<String, RuntimePerfStoreTiming>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimePerfStoreTiming {
    pub(crate) calls: u64,
    pub(crate) total_micros: u64,
}

impl RuntimePerfStoreMetrics {
    fn record_call(&self, operation: &str) {
        *self
            .calls
            .lock_recover()
            .entry(operation.to_string())
            .or_default() += 1;
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
        let calls = self.calls.lock_recover();
        let mut counters = calls
            .iter()
            .map(|(operation, count)| (format!("store_calls.{operation}"), *count))
            .collect::<BTreeMap<_, _>>();
        counters.insert("store_calls.total".to_string(), calls.values().sum());
        counters
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
        self.metrics.record_call("commit_runtime_state");
        if self.measure_commit_bytes {
            self.metrics.record_commit(&commit);
        }
        let node_ids = commit
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let started = Instant::now();
        let receipt = self.inner.commit_runtime_state(commit).await;
        self.metrics
            .record_timing("store_transaction", started.elapsed());
        let receipt = receipt?;
        self.committed_node_ids.lock_recover().extend(node_ids);
        Ok(receipt)
    }

    async fn load_session(
        &self,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, StoreError> {
        self.metrics.record_call("load_session");
        self.inner.load_session().await
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, StoreError> {
        self.metrics.record_call("load_session_head_meta");
        self.inner.load_session_head_meta().await
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, StoreError> {
        self.metrics.record_call("load_node");
        self.inner.load_node(node_id).await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, StoreError> {
        self.metrics.record_call("admit_and_bind_session");
        self.inner.admit_and_bind_session(binding).await
    }

    async fn save_session_meta(&self, meta: lash_core::SessionMeta) -> Result<(), StoreError> {
        self.metrics.record_call("save_session_meta");
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<lash_core::SessionMeta>, StoreError> {
        self.metrics.record_call("load_session_meta");
        self.inner.load_session_meta().await
    }

    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        self.metrics.record_call("enqueue_pending_turn_input");
        let started = Instant::now();
        let result = self.inner.enqueue_pending_turn_input(input).await;
        self.metrics
            .record_timing("queue_enqueue", started.elapsed());
        result
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::WorkClaim<lash_core::runtime::TurnInputClaimData>>, StoreError>
    {
        self.metrics.record_call("claim_next_turn_inputs");
        let started = Instant::now();
        let result = self
            .inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await;
        self.metrics.record_timing("claim_scan", started.elapsed());
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
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
        self.metrics.record_call("claim_checkpoint_work");
        let started = Instant::now();
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
        result
    }

    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, StoreError> {
        self.metrics
            .record_call("try_claim_session_execution_lease");
        self.inner
            .try_claim_session_execution_lease(session_id, owner, executor_id, lease_ttl_ms)
            .await
    }

    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, StoreError> {
        self.metrics
            .record_call("try_claim_session_execution_lease_with_token");
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
        self.metrics.record_call("renew_session_execution_lease");
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        self.metrics.record_call("release_session_execution_lease");
        self.inner.release_session_execution_lease(completion).await
    }
}

#[derive(Clone)]
pub(crate) struct RuntimePerfStoreFactory {
    pub(crate) store: Arc<RuntimePerfStore>,
    child_stores: Arc<Mutex<HashMap<String, Arc<RuntimePerfStore>>>>,
    inner: Option<Arc<dyn SessionStoreFactory>>,
    metrics: Arc<RuntimePerfStoreMetrics>,
}

impl RuntimePerfStoreFactory {
    pub(crate) fn new(store: Arc<RuntimePerfStore>) -> Self {
        let metrics = store.metrics();
        Self {
            store,
            child_stores: Arc::new(Mutex::new(HashMap::new())),
            inner: None,
            metrics,
        }
    }

    pub(crate) fn decorating(inner: Arc<dyn SessionStoreFactory>) -> Self {
        let metrics = Arc::new(RuntimePerfStoreMetrics::default());
        Self {
            store: Arc::new(RuntimePerfStore::wrap(
                Arc::new(lash_core::facade_support::InMemorySessionStore::default()),
                Arc::clone(&metrics),
                true,
            )),
            child_stores: Arc::new(Mutex::new(HashMap::new())),
            inner: Some(inner),
            metrics,
        }
    }

    pub(crate) fn metrics(&self) -> Arc<RuntimePerfStoreMetrics> {
        Arc::clone(&self.metrics)
    }
}

// The benchmark factory does not own an attachment blob store, so attachment
// reclamation has no roots to discover in a runtime-perf run.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for RuntimePerfStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, StoreError> {
        if let Some(inner) = &self.inner {
            return inner
                .live_attachment_refs(intent_grace_cutoff_epoch_ms)
                .await;
        }
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        if let Some(inner) = &self.inner {
            return inner
                .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
                .await;
        }
        Ok(false)
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for RuntimePerfStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        if let Some(inner) = &self.inner {
            let store = inner.create_store(request).await?;
            return Ok(Arc::new(RuntimePerfStore::wrap(
                store,
                Arc::clone(&self.metrics),
                true,
            )));
        }
        if request.parent_session_id().is_none() {
            return Ok(Arc::clone(&self.store) as Arc<dyn RuntimePersistence>);
        }
        let mut stores = self.child_stores.lock_recover();
        let store = stores
            .entry(request.session_id.clone())
            .or_insert_with(|| Arc::new(RuntimePerfStore::default()));
        Ok(Arc::clone(store) as Arc<dyn RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        let store = inner.open_existing_store(request).await?;
        Ok(store.map(|store| {
            Arc::new(RuntimePerfStore::wrap(
                store,
                Arc::clone(&self.metrics),
                true,
            )) as Arc<dyn RuntimePersistence>
        }))
    }

    async fn has_claimable_queued_work(
        &self,
        request: &SessionStoreCreateRequest,
        now_epoch_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        inner.has_claimable_queued_work(request, now_epoch_ms).await
    }

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        if let Some(inner) = &self.inner {
            return inner.session_was_deleted(session_id).await;
        }
        Ok(false)
    }

    async fn delete_session(
        &self,
        session_id: &str,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        if let Some(inner) = &self.inner {
            return inner.delete_session(session_id).await;
        }
        Err(lash_core::MaintenanceFailure::failed_before_any_work(
            StoreError::UnsupportedStoreOperation {
                operation: "delete_session",
            },
        ))
    }
}

#[cfg(test)]
mod tests;
