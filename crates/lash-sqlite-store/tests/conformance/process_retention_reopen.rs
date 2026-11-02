#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_prune_cleanup_obligation_survives_reopen() {
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_execution_contracts::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let registry = backend.process_registry();
    let registered = registry
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::json!({"module_ref": "module-sqlite"}),
                launch: lash_execution_contracts::process::EngineLaunch::new(
                    lash_execution_contracts::testing::process_execution_env_fixture_ref(),
                    None,
                ),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("register cleanup process");
    registry
        .complete_process(
            &registered.id,
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_protocol_types::tool_output::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                "sqlite-prune-cleanup",
            ),
        )
        .await
        .expect("complete cleanup process");
    registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_execution_contracts::process::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune with atomic cleanup evidence");
    drop(registry);

    let reopened = backend.reopen().await;
    let conn = reopened.raw();
    let (obligation_id, state, kind, referrer_id, body): (String, String, String, String, String) =
        conn.query_row(
            "SELECT obligation_id, obligation_state, referrer_kind, referrer_id, cleanup_json \
             FROM artifact_cleanup_obligations",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("read cleanup obligation after reopen");
    assert_eq!(state, "due");
    let referrer =
        lash_core_store::artifact_referrer::ArtifactReferrer::decode(&kind, &referrer_id)
            .expect("decode process referrer");
    assert_eq!(
        obligation_id,
        lash_core_store::store::ObligationKey::ArtifactCleanup {
            referrer: referrer.clone(),
        }
        .id()
        .as_str()
    );
    assert_eq!(
        referrer,
        lash_core_store::artifact_referrer::ArtifactReferrer::ProcessRecord(registered.id)
    );
    let cleanup = lash_core_store::artifact_referrer::ArtifactCleanup::from_json(&body, &referrer)
        .expect("decode cleanup body");
    assert!(
        matches!(cleanup, lash_core_store::artifact_referrer::ArtifactCleanup::Ended { ref carries, .. } if carries.is_empty())
    );
    assert!(cleanup.gate().is_none());
}
