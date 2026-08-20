//! Deterministic transaction interleaving for checkpoint publication versus
//! component deletion. The test observes PostgreSQL's lock wait directly; no
//! timing sleep decides which transaction won.

use std::time::Duration;

use lash_core::{
    HydratedCheckpointComponent, RuntimeCommit, RuntimeSessionState, SessionCommitStore,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};
use lash_postgres_store::{PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaProvisioning};
use sqlx::postgres::PgPoolOptions;

mod support;

use support::{SharedDatabaseLock, database_url};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_commit_waits_for_delete_then_refuses_missing_component_when_configured() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres commit-vs-delete law: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect Postgres commit-vs-delete fixture");
    reset(&storage).await;
    let factory = storage.session_store_factory();

    let victim = factory
        .create_store(&request("commit-delete-victim"))
        .await
        .expect("create delete victim");
    let mut victim_state = RuntimeSessionState {
        session_id: "commit-delete-victim".to_string(),
        ..RuntimeSessionState::new(request("commit-delete-victim").policy)
    };
    victim_state.ensure_agent_frame_initialized();
    let mut victim_commit = RuntimeCommit::persisted_state_for_test(&victim_state, &[]);
    victim_commit.checkpoint.components.insert(
        "law/commit-delete-shared".to_string(),
        HydratedCheckpointComponent::changed(b"commit-delete-shared".to_vec()),
    );
    let victim_receipt = victim
        .commit_runtime_state(victim_commit)
        .await
        .expect("commit delete victim checkpoint");
    let shared = victim_receipt.manifest.components["law/commit-delete-shared"].clone();

    let _target = factory
        .create_store(&request("commit-delete-target"))
        .await
        .expect("create commit target");
    let mut target_state = RuntimeSessionState {
        session_id: "commit-delete-target".to_string(),
        ..RuntimeSessionState::new(request("commit-delete-target").policy)
    };
    target_state.ensure_agent_frame_initialized();
    let mut target_commit = RuntimeCommit::persisted_state_for_test(&target_state, &[]);
    target_commit.checkpoint.components.insert(
        "law/commit-delete-shared".to_string(),
        HydratedCheckpointComponent::unchanged(&shared),
    );
    target_commit.checkpoint.components.insert(
        "law/commit-delete-target-only".to_string(),
        HydratedCheckpointComponent::changed(b"commit-delete-target-only".to_vec()),
    );

    let mut deleting = storage
        .pool()
        .begin()
        .await
        .expect("begin controlled delete transaction");
    sqlx::query("SELECT hash FROM lash_blobs WHERE hash = $1 FOR UPDATE")
        .bind(shared.blob_ref.as_str())
        .fetch_one(&mut *deleting)
        .await
        .expect("lock shared component for delete");

    let application_name = format!("lash-commit-delete-law-{}", uuid::Uuid::new_v4().simple());
    let app_for_connect = application_name.clone();
    let commit_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |connection, _meta| {
            let application_name = app_for_connect.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('application_name', $1, false)")
                    .bind(application_name)
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("connect tagged commit pool");
    let commit_storage = PostgresStorage::from_pool_with(
        commit_pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::HostProvisioned,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("open tagged commit storage");
    let commit_target = commit_storage.session_store("commit-delete-target");
    let commit_task =
        tokio::spawn(async move { commit_target.commit_runtime_state(target_commit).await });

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                !commit_task.is_finished(),
                "checkpoint publication completed before the delete released its component row"
            );
            let waiting_on_lock = sqlx::query_scalar::<_, bool>(
                "SELECT COALESCE(wait_event_type = 'Lock', FALSE)
                 FROM pg_stat_activity
                 WHERE application_name = $1 AND state = 'active'",
            )
            .bind(&application_name)
            .fetch_optional(storage.pool())
            .await
            .expect("observe tagged commit activity")
            .unwrap_or(false);
            if waiting_on_lock {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("checkpoint publication never reached the component row lock");

    sqlx::query("DELETE FROM lash_sessions WHERE session_id = 'commit-delete-victim'")
        .execute(&mut *deleting)
        .await
        .expect("sever victim head inside controlled delete");
    sqlx::query("DELETE FROM lash_blobs WHERE hash = $1")
        .bind(victim_receipt.checkpoint_ref.as_str())
        .execute(&mut *deleting)
        .await
        .expect("delete victim checkpoint root");
    sqlx::query("DELETE FROM lash_blobs WHERE hash = $1")
        .bind(shared.blob_ref.as_str())
        .execute(&mut *deleting)
        .await
        .expect("delete shared component");
    deleting
        .commit()
        .await
        .expect("commit controlled component delete");

    let error = tokio::time::timeout(Duration::from_secs(10), commit_task)
        .await
        .expect("blocked checkpoint publication did not resume")
        .expect("join checkpoint publication")
        .expect_err("publication must refuse a component deleted before its edge");
    assert!(
        matches!(
            error,
            StoreError::CheckpointComponentMissing { ref blob_ref, .. }
                if blob_ref == &shared.blob_ref
        ),
        "the losing commit must report the exact missing component: {error}"
    );
    commit_pool.close().await;
}

async fn reset(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list lash tables for commit-vs-delete reset");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .expect("reset commit-vs-delete fixture");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(storage.pool())
    .await
    .expect("reset process change clock");
}

fn request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}
