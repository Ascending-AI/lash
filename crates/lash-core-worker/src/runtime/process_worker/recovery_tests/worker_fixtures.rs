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
    Arc<crate::testing::ProcessRegistryFaults>,
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
    Arc<crate::testing::ProcessRegistryFaults>,
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
    Arc<crate::testing::ProcessRegistryFaults>,
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
            crate::NativeSubstrateConfig::default(),
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
    Arc<crate::testing::ProcessRegistryFaults>,
) {
    worker_with_engine_registry_timings_supplier_and_sink(
        concurrency,
        engine,
        run_handle,
        lease_timings,
        supplier,
        None,
        crate::NativeSubstrateConfig::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn worker_with_engine_registry_timings_supplier_and_sink(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessWork>,
    lease_timings: Option<crate::LeaseTimings>,
    supplier: Option<Arc<dyn crate::WorkerSlotSupplier>>,
    sink: Option<Arc<dyn crate::ProcessEventSink>>,
    native_substrate: crate::NativeSubstrateConfig,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessWork>,
    ProcessExecutionEnvRef,
    Arc<crate::testing::ProcessRegistryFaults>,
) {
    let (backend, test_registry) = faulted_memory_backend().await;
    let (registry, _driver_hub, process_work) =
        late_bound_process_work_wiring(backend.process_registry(), Arc::clone(&run_handle));
    let mut runtime_host = test_host_config(&backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new()
        .with_registration(crate::ProcessEngineRegistration::accepting(engine));
    if let Some(lease_timings) = lease_timings {
        runtime_host = runtime_host.with_lease_timings(lease_timings);
    }
    let policy = test_session_policy();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ArtifactOwner::host("worker-fixture"),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        runtime_host,
        crate::WorkerProcessWork::External(process_work),
        Arc::new(crate::NoSessionWork::new()),
        local_owner("engine-worker", "host-a", "engine-start"),
    )
    .with_session_policy(policy)
    .with_process_execution_concurrency(concurrency)
    .expect("valid test process execution concurrency");
    config.native_substrate = native_substrate;
    if let Some(supplier) = supplier {
        config = config.with_worker_slot_supplier(supplier);
    }
    if let Some(sink) = sink {
        config = config.with_process_event_sink(sink);
    }
    let worker = DurableProcessWorker::new(config).expect("valid test native substrate config");
    run_handle
        .worker
        .set(worker.clone())
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    (worker, registry, run_handle, env_ref, test_registry)
}

/// A worker over `backend`, for the laws that read a committed turn back out
/// of the store the turn committed to.
pub(super) async fn worker_on_backend(
    engine: Arc<dyn crate::ProcessEngine>,
    backend: &crate::Backend,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    ProcessExecutionEnvRef,
) {
    let run_handle = Arc::new(LateBoundProcessWork::default());
    let (registry, _driver_hub, process_work) =
        late_bound_process_work_wiring(backend.process_registry(), Arc::clone(&run_handle));
    let mut runtime_host = test_host_config(backend);
    runtime_host.process_engines = crate::ProcessEngineRegistry::new()
        .with_registration(crate::ProcessEngineRegistration::accepting(engine));
    let policy = test_session_policy();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        runtime_host.durability.process_env_store.as_ref(),
        &crate::ArtifactOwner::host("parent-end-redrive-fixture"),
        &crate::ProcessExecutionEnvSpec::new(crate::PluginOptions::default(), policy.clone()),
    )
    .await
    .expect("persist process env");
    let config = DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        runtime_host,
        crate::WorkerProcessWork::External(process_work),
        Arc::new(crate::NoSessionWork::new()),
        local_owner("redrive-worker", "host-a", "redrive-start"),
    )
    .with_session_policy(policy)
    .with_process_execution_concurrency(1)
    .expect("valid test process execution concurrency");
    let worker = DurableProcessWorker::new(config).expect("valid test native substrate config");
    run_handle
        .worker
        .set(worker.clone())
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    (worker, registry, env_ref)
}

pub(super) fn engine_registration(
    kind: &str,
    env_ref: ProcessExecutionEnvRef,
    payload: serde_json::Value,
) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::Engine {
            kind: kind.to_string(),
            payload,
        },
        RecoveryContract::Rerunnable,
        crate::ProcessProvenance::host(),
        crate::ProcessLifecyclePolicy::new(crate::ParentScope::Host, crate::OnParentEnd::Abandon),
    )
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn terminal_count(registry: &Arc<dyn ProcessRegistry>) -> usize {
    registry
        .list_processes(&ProcessListFilter {
            status: crate::ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .expect("list processes")
        .into_iter()
        .filter(ProcessRecord::is_terminal)
        .count()
}

pub(super) async fn wait_for_terminal_count(
    registry: &Arc<dyn ProcessRegistry>,
    expected: usize,
    description: &str,
) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while terminal_count(registry).await < expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if result.is_err() {
        let records = registry
            .list_processes(&ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("list timed-out processes");
        panic!(
            "timed out waiting for {description}: {}",
            records
                .iter()
                .map(|record| format!("{}={}", record.id, record.status.label()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

pub(super) async fn native_worker(
    backend: &crate::Backend,
    lease_owner: LeaseOwnerIdentity,
) -> DurableProcessWorker {
    let watched = crate::watch_process_registry(backend.process_registry());
    DurableProcessWorker::new(DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        host_config_with_fixture_env(backend).await,
        crate::WorkerProcessWork::SelfNative(watched),
        Arc::new(crate::NoSessionWork::new()),
        lease_owner,
    ))
    .expect("valid test native substrate config")
}

/// A worker whose trigger-delivery reconcile can re-enter the work driver: the
/// driver's run handle drives this same worker, which is the shape the facade
/// builds and the shape that produced the "a call reports its own admission as
/// `Busy`" defect.
pub(super) async fn reentrant_worker(
    backend: &crate::Backend,
    lease_owner: LeaseOwnerIdentity,
    run_handle: Arc<LateBoundProcessWork>,
) -> DurableProcessWorker {
    let (_driver_registry, _driver_hub, process_work) =
        late_bound_process_work_wiring(backend.process_registry(), Arc::clone(&run_handle));
    let worker = DurableProcessWorker::new(DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        host_config_with_fixture_env(backend).await,
        crate::WorkerProcessWork::External(process_work),
        Arc::new(crate::NoSessionWork::new()),
        lease_owner,
    ))
    .expect("valid test native substrate config");
    run_handle
        .worker
        .set(worker.clone())
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    worker
}

/// The worker's host config over `backend`, with the fixture execution
/// environment published into the backend's process-exec-env store: a trigger
/// delivery the worker starts loads that environment by the reference its
/// subscription recorded.
async fn host_config_with_fixture_env(backend: &crate::Backend) -> RuntimeHostConfig {
    let config = test_host_config(backend).with_process_engine_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(crate::testing::FixtureProcessEngine)),
    );
    crate::testing::process_execution_env_fixture(config.durability.process_env_store.as_ref())
        .await;
    config
}
