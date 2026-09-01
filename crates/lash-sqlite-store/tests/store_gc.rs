use lash_core::store::GraphAppend;
use lash_core::{
    Message, MessageRole, ModelSpec, Part, PluginSessionSnapshot, RuntimeCommit,
    RuntimeSessionState, SessionCommitStore, SessionPolicy, SessionStoreCreateRequest,
    SessionStoreFactory, StoreError, StoreMaintenance, TokenLedgerEntry, TokenUsage, ToolState,
    facade_support::shared_parts,
};
use lash_sqlite_store::{BlobArtifactDescriptor, SqliteSessionStoreFactory, Store};

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

fn user_message(id: &str, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            content.to_string(),
            None,
        )]),
        origin: None,
    }
}

async fn factory_state(
    store: &std::sync::Arc<dyn lash_core::RuntimePersistence>,
    session_id: &str,
    head_revision: u64,
) -> RuntimeSessionState {
    store
        .load_session_meta()
        .await
        .expect("load factory session metadata")
        .expect("factory session metadata");
    RuntimeSessionState {
        session_id: session_id.to_string(),
        head_revision,
        ..RuntimeSessionState::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    }
}

#[tokio::test]
async fn gc_unreachable_keeps_rooted_checkpoint_blobs() {
    let store = Store::memory().await.expect("store");
    let tool_state = persisted_tool_state_at_generation(7);
    let plugin_snapshot = PluginSessionSnapshot {
        plugins: Default::default(),
    };
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: 1,
        plugin_snapshot_revision: Some(11),
        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    state.set_tool_state_snapshot(Some(tool_state));
    state.set_plugin_snapshot(Some(plugin_snapshot));
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(state.session_id.clone()))
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
        .component_ref(lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT)
        .expect("dynamic state ref")
        .clone();
    let plugin_ref = checkpoint
        .component_ref(lash_core::store::PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT)
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

#[test]
fn sqlite_factory_uses_one_deterministic_catalog_path() {
    let root = unique_temp_dir("paths");
    let factory = SqliteSessionStoreFactory::new(&root);

    let first = factory.catalog_path();
    let second = factory.catalog_path();

    assert_eq!(first, second);
    assert_eq!(first.parent(), Some(root.as_path()));
    assert!(
        first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".db")
    );
    assert!(!first.file_name().unwrap().to_string_lossy().contains('/'));
}

#[tokio::test]
async fn sqlite_catalog_indexes_usage_by_session() {
    let root = unique_temp_dir("usage-index");
    let factory = SqliteSessionStoreFactory::new(&root);
    factory
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: "usage-index".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create store");
    let conn = rusqlite::Connection::open(factory.catalog_path()).expect("open catalog");
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
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "chat/alpha".to_string(),
        relation: lash_core::SessionRelation::Child {
            parent_session_id: "parent".to_string(),
            caused_by: None,
        },
        policy: SessionPolicy {
            model: model_spec("first-model"),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    };

    let store = factory.create_store(&request).await.expect("create store");
    let meta = store
        .load_session_meta()
        .await
        .expect("load meta")
        .expect("meta");
    assert_eq!(meta.session_id, "chat/alpha");
    assert_eq!(meta.parent_session_id(), Some("parent"));

    store
        .save_session_meta(lash_core::SessionMeta {
            pending_observer_intents: Vec::new(),
            session_id: "chat/alpha".to_string(),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "preserved-parent".to_string(),
                caused_by: None,
            },
        })
        .await
        .expect("save meta");

    let reopened = factory
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy {
                model: model_spec("second-model"),
                ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
            },
            ..request
        })
        .await
        .expect("reopen store");
    let reopened_meta = reopened
        .load_session_meta()
        .await
        .expect("load reopened meta")
        .expect("reopened meta");
    assert_eq!(reopened_meta.parent_session_id(), Some("preserved-parent"));
}

