// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_core_execution::store::GraphAppend;
use lash_core_execution::store::WindowSelector;
use lash_core_execution::{
    FleetFormatStore, ModelSpec, PluginState, RuntimeCommit, RuntimeSessionState,
    SessionCatalogStore, SessionCommitStore, SessionHistoryStore, SessionLookup, SessionPolicy,
    SessionStoreCreateRequest, StoreError, StoreMaintenance, TokenLedgerEntry, TokenUsage,
    ToolState, TurnInputStore,
};
use lash_sansio::SessionId;
use lash_sqlite_store::{BlobArtifactDescriptor, SqliteStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn catalog_uri(root: &Path) -> PathBuf {
    root.join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name())
}

async fn admit_store(
    catalog: &Arc<SqliteStore>,
    request: &SessionStoreCreateRequest,
) -> Result<Arc<SqliteStore>, StoreError> {
    catalog.admit_session(request).await?;
    Ok(Arc::clone(catalog))
}

fn model_spec(id: &str) -> ModelSpec {
    ModelSpec::builder(id)
        .context_window_tokens(200_000)
        .build()
        .expect("valid test model spec")
}

fn persisted_tool_state_at_generation(generation: u64) -> ToolState {
    serde_json::from_value(serde_json::json!({
        "generation": generation,
        "tools": {}
    }))
    .expect("deserialize persisted tool state")
}

async fn factory_state(
    store: &Arc<SqliteStore>,
    session_id: &SessionId,
    head_revision: u64,
) -> RuntimeSessionState {
    store
        .load_session_meta(session_id)
        .await
        .expect("load factory session metadata")
        .expect("factory session metadata");
    RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        head_revision,
        ..RuntimeSessionState::new(SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    }
}

#[tokio::test]
async fn gc_unreachable_keeps_rooted_checkpoint_blobs() {
    let store = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("memory backend")
        .open_store()
        .await
        .expect("store");
    let tool_state = persisted_tool_state_at_generation(7);
    let plugin_state = PluginState {
        plugins: Default::default(),
    };
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index: 1,
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    state.set_tool_state_snapshot(Some(tool_state));
    state.set_plugin_state(Some(plugin_state));
    store
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: state.session_id.clone(),
            relation: lash_core_execution::SessionRelation::Root,
            policy: state.policy.clone(),
        })
        .await
        .expect("bind session to store");
    state.ensure_agent_frame_initialized();
    let stored = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session state");
    let orphan = store
        .put_unrooted_artifact_blob_for_testing(
            BlobArtifactDescriptor::checkpoint_component(),
            b"orphan",
        )
        .await
        .expect("store orphan");

    let report = store.gc_unreachable().await.expect("gc sweeps");

    assert_eq!(report.deleted_blob_count, 1);
    let checkpoint = store
        .get_checkpoint(&stored.checkpoint_ref)
        .await
        .expect("read checkpoint")
        .expect("checkpoint manifest");
    let dynamic_ref = checkpoint
        .component_ref(lash_core_execution::store::TOOL_STATE_CHECKPOINT_COMPONENT)
        .expect("dynamic state ref")
        .clone();
    let plugin_ref = checkpoint
        .component_ref(lash_core_execution::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        .expect("plugin snapshot ref")
        .clone();
    assert!(
        store
            .get_blob(&stored.checkpoint_ref)
            .await
            .expect("read checkpoint blob")
            .is_some()
    );
    assert!(
        store
            .get_blob(&dynamic_ref)
            .await
            .expect("read dynamic blob")
            .is_some()
    );
    assert!(
        store
            .get_blob(&plugin_ref)
            .await
            .expect("read plugin blob")
            .is_some()
    );
    assert!(
        store
            .get_blob(&orphan)
            .await
            .expect("read orphan blob")
            .is_none()
    );
}

#[tokio::test]
async fn sqlite_catalog_indexes_usage_by_session() {
    let root = unique_temp_dir("usage-index");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));
    admit_store(
        &factory,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("usage-index"),
            relation: lash_core_execution::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
        },
    )
    .await
    .expect("create store");
    let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
    let indexed: bool = conn
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_index_list('usage_deltas')
                 WHERE name = 'idx_usage_deltas_session_seq'
             )",
            [],
            |row| row.get(0),
        )
        .expect("query usage indexes");
    assert!(
        indexed,
        "shared-catalog usage reads require a session index"
    );
}

