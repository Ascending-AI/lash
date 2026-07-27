use lash_core::store::GraphCommitDelta;
use lash_core::{
    HydratedSessionCheckpoint, LeaseOwnerIdentity, ModelSpec, PersistedSessionConfig,
    PersistedTurnState, PluginSessionSnapshot, RuntimeCommit, RuntimeSessionState,
    SessionCommitStore, SessionExecutionLeaseStore, SessionGraph, SessionHead, SessionPolicy,
    SessionStoreCreateRequest, SessionStoreFactory, TokenLedgerEntry, TokenUsage, ToolState,
};
use lash_sqlite_store::{
    BlobArtifactDescriptor, BuiltinBlobProfile, SqliteSessionStoreFactory, Store, StoreGcPolicy,
    StoreOptions,
};

fn model_spec(id: &str) -> ModelSpec {
    ModelSpec::from_token_limits(id, Default::default(), 200_000, None)
        .expect("valid test model spec")
}

fn test_model_spec() -> ModelSpec {
    model_spec("gpt-5.4-mini")
}

fn lease_owner(owner_id: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

fn persisted_tool_state_at_generation(generation: u64) -> ToolState {
    serde_json::from_value(serde_json::json!({
        "generation": generation,
        "tools": {}
    }))
    .expect("deserialize persisted tool state")
}

#[tokio::test]
async fn gc_unreachable_keeps_rooted_checkpoint_blobs() {
    let store = Store::memory().await.expect("store");
    let stored = store
        .put_checkpoint(&HydratedSessionCheckpoint {
            turn_state: PersistedTurnState {
                turn_index: 1,
                token_usage: TokenUsage::default(),
                last_prompt_usage: None,
                protocol_turn_options: Default::default(),
            },
            tool_state_ref: None,
            tool_state: Some(persisted_tool_state_at_generation(7)),
            plugin_snapshot_ref: None,
            plugin_snapshot_revision: Some(11),
            plugin_snapshot: Some(PluginSessionSnapshot {
                plugins: Default::default(),
            }),
            execution_state_ref: None,
            execution_state: None,
        })
        .await;
    store
        .save_session_head(SessionHead {
            session_id: "root".to_string(),
            head_revision: 0,
            agent_frames: Vec::new(),
            current_agent_frame_id: String::new(),
            graph: SessionGraph::default(),
            config: PersistedSessionConfig {
                provider_id: "openai-compatible".into(),
                model: test_model_spec(),
            },
            checkpoint_ref: Some(stored.checkpoint_ref.clone()),
            token_ledger: Vec::new(),
        })
        .await;
    let orphan = store
        .put_artifact_blob(BlobArtifactDescriptor::plugin_session_snapshot(), b"orphan")
        .await;

    let report = store.gc_unreachable().await;

    assert_eq!(report.deleted_blob_count, 1);
    let checkpoint = store
        .get_checkpoint(&stored.checkpoint_ref)
        .await
        .expect("checkpoint manifest");
    let dynamic_ref = checkpoint.tool_state_ref.expect("dynamic state ref");
    let plugin_ref = checkpoint.plugin_snapshot_ref.expect("plugin snapshot ref");
    assert!(store.get_blob(&stored.checkpoint_ref).await.is_some());
    assert!(store.get_blob(&dynamic_ref).await.is_some());
    assert!(store.get_blob(&plugin_ref).await.is_some());
    assert!(store.get_blob(&orphan).await.is_none());
}

#[tokio::test]
async fn auto_gc_runs_after_commit_without_reentrant_locking() {
    let store = Store::memory_with_options(StoreOptions {
        blob_profile: BuiltinBlobProfile::LowLatency,
        gc_policy: StoreGcPolicy {
            auto_run_every_commits: Some(1),
        },
    })
    .await
    .expect("store");
    let orphan = store
        .put_artifact_blob(BlobArtifactDescriptor::plugin_session_snapshot(), b"orphan")
        .await;
    let state = RuntimeSessionState {
        session_id: "auto-gc".to_string(),
        ..RuntimeSessionState::default()
    };
    let owner = lease_owner("auto-gc-test");
    let session_lease = store
        .try_claim_session_execution_lease("auto-gc", &owner, 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease");

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state(&state, &[])
                .with_session_execution_lease(session_lease.fence())
                .releasing_session_execution_lease(session_lease.completion()),
        )
        .await
        .expect("commit");

    assert!(store.get_blob(&orphan).await.is_none());
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
            session_id: "usage-index".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::default(),
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
        session_id: "chat/alpha".to_string(),
        relation: lash_core::SessionRelation::Child {
            parent_session_id: "parent".to_string(),
            caused_by: None,
        },
        policy: SessionPolicy {
            model: model_spec("first-model"),
            ..SessionPolicy::default()
        },
    };

    let store = factory.create_store(&request).await.expect("create store");
    let meta = store
        .load_session_meta()
        .await
        .expect("load meta")
        .expect("meta");
    assert_eq!(meta.session_id, "chat/alpha");
    assert_eq!(meta.model, "first-model");

    store
        .save_session_meta(lash_core::SessionMeta {
            session_id: "chat/alpha".to_string(),
            session_name: "Renamed".to_string(),
            created_at: "original".to_string(),
            model: "preserved-model".to_string(),
            cwd: Some("/tmp/original".to_string()),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: "parent".to_string(),
                caused_by: None,
            },
        })
        .await
        .expect("save meta");

    let reopened = factory
        .create_store(&SessionStoreCreateRequest {
            policy: SessionPolicy {
                model: model_spec("second-model"),
                ..SessionPolicy::default()
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
    assert_eq!(reopened_meta.session_name, "Renamed");
    assert_eq!(reopened_meta.model, "preserved-model");
    assert_eq!(reopened_meta.created_at, "original");
}

#[tokio::test]
async fn sqlite_factory_is_explicitly_usable_as_session_store_factory() {
    let root = unique_temp_dir("explicit");
    let factory: std::sync::Arc<dyn SessionStoreFactory> =
        std::sync::Arc::new(SqliteSessionStoreFactory::new(&root));
    let request = SessionStoreCreateRequest {
        session_id: "explicit".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy {
            model: model_spec("model"),
            ..SessionPolicy::default()
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
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy {
            model: model_spec("model"),
            ..SessionPolicy::default()
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
    deleted_store
        .commit_runtime_state(RuntimeCommit::persisted_state(
            &RuntimeSessionState {
                session_id: "delete/me".to_string(),
                execution_state_snapshot: Some(vec![1, 2, 3]),
                ..Default::default()
            },
            &[],
        ))
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
async fn sqlite_catalog_enforces_global_node_ids_across_sessions() {
    let root = unique_temp_dir("global-node-id");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store_for = |session_id: &str| SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::default(),
    };
    let first = factory
        .create_store(&store_for("first"))
        .await
        .expect("first store");
    let second = factory
        .create_store(&store_for("second"))
        .await
        .expect("second store");
    let node = lash_core::SessionNodeRecord {
        node_id: "factory-global-node".to_string(),
        parent_node_id: None,
        caused_by: None,
        agent_frame_id: None,
        timestamp: "2026-07-26T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::Event {
            event: lash_core::SessionHistoryRecord::Protocol(
                lash_core::ProtocolEvent::typed("global-node-id", serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let commit = |session_id: &str| {
        let state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..Default::default()
        };
        let usage = TokenLedgerEntry {
            source: "rollback-probe".to_string(),
            model: "test".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..Default::default()
            },
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[usage]);
        commit.graph = GraphCommitDelta::Append {
            nodes: vec![node.clone()],
            leaf_node_id: Some(node.node_id.clone()),
        };
        commit
    };

    first
        .commit_runtime_state(commit("first"))
        .await
        .expect("first node insert");
    let error = second
        .commit_runtime_state(commit("second"))
        .await
        .expect_err("second session must not reuse a global node id");

    assert!(matches!(
        error,
        lash_core::StoreError::NodeIdCollision { node_id }
            if node_id == "factory-global-node"
    ));
    let conn = rusqlite::Connection::open(factory.catalog_path()).expect("open catalog");
    let usage_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM usage_deltas WHERE session_id = 'second'",
            [],
            |row| row.get(0),
        )
        .expect("count usage rows");
    assert_eq!(
        usage_count, 0,
        "the usage write before the conflicting node insert must roll back"
    );
}

#[tokio::test]
async fn sqlite_catalog_leaf_validation_is_session_scoped() {
    let root = unique_temp_dir("leaf-scope");
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = |session_id: &str| SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::default(),
    };
    let first = factory
        .create_store(&request("leaf-a"))
        .await
        .expect("first store");
    let second = factory
        .create_store(&request("leaf-b"))
        .await
        .expect("second store");
    let node = lash_core::SessionNodeRecord {
        node_id: "leaf-a-node".to_string(),
        parent_node_id: None,
        caused_by: None,
        agent_frame_id: None,
        timestamp: "2026-07-26T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::Event {
            event: lash_core::SessionHistoryRecord::Protocol(
                lash_core::ProtocolEvent::typed("leaf-scope", serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let mut first_commit = RuntimeCommit::persisted_state(
        &RuntimeSessionState {
            session_id: "leaf-a".to_string(),
            ..Default::default()
        },
        &[],
    );
    first_commit.graph = GraphCommitDelta::Append {
        nodes: vec![node.clone()],
        leaf_node_id: Some(node.node_id.clone()),
    };
    first
        .commit_runtime_state(first_commit)
        .await
        .expect("commit first session node");

    second
        .commit_runtime_state(RuntimeCommit::persisted_state(
            &RuntimeSessionState {
                session_id: "leaf-b".to_string(),
                ..Default::default()
            },
            &[],
        ))
        .await
        .expect("another session's live node must not invalidate an empty session");

    let mut cross_session_leaf = RuntimeCommit::persisted_state(
        &RuntimeSessionState {
            session_id: "leaf-b".to_string(),
            head_revision: Some(1),
            ..Default::default()
        },
        &[],
    );
    cross_session_leaf.graph = GraphCommitDelta::Unchanged {
        leaf_node_id: Some(node.node_id),
    };
    assert!(matches!(
        second.commit_runtime_state(cross_session_leaf).await,
        Err(lash_core::StoreError::InvalidGraphLeaf { .. })
    ));
}

#[tokio::test]
async fn sqlite_maintenance_is_scoped_to_the_bound_session() {
    let root = unique_temp_dir("maintenance-scope");
    let factory = SqliteSessionStoreFactory::new(&root);
    let request = |session_id: &str| SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: SessionPolicy::default(),
    };
    let first = factory
        .create_store(&request("maintenance-a"))
        .await
        .expect("first store");
    let second = factory
        .create_store(&request("maintenance-b"))
        .await
        .expect("second store");
    let node = lash_core::SessionNodeRecord {
        node_id: "maintenance-b-node".to_string(),
        parent_node_id: None,
        caused_by: None,
        agent_frame_id: None,
        timestamp: "2026-07-26T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::Event {
            event: lash_core::SessionHistoryRecord::Protocol(
                lash_core::ProtocolEvent::typed("maintenance", serde_json::Value::Null)
                    .expect("protocol event"),
            ),
        },
    };
    let mut commit = RuntimeCommit::persisted_state(
        &RuntimeSessionState {
            session_id: "maintenance-b".to_string(),
            ..Default::default()
        },
        &[],
    );
    commit.graph = GraphCommitDelta::Append {
        nodes: vec![node.clone()],
        leaf_node_id: Some(node.node_id.clone()),
    };
    second
        .commit_runtime_state(commit)
        .await
        .expect("commit second session node");

    first
        .tombstone_nodes(std::slice::from_ref(&node.node_id))
        .await
        .expect("cross-session tombstone attempt");
    assert!(
        second
            .load_node(&node.node_id)
            .await
            .expect("load second node")
            .is_some(),
        "session A must not tombstone session B's node"
    );
    second
        .tombstone_nodes(std::slice::from_ref(&node.node_id))
        .await
        .expect("tombstone own node");

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
    assert_eq!(second_report.removed_node_count, 1);
    assert_eq!(second_report.removed_pending_turn_input_tombstone_count, 1);
}

#[tokio::test]
async fn sqlite_snapshot_read_propagates_graph_statement_errors() {
    let root = unique_temp_dir("graph-read-error");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: "graph-read-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::default(),
        })
        .await
        .expect("create store");
    store
        .commit_runtime_state(RuntimeCommit::persisted_state(
            &RuntimeSessionState {
                session_id: "graph-read-error".to_string(),
                ..Default::default()
            },
            &[],
        ))
        .await
        .expect("commit head");
    rusqlite::Connection::open(factory.catalog_path())
        .expect("open catalog")
        .execute("DROP TABLE graph_nodes", [])
        .expect("drop graph table");

    assert!(
        store
            .load_session(lash_core::SessionReadScope::FullGraph)
            .await
            .is_err(),
        "a graph statement error must not decode as an empty snapshot"
    );
}

#[tokio::test]
async fn sqlite_snapshot_read_propagates_usage_statement_errors() {
    let root = unique_temp_dir("usage-read-error");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: "usage-read-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::default(),
        })
        .await
        .expect("create store");
    store
        .commit_runtime_state(RuntimeCommit::persisted_state(
            &RuntimeSessionState {
                session_id: "usage-read-error".to_string(),
                ..Default::default()
            },
            &[],
        ))
        .await
        .expect("commit head");
    rusqlite::Connection::open(factory.catalog_path())
        .expect("open catalog")
        .execute("DROP TABLE usage_deltas", [])
        .expect("drop usage table");

    assert!(
        store
            .load_session(lash_core::SessionReadScope::FullGraph)
            .await
            .is_err(),
        "a usage statement error must not decode as an empty ledger"
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