#[tokio::test]
async fn sqlite_factory_is_explicitly_usable_as_session_store_factory() {
    let root = unique_temp_dir("explicit");
    let factory: std::sync::Arc<dyn SessionStoreFactory> =
        std::sync::Arc::new(SqliteSessionStoreFactory::new(&root));
    let request = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "explicit".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy {
            model: model_spec("model"),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    };

    let store = factory.create_store(&request).await.expect("create store");

    assert!(
        store
            .load_session_meta()
            .await
            .expect("load meta")
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_factory_delete_session_removes_only_the_selected_session() {
    let root = unique_temp_dir("delete-session");
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = |session_id: &str| SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy {
            model: model_spec("model"),
            ..SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    };
    let deleted_store = factory
        .create_store(&request("delete/me"))
        .await
        .expect("create deleted session");
    factory
        .create_store(&request("keep/me"))
        .await
        .expect("create retained session");
    let mut deleted_state = factory_state(&deleted_store, "delete/me", 0).await;
    deleted_state.set_execution_state_snapshot(Some(vec![1, 2, 3]));
    deleted_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&deleted_state, &[]))
        .await
        .expect("commit deleted session checkpoint");
    {
        let conn = rusqlite::Connection::open(factory.catalog_path()).expect("open catalog");
        conn.execute(
            "INSERT INTO blobs (hash, content) VALUES ('session-trigger-blob', X'01')",
            [],
        )
        .expect("insert session trigger blob");
        conn.execute(
            "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
             VALUES ('lashlang_trigger_manifest', 'session:delete/me', 'session-trigger-blob')",
            [],
        )
        .expect("insert session trigger ref");
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
        .delete_session("delete/me")
        .await
        .expect("delete session");
    factory
        .delete_session("delete/me")
        .await
        .expect("delete session again");

    assert!(factory.catalog_path().exists());
    assert!(
        factory
            .open_existing_store(&request("delete/me"))
            .await
            .expect("probe deleted session")
            .is_none()
    );
    assert!(
        factory
            .open_existing_store(&request("keep/me"))
            .await
            .expect("probe retained session")
            .is_some()
    );
    let conn = rusqlite::Connection::open(factory.catalog_path()).expect("open catalog");
    let deleted_ref_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_refs
             WHERE namespace = 'lashlang_trigger_manifest'
               AND artifact_ref = 'session:delete/me'",
            [],
            |row| row.get(0),
        )
        .expect("count deleted trigger refs");
    assert_eq!(
        deleted_ref_count, 0,
        "session-owned artifact ref must be deleted"
    );
    let deleted_blob_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blobs WHERE hash = 'session-trigger-blob'",
            [],
            |row| row.get(0),
        )
        .expect("count deleted trigger blob");
    assert_eq!(
        deleted_blob_count, 0,
        "unrooted session blob must be reclaimed"
    );
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
        "deleting the session must reclaim its checkpoint tree and trigger blob"
    );
}

