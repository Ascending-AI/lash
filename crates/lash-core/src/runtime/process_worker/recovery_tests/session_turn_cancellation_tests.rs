use super::*;

#[tokio::test]
async fn repeated_session_turn_cleanup_failure_is_faulted_per_attempt_then_abandoned() {
    let raw_registry = Arc::new(TestLocalProcessRegistry::default());
    let raw_registry_port: Arc<dyn ProcessRegistry> = raw_registry.clone();
    let sink = Arc::new(RecordingProcessEventSink::default());
    let watched = crate::watch_process_registry_with_sink(
        raw_registry_port,
        Some(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>),
    );
    let registry = Arc::clone(watched.registry());
    let factory = Arc::new(crate::InMemorySessionStoreFactory::new());
    let policy = test_session_policy();
    let foreign_session_id = "cleanup-failure-foreign-root";
    factory
        .create_store(&crate::SessionStoreCreateRequest {
            session_id: foreign_session_id.to_string(),
            relation: crate::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: policy.clone(),
        })
        .await
        .expect("materialize unrelated durable root session");
    let worker = DurableProcessWorker::new(
        DurableProcessWorkerConfig::new(
            Arc::new(PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            RuntimeHostConfig::in_memory(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
            ),
            factory,
            crate::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoQueuedWork::new()),
            local_owner("cleanup-failure-worker", "host-a", "cleanup-failure-start"),
        )
        .with_session_policy(policy)
        .with_process_event_sink(Arc::clone(&sink) as Arc<dyn crate::ProcessEventSink>),
    )
    .expect("valid cleanup-failure worker");
    let process_id = "session-turn-repeated-cleanup-failure";
    registry
        .register_process(
            session_turn_registration(process_id, foreign_session_id).with_max_attempts(Some(2)),
        )
        .await
        .expect("register cleanup-failure SessionTurn");
    registry
        .append_event(
            process_id,
            crate::ProcessEventAppendRequest::cancel_requested(
                process_id,
                Some("exercise repeated cleanup failure".to_string()),
            ),
        )
        .await
        .expect("append durable cancellation");

    for expected_faults in 1..=2 {
        let report = worker
            .drive_pending_processes()
            .await
            .expect("admit failed cleanup attempt");
        assert_eq!(report.admitted, vec![process_id.to_string()]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while sink.faults().len() < expected_faults {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed cleanup attempt reaches the fault sink");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let idle = {
                    let state = worker.execution_scheduler.state.lock_recover();
                    state.active == 0 && !state.dispatcher_running
                };
                if idle {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed cleanup attempt leaves the worker idle");
        let record = registry
            .get_process(process_id)
            .await
            .expect("read failed cleanup process")
            .expect("failed cleanup process remains retained");
        assert!(
            !record.is_terminal(),
            "cleanup attempt {expected_faults} must remain retryable"
        );
    }

    let faults = sink.faults();
    assert_eq!(faults.len(), 2);
    for fault in &faults {
        match fault {
            ProcessWorkerFault::RecoveryRunFailed {
                process_id: fault_process_id,
                error,
            } => {
                assert_eq!(fault_process_id, process_id);
                assert!(
                    error.contains("not owned by process"),
                    "cleanup fault preserves the ownership failure: {error}"
                );
            }
            other => panic!("expected RecoveryRunFailed, got {other:?}"),
        }
    }

    let report = worker
        .drive_pending_processes()
        .await
        .expect("admit exhausted cleanup process");
    assert_eq!(report.admitted, vec![process_id.to_string()]);
    await_terminal(&registry, process_id).await;
    let evidence = abandoned_evidence(&registry, process_id).await;
    assert_eq!(evidence.writer, AbandonWriter::EngineGaveUp);
    assert_eq!(
        sink.faults().len(),
        2,
        "attempt-budget abandonment must not run cleanup a third time"
    );
}
