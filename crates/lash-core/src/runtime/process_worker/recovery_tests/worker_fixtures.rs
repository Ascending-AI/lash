use super::*;

pub(super) async fn worker_with_engine(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
) {
    let (worker, registry, run_handle, env_ref, _) =
        worker_with_engine_and_registry(concurrency, engine, run_handle).await;
    (worker, registry, run_handle, env_ref)
}

pub(super) async fn worker_with_engine_and_registry(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
) {
    worker_with_engine_registry_timings_and_supplier(concurrency, engine, run_handle, None, None)
        .await
}

pub(super) async fn worker_with_engine_registry_and_timings(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
    lease_timings: Option<crate::LeaseTimings>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
) {
    worker_with_engine_registry_timings_and_supplier(
        concurrency,
        engine,
        run_handle,
        lease_timings,
        None,
    )
    .await
}

/// Records every [`ProcessWorkerFault`] a worker reports, so a test can assert
/// on the unconditional fault surface instead of a swallowed disposition.
#[derive(Default)]
pub(super) struct RecordingProcessEventSink {
    faults: Mutex<Vec<ProcessWorkerFault>>,
}

impl RecordingProcessEventSink {
    pub(super) fn faults(&self) -> Vec<ProcessWorkerFault> {
        self.faults.lock_recover().clone()
    }

    /// Wait for at least one fault, failing the test rather than hanging.
    pub(super) async fn await_first_fault(&self, description: &str) -> ProcessWorkerFault {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(fault) = self.faults().into_iter().next() {
                    return fault;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
    }
}

#[async_trait::async_trait]
impl crate::ProcessEventSink for RecordingProcessEventSink {
    async fn emit(&self, _event: &crate::ProcessEvent) {}

    async fn emit_worker_fault(&self, fault: &ProcessWorkerFault) {
        self.faults.lock_recover().push(fault.clone());
    }
}

/// Worker wired to a recording fault sink, for the admission-honesty tests.
pub(super) async fn worker_with_engine_and_fault_sink(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
    Arc<RecordingProcessEventSink>,
) {
    let sink = Arc::new(RecordingProcessEventSink::default());
    let (worker, registry, run_handle, env_ref, test_registry) =
        worker_with_engine_registry_timings_supplier_and_sink(
            concurrency,
            engine,
            run_handle,
            None,
            None,
            Some(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>),
        )
        .await;
    (worker, registry, run_handle, env_ref, test_registry, sink)
}

pub(super) async fn worker_with_engine_registry_timings_and_supplier(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
    lease_timings: Option<crate::LeaseTimings>,
    supplier: Option<Arc<dyn crate::WorkerSlotSupplier>>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
) {
    worker_with_engine_registry_timings_supplier_and_sink(
        concurrency,
        engine,
        run_handle,
        lease_timings,
        supplier,
        None,
    )
    .await
}

pub(super) async fn worker_with_engine_registry_timings_supplier_and_sink(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
    lease_timings: Option<crate::LeaseTimings>,
    supplier: Option<Arc<dyn crate::WorkerSlotSupplier>>,
    sink: Option<Arc<dyn crate::ProcessEventSink>>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
) {
    let test_registry = Arc::new(TestLocalProcessRegistry::default());
    let raw_registry: Arc<dyn ProcessRegistry> = test_registry.clone();
    let (registry, driver_hub, process_work) =
        late_bound_process_work_wiring(raw_registry, Arc::clone(&run_handle));
    let mut runtime_host = RuntimeHostConfig::in_memory(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    runtime_host.process_engines = crate::ProcessEngineRegistry::new().with_engine(engine);
    if let Some(lease_timings) = lease_timings {
        runtime_host = runtime_host.with_lease_timings(lease_timings);
    }
    let policy = test_session_policy();
    let env_ref = crate::persist_process_execution_env(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(Vec::new())),
        runtime_host,
        Arc::new(TestSessionStoreFactory),
        Arc::clone(&registry),
        driver_hub,
        crate::WorkerProcessWork::External(process_work),
        local_owner("engine-worker", "host-a", "engine-start"),
    )
    .with_session_policy(policy)
    .with_process_execution_concurrency(concurrency)
    .expect("valid test process execution concurrency");
    if let Some(supplier) = supplier {
        config = config.with_worker_slot_supplier(supplier);
    }
    if let Some(sink) = sink {
        config = config.with_process_event_sink(sink);
    }
    let worker = DurableProcessWorker::new(config);
    run_handle
        .worker
        .set(worker.clone())
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    (worker, registry, run_handle, env_ref, test_registry)
}