#[tokio::test]
async fn sqlite_catalog_partitions_derived_node_ids_by_session() {
    let root = unique_temp_dir("global-node-id");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store_for = |session_id: &str| SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let first = factory
        .create_store(&store_for("first"))
        .await
        .expect("first store");
    let second = factory
        .create_store(&store_for("second"))
        .await
        .expect("second store");
    let first_state = factory_state(&first, "first", 0).await;
    let second_state = factory_state(&second, "second", 0).await;
    let commit = |state: &RuntimeSessionState| {
        let frame_key = lash_core::FrameKey::from_caller_material("shared-frame-key")
            .expect("non-empty frame material");
        let frame_node_id =
            lash_core::facade_support::frame_node_id(&state.session_id, frame_key.as_str());
        let node = lash_core::SessionNodeRecord {
            node_id: frame_node_id.to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: lash_core::SessionNodePayload::FrameOpen {
                frame_key,
                reason: lash_core::AgentFrameReason::initial(),
                assignment: lash_core::AgentFrameAssignment::from_policy(SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                )),
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
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(state, &[usage]);
        commit.graph = GraphAppend {
            nodes: vec![node.clone()],
            leaf_node_id: Some(node.node_id.clone()),
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

    let frame_key = lash_core::FrameKey::from_caller_material("shared-frame-key")
        .expect("non-empty frame material");
    let first_node_id =
        lash_core::facade_support::frame_node_id(&first_state.session_id, frame_key.as_str());
    let second_node_id =
        lash_core::facade_support::frame_node_id(&second_state.session_id, frame_key.as_str());
    assert_ne!(first_node_id, second_node_id);
    assert!(first.load_node(&first_node_id).await.unwrap().is_some());
    assert!(second.load_node(&second_node_id).await.unwrap().is_some());
}

#[tokio::test]
async fn sqlite_catalog_leaf_validation_is_session_scoped() {
    let root = unique_temp_dir("leaf-scope");
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = |session_id: &str| SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let first = factory
        .create_store(&request("leaf-a"))
        .await
        .expect("first store");
    let second = factory
        .create_store(&request("leaf-b"))
        .await
        .expect("second store");
    let first_state = factory_state(&first, "leaf-a", 0).await;
    let second_state = factory_state(&second, "leaf-b", 0).await;
    let frame_key =
        lash_core::FrameKey::from_caller_material("leaf-a-node").expect("non-empty frame material");
    let frame_node_id =
        lash_core::facade_support::frame_node_id(&first_state.session_id, frame_key.as_str());
    let node = lash_core::SessionNodeRecord {
        node_id: frame_node_id.to_string(),
        parent_node_id: None,
        timestamp: "2026-07-26T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::FrameOpen {
            frame_key,
            reason: lash_core::AgentFrameReason::initial(),
            assignment: lash_core::AgentFrameAssignment::from_policy(SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            )),
            protocol_turn_options: Default::default(),
        },
    };
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&first_state, &[]);
    first_commit.graph = GraphAppend {
        nodes: vec![node.clone()],
        leaf_node_id: Some(node.node_id.clone()),
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
    let mut cross_session_leaf = RuntimeCommit::persisted_state_for_test(&second_state, &[]);
    cross_session_leaf.graph = GraphAppend {
        nodes: Vec::new(),
        leaf_node_id: Some(node.node_id),
    };
    assert!(matches!(
        second.commit_runtime_state(cross_session_leaf).await,
        Err(lash_core::StoreError::InvalidGraphLeaf { .. })
    ));
}

#[tokio::test]
async fn sqlite_vacuum_is_scoped_to_the_bound_session() {
    let root = unique_temp_dir("maintenance-scope");
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = |session_id: &str| SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let first = factory
        .create_store(&request("maintenance-a"))
        .await
        .expect("first store");
    let second = factory
        .create_store(&request("maintenance-b"))
        .await
        .expect("second store");
    let source_key = "maintenance-b-source";
    let cancelled = second
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                "maintenance-b",
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text("dedupe fence"),
            )
            .with_source_key(source_key),
        )
        .await
        .expect("enqueue second input");
    second
        .cancel_pending_turn_input("maintenance-b", &cancelled.input_id)
        .await
        .expect("cancel second input");

    let first_report = first.vacuum().await.expect("vacuum first session");
    assert_eq!(first_report.removed_node_count, 0);
    assert_eq!(first_report.removed_pending_turn_input_tombstone_count, 0);
    let replay = second
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                "maintenance-b",
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text("dedupe fence"),
            )
            .with_source_key(source_key),
        )
        .await
        .expect("replay second input");
    assert_eq!(replay.input_id, cancelled.input_id);
    assert_eq!(replay.state, lash_core::TurnInputState::Cancelled);

    let second_report = second.vacuum().await.expect("vacuum second session");
    assert_eq!(second_report.removed_node_count, 0);
    assert_eq!(second_report.removed_pending_turn_input_tombstone_count, 1);
}

#[tokio::test]
async fn sqlite_snapshot_read_propagates_graph_statement_errors() {
    let root = unique_temp_dir("graph-read-error");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: "graph-read-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create store");
    let mut state = factory_state(&store, "graph-read-error", 0).await;
    state.ensure_agent_frame_initialized();
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit head");
    rusqlite::Connection::open(factory.catalog_path())
        .expect("open catalog")
        .execute("DROP TABLE graph_nodes", [])
        .expect("drop graph table");

    assert!(matches!(
        store.load_session().await,
        Err(StoreError::StorageFailure {
            backend: "sqlite",
            ..
        })
    ));
}

