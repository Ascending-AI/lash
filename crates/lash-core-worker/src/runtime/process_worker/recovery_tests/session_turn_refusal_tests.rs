use super::*;

/// FIG-3487: a `ProcessInput::SessionTurn` whose configured catalog cannot
/// resolve a session by id must fail the process on its first admission.
///
/// Before the seam's refusal was typed, the catalog's `Err` mapped to
/// `SessionTurnInitError::Create` — a recoverable infrastructure failure — so
/// the worker released the claim and re-admitted the same uninitializable
/// row on every sweep; under a paused clock that loop is a true deadlock.
/// The refusal is a capability fact no retry can change, so the process
/// terminalizes `Failed` instead.
#[tokio::test]
async fn session_turn_against_a_catalog_without_by_id_lookup_fails_in_one_admission() {
    let raw_registry = Arc::new(TestLocalProcessRegistry::default());
    let raw_registry_port: Arc<dyn ProcessRegistry> = raw_registry.clone();
    let sink = Arc::new(RecordingProcessEventSink::default());
    let watched = crate::watch_process_registry_with_sink(
        raw_registry_port,
        Some(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>),
    );
    let registry = Arc::clone(watched.registry());
    let factory = Arc::new(NoByIdLookupSessionStoreFactory::default());
    let policy = test_session_policy();
    // This test drives the attempt by hand, so the idle dispatcher's
    // autonomous rescan is pushed outside the test's window.
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        ),
        factory.clone(),
        crate::WorkerProcessWork::SelfNative(watched),
        Arc::new(crate::NoQueuedWork::new()),
        local_owner("no-by-id-lookup-worker", "host-a", "no-by-id-lookup"),
    )
    .with_session_policy(policy)
    .with_process_event_sink(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>);
    config.native_substrate.worker_sweep.rescan_interval = Duration::from_secs(3600);
    let worker = DurableProcessWorker::new(config).expect("valid worker");
    let process_id = "session-turn-no-by-id-catalog";
    registry
        .register_process(session_turn_registration(
            &ProcessId::from(process_id),
            &SessionId::from("no-by-id-catalog-child"),
        ))
        .await
        .expect("register SessionTurn fixture");

    let report = worker
        .drive_pending_processes()
        .await
        .expect("admit the SessionTurn");
    assert_eq!(report.admitted, vec![process_id.to_string()]);
    await_terminal(&registry, &ProcessId::from(process_id)).await;
    let record = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read refused SessionTurn")
        .expect("refused SessionTurn remains retained");
    assert_eq!(
        record.status,
        ProcessStatus::Failed,
        "a catalog that cannot resolve by id refuses the turn terminally"
    );
    assert_eq!(
        factory.by_id_opens(),
        1,
        "the refusal is decided on the first admission — a recoverable \
         mapping would release the claim and probe the catalog again"
    );
    // A terminal row is not claimable: the next sweep admits nothing.
    let second = worker
        .drive_pending_processes()
        .await
        .expect("second sweep");
    assert!(
        second.admitted.is_empty(),
        "a refused process is never re-admitted: {:?}",
        second.admitted
    );
    assert_eq!(factory.by_id_opens(), 1);
}
