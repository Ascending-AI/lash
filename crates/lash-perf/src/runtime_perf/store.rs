//! Persistence-decorator measurements emitted by the runtime perf harness.
//!
//! Each `store.op.<name>.observed_micros` sample starts immediately before the
//! decorator delegates to the inner persistence implementation and ends when
//! that future returns, including errors. The bracket therefore includes any
//! pool or connection acquisition, backend I/O, and thread dispatch performed
//! by the inner implementation; it does not isolate any of those components.
//! Decorator-side commit sizing and node bookkeeping sit outside the bracket.
//! Queue-driver wake dispatch does not pass through this decorator at all and
//! remains owned by the existing `wait.*` phase metrics.

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::store::{RuntimeCommitReceipt, RuntimeStoreDecorator};
use lash_core::{
    DeploymentStore, DeploymentStoreDecorator, RuntimeCommit, SessionCreationHead,
    SessionStoreCreateRequest, StoreError,
};

/// One measured store for the catalog. Its node counter holds no identities.
#[derive(Clone)]
pub(crate) struct RuntimePerfStore {
    inner: Arc<dyn DeploymentStore>,
    committed_nodes: Arc<AtomicU64>,
    known_sessions: Arc<Mutex<HashSet<SessionId>>>,
    metrics: Arc<RuntimePerfStoreMetrics>,
    measure_commit_bytes: bool,
}

impl RuntimePerfStore {
    pub(crate) fn graph_node_count(&self) -> usize {
        self.committed_nodes.load(Ordering::Relaxed) as usize
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
impl RuntimeStoreDecorator for RuntimePerfStore {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<lash_core::SessionAdmission, StoreError> {
        let admission = self.inner.admit_session(request).await?;
        self.known_sessions
            .lock_recover()
            .insert(request.session_id.clone());
        Ok(admission)
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        if self.measure_commit_bytes {
            self.metrics.record_commit(&commit);
        }
        let count = commit.graph.nodes().len() as u64;
        let observation = self.metrics.observe_call("commit_runtime_state");
        let started = observation.started_at;
        let receipt = self.inner.commit_runtime_state(commit).await;
        self.metrics
            .record_timing("store_transaction", started.elapsed());
        drop(observation);
        let receipt = receipt?;
        if !receipt.receipt_replayed {
            self.committed_nodes.fetch_add(count, Ordering::Relaxed);
        }
        Ok(receipt)
    }

    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: lash_core::store::WindowSelector,
    ) -> Result<Option<lash_core::store::SessionWindowRead>, StoreError> {
        let _observation = self.metrics.observe_call("load_session_window");
        self.inner.load_session_window(session_id, selector).await
    }

    async fn load_ancestors(
        &self,
        session_id: &SessionId,
        anchor: lash_core::store::HistoryAnchor,
        budget: lash_core::store::HistoryBudget,
    ) -> Result<lash_core::store::HistoryPage, StoreError> {
        let _observation = self.metrics.observe_call("load_ancestors");
        self.inner.load_ancestors(session_id, anchor, budget).await
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, StoreError> {
        let _observation = self.metrics.observe_call("load_session_head_meta");
        self.inner.load_session_head_meta(session_id).await
    }

    async fn enqueue_pending_turn_inputs(
        &self,
        batch: lash_core::PendingTurnInputBatch,
    ) -> Result<Vec<lash_core::PendingTurnInput>, StoreError> {
        let observation = self.metrics.observe_call("enqueue_pending_turn_inputs");
        let started = observation.started_at;
        let result = self.inner.enqueue_pending_turn_inputs(batch).await;
        self.metrics
            .record_timing("queue_enqueue", started.elapsed());
        drop(observation);
        result
    }

    async fn admit_root(
        &self,
        request: &lash_core::store::AdmitRootRequest,
    ) -> Result<Option<lash_core::store::RootAdmission>, StoreError> {
        let observation = self.metrics.observe_call("admit_root");
        let started = observation.started_at;
        let result = self.inner.admit_root(request).await;
        self.metrics
            .record_timing("admission_scan", started.elapsed());
        drop(observation);
        result
    }

    async fn admit_at_checkpoint(
        &self,
        request: &lash_core::store::CheckpointAdmissionRequest,
    ) -> Result<lash_core::store::CheckpointAdmission, StoreError> {
        let observation = self.metrics.observe_call("admit_at_checkpoint");
        let started = observation.started_at;
        let result = self.inner.admit_at_checkpoint(request).await;
        self.metrics
            .record_timing("admission_scan", started.elapsed());
        drop(observation);
        result
    }

    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &lash_core::store::AdmissionId,
        observed_epoch: u64,
        root_start: &lash_core::store::RootStartNonce,
    ) -> Result<lash_core::store::DriveEpochSeal, StoreError> {
        let _observation = self.metrics.observe_call("seal_drive_epoch");
        self.inner
            .seal_drive_epoch(session_id, admission, observed_epoch, root_start)
            .await
    }
}

impl DeploymentStoreDecorator for RuntimePerfStore {}

/// The harness keeps this name for its catalog handle. It is the same
/// decorated store across every session of a benchmark catalog.
pub(crate) type RuntimePerfStoreFactory = RuntimePerfStore;

impl RuntimePerfStore {
    pub(crate) fn decorating(inner: Arc<dyn DeploymentStore>) -> Self {
        Self::decorating_with_commit_measurement(inner, true)
    }

    pub(crate) fn decorating_without_commit_measurement(inner: Arc<dyn DeploymentStore>) -> Self {
        Self::decorating_with_commit_measurement(inner, false)
    }

    fn decorating_with_commit_measurement(
        inner: Arc<dyn DeploymentStore>,
        measure_commit_bytes: bool,
    ) -> Self {
        Self {
            inner,
            committed_nodes: Arc::new(AtomicU64::new(0)),
            known_sessions: Arc::new(Mutex::new(HashSet::new())),
            metrics: Arc::new(RuntimePerfStoreMetrics::default()),
            measure_commit_bytes,
        }
    }

    pub(crate) fn metrics(&self) -> Arc<RuntimePerfStoreMetrics> {
        Arc::clone(&self.metrics)
    }

    pub(crate) fn session_store(&self, session_id: &SessionId) -> Option<Arc<RuntimePerfStore>> {
        self.known_sessions
            .lock_recover()
            .contains(session_id)
            .then(|| Arc::new(self.clone()))
    }

    pub(crate) async fn root_store(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<RuntimePerfStore>, StoreError> {
        let request = SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
            head: SessionCreationHead::CommittedByCreator,
        };
        self.admit_session(&request).await?;
        Ok(Arc::new(self.clone()))
    }
}

#[cfg(test)]
mod tests;
