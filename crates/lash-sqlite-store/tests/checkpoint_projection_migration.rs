use std::sync::Arc;

use lash_core::{
    CheckpointComponentDescriptor, HydratedCheckpointComponent, RuntimeCommit, RuntimeSessionState,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory,
};
use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

const SOURCE_SESSION: &str = "legacy-projection-source";
const LEGACY_FORK: &str = "legacy-projection-fork";
const POST_MIGRATION_SIBLING: &str = "post-migration-projection-sibling";
const SHARED_COMPONENT: &str = "law/legacy-shared-component";
const REGENERATE_ENV: &str = "LASH_REGENERATE_DURABLE_READ_FIXTURES";

#[tokio::test]
async fn sqlite_37_to_38_backfill_preserves_legacy_fork_components_when_new_sibling_is_deleted() {
    let dir = tempfile::tempdir().expect("SQLite projection migration tempdir");
    let path = dir.path().join("durable-core.db");
    let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()));
    let (leaf_node_id, legacy_checkpoint_ref, shared) = seed_legacy_fork(&factory).await;
    drop(factory);

    let connection = rusqlite::Connection::open(&path).expect("open legacy SQLite catalog");
    connection
        .execute_batch(
            "DROP TABLE checkpoint_blob_refs;
             DROP INDEX idx_session_head_checkpoint_ref;
             DROP INDEX idx_node_anchors_checkpoint_ref;",
        )
        .expect("rewind SQLite catalog before exact-edge projection");
    connection
        .pragma_update(None, "user_version", 37)
        .expect("stamp SQLite durable-core 37");
    drop(connection);

    // Opening is the migration boundary. The projection and version stamp must
    // land together before any post-migration writer can publish a new edge.
    drop(Store::open(&path).await.expect("migrate SQLite 37 to 38"));
    let connection = rusqlite::Connection::open(&path).expect("inspect migrated SQLite catalog");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read migrated SQLite version"),
        38
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM checkpoint_blob_refs
                 WHERE checkpoint_ref = ?1 AND blob_ref = ?2",
                rusqlite::params![legacy_checkpoint_ref.as_str(), shared.blob_ref.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("read SQLite backfilled edge"),
        1,
        "the legacy rooted manifest must be projected before version 38 is visible"
    );
    drop(connection);

    let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()));
    publish_and_delete_post_migration_sibling(&factory, &leaf_node_id, &shared).await;
    let connection = rusqlite::Connection::open(&path).expect("probe retained SQLite component");
    assert!(
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)",
                rusqlite::params![shared.blob_ref.as_str()],
                |row| row.get::<_, bool>(0),
            )
            .expect("read retained SQLite component"),
        "deleting the post-migration sibling must not delete the legacy fork's component"
    );
    drop(connection);
    let legacy = factory
        .open_existing_store(&request(LEGACY_FORK))
        .await
        .expect("open legacy SQLite fork after sibling delete")
        .expect("legacy SQLite fork survives");
    assert!(
        lash_core::store::load_persisted_session_state(legacy.as_ref())
            .await
            .expect("hydrate legacy SQLite fork after sibling delete")
            .is_some()
    );
}

#[tokio::test]
#[ignore = "rewrites the committed component-v1 refusal fixture"]
async fn regenerate_sqlite_component_v1_refusal_projection() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge replacing the committed SQLite fixture"
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/checkpoint-component-v1-refusal/sqlite/durable-core.db");
    let connection = rusqlite::Connection::open(&path).expect("open component-v1 fixture");
    connection
        .execute_batch(
            "DROP TABLE checkpoint_blob_refs;
             DROP INDEX idx_session_head_checkpoint_ref;
             DROP INDEX idx_node_anchors_checkpoint_ref;",
        )
        .expect("rewind component-v1 fixture projection");
    connection
        .pragma_update(None, "user_version", 37)
        .expect("rewind component-v1 fixture stamp");
    drop(connection);

    drop(
        Store::open(&path)
            .await
            .expect("migrate component-v1 fixture projection"),
    );
    let connection = rusqlite::Connection::open(&path).expect("checkpoint migrated fixture");
    let busy: i64 = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .expect("checkpoint migrated component-v1 fixture");
    assert_eq!(busy, 0);
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
            .expect("read component-v1 fixture version"),
        38
    );
    let (heads, edges): (i64, i64) = connection
        .query_row(
            "SELECT (SELECT count(*) FROM session_head WHERE checkpoint_ref IS NOT NULL),
                    (SELECT count(*) FROM checkpoint_blob_refs)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read component-v1 fixture projection counts");
    assert!(heads > 0, "component-v1 fixture must retain its live head");
    assert!(
        edges > 0,
        "every live fixture head must carry projected edges"
    );
}

async fn seed_legacy_fork(
    factory: &Arc<SqliteSessionStoreFactory>,
) -> (String, lash_core::BlobRef, CheckpointComponentDescriptor) {
    let store = factory
        .create_store(&request(SOURCE_SESSION))
        .await
        .expect("create legacy SQLite source");
    let mut state = RuntimeSessionState {
        session_id: SOURCE_SESSION.to_string(),
        ..RuntimeSessionState::new(request(SOURCE_SESSION).policy)
    };
    state.ensure_agent_frame_initialized();
    let leaf_node_id = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("legacy SQLite source leaf");
    let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
    commit.checkpoint.components.insert(
        SHARED_COMPONENT.to_string(),
        HydratedCheckpointComponent::changed(b"legacy-shared-component".to_vec()),
    );
    let receipt = store
        .commit_runtime_state(commit)
        .await
        .expect("commit legacy SQLite checkpoint");
    let shared = receipt.manifest.components[SHARED_COMPONENT].clone();
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            session_id: LEGACY_FORK.to_string(),
            node_id: leaf_node_id.clone(),
            relation: SessionRelation::Root,
            policy: request(LEGACY_FORK).policy,
        })
        .await
        .expect("fork legacy SQLite checkpoint");
    (leaf_node_id, receipt.checkpoint_ref, shared)
}

async fn publish_and_delete_post_migration_sibling(
    factory: &Arc<SqliteSessionStoreFactory>,
    leaf_node_id: &str,
    shared: &CheckpointComponentDescriptor,
) {
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            session_id: POST_MIGRATION_SIBLING.to_string(),
            node_id: leaf_node_id.to_string(),
            relation: SessionRelation::Root,
            policy: request(POST_MIGRATION_SIBLING).policy,
        })
        .await
        .expect("fork post-migration SQLite sibling");
    let sibling = factory
        .open_existing_store(&request(POST_MIGRATION_SIBLING))
        .await
        .expect("open post-migration SQLite sibling")
        .expect("post-migration SQLite sibling exists");
    let state = lash_core::store::load_persisted_session_state(sibling.as_ref())
        .await
        .expect("load post-migration SQLite sibling")
        .expect("post-migration SQLite sibling state");
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
        .expect("publish post-migration SQLite sibling checkpoint");
    factory
        .delete_session(POST_MIGRATION_SIBLING)
        .await
        .expect("delete post-migration SQLite sibling");
}

fn request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}
