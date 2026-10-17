//! Postgres proofs that process pruning preserves independent session roots
//! and durably fences the ProcessRecord referrer with its cleanup obligation.
//!
//! This lives in its own test target rather than the conformance suite, which is
//! at its line budget.

use std::sync::Arc;

use lash_execution_contracts::deployment::DeploymentStore;
use lash_execution_contracts::process::ProcessEnvStore;
use lash_execution_contracts::process::ProcessLifecycle as _;
use lash_execution_contracts::process::ProcessRegistrar as _;
use lash_execution_contracts::process::ProcessRegistry;
use lash_execution_contracts::process::ProcessRetention as _;
use lash_postgres_store::{PostgresStorage, testing::IsolatedDatabase};

use crate::support::database_url;

#[path = "blob_probe.rs"]
mod blob_probe;
#[path = "definition_graph_probe.rs"]
mod definition_graph_probe;

async fn storage() -> Option<(IsolatedDatabase, PostgresStorage)> {
    let url = database_url()?;
    let database = IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect postgres");
    Some((database, storage))
}

lash_conformance::process_prune_reclaim_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres process-prune blob reclaim law: database URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    let factory = Arc::new(storage.store()) as Arc<dyn DeploymentStore>;
    lash_execution_contracts::testing::process_execution_env_fixture(&storage.process_env_store())
        .await;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(blob_probe::PostgresBlobProbe::new(
        storage,
        "fail_process_prune_blob_delete",
    ));
    (database, "postgres", factory, registry, probe)
});

lash_conformance::process_start_staging_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres refused-start staging law: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        Arc::new(storage.lash_vm_artifact_store()),
        Arc::new(storage.process_env_store()),
        Arc::new(storage.lash_vm_artifact_store()),
        Arc::new(storage.store()) as Arc<dyn lash_core_store::store::AttachmentReferrers>,
        storage.artifact_cleanup(),
        Arc::new(lash_core_ids::clock::SystemClock),
    );
    (database, registry, ports)
});

lash_conformance::start_operation_staging_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres start-operation staging laws: database URL is not set");
        return;
    };
    let attachments = tempfile::tempdir().expect("attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open(
            (attachments.path()).join("attachments.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("SQLite attachment store")
        .attachment_store(),
    ));
    (
        (database, attachments),
        lash_conformance::backend_over(stores),
    )
});

lash_conformance::process_definition_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres process-definition laws: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        Arc::new(storage.lash_vm_artifact_store()),
        Arc::new(storage.process_env_store()),
        Arc::new(storage.lash_vm_artifact_store()),
        Arc::new(storage.store()) as Arc<dyn lash_core_store::store::AttachmentReferrers>,
        storage.artifact_cleanup(),
        Arc::new(lash_core_ids::clock::SystemClock),
    );
    (database, registry, ports)
});

lash_conformance::process_definition_graph_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres definition graph laws: database URL is not set");
        return;
    };
    let storage = Arc::new(storage);
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        Arc::new(storage.lash_vm_artifact_store()),
        Arc::new(storage.process_env_store()),
        Arc::new(storage.lash_vm_artifact_store()),
        Arc::new(storage.store()) as Arc<dyn lash_core_store::store::AttachmentReferrers>,
        storage.artifact_cleanup(),
        Arc::new(lash_core_ids::clock::SystemClock),
    );
    let probe = Arc::new(definition_graph_probe::PostgresDefinitionGraphProbe::new(
        storage,
    )) as Arc<dyn lash_conformance::DefinitionGraphProbe>;
    (database, registry, ports, probe)
});

lash_conformance::process_prune_start_staging_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres prune and late-transfer law: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let env_store = Arc::new(storage.process_env_store()) as Arc<dyn ProcessEnvStore>;
    (database, registry, env_store)
});

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_fence_and_obligation_survive_reopen_when_configured() {
    let Some((_database, storage)) = storage().await else {
        eprintln!("skipping Postgres process cleanup recovery: database URL is not set");
        return;
    };
    lash_execution_contracts::testing::process_execution_env_fixture(&storage.process_env_store())
        .await;
    let registry = storage.process_registry();
    let registered = registry
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::json!({"module_ref": "module-postgres"}),
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
                "postgres-prune-cleanup",
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
        .expect("prune with cleanup evidence");
    drop(registry);

    let referrer =
        lash_core_store::artifact_referrer::ArtifactReferrer::ProcessRecord(registered.id);
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
    let cleanup = lash_core_store::store::ArtifactCleanupLedger::load_cleanup(
        storage.artifact_cleanup().as_ref(),
        &lash_core_store::store::ObligationId::new(id),
    )
    .await
    .expect("load cleanup body")
    .expect("prune owes a cleanup");
    assert_eq!(cleanup.referrer(), referrer);
    assert!(
        matches!(cleanup, lash_core_store::artifact_referrer::ArtifactCleanup::Ended { ref carries, .. } if carries.is_empty())
    );
}
