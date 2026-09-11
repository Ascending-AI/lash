use super::*;

#[tokio::test]
async fn deployment_drain_status_keeps_waiting_process_non_drained() {
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("open in-memory process registry"),
    );
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(crate::TurnBudget::Unbounded)
            .model(mock_model_spec())
            .store_factory(Arc::new(
                crate::persistence::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(registry.clone()),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("build core with a process registry");
    let process_id = "deployment-drain-status-waiting";
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register waiting process");
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id,
        "deployment-drain-status-waiting-run",
    )
    .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("attempt-bound invocation has a start fact");
    registry
        .record_first_started_with_authority(&ProcessId::from(process_id), started, &authority)
        .await
        .expect("record process start");
    registry
        .set_process_wait_with_authority(
            &ProcessId::from(process_id),
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "deployment-drain-status".to_string(),
                    event_type: "deployment.drain_status".to_string(),
                    key: "deployment-drain-status-waiting:signal".to_string(),
                    ordinal: 1,
                },
            },
            &authority,
        )
        .await
        .expect("set process waiting");

    let status = core
        .drain_status(false)
        .await
        .expect("read deployment drain status");
    assert_eq!(status.remaining_invocations, 1);
    assert!(!status.drained);
}

#[tokio::test]
async fn testing_facade_run_tool_executes_provider() {
    let outcome = crate::testing::run_tool(
        &AppTools,
        "app_lookup",
        &serde_json::json!({ "query": "weather" }),
    )
    .await;

    assert!(outcome.is_success());
    assert_eq!(
        outcome.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
}
