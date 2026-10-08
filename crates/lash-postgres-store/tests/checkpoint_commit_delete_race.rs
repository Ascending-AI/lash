//! Deterministic transaction interleaving for checkpoint publication versus
//! component deletion. The test observes PostgreSQL's lock wait directly; no
//! timing sleep decides which transaction won.

use lash_sansio::SessionId;
use std::time::Duration;

use lash_core_execution::{
    HydratedCheckpointComponent, RuntimeCommit, RuntimeSessionState, SessionCatalogStore,
    SessionCommitStore, SessionCreationHead, SessionRelation, SessionStoreCreateRequest,
    StoreError,
};
use lash_postgres_store::{SchemaCheck, testing::IsolatedDatabase};
use sqlx::postgres::PgPoolOptions;

use crate::support::database_url;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_commit_waits_for_delete_then_refuses_missing_component_when_configured() {
    commit_waits_for_delete_then_refuses(ReuseBranch::UnchangedComponent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_commit_with_changed_component_waits_for_delete_then_refuses_missing_component_when_configured()
 {
    commit_waits_for_delete_then_refuses(ReuseBranch::ChangedComponent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_commit_with_existing_root_waits_for_delete_then_refuses_missing_root_when_configured()
 {
    commit_waits_for_delete_then_refuses(ReuseBranch::ExistingRoot).await;
}

#[derive(Clone, Copy)]
enum ReuseBranch {
    UnchangedComponent,
    ChangedComponent,
    ExistingRoot,
}

impl ReuseBranch {
    const fn label(self) -> &'static str {
        match self {
            Self::UnchangedComponent => "unchanged",
            Self::ChangedComponent => "changed",
            Self::ExistingRoot => "root",
        }
    }
}

async fn commit_waits_for_delete_then_refuses(branch: ReuseBranch) {
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres commit-vs-delete law: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect Postgres commit-vs-delete fixture");
    let factory = storage.session_store_factory();

    factory
        .admit_session(&request(&SessionId::from("commit-delete-victim")))
        .await
        .expect("create delete victim");
    let mut victim_state = RuntimeSessionState {
        session_id: SessionId::from("commit-delete-victim"),
        ..RuntimeSessionState::new(
            request(&SessionId::from("commit-delete-victim"))
                .config
                .session_policy(),
        )
    };
    victim_state.ensure_agent_frame_initialized();
    let mut victim_commit = RuntimeCommit::persisted_state_for_test(&victim_state);
    victim_commit.checkpoint.components.insert(
        "law/commit-delete-shared".to_string(),
        HydratedCheckpointComponent::changed(b"commit-delete-shared".to_vec()),
    );
    let victim_receipt = factory
        .commit_runtime_state(victim_commit)
        .await
        .expect("commit delete victim checkpoint");
    let shared = victim_receipt.manifest.components["law/commit-delete-shared"].clone();

    factory
        .admit_session(&request(&SessionId::from("commit-delete-target")))
        .await
        .expect("create commit target");
    let mut target_state = RuntimeSessionState {
        session_id: SessionId::from("commit-delete-target"),
        ..RuntimeSessionState::new(
            request(&SessionId::from("commit-delete-target"))
                .config
                .session_policy(),
        )
    };
    target_state.ensure_agent_frame_initialized();
    let mut target_commit = RuntimeCommit::persisted_state_for_test(&target_state);
    target_commit.checkpoint.components.insert(
        "law/commit-delete-shared".to_string(),
        match branch {
            ReuseBranch::ChangedComponent => {
                HydratedCheckpointComponent::changed(b"commit-delete-shared".to_vec())
            }
            ReuseBranch::UnchangedComponent | ReuseBranch::ExistingRoot => {
                HydratedCheckpointComponent::unchanged(&shared)
            }
        },
    );
    if !matches!(branch, ReuseBranch::ExistingRoot) {
        target_commit.checkpoint.components.insert(
            "law/commit-delete-target-only".to_string(),
            HydratedCheckpointComponent::changed(b"commit-delete-target-only".to_vec()),
        );
    }

    let held_ref = match branch {
        ReuseBranch::UnchangedComponent | ReuseBranch::ChangedComponent => shared.blob_ref.clone(),
        ReuseBranch::ExistingRoot => victim_receipt.checkpoint_ref.clone(),
    };

    let mut deleting = storage
        .pool()
        .begin()
        .await
        .expect("begin controlled delete transaction");
    sqlx::query("SELECT hash FROM lash_blobs WHERE hash = $1 FOR UPDATE")
        .bind(held_ref.as_str())
        .fetch_one(&mut *deleting)
        .await
        .expect("lock reused checkpoint blob for delete");

    let application_name = format!(
        "lash-cd-{}-{}",
        branch.label(),
        uuid::Uuid::new_v4().simple()
    );
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
        .connect(database.url())
        .await
        .expect("connect tagged commit pool");
    let commit_storage = lash_postgres_store::testing::from_pool(
        commit_pool.clone(),
        &lash_postgres_store::PostgresHostConfig {
            schema_check: SchemaCheck::Enforce,
            ..lash_postgres_store::PostgresHostConfig::default()
        },
    )
    .await
    .expect("open tagged commit storage");
    let commit_target = commit_storage.store();
    let commit_task =
        tokio::spawn(async move { commit_target.commit_runtime_state(target_commit).await });

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                !commit_task.is_finished(),
                "checkpoint publication completed before the delete released its blob row"
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
    .expect("checkpoint publication never reached the reused blob row lock");

    sqlx::query("DELETE FROM lash_session_head WHERE session_id = 'commit-delete-victim'")
        .execute(&mut *deleting)
        .await
        .expect("sever victim head inside controlled delete");
    sqlx::query("DELETE FROM lash_blobs WHERE hash = $1")
        .bind(victim_receipt.checkpoint_ref.as_str())
        .execute(&mut *deleting)
        .await
        .expect("delete victim checkpoint root");
    if !matches!(branch, ReuseBranch::ExistingRoot) {
        sqlx::query("DELETE FROM lash_blobs WHERE hash = $1")
            .bind(shared.blob_ref.as_str())
            .execute(&mut *deleting)
            .await
            .expect("delete shared component");
    }
    deleting
        .commit()
        .await
        .expect("commit controlled component delete");

    let error = tokio::time::timeout(Duration::from_secs(10), commit_task)
        .await
        .expect("blocked checkpoint publication did not resume")
        .expect("join checkpoint publication")
        .expect_err("publication must refuse a component deleted before its edge");
    match (branch, error) {
        (
            ReuseBranch::UnchangedComponent | ReuseBranch::ChangedComponent,
            StoreError::CheckpointComponentMissing { key, blob_ref },
        ) => {
            assert_eq!(key, "law/commit-delete-shared");
            assert_eq!(blob_ref, shared.blob_ref);
        }
        (ReuseBranch::ExistingRoot, StoreError::CheckpointRootMissing { blob_ref }) => {
            assert_eq!(blob_ref, victim_receipt.checkpoint_ref);
        }
        (_, other) => panic!("the losing commit returned the wrong typed error: {other}"),
    }
    commit_pool.close().await;
}

fn request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: session_id.clone(),
        relation: SessionRelation::Root,
        config: lash_core_execution::PersistedSessionConfig::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
            lash_core_execution::SessionToolAccess::ambient(),
        ),
        head: SessionCreationHead::Config,
    }
}