#[tokio::test]
async fn sqlite_factory_creates_metadata_once_and_preserves_on_reopen() {
    let root = unique_temp_dir("metadata");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("chat/alpha"),
        relation: lash_core_execution::SessionRelation::Child {
            parent_session_id: SessionId::from("preserved-parent"),
            caused_by: None,
        },
        policy: SessionPolicy {
            model: model_spec("first-model"),
            ..SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded)
        },
    };

    let store = admit_store(&factory, &request).await.expect("create store");
    let meta = store
        .load_session_meta(&request.session_id)
        .await
        .expect("load meta")
        .expect("meta");
    assert_eq!(meta.session_id, "chat/alpha");
    assert_eq!(meta.parent_session_id(), Some("preserved-parent"));

    let reopened = admit_store(
        &factory,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            relation: lash_core_execution::SessionRelation::Root,
            policy: SessionPolicy {
                model: model_spec("second-model"),
                ..SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded)
            },
            ..request
        },
    )
    .await
    .expect("reopen store");
    let reopened_meta = reopened
        .load_session_meta(&SessionId::from("chat/alpha"))
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta.parent_session_id(), Some("preserved-parent"));
}

#[tokio::test]
async fn sqlite_factory_delete_session_removes_only_the_selected_session() {
    let root = unique_temp_dir("delete-session");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));
    let request = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash_core_execution::SessionRelation::Root,
        policy: SessionPolicy {
            model: model_spec("model"),
            ..SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded)
        },
    };
    let deleted_store = admit_store(&factory, &request(&SessionId::from("delete/me")))
        .await
        .expect("create deleted session");
    admit_store(&factory, &request(&SessionId::from("keep/me")))
        .await
        .expect("create retained session");
    let mut deleted_state = factory_state(&deleted_store, &SessionId::from("delete/me"), 0).await;
    deleted_state.set_execution_state_snapshot(Some(vec![1, 2, 3].into()));
    deleted_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&deleted_state, &[]))
        .await
        .expect("commit deleted session checkpoint");
    {
        let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
        conn.execute(
            "INSERT INTO blobs (hash, content) VALUES ('host-artifact-blob', X'02')",
            [],
        )
        .expect("insert host artifact blob");
        conn.execute(
            "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
             VALUES ('lashlang_module', 'shared-module', 'host-artifact-blob')",
            [],
        )
        .expect("insert host artifact ref");
    }

    factory
        .delete_session(&SessionId::from("delete/me"))
        .await
        .expect("delete session");
    factory
        .delete_session(&SessionId::from("delete/me"))
        .await
        .expect("delete session again");

    assert!(
        root.join(lash_sqlite_store::SqliteDatabase::DurableCore.file_name())
            .exists()
    );
    assert!(
        factory
            .lookup_session(&SessionId::from("delete/me"))
            .await
            .expect("probe deleted session")
            == SessionLookup::Deleted
    );
    assert!(matches!(
        factory
            .lookup_session(&SessionId::from("keep/me"))
            .await
            .expect("probe retained session"),
        SessionLookup::Live(_)
    ));
    let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
    let host_ref_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_refs
             WHERE namespace = 'lashlang_module' AND artifact_ref = 'shared-module'",
            [],
            |row| row.get(0),
        )
        .expect("count host refs");
    assert_eq!(
        host_ref_count, 1,
        "factory artifact refs without session attribution remain host-owned"
    );
    let blob_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
        .expect("count retained blobs");
    assert_eq!(
        blob_count, 1,
        "deleting the session must reclaim its checkpoint tree"
    );
}