#[tokio::test]
async fn sqlite_snapshot_read_rejects_undecodable_graph_nodes() {
    let root = unique_temp_dir("graph-node-decode-error");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: "graph-node-decode-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create store");
    let mut state = factory_state(&store, "graph-node-decode-error", 0).await;
    state.ensure_agent_frame_initialized();
    state.append_active_conversation_messages(&[
        user_message("first", "first"),
        user_message("second", "second"),
    ]);
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit graph");
    rusqlite::Connection::open(factory.catalog_path())
        .expect("open catalog")
        .execute(
            "UPDATE graph_nodes
             SET node_json = '{\"totally\":\"unreadable\"}'
             WHERE node_id = (
                 SELECT node_id FROM graph_nodes
                 WHERE session_id = ?1
                 ORDER BY generation ASC
                 LIMIT 1 OFFSET 1
             )",
            ["graph-node-decode-error"],
        )
        .expect("corrupt middle graph node");

    let error = store
        .load_session()
        .await
        .expect_err("an undecodable graph node must fail the snapshot");
    assert!(matches!(
        error,
        StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph node",
            ..
        }
    ));
}

#[tokio::test]
async fn sqlite_snapshot_read_propagates_usage_statement_errors() {
    let root = unique_temp_dir("usage-read-error");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: "usage-read-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create store");
    let state = factory_state(&store, "usage-read-error", 0).await;
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit head");
    rusqlite::Connection::open(factory.catalog_path())
        .expect("open catalog")
        .execute("DROP TABLE usage_deltas", [])
        .expect("drop usage table");

    assert!(matches!(
        store.load_session().await,
        Err(StoreError::StorageFailure {
            backend: "sqlite",
            ..
        })
    ));
}

#[tokio::test]
async fn sqlite_unbound_vacuum_returns_typed_error_and_preserves_catalog() {
    let root = unique_temp_dir("unbound-vacuum");
    let factory = SqliteSessionStoreFactory::new(&root);

    // 1. Live session with cancelled pending input
    let live_req = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "unbound-vacuum-live".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let live_store = factory
        .create_store(&live_req)
        .await
        .expect("create live store");
    let cancelled = live_store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                "unbound-vacuum-live",
                lash_core::TurnInputIngress::NextTurn,
                lash_core::TurnInput::text("input"),
            )
            .with_source_key("test-key"),
        )
        .await
        .expect("enqueue");
    live_store
        .cancel_pending_turn_input("unbound-vacuum-live", &cancelled.input_id)
        .await
        .expect("cancel");

    // 2. Deleted session with unpinned tombstoned node
    let del_req = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "unbound-vacuum-del".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let del_store = factory
        .create_store(&del_req)
        .await
        .expect("create del store");
    let mut state = factory_state(&del_store, "unbound-vacuum-del", 0).await;
    state.ensure_agent_frame_initialized();
    let leaf = state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("leaf node id");
    del_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit");
    factory.pin(&leaf).await.expect("pin");
    factory
        .delete_session(&del_req.session_id)
        .await
        .expect("delete");
    factory.unpin(&leaf).await.expect("unpin");

    // Open an unbound store handle over the catalog path
    let unbound = Store::open(&factory.catalog_path())
        .await
        .expect("open unbound store");
    let err = unbound
        .vacuum()
        .await
        .expect_err("unbound vacuum must return typed error");
    assert!(
        matches!(
            err.stop,
            lash_core::MaintenanceStop::Failed(StoreError::SessionNotBound)
        ),
        "expected SessionNotBound, got {err:?}"
    );

    // Verify catalog rows were NOT deleted by unbound vacuum
    let live_report = live_store.vacuum().await.expect("vacuum live store");
    assert_eq!(live_report.removed_node_count, 0);
    assert_eq!(live_report.removed_pending_turn_input_tombstone_count, 1);

    let del_report = del_store.vacuum().await.expect("vacuum del store");
    assert_eq!(del_report.removed_node_count, 1);
    assert_eq!(del_report.removed_pending_turn_input_tombstone_count, 0);
}

