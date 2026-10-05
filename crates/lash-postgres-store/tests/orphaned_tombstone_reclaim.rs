//! Postgres proof that a session delete reclaims tombstoned graph nodes whose
//! owning session was deleted earlier.
//!
//! This lives in its own test target rather than the conformance suite: the
//! reclaim law is asserted through raw catalog reads, and the conformance file is
//! at its line budget.

use lash_core_execution::SessionCatalogStore;
use lash_postgres_store::{PostgresStorage, testing::IsolatedDatabase};
use lash_sansio::SessionId;

use crate::support::database_url;

async fn storage() -> Option<(IsolatedDatabase, PostgresStorage)> {
    let url = database_url()?;
    let database = IsolatedDatabase::create(&url).await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("connect postgres");
    Some((database, storage))
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
    let Some((_database, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres orphaned-tombstone reclaim conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let pool = storage.pool().clone();
    let factory = storage.session_store_factory();
    let policy = lash_core_execution::SessionPolicy::new(
        lash_core_execution::TurnBudget::Unbounded,
        lash_core_execution::MaxToolCalls::new(1024),
    );

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
        factory: &(impl SessionCatalogStore + lash_core_execution::SessionCommitStore),
        session_id: &SessionId,
        policy: &lash_core_execution::SessionPolicy,
    ) -> String {
        factory
            .admit_session(&lash_core_execution::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash_core_execution::SessionRelation::Root,
                config: policy.clone().into(),
                head: lash_core_execution::SessionCreationHead::Config,
            })
            .await
            .expect("create store");
        let mut state = lash_core_execution::RuntimeSessionState {
            session_id: SessionId::fixture(session_id.to_string()),
            ..lash_core_execution::RuntimeSessionState::new(policy.clone())
        };
        state.ensure_agent_frame_initialized();
        let leaf = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("root leaf node id");
        factory
            .commit_runtime_state(
                lash_core_execution::store::RuntimeCommit::persisted_state_for_test(&state),
            )
            .await
            .expect("commit root node");
        leaf.to_string()
    }

    // Flow 1: a pin held at the owning session's delete. The pin is deleted
    // with its session, so the delete reclaims the pinned leaf itself.
    let owner_leaf =
        commit_single_root_node(&factory, &SessionId::from("orphan-owner"), &policy).await;
    factory
        .pin(
            &SessionId::from("orphan-owner"),
            &lash_core_execution::Target::Revision(1),
        )
        .await
        .expect("pin the owner's committed revision");
    factory
        .delete_session(&SessionId::from("orphan-owner"))
        .await
        .expect("delete owner session");
    assert!(
        !resident_node_ids(&pool).await.contains(&owner_leaf),
        "a pin must not keep its deleted session's leaf"
    );

    // Flow 2: fork ancestry tombstoned only at the child's delete, after its
    // owner was already deleted.
    let parent_leaf =
        commit_single_root_node(&factory, &SessionId::from("orphan-fork-parent"), &policy).await;
    factory
        .fork_session(&lash_core_execution::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("orphan-fork-child"),
            source_session_id: SessionId::from("orphan-fork-parent"),
            head_revision: 1,
            relation: lash_core_execution::SessionRelation::Root,
            config: policy.clone().into(),
        })
        .await
        .expect("fork at the parent's live tip");
    {
        let child = lash_core_execution::store::SessionStore::new(
            std::sync::Arc::new(factory.clone()),
            SessionId::from("orphan-fork-child"),
        )
        .expect("child session view");
        let mut child_state = lash_core_execution::store::load_session_window_state(
            &child,
            lash_core_execution::store::WindowSelector::Current,
        )
        .await
        .expect("load child state")
        .expect("child state exists")
        .state;
        let parent_node_id = child_state.session_graph.leaf_node_id.clone();
        child_state
            .session_graph
            .apply_append(&lash_core_execution::store::GraphAppend::Extend {
                nodes: vec![lash_core_execution::SessionNodeRecord {
                    node_id: lash_core::NodeId::from("orphan-fork-child-node"),
                    parent_node_id,
                    timestamp: "2026-08-17T00:00:00.000000000Z"
                        .parse()
                        .expect("canonical node timestamp"),
                    payload: lash_core_execution::SessionNodePayload::Event {
                        event: lash_core_execution::SessionHistoryRecord::Protocol(
                            lash_core_execution::ProtocolEvent::typed(
                                "orphan-fork-child-event",
                                serde_json::json!({ "content": "child node" }),
                            )
                            .expect("typed child event"),
                        ),
                    },
                }],
            })
            .expect("append child node");
        child
            .commit_runtime_state(
                lash_core_execution::store::RuntimeCommit::persisted_state_for_test(&child_state),
            )
            .await
            .expect("advance forked child");
    }
    factory
        .delete_session(&SessionId::from("orphan-fork-parent"))
        .await
        .expect("delete parent session");
    assert!(
        resident_node_ids(&pool).await.contains(&parent_leaf),
        "the parent's node survives its own delete while the fork child hangs off it"
    );

    factory
        .delete_session(&SessionId::from("orphan-fork-child"))
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