#[tokio::test]
async fn sqlite_catalog_partitions_derived_node_ids_by_session() {
    let root = unique_temp_dir("global-node-id");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));
    let store_for = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash_core_execution::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    let first = admit_store(&factory, &store_for(&SessionId::from("first")))
        .await
        .expect("first store");
    let second = admit_store(&factory, &store_for(&SessionId::from("second")))
        .await
        .expect("second store");
    let first_state = factory_state(&first, &SessionId::from("first"), 0).await;
    let second_state = factory_state(&second, &SessionId::from("second"), 0).await;
    let commit = |state: &RuntimeSessionState| {
        let frame_key = lash_core_execution::FrameKey::from_caller_material("shared-frame-key")
            .expect("non-empty frame material");
        let frame_node_id = lash_core_execution::facade_support::frame_node_id(
            &state.session_id,
            frame_key.as_str(),
        );
        let node = lash_core_execution::SessionNodeRecord {
            node_id: frame_node_id.to_string().into(),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: lash_core_execution::SessionNodePayload::FrameOpen {
                frame_key,
                reason: lash_core_execution::AgentFrameReason::initial(),
                assignment: lash_core_execution::AgentFrameAssignment::from_policy(
                    SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
                ),
                protocol_turn_options: Default::default(),
            },
        };
        let usage = TokenLedgerEntry {
            source: "session-partition-probe".to_string(),
            model: "test".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..Default::default()
            },
            usage_disposition: Default::default(),
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(state, &[usage]);
        commit.graph = GraphAppend::Extend {
            nodes: vec![node.clone()],
        };
        commit.current_frame_node_id = Some(frame_node_id);
        commit
    };

    first
        .commit_runtime_state(commit(&first_state))
        .await
        .expect("first node insert");
    second
        .commit_runtime_state(commit(&second_state))
        .await
        .expect("second session derives a distinct node id");

    let frame_key = lash_core_execution::FrameKey::from_caller_material("shared-frame-key")
        .expect("non-empty frame material");
    let first_node_id = lash_core_execution::facade_support::frame_node_id(
        &first_state.session_id,
        frame_key.as_str(),
    );
    let second_node_id = lash_core_execution::facade_support::frame_node_id(
        &second_state.session_id,
        frame_key.as_str(),
    );
    assert_ne!(first_node_id, second_node_id);
    assert!(
        first
            .contains_active_ancestor(&first_state.session_id, &first_node_id.to_string().into())
            .await
            .unwrap()
    );
    assert!(
        second
            .contains_active_ancestor(&second_state.session_id, &second_node_id.to_string().into())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sqlite_catalog_leaf_validation_is_session_scoped() {
    let root = unique_temp_dir("leaf-scope");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));
    let request = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash_core_execution::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    let first = admit_store(&factory, &request(&SessionId::from("leaf-a")))
        .await
        .expect("first store");
    let second = admit_store(&factory, &request(&SessionId::from("leaf-b")))
        .await
        .expect("second store");
    let first_state = factory_state(&first, &SessionId::from("leaf-a"), 0).await;
    let second_state = factory_state(&second, &SessionId::from("leaf-b"), 0).await;
    let frame_key = lash_core_execution::FrameKey::from_caller_material("leaf-a-node")
        .expect("non-empty frame material");
    let frame_node_id = lash_core_execution::facade_support::frame_node_id(
        &first_state.session_id,
        frame_key.as_str(),
    );
    let node = lash_core_execution::SessionNodeRecord {
        node_id: frame_node_id.to_string().into(),
        parent_node_id: None,
        timestamp: "2026-07-26T00:00:00Z".to_string(),
        payload: lash_core_execution::SessionNodePayload::FrameOpen {
            frame_key,
            reason: lash_core_execution::AgentFrameReason::initial(),
            assignment: lash_core_execution::AgentFrameAssignment::from_policy(SessionPolicy::new(
                lash_core_execution::TurnBudget::Unbounded,
            )),
            protocol_turn_options: Default::default(),
        },
    };
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&first_state, &[]);
    first_commit.graph = GraphAppend::Extend {
        nodes: vec![node.clone()],
    };
    first_commit.current_frame_node_id = Some(frame_node_id);
    first
        .commit_runtime_state(first_commit)
        .await
        .expect("commit first session node");

    second
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&second_state, &[]))
        .await
        .expect("another session's live node must not invalidate an empty session");

    let mut second_state = second_state;
    second_state.head_revision = 1;
    let resident_leaf = second_state.session_graph.leaf_node_id.clone();
    let mut cross_session_leaf = RuntimeCommit::persisted_state_for_test(&second_state, &[]);
    cross_session_leaf.graph = GraphAppend::PreserveHead;
    second
        .commit_runtime_state(cross_session_leaf)
        .await
        .expect("a preserve-head append cannot adopt another session's leaf");
    let head = second
        .load_session_head_meta(&SessionId::from("leaf-b"))
        .await
        .expect("load head after preserve-head append")
        .expect("session head remains published");
    assert_eq!(head.leaf_node_id, resident_leaf);
    assert_ne!(head.leaf_node_id, Some(node.node_id));
}

