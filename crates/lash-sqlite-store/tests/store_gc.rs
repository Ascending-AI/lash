use lash_core::store::GraphCommitDelta;
use lash_core::{
    HydratedSessionCheckpoint, LeaseOwnerIdentity, ModelSpec, PersistedSessionConfig,
    PersistedTurnState, PluginSessionSnapshot, RuntimeCommit, RuntimeSessionState,
    SessionCommitStore, SessionExecutionLeaseStore, SessionGraph, SessionHead, SessionPolicy,
    SessionStoreCreateRequest, SessionStoreFactory, TokenUsage, ToolState,
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
    factory
        .create_store(&request("delete/me"))
        .await
        .expect("create deleted session");
    factory
        .create_store(&request("keep/me"))
        .await
        .expect("create retained session");

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
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
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

    assert!(matches!(error, lash_core::StoreError::Backend(_)));
    assert!(
        second
            .load_session(lash_core::SessionReadScope::FullGraph)
            .await
            .expect("load second session")
            .is_none(),
        "the failed cross-session collision must roll back the whole commit"
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
