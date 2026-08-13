use super::*;

pub(super) async fn worker_with_engine(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessRunHandle>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessRunHandle>,
    ProcessExecutionEnvRef,
) {
    let (worker, registry, run_handle, env_ref, _) =
        worker_with_engine_and_registry(concurrency, engine, run_handle).await;
    (worker, registry, run_handle, env_ref)
}

pub(super) async fn worker_with_engine_and_registry(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessRunHandle>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessRunHandle>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
) {
    worker_with_engine_registry_timings_and_supplier(concurrency, engine, run_handle, None, None)
        .await
}

pub(super) async fn worker_with_engine_registry_and_timings(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessRunHandle>,
    lease_timings: Option<crate::LeaseTimings>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessRunHandle>,
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

pub(super) async fn worker_with_engine_registry_timings_and_supplier(
    concurrency: usize,
    engine: Arc<dyn crate::ProcessEngine>,
    run_handle: Arc<LateBoundProcessRunHandle>,
    lease_timings: Option<crate::LeaseTimings>,
    supplier: Option<Arc<dyn crate::WorkerSlotSupplier>>,
) -> (
    DurableProcessWorker,
    Arc<dyn ProcessRegistry>,
    Arc<LateBoundProcessRunHandle>,
    ProcessExecutionEnvRef,
    Arc<TestLocalProcessRegistry>,
) {
    let test_registry = Arc::new(TestLocalProcessRegistry::default());
    let raw_registry: Arc<dyn ProcessRegistry> = test_registry.clone();
    let driver = crate::ProcessWorkDriver::new(
        Arc::clone(&raw_registry),
        Arc::clone(&run_handle) as Arc<dyn crate::ProcessRunHandle>,
    );
    let registry = driver.process_registry();
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
        local_owner("engine-worker", "host-a", "engine-start"),
    )
    .with_session_policy(policy)
    .with_process_execution_concurrency(concurrency)
    .expect("valid test process execution concurrency")
    .with_change_hub(driver.change_hub())
    .with_process_work_driver(driver);
    if let Some(supplier) = supplier {
        config = config.with_worker_slot_supplier(supplier);
    }
    let worker = DurableProcessWorker::new(config);
    run_handle
        .worker
        .set(worker.clone())
        .unwrap_or_else(|_| panic!("test process worker is bound exactly once"));
    (worker, registry, run_handle, env_ref, test_registry)
}
