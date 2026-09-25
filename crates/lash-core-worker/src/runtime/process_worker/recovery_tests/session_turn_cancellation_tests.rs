use super::*;

#[tokio::test]
async fn cancelled_session_turn_never_creates_and_leaves_foreign_sessions_alone() {
    let backend = memory_backend().await;
    let raw_registry_port = backend.process_registry();
    let sink = Arc::new(RecordingProcessEventSink::default());
    let watched = crate::watch_process_registry_with_sink(
        raw_registry_port,
        Some(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>),
    );
    let registry = Arc::clone(watched.registry());
    let factory = backend.session_store_factory();
    let policy = test_session_policy();
    let foreign_session_id = "cancel-never-creates-foreign-root";
    factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: SessionId::from(foreign_session_id.to_string()),
            relation: crate::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: policy.clone(),
        })
        .await
        .expect("materialize unrelated durable root session");
    // This test drives the attempt by hand, so the idle dispatcher's
    // autonomous rescan is pushed outside the test's window.
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(PluginHost::new(
            crate::testing::test_standard_protocol_factories(),
        )),
        test_host_config(&backend),
        crate::WorkerProcessWork::SelfNative(watched),
        Arc::new(crate::NoSessionWork::new()),
        local_owner(
            "cancel-before-start-worker",
            "host-a",
            "cancel-before-start",
        ),
    )
    .with_session_policy(policy)
    .with_process_event_sink(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>);
    config.native_substrate.worker_sweep.rescan_interval = Duration::from_secs(3600);
    let worker = DurableProcessWorker::new(config).expect("valid cancel worker");
    let process_id = "session-turn-cancelled-before-start";
    registry
        .register_process(session_turn_registration(
            &ProcessId::from(process_id),
            &SessionId::from(foreign_session_id),
        ))
        .await
        .expect("register SessionTurn fixture");
    registry
        .append_event(
            &ProcessId::from(process_id),
            crate::ProcessEventAppendRequest::cancel_requested(&registry.resolve_process_ref(&ProcessId::from(process_id)).await.expect("retained cancellation target"),
&crate::CancelRequest::new(crate::CancelOrigin::OperatorRequested, "actor:fixture:cancelled_session_turn_never_creates_and_leaves_foreign_sessions_alone", 11)),
        )
        .await
        .expect("append durable cancellation");

    let report = worker
        .drive_pending_processes()
        .await
        .expect("admit cancelled SessionTurn");
    assert_eq!(report.admitted, vec![process_id.to_string()]);
    await_terminal(&registry, &ProcessId::from(process_id)).await;
    let record = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read cancelled SessionTurn")
        .expect("cancelled SessionTurn remains retained");
    assert_eq!(
        record.status,
        ProcessStatus::Cancelled,
        "cancellation observed before initialization settles the process Cancelled"
    );
    assert!(
        sink.faults().is_empty(),
        "cancellation before initialization is not a fault: {:?}",
        sink.faults()
    );
    // The registration named a foreign session id; the cancellation path must
    // not touch it — lash never deletes a session because a process was
    // cancelled, and the port creates nothing before the create commit.
    assert!(
        factory
            .open_existing_store_by_id(&SessionId::from(foreign_session_id))
            .await
            .expect("foreign session still resolvable")
            .is_some(),
        "the foreign session the cancelled process named stays retained"
    );
}