#[tokio::test]
async fn sqlite_vacuum_is_scoped_to_the_bound_session() {
    let root = unique_temp_dir("maintenance-scope");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));
    let request = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash_core_execution::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    };
    let first = admit_store(&factory, &request(&SessionId::from("maintenance-a")))
        .await
        .expect("first store");
    let second = admit_store(&factory, &request(&SessionId::from("maintenance-b")))
        .await
        .expect("second store");
    let source_key = "maintenance-b-source";
    let cancelled = second
        .enqueue_pending_turn_input(
            lash_core_execution::PendingTurnInputDraft::new(
                "maintenance-b",
                lash_core_execution::TurnInputIngress::NextTurn,
                lash_core_execution::TurnInput::text("dedupe fence"),
            )
            .with_source_key(source_key),
        )
        .await
        .expect("enqueue second input");
    second
        .cancel_pending_turn_input(&SessionId::from("maintenance-b"), &cancelled.input_id)
        .await
        .expect("cancel second input");

    let first_report = first
        .vacuum(&SessionId::from("maintenance-a"))
        .await
        .expect("vacuum first session");
    assert_eq!(first_report.removed_node_count, 0);
    assert_eq!(first_report.removed_pending_turn_input_tombstone_count, 0);
    let replay = second
        .enqueue_pending_turn_input(
            lash_core_execution::PendingTurnInputDraft::new(
                "maintenance-b",
                lash_core_execution::TurnInputIngress::NextTurn,
                lash_core_execution::TurnInput::text("dedupe fence"),
            )
            .with_source_key(source_key),
        )
        .await
        .expect("replay second input");
    assert_eq!(replay.input_id, cancelled.input_id);
    assert_eq!(
        replay.state.kind(),
        lash_core_execution::runtime::TurnInputStateKind::Cancelled
    );

    let second_report = second
        .vacuum(&SessionId::from("maintenance-b"))
        .await
        .expect("vacuum second session");
    assert_eq!(second_report.removed_node_count, 0);
    assert_eq!(second_report.removed_pending_turn_input_tombstone_count, 1);
}

/// Node ids physically resident in the catalog, tombstoned or not: reads hide
/// tombstones, so only raw SQL can tell a reclaimed row from a hidden one.
fn resident_graph_node_ids(root: &Path) -> Vec<String> {
    raw_node_ids(root, "SELECT node_id FROM graph_nodes ORDER BY node_id")
}

fn resident_tombstoned_node_ids(root: &Path) -> Vec<String> {
    raw_node_ids(
        root,
        "SELECT node_id FROM graph_nodes WHERE tombstoned = 1 ORDER BY node_id",
    )
}

fn raw_node_ids(root: &Path, sql: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
    let mut statement = conn.prepare(sql).expect("prepare node id probe");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query node ids")
        .collect::<Result<Vec<_>, _>>()
        .expect("read node ids")
}

async fn commit_single_root_node(
    factory: &Arc<SqliteStore>,
    session_id: &SessionId,
) -> (Arc<SqliteStore>, lash_core_execution::NodeId) {
    let store = admit_store(
        factory,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(session_id.to_string()),
            relation: lash_core_execution::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
        },
    )
    .await
    .expect("create store");
    let mut state = factory_state(&store, session_id, 0).await;
    state.ensure_agent_frame_initialized();
    let leaf = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("root leaf node id");
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit root node");
    (store, leaf)
}