/// Node ids physically resident in the catalog, tombstoned or not: reads hide
/// tombstones, so only raw SQL can tell a reclaimed row from a hidden one.
fn resident_graph_node_ids(factory: &SqliteSessionStoreFactory) -> Vec<String> {
    raw_node_ids(factory, "SELECT node_id FROM graph_nodes ORDER BY node_id")
}

fn resident_tombstoned_node_ids(factory: &SqliteSessionStoreFactory) -> Vec<String> {
    raw_node_ids(
        factory,
        "SELECT node_id FROM graph_nodes WHERE tombstoned = 1 ORDER BY node_id",
    )
}

fn raw_node_ids(factory: &SqliteSessionStoreFactory, sql: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(factory.catalog_path()).expect("open catalog");
    let mut statement = conn.prepare(sql).expect("prepare node id probe");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query node ids")
        .collect::<Result<Vec<_>, _>>()
        .expect("read node ids")
}

async fn commit_single_root_node(
    factory: &SqliteSessionStoreFactory,
    session_id: &str,
) -> (std::sync::Arc<dyn lash_core::RuntimePersistence>, String) {
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
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
    let factory = SqliteSessionStoreFactory::new(&root);

    let leaf = {
        let (store, leaf) = commit_single_root_node(&factory, "orphan-owner").await;
        drop(store);
        leaf
    };
    factory.pin(&leaf).await.expect("pin owner leaf");
    factory
        .delete_session("orphan-owner")
        .await
        .expect("delete owner session");
    factory
        .unpin(&leaf)
        .await
        .expect("unpin after owner delete");

    assert_eq!(
        resident_tombstoned_node_ids(&factory),
        vec![leaf.clone()],
        "the unpin must tombstone the deleted owner's leaf"
    );

    drop(commit_single_root_node(&factory, "orphan-sweeper").await);
    factory
        .delete_session("orphan-sweeper")
        .await
        .expect("delete sweeper session");

    assert!(
        resident_tombstoned_node_ids(&factory).is_empty(),
        "a delete must reclaim tombstones owned by already-deleted sessions"
    );
    assert!(
        !resident_graph_node_ids(&factory).contains(&leaf),
        "the orphaned tombstone row must be physically gone, not just hidden"
    );
}

/// Fork ancestry owned by a session deleted *before* its child is the second
/// orphaning flow: the ancestry is only tombstoned when the child is deleted, by
/// which time its owner is already unbindable. The same delete must reclaim it.
#[tokio::test]
async fn sqlite_delete_reclaims_fork_ancestry_orphaned_by_earlier_owner_delete() {
    let root = unique_temp_dir("orphan-fork-ancestry");
    let factory = SqliteSessionStoreFactory::new(&root);

    let parent_leaf = {
        let (store, leaf) = commit_single_root_node(&factory, "orphan-fork-parent").await;
        drop(store);
        leaf
    };
    let policy = SessionPolicy::new(lash_core::TurnBudget::Unbounded);
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            pending_observer_intents: Vec::new(),
            session_id: "orphan-fork-child".to_string(),
            node_id: parent_leaf.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("fork at the parent's live tip");
    {
        let child = factory
            .open_existing_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: "orphan-fork-child".to_string(),
                relation: lash_core::SessionRelation::Root,
                policy,
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
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&child_state, &[]))
            .await
            .expect("advance forked child");
    }

    factory
        .delete_session("orphan-fork-parent")
        .await
        .expect("delete parent session");
    assert!(
        resident_graph_node_ids(&factory).contains(&parent_leaf),
        "the parent's node survives its own delete while the fork child hangs off it"
    );

    factory
        .delete_session("orphan-fork-child")
        .await
        .expect("delete forked child session");

    assert!(
        resident_tombstoned_node_ids(&factory).is_empty(),
        "the child's delete must reclaim ancestry owned by the already-deleted parent"
    );
    let resident = resident_graph_node_ids(&factory);
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
