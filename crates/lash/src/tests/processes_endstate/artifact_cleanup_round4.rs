use super::*;

#[tokio::test]
async fn stale_process_cleanup_cannot_release_reregistered_incarnation_owner() -> Result<()> {
    let dir = tempfile::tempdir().expect("stale cleanup tempdir");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open process registry"),
    );
    let artifact_store = Arc::new(
        lash_sqlite_store::Store::open(&dir.path().join("artifacts.db"))
            .await
            .expect("open production artifact store"),
    );
    let engine = Arc::new(FailOnceReleaseEngine {
        state: std::sync::Mutex::new(PruneEngineState::default()),
        release_failures: std::sync::atomic::AtomicUsize::new(1),
    });
    let process_id = ProcessId::from("reused-process-owner");
    let env_spec = process_env_spec();
    let env_ref = env_spec.stable_ref().expect("stable environment ref");
    let env_bytes = env_spec.to_store_bytes().expect("environment bytes");

    let first = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                process_id.clone(),
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "reused-process-owner"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    let first_owner = lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&first));
    artifact_store
        .publish_process_execution_env(&first_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(first_owner.clone());
    registry
        .complete_process(
            &first.id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.to_string()),
        )
        .await?;

    let core = prune_recovery_core(
        registry.clone() as Arc<dyn lash_core::ProcessRegistry>,
        artifact_store.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>,
        Arc::clone(&engine),
    )?;
    assert!(
        core.processes()
            .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
            .await
            .is_err(),
        "engine release fault must leave the first incarnation cleanup pending"
    );

    let second = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                process_id.clone(),
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "reused-process-owner"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    assert_ne!(first.incarnation, second.incarnation);
    let second_owner =
        lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&second));
    assert_ne!(
        first_owner, second_owner,
        "owner identity is incarnation-typed"
    );
    assert!(matches!(
        registry
            .get_process_ref(&lash_core::ProcessRef::from_record(&first))
            .await,
        Err(lash_core::PluginError::ProcessIncarnationSuperseded {
            requested_incarnation,
            current_incarnation,
            ..
        }) if requested_incarnation == first.incarnation
            && current_incarnation == second.incarnation
    ));
    artifact_store
        .publish_process_execution_env(&second_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(second_owner);

    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(
        report.artifact_cleanup_acknowledgements,
        vec![lash_core::ProcessArtifactCleanupAck::StaleIncarnation {
            expected: lash_core::ProcessRef::from_record(&first),
            found: lash_core::ProcessRef::from_record(&second),
        }],
        "the facade must surface that the durable cleanup belonged to a predecessor incarnation"
    );
    assert!(
        artifact_store
            .get_process_execution_env(&env_ref)
            .await?
            .is_some(),
        "acknowledging the stale cleanup must preserve the live incarnation's owner"
    );
    assert!(
        registry
            .pending_process_artifact_cleanup()
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_cleanup_fault_reopens_retries_and_acknowledges_when_configured()
-> Result<()> {
    use sqlx::Connection as _;

    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(url) if !url.is_empty() => url,
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL is required")
        }
        _ => {
            eprintln!("skipping PostgreSQL process-cleanup recovery: database URL is not set");
            return Ok(());
        }
    };
    let mut lock = sqlx::PgConnection::connect(&database_url)
        .await
        .expect("connect PostgreSQL cleanup-test advisory lock");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(0x4c41_5348_5f50_4754_i64)
        .execute(&mut lock)
        .await
        .expect("acquire PostgreSQL cleanup-test advisory lock");
    let storage = lash_postgres_store::PostgresStorage::connect(&database_url).await?;
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list PostgreSQL cleanup-test tables");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .expect("reset PostgreSQL cleanup-test tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = 0",
    )
    .execute(storage.pool())
    .await
    .expect("reset PostgreSQL process change clock");

    let registry = Arc::new(storage.process_registry());
    let env_store = Arc::new(storage.process_env_store());
    let engine = Arc::new(FailOnceReleaseEngine {
        state: std::sync::Mutex::new(PruneEngineState::default()),
        release_failures: std::sync::atomic::AtomicUsize::new(1),
    });
    let env_spec = process_env_spec();
    let env_ref = env_spec.stable_ref()?;
    let env_bytes = env_spec.to_store_bytes()?;
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "postgres-prune-recovery",
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "postgres-prune-recovery"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    let process_owner =
        lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&registered));
    let shared_owner = lash_core::ArtifactOwner::host("postgres-prune-shared");
    env_store
        .publish_process_execution_env(&process_owner, &env_ref, &env_bytes)
        .await?;
    env_store
        .publish_process_execution_env(&shared_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(process_owner);
    engine.retain(shared_owner.clone());
    registry
        .complete_process(
            &registered.id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key("postgres-prune-recovery"),
        )
        .await?;
    let first_core = prune_recovery_core(
        registry.clone() as Arc<dyn lash_core::ProcessRegistry>,
        env_store.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>,
        Arc::clone(&engine),
    )?;
    assert!(
        first_core
            .processes()
            .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
            .await
            .is_err()
    );
    assert_eq!(registry.pending_process_artifact_cleanup().await?.len(), 1);
    drop((first_core, registry, env_store, storage));

    let reopened = lash_postgres_store::PostgresStorage::connect(&database_url).await?;
    let reopened_registry = Arc::new(reopened.process_registry());
    let reopened_env = Arc::new(reopened.process_env_store());
    let recovered_core = prune_recovery_core(
        reopened_registry.clone() as Arc<dyn lash_core::ProcessRegistry>,
        reopened_env.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>,
        Arc::clone(&engine),
    )?;
    let successor = reopened_registry
        .register_process(
            lash_core::ProcessRegistration::new(
                registered.id.clone(),
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "postgres-prune-recovery"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    assert_ne!(registered.incarnation, successor.incarnation);
    let successor_owner =
        lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&successor));
    reopened_env
        .publish_process_execution_env(&successor_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(successor_owner.clone());

    let report = recovered_core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(
        report.artifact_cleanup_acknowledgements,
        vec![lash_core::ProcessArtifactCleanupAck::StaleIncarnation {
            expected: lash_core::ProcessRef::from_record(&registered),
            found: lash_core::ProcessRef::from_record(&successor),
        }],
        "the reopened PostgreSQL facade must surface predecessor cleanup explicitly"
    );
    assert!(
        reopened_registry
            .pending_process_artifact_cleanup()
            .await?
            .is_empty(),
        "retry after reopen acknowledges the durable cleanup only after both stores succeed"
    );
    assert!(
        reopened_env
            .get_process_execution_env(&env_ref)
            .await?
            .is_some()
    );
    let (engine_bytes, engine_owners) = engine.snapshot();
    assert!(engine_bytes);
    assert_eq!(
        engine_owners,
        HashSet::from([shared_owner, successor_owner])
    );
    Ok(())
}