/// Unpinning a pinned leaf *after* its owning session was deleted tombstones a
/// row whose owner can never be bound again, so no session-scoped vacuum can
/// reach it. The next session delete must drain that residue.
///
/// The owner's store handle is dropped before its delete on purpose: a live
/// handle can still vacuum its own session and would mask the leak.
#[tokio::test]
async fn sqlite_delete_reclaims_tombstone_orphaned_by_unpin_after_owner_delete() {
    let root = unique_temp_dir("orphan-unpin-after-delete");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));

    let leaf = {
        let (store, leaf) =
            commit_single_root_node(&factory, &SessionId::from("orphan-owner")).await;
        drop(store);
        leaf
    };
    factory.pin(&leaf).await.expect("pin owner leaf");
    factory
        .delete_session(&SessionId::from("orphan-owner"))
        .await
        .expect("delete owner session");
    factory
        .unpin(&leaf)
        .await
        .expect("unpin after owner delete");

    assert_eq!(
        resident_tombstoned_node_ids(&root),
        vec![leaf.to_string()],
        "the unpin must tombstone the deleted owner's leaf"
    );

    drop(commit_single_root_node(&factory, &SessionId::from("orphan-sweeper")).await);
    factory
        .delete_session(&SessionId::from("orphan-sweeper"))
        .await
        .expect("delete sweeper session");

    assert!(
        resident_tombstoned_node_ids(&root).is_empty(),
        "a delete must reclaim tombstones owned by already-deleted sessions"
    );
    assert!(
        !resident_graph_node_ids(&root).contains(&leaf.to_string()),
        "the orphaned tombstone row must be physically gone, not just hidden"
    );
}

/// Fork ancestry owned by a session deleted *before* its child is the second
/// orphaning flow: the ancestry is only tombstoned when the child is deleted, by
/// which time its owner is already unbindable. The same delete must reclaim it.
#[tokio::test]
async fn sqlite_delete_reclaims_fork_ancestry_orphaned_by_earlier_owner_delete() {
    let root = unique_temp_dir("orphan-fork-ancestry");
    let factory = std::sync::Arc::new(SqliteStore::open(&root).await.expect("open catalog"));

    let parent_leaf = {
        let (store, leaf) =
            commit_single_root_node(&factory, &SessionId::from("orphan-fork-parent")).await;
        drop(store);
        leaf
    };
    let policy = SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded);
    factory
        .fork_session(&lash_core_execution::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("orphan-fork-child"),
            node_id: parent_leaf.clone().into(),
            relation: lash_core_execution::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("fork at the parent's live tip");
    {
        let child = &factory;
        let read = child
            .load_session_window(
                &SessionId::from("orphan-fork-child"),
                WindowSelector::Current,
            )
            .await
            .expect("load child window")
            .expect("child window exists");
        let mut child_state = lash_core_execution::store::window_state(read, child.fleet_format())
            .expect("adopt child window")
            .state;
        let parent_node_id = child_state.session_graph.leaf_node_id.clone();
        child_state
            .session_graph
            .apply_append(&lash_core_execution::store::GraphAppend::Extend {
                nodes: vec![lash_core_execution::SessionNodeRecord {
                    node_id: "orphan-fork-child-node".to_string().into(),
                    parent_node_id,
                    timestamp: "2026-08-17T00:00:00Z".to_string(),
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
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&child_state, &[]))
            .await
            .expect("advance forked child");
    }

    factory
        .delete_session(&SessionId::from("orphan-fork-parent"))
        .await
        .expect("delete parent session");
    assert!(
        resident_graph_node_ids(&root).contains(&parent_leaf.to_string()),
        "the parent's node survives its own delete while the fork child hangs off it"
    );

    factory
        .delete_session(&SessionId::from("orphan-fork-child"))
        .await
        .expect("delete forked child session");

    assert!(
        resident_tombstoned_node_ids(&root).is_empty(),
        "the child's delete must reclaim ancestry owned by the already-deleted parent"
    );
    let resident = resident_graph_node_ids(&root);
    assert!(
        resident.is_empty(),
        "both the child's nodes and the orphaned parent ancestry must be gone, got {resident:?}"
    );
}

fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lash-sqlite-store-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}
