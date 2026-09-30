//! Postgres proof that the process-prune delete path obeys the tombstone-reclaim
//! law: the batch delete drains rows orphaned under sessions outside the batch,
//! pruned process-session ids join the deleted set, and a later delete drains
//! rows orphaned under them.
//!
//! This lives in its own test target rather than the conformance suite, which is
//! at its line budget.

use lash_core_execution::testing::store_fixtures::RuntimeStoreTestDriveExt as _;
use std::sync::Arc;

use lash_core_execution::store::RootStore;
use lash_core_execution::{
    DeploymentStore, ProcessExecutionEnvStore, ProcessLifecycle as _, ProcessRegistrar as _,
    ProcessRegistry, ProcessRetention as _, SessionCatalogStore as _, TurnInputStore,
};
use lash_postgres_store::PostgresStorage;

use crate::support::{SharedDatabaseLock, database_url};

#[path = "blob_probe.rs"]
mod blob_probe;

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect postgres");
    Some((database_lock, storage))
}

/// Truncate every `lash_*` fixture table, derived from the live catalog so a new
/// table cannot silently bleed state in. `lash_schema_versions` holds the
/// component version gate, not fixture rows.
async fn reset(storage: &PostgresStorage) {
    let pool = storage.pool();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_catalog_identity', 'lash_fleet_format')
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await
    .expect("list lash_* tables");
    assert!(!tables.is_empty(), "lash_* schema tables must exist");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(pool)
    .await
    .expect("reset postgres tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(pool)
    .await
    .expect("reset postgres process change clock");
}

lash_conformance::process_prune_reclaim_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process-prune blob reclaim law: database URL is not set");
        return;
    };
    reset(&storage).await;
    let storage = Arc::new(storage);
    let factory = Arc::new(storage.store()) as Arc<dyn DeploymentStore>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(blob_probe::PostgresBlobProbe::new(
        storage,
        "fail_process_prune_blob_delete",
    ));
    (database_lock, "postgres", factory, registry, probe)
});

lash_conformance::process_start_staging_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres refused-start staging law: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        Arc::new(storage.lashlang_artifact_store()),
        Arc::new(storage.process_env_store()),
        storage.artifact_cleanup(),
        Arc::new(lash_core_execution::facade_support::SystemClock),
    );
    (database_lock, registry, ports)
});

lash_conformance::process_prune_start_staging_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres prune and late-transfer law: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let env_store = Arc::new(storage.process_env_store()) as Arc<dyn ProcessExecutionEnvStore>;
    (database_lock, registry, env_store)
});

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_fence_and_obligation_survive_reopen_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process cleanup recovery: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = storage.process_registry();
    let registered = registry
        .register_process(
            lash_core_execution::ProcessRegistration::new(
                lash_core_execution::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::json!({"module_ref": "module-postgres"}),
                },
                lash_core_execution::ProcessProvenance::host(),
                lash_core_execution::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(
                lash_core_execution::ProcessExecutionEnvRef::new("process-env:postgres-cleanup"),
            )),
        )
        .await
        .expect("register cleanup process");
    registry
        .complete_process(
            &registered.id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_core_execution::ProcessCompletionAuthority::workflow_key("postgres-prune-cleanup"),
        )
        .await
        .expect("complete cleanup process");
    registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune with cleanup evidence");
    drop(registry);

    let referrer = lash_core_execution::ArtifactReferrer::ProcessRecord(registered.id);
    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM lash_referrer_fences
         WHERE referrer_kind = $1 AND referrer_id = $2)",
    )
    .bind(referrer.kind().as_str())
    .bind(referrer.canonical_id())
    .fetch_one(storage.pool())
    .await
    .expect("read durable prune fence");
    assert!(fenced);
    let id: String = sqlx::query_scalar(
        "SELECT obligation_id FROM lash_artifact_cleanup_obligations
         WHERE referrer_kind = $1 AND referrer_id = $2",
    )
    .bind(referrer.kind().as_str())
    .bind(referrer.canonical_id())
    .fetch_one(storage.pool())
    .await
    .expect("read durable cleanup obligation");
    let cleanup = lash_core_execution::store::ArtifactCleanupLedger::load_cleanup(
        storage.artifact_cleanup().as_ref(),
        &lash_core_execution::store::ObligationId::new(id),
    )
    .await
    .expect("load cleanup body")
    .expect("prune owes a cleanup");
    assert_eq!(cleanup.referrer, referrer);
    assert!(
        matches!(cleanup.plan, lash_core_execution::ArtifactCleanupPlan::Ended { carries } if carries.is_empty())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_removes_an_admitted_roots_record() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres process-prune root-admission law: database URL is not set");
        return;
    };
    reset(&storage).await;
    let registry = storage.process_registry();
    let process = registry
        .register_process(lash_core_execution::ProcessRegistration::new(
            lash_core_execution::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core_execution::ProcessProvenance::host(),
            lash_core_execution::Lifetime::Detached,
        ))
        .await
        .expect("register process");
    let session_id =
        lash_core_execution::facade_support::process_runtime_session_ids(&process.id)[0].clone();
    let factory = storage.store();
    factory
        .admit_session(&lash_core_execution::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core_execution::SessionRelation::default(),
            config: lash_core_execution::SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            )
            .into(),
            head: lash_core_execution::SessionCreationHead::CommittedByCreator,
        })
        .await
        .expect("create process-owned session");
    let store = factory;
    let input = store
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            &session_id,
            lash_core_execution::TurnInputIngress::NextTurn,
            lash_core_execution::TurnInput::text("queued before prune"),
        ))
        .await
        .expect("enqueue pending input");
    let lease = store
        .seal_drive_epoch_for_test(
            &session_id,
            &lash_core_execution::LeaseOwnerIdentity::opaque(
                "prune-run-owner",
                "prune-run-incarnation",
            ),
            "prune-run-executor",
            60_000,
        )
        .await
        .expect("seal drive")
        .acquired()
        .expect("drive sealed");
    let admission = store
        .admit_root(
            &lash_core_execution::testing::store_fixtures::admit_root_request_for_test(
                &lease,
                &lash_core_execution::TurnId::from("prune-root"),
                lash_core_execution::store::AdmittedHead::Input(input.input_id.clone()),
            ),
        )
        .await
        .expect("admit the root")
        .expect("the root reaches its head");
    assert_eq!(admission.input_ids(), vec![input.input_id]);
    let admitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lash_session_roots \
         WHERE session_id = $1 AND admission_json IS NOT NULL",
    )
    .bind(session_id.as_str())
    .fetch_one(storage.pool())
    .await
    .expect("count admitted roots before prune");
    assert_eq!(
        admitted, 1,
        "the root's record carries the process-owned admission"
    );
    let terminal = registry
        .complete_process(
            &process.id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_core_execution::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete process");
    registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            lash_core_execution::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune process-owned session");
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM lash_session_roots WHERE session_id = $1")
            .bind(session_id.as_str())
            .fetch_one(storage.pool())
            .await
            .expect("count root records after prune");
    assert_eq!(
        remaining, 0,
        "lash_session_roots must not retain a deleted process session"
    );
}
