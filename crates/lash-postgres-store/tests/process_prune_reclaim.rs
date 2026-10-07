//! Postgres proofs that process pruning preserves independent session roots
//! and durably fences the ProcessRecord referrer with its cleanup obligation.
//!
//! This lives in its own test target rather than the conformance suite, which is
//! at its line budget.

use std::sync::Arc;

use lash_core_execution::{
    DeploymentStore, ProcessExecutionEnvStore, ProcessLifecycle as _, ProcessRegistrar as _,
    ProcessRegistry, ProcessRetention as _,
};
use lash_postgres_store::{PostgresStorage, testing::IsolatedDatabase};

use crate::support::database_url;

#[path = "blob_probe.rs"]
mod blob_probe;

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
        Arc::new(storage.lashlang_artifact_store()),
        Arc::new(storage.process_env_store()),
        Arc::new(storage.lashlang_artifact_store()),
        Arc::new(storage.store()) as Arc<dyn lash_core_execution::AttachmentReferrers>,
        storage.artifact_cleanup(),
        Arc::new(lash_core_execution::facade_support::SystemClock),
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
        Arc::new(lash_core_execution::facade_support::FileAttachmentStore::new(attachments.path())),
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
        Arc::new(storage.lashlang_artifact_store()),
        Arc::new(storage.process_env_store()),
        Arc::new(storage.lashlang_artifact_store()),
        Arc::new(storage.store()) as Arc<dyn lash_core_execution::AttachmentReferrers>,
        storage.artifact_cleanup(),
        Arc::new(lash_core_execution::facade_support::SystemClock),
    );
    (database, registry, ports)
});

lash_conformance::process_prune_start_staging_tests!({
    let Some((database, storage)) = storage().await else {
        eprintln!("skipping Postgres prune and late-transfer law: database URL is not set");
        return;
    };
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    let env_store = Arc::new(storage.process_env_store()) as Arc<dyn ProcessExecutionEnvStore>;
    (database, registry, env_store)
});

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_process_prune_fence_and_obligation_survive_reopen_when_configured() {
    let Some((_database, storage)) = storage().await else {
        eprintln!("skipping Postgres process cleanup recovery: database URL is not set");
        return;
    };
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
    assert_eq!(cleanup.referrer(), referrer);
    assert!(
        matches!(cleanup, lash_core_execution::ArtifactCleanup::Ended { ref carries, .. } if carries.is_empty())
    );
}
