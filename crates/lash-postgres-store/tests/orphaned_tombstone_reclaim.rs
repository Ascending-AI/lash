//! Postgres proof that a session delete reclaims tombstoned graph nodes whose
//! owning session was deleted earlier.
//!
//! This lives in its own test target rather than the conformance suite: the
//! reclaim law is asserted through raw catalog reads, and the conformance file is
//! at its line budget.

use lash_core::SessionStoreFactory;
use lash_postgres_store::PostgresStorage;

#[allow(dead_code)]
mod support;

use support::{SharedDatabaseLock, database_url};

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
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
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

/// Both orphaning flows for tombstoned graph nodes, on the Postgres backend:
/// unpinning a pinned leaf after its owning session was deleted, and fork
/// ancestry only tombstoned when the child is deleted. In both cases the owning
/// session id is permanently unbindable, so no session-scoped vacuum can reach
/// the row and `delete_session` must reclaim it.
///
/// Store handles are dropped before their session's delete on purpose: a live
/// handle can still vacuum its own session and would mask the leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_delete_reclaims_tombstones_orphaned_by_earlier_delete_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres orphaned-tombstone reclaim conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let pool = storage.pool().clone();
    let factory = storage.session_store_factory();
    let policy = lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded);

    // Reads hide tombstones, so only raw SQL can tell a reclaimed row from a
    // hidden one.
    async fn resident_node_ids(pool: &sqlx::PgPool) -> Vec<String> {
        sqlx::query_scalar::<_, String>("SELECT node_id FROM lash_graph_nodes ORDER BY node_id")
            .fetch_all(pool)
            .await
            .expect("probe resident graph nodes")
    }

    async fn resident_tombstoned_node_ids(pool: &sqlx::PgPool) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT node_id FROM lash_graph_nodes WHERE tombstoned ORDER BY node_id",
        )
        .fetch_all(pool)
        .await
        .expect("probe tombstoned graph nodes")
    }

    async fn commit_single_root_node(
        factory: &impl SessionStoreFactory,
        session_id: &str,
        policy: &lash_core::SessionPolicy,
    ) -> String {
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                session_id: session_id.to_string(),
                relation: lash_core::SessionRelation::Root,
                policy: policy.clone(),
            })
            .await
            .expect("create store");
        let mut state = lash_core::RuntimeSessionState {
            session_id: session_id.to_string(),
            ..lash_core::RuntimeSessionState::new(policy.clone())
        };
        state.ensure_agent_frame_initialized();
        let leaf = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("root leaf node id");
        store
            .commit_runtime_state(lash_core::store::RuntimeCommit::persisted_state_for_test(
                &state,
                &[],
            ))
            .await
            .expect("commit root node");
        leaf
    }

    // Flow 1: unpin after the owning session's delete.
    let owner_leaf = commit_single_root_node(&factory, "orphan-owner", &policy).await;
    factory.pin(&owner_leaf).await.expect("pin owner leaf");
    factory
        .delete_session("orphan-owner")
        .await
        .expect("delete owner session");
    factory
        .unpin(&owner_leaf)
        .await
        .expect("unpin after owner delete");
    assert_eq!(
        resident_tombstoned_node_ids(&pool).await,
        vec![owner_leaf.clone()],
        "the unpin must tombstone the deleted owner's leaf"
    );

    // Flow 2: fork ancestry tombstoned only at the child's delete, after its
    // owner was already deleted. The same delete also drains flow 1's orphan.
    let parent_leaf = commit_single_root_node(&factory, "orphan-fork-parent", &policy).await;
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            session_id: "orphan-fork-child".to_string(),
            node_id: parent_leaf.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("fork at the parent's live tip");
    {
        let child = factory
            .open_existing_store(&lash_core::SessionStoreCreateRequest {
                session_id: "orphan-fork-child".to_string(),
                relation: lash_core::SessionRelation::Root,
                policy: policy.clone(),
            })
            .await
            .expect("open forked child")
            .expect("forked child exists");
        let mut child_state = lash_core::store::load_persisted_session_state(child.as_ref())
            .await
            .expect("load child state")
            .expect("child state exists");
        let parent_node_id = child_state.session_graph.leaf_node_id.clone();
        child_state
            .session_graph
            .push_node_record(lash_core::SessionNodeRecord {
                node_id: "orphan-fork-child-node".to_string(),
                parent_node_id,
                timestamp: "2026-08-17T00:00:00Z".to_string(),
                payload: lash_core::SessionNodePayload::Event {
                    event: lash_core::SessionHistoryRecord::Protocol(
                        lash_core::ProtocolEvent::typed(
                            "orphan-fork-child-event",
                            serde_json::json!({ "content": "child node" }),
                        )
                        .expect("typed child event"),
                    ),
                },
            });
        child_state
            .session_graph
            .set_leaf_node_id(Some("orphan-fork-child-node".to_string()));
        child
            .commit_runtime_state(lash_core::store::RuntimeCommit::persisted_state_for_test(
                &child_state,
                &[],
            ))
            .await
            .expect("advance forked child");
    }
    factory
        .delete_session("orphan-fork-parent")
        .await
        .expect("delete parent session");
    assert!(
        resident_node_ids(&pool).await.contains(&parent_leaf),
        "the parent's node survives its own delete while the fork child hangs off it"
    );

    factory
        .delete_session("orphan-fork-child")
        .await
        .expect("delete forked child session");

    assert!(
        resident_tombstoned_node_ids(&pool).await.is_empty(),
        "a delete must reclaim tombstones owned by already-deleted sessions"
    );
    let resident = resident_node_ids(&pool).await;
    assert!(
        resident.is_empty(),
        "every orphaned row must be physically gone, not just hidden, got {resident:?}"
    );
}
