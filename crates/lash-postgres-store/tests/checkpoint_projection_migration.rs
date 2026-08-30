use std::sync::Arc;

use lash_core::{
    CheckpointComponentDescriptor, HydratedCheckpointComponent, RuntimeCommit, RuntimeSessionState,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory,
};
use lash_postgres_store::{PostgresStorage, PostgresStoreConfig, SchemaCheck, SchemaProvisioning};

#[allow(dead_code)]
mod support;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::{REWIND_PAST_56_ARTIFACTS, REWIND_PENDING_OBSERVER_INTENT_ARTIFACTS, ScratchSchema};
use support::database_url;

const SOURCE_SESSION: &str = "legacy-projection-source";
const LEGACY_FORK: &str = "legacy-projection-fork";
const POST_MIGRATION_SIBLING: &str = "post-migration-projection-sibling";
const SHARED_COMPONENT: &str = "law/legacy-shared-component";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_56_to_57_backfill_preserves_legacy_fork_components_when_new_sibling_is_deleted() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping Postgres seeded legacy backfill law: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let seeded = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("open current Postgres schema for legacy seed");
    let factory = Arc::new(seeded.session_store_factory());
    let (leaf_node_id, legacy_checkpoint_ref, shared) = seed_legacy_fork(&factory).await;
    drop(factory);
    drop(seeded);

    scratch
        .apply(&format!(
            "{REWIND_PAST_56_ARTIFACTS}
             {REWIND_PENDING_OBSERVER_INTENT_ARTIFACTS}
             UPDATE lash_schema_versions
             SET version = 56
             WHERE component = 'lash-postgres-store'"
        ))
        .await;
    let migrated = PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::LashManaged,
            schema_check: SchemaCheck::Enforce,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("migrate seeded Postgres component 56 to 65");
    let foreign_key_actions = sqlx::query_as::<_, (String, String)>(
        "SELECT conname, confdeltype::TEXT
         FROM pg_catalog.pg_constraint
         WHERE conrelid = 'lash_checkpoint_blob_refs'::regclass
           AND contype = 'f'
         ORDER BY conname",
    )
    .fetch_all(&scratch.pool)
    .await
    .expect("read migrated checkpoint projection foreign keys");
    assert_eq!(
        foreign_key_actions,
        vec![
            (
                "lash_checkpoint_blob_refs_blob_ref_fkey".to_string(),
                "a".to_string(),
            ),
            (
                "lash_checkpoint_blob_refs_checkpoint_ref_fkey".to_string(),
                "c".to_string(),
            ),
        ],
        "component deletion must be restrictive while root deletion owns the only cascade"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM lash_checkpoint_blob_refs
             WHERE checkpoint_ref = $1 AND blob_ref = $2",
        )
        .bind(legacy_checkpoint_ref.as_str())
        .bind(shared.blob_ref.as_str())
        .fetch_one(&scratch.pool)
        .await
        .expect("read Postgres backfilled edge"),
        1,
        "the legacy rooted manifest must be projected before component 58 is visible"
    );

    let factory = Arc::new(migrated.session_store_factory());
    publish_and_delete_post_migration_sibling(&factory, &leaf_node_id, &shared).await;
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM lash_blobs WHERE hash = $1)",)
            .bind(shared.blob_ref.as_str())
            .fetch_one(&scratch.pool)
            .await
            .expect("read retained Postgres component"),
        "deleting the post-migration sibling must not delete the legacy fork's component"
    );
    let legacy = factory
        .open_existing_store(&request(LEGACY_FORK))
        .await
        .expect("open legacy Postgres fork after sibling delete")
        .expect("legacy Postgres fork survives");
    assert!(
        lash_core::store::load_persisted_session_state(legacy.as_ref())
            .await
            .expect("hydrate legacy Postgres fork after sibling delete")
            .is_some()
    );
    drop(legacy);
    drop(factory);
    drop(migrated);
    scratch.cleanup().await;
}

async fn seed_legacy_fork(
    factory: &Arc<lash_postgres_store::PostgresSessionStoreFactory>,
) -> (String, lash_core::BlobRef, CheckpointComponentDescriptor) {
    let store = factory
        .create_store(&request(SOURCE_SESSION))
        .await
        .expect("create legacy Postgres source");
    let mut state = RuntimeSessionState {
        session_id: SOURCE_SESSION.to_string(),
        ..RuntimeSessionState::new(request(SOURCE_SESSION).policy)
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("legacy Postgres source leaf");
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        SHARED_COMPONENT.to_string(),
        HydratedCheckpointComponent::changed(b"legacy-shared-component".to_vec()),
    );
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("commit legacy Postgres checkpoint");
    let shared = receipt.manifest.components[SHARED_COMPONENT].clone();
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: LEGACY_FORK.to_string(),
            node_id: leaf_node_id.clone(),
            relation: SessionRelation::Root,
            policy: request(LEGACY_FORK).policy,
        })
        .await
        .expect("fork legacy Postgres checkpoint");
    (leaf_node_id, receipt.checkpoint_ref, shared)
}

async fn publish_and_delete_post_migration_sibling(
    factory: &Arc<lash_postgres_store::PostgresSessionStoreFactory>,
    leaf_node_id: &str,
    shared: &CheckpointComponentDescriptor,
) {
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: POST_MIGRATION_SIBLING.to_string(),
            node_id: leaf_node_id.to_string(),
            relation: SessionRelation::Root,
            policy: request(POST_MIGRATION_SIBLING).policy,
        })
        .await
        .expect("fork post-migration Postgres sibling");
    let sibling = factory
        .open_existing_store(&request(POST_MIGRATION_SIBLING))
        .await
        .expect("open post-migration Postgres sibling")
        .expect("post-migration Postgres sibling exists");
    let state = lash_core::store::load_persisted_session_state(sibling.as_ref())
        .await
        .expect("load post-migration Postgres sibling")
        .expect("post-migration Postgres sibling state");
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        SHARED_COMPONENT.to_string(),
        HydratedCheckpointComponent::unchanged(shared),
    );
    commit.checkpoint.components.insert(
        "law/post-migration-only".to_string(),
        HydratedCheckpointComponent::changed(b"post-migration-only".to_vec()),
    );
    sibling
        .commit_runtime_state(commit)
        .await
        .expect("publish post-migration Postgres sibling checkpoint");
    factory
        .delete_session(POST_MIGRATION_SIBLING)
        .await
        .expect("delete post-migration Postgres sibling");
}

fn request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}
