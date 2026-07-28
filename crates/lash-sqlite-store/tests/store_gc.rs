use lash_core::store::GraphAppend;
use lash_core::{
    HydratedSessionCheckpoint, LeaseOwnerIdentity, Message, MessageRole, ModelSpec, Part, PartKind,
    PersistedTurnState, PluginSessionSnapshot, PruneState, RuntimeCommit, RuntimeSessionState,
    SessionCommitStore, SessionExecutionLeaseStore, SessionPolicy, SessionStoreCreateRequest,
    SessionStoreFactory, TokenLedgerEntry, TokenUsage, ToolState, shared_parts,
};
use lash_sqlite_store::{
    BlobArtifactDescriptor, BuiltinBlobProfile, SqliteSessionStoreFactory, Store, StoreGcPolicy,
    StoreOptions,
};

fn model_spec(id: &str) -> ModelSpec {
    ModelSpec::from_token_limits(id, Default::default(), 200_000, None)
        .expect("valid test model spec")
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

fn user_message(id: &str, content: &str) -> Message {
    Message {
        id: id.to_string(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part {
            id: format!("{id}.p0"),
            kind: PartKind::Text,
            content: content.to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: None,
    }
}

async fn factory_state(
    store: &std::sync::Arc<dyn lash_core::RuntimePersistence>,
    session_id: &str,
    head_revision: u64,
) -> RuntimeSessionState {
    let incarnation_id = store
        .load_session_meta()
        .await
        .expect("load factory session metadata")
        .expect("factory session metadata")
        .incarnation_id;
    RuntimeSessionState {
        session_id: session_id.to_string(),
        session_lifetime: lash_core::SessionLifetime::durable(incarnation_id),
        head_revision,
        ..Default::default()
    }
}

#[tokio::test]
async fn gc_unreachable_keeps_rooted_checkpoint_blobs() {
    let store = Store::memory().await.expect("store");
    let checkpoint = HydratedSessionCheckpoint {
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
    };
    let stored = store.put_checkpoint(&checkpoint).await;
    let mut state = RuntimeSessionState {
        session_id: "root".to_string(),
        turn_index: checkpoint.turn_state.turn_index,
        tool_state_ref: stored.manifest.tool_state_ref.clone(),
        tool_state_snapshot: checkpoint.tool_state.clone(),
        plugin_snapshot_ref: stored.manifest.plugin_snapshot_ref.clone(),
        plugin_snapshot_revision: checkpoint.plugin_snapshot_revision,
        plugin_snapshot: checkpoint.plugin_snapshot.clone(),
        checkpoint_ref: Some(stored.checkpoint_ref.clone()),
        ..RuntimeSessionState::default()
    };
    let incarnation_id = store
        .ensure_session_incarnation(&state.session_id, &state.policy)
        .await
        .expect("realize session incarnation");
    state.session_lifetime = lash_core::SessionLifetime::durable(incarnation_id);
    state.ensure_agent_frame_initialized();
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit session state");
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
    let mut state = RuntimeSessionState {
        session_id: "auto-gc".to_string(),
        ..RuntimeSessionState::default()
    };
    let incarnation_id = store
        .ensure_session_incarnation(&state.session_id, &state.policy)
        .await
        .expect("realize session incarnation");
    state.session_lifetime = lash_core::SessionLifetime::durable(incarnation_id);
    let owner = lease_owner("auto-gc-test");
    let session_lease = store
        .try_claim_session_execution_lease("auto-gc", &owner, 60_000)
        .await
        .expect("claim session execution lease")
        .acquired()
        .expect("session execution lease");

    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
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
            incarnation_id: meta.incarnation_id,
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
    let mut deleted_state = factory_state(&deleted_store, "delete/me", 0).await;
    deleted_state.execution_state_snapshot = Some(vec![1, 2, 3]);
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
async fn sqlite_catalog_partitions_derived_node_ids_by_incarnation() {
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
    let first_state = factory_state(&first, "first", 0).await;
    let second_state = factory_state(&second, "second", 0).await;
    let commit = |state: &RuntimeSessionState| {
        let node = lash_core::SessionNodeRecord {
            node_id: lash_core::frame_node_id(
                &state.session_id,
                state
                    .durable_incarnation_id("test frame derivation")
                    .expect("durable fixture"),
                "shared-frame-key",
            ),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: lash_core::SessionNodePayload::FrameOpen {
                frame_key: "shared-frame-key".to_string(),
                reason: lash_core::AgentFrameReason::initial(),
                assignment: lash_core::AgentFrameAssignment::from_policy(SessionPolicy::default()),
                protocol_turn_options: Default::default(),
            },
        };
        let usage = TokenLedgerEntry {
            source: "incarnation-partition-probe".to_string(),
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
        commit.current_frame_node_id = Some(node.node_id.clone());
        commit
    };

    first
        .commit_runtime_state(commit(&first_state))
        .await
        .expect("first node insert");
    second
        .commit_runtime_state(commit(&second_state))
        .await
        .expect("second incarnation derives a distinct node id");

    let first_node_id = lash_core::frame_node_id(
        &first_state.session_id,
        first_state
            .durable_incarnation_id("test frame derivation")
            .expect("durable fixture"),
        "shared-frame-key",
    );
    let second_node_id = lash_core::frame_node_id(
        &second_state.session_id,
        second_state
            .durable_incarnation_id("test frame derivation")
            .expect("durable fixture"),
        "shared-frame-key",
    );
    assert_ne!(first_node_id, second_node_id);
    assert!(first.load_node(&first_node_id).await.unwrap().is_some());
    assert!(second.load_node(&second_node_id).await.unwrap().is_some());
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
    let first_state = factory_state(&first, "leaf-a", 0).await;
    let second_state = factory_state(&second, "leaf-b", 0).await;
    let frame_key = "leaf-a-node";
    let node = lash_core::SessionNodeRecord {
        node_id: lash_core::frame_node_id(
            &first_state.session_id,
            first_state
                .durable_incarnation_id("test frame derivation")
                .expect("durable fixture"),
            frame_key,
        ),
        parent_node_id: None,
        timestamp: "2026-07-26T00:00:00Z".to_string(),
        payload: lash_core::SessionNodePayload::FrameOpen {
            frame_key: frame_key.to_string(),
            reason: lash_core::AgentFrameReason::initial(),
            assignment: lash_core::AgentFrameAssignment::from_policy(SessionPolicy::default()),
            protocol_turn_options: Default::default(),
        },
    };
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&first_state, &[]);
    first_commit.graph = GraphAppend {
        nodes: vec![node.clone()],
        leaf_node_id: Some(node.node_id.clone()),
    };
    first_commit.current_frame_node_id = Some(node.node_id.clone());
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
            session_id: "graph-read-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::default(),
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

    assert!(
        store.load_session().await.is_err(),
        "a graph statement error must not decode as an empty snapshot"
    );
}

#[tokio::test]
async fn sqlite_snapshot_read_rejects_undecodable_graph_nodes() {
    let root = unique_temp_dir("graph-node-decode-error");
    let factory = SqliteSessionStoreFactory::new(&root);
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: "graph-node-decode-error".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::default(),
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
                 ORDER BY seq ASC
                 LIMIT 1 OFFSET 1
             )",
            ["graph-node-decode-error"],
        )
        .expect("corrupt middle graph node");

    let error = store
        .load_session()
        .await
        .expect_err("an undecodable graph node must fail the snapshot");
    assert!(
        error
            .to_string()
            .contains("failed to decode session graph node"),
        "unexpected graph decode error: {error}"
    );
}

#[tokio::test]
async fn sqlite_picker_reads_inherited_history_from_fork_head_columns() {
    let root = unique_temp_dir("fork-picker");
    let factory = SqliteSessionStoreFactory::new(&root);
    let policy = SessionPolicy::default();
    let source = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: "picker-source".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("create source store");
    let mut source_state = factory_state(&source, "picker-source", 0).await;
    source_state.ensure_agent_frame_initialized();
    source_state.append_active_conversation_messages(&[
        user_message("first", "first inherited message"),
        user_message("second", "second inherited message"),
    ]);
    source
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&source_state, &[]))
        .await
        .expect("commit source graph");
    let source_read = source
        .load_session()
        .await
        .expect("load source")
        .expect("source exists");
    let source_leaf = source_read.graph.leaf_node_id.clone().expect("source leaf");
    factory
        .fork_at(&lash_core::ForkSessionRequest {
            session_id: "picker-fork".to_string(),
            node_id: source_leaf,
            relation: lash_core::SessionRelation::Root,
            policy,
        })
        .await
        .expect("fork source");
    let branch = factory
        .open_existing_store(&SessionStoreCreateRequest {
            session_id: "picker-fork".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: SessionPolicy::default(),
        })
        .await
        .expect("open fork")
        .expect("fork exists");
    let branch_state = lash_core::store::load_persisted_session_state(branch.as_ref())
        .await
        .expect("load fork state")
        .expect("fork state exists");
    assert_eq!(branch_state.session_graph.nodes.len(), 3);

    let picker_store = Store::open(&factory.catalog_path())
        .await
        .expect("open picker store");
    picker_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&branch_state, &[]))
        .await
        .expect("bind picker store to fork");
    let picker = picker_store.load_picker_info().await.expect("picker info");

    assert_eq!(picker.first_user_message, "first inherited message");
    assert_eq!(picker.user_message_count, 2);
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
    let state = factory_state(&store, "usage-read-error", 0).await;
    store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit head");
    rusqlite::Connection::open(factory.catalog_path())
        .expect("open catalog")
        .execute("DROP TABLE usage_deltas", [])
        .expect("drop usage table");

    assert!(
        store.load_session().await.is_err(),
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
