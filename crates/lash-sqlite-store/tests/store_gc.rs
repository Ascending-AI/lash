// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_core_store::plugin_state::PluginState;
use lash_core_store::session_policy::SessionPolicy;
use lash_core_store::session_state::RuntimeSessionState;
use lash_core_store::session_store_factory_types::SessionCreationHead;
use lash_core_store::session_store_factory_types::SessionStoreCreateRequest;
use lash_core_store::store::FleetFormatStore;
use lash_core_store::store::GraphAppend;
use lash_core_store::store::RuntimeCommit;
use lash_core_store::store::SessionCatalogStore;
use lash_core_store::store::SessionCommitStore;
use lash_core_store::store::SessionHistoryStore;
use lash_core_store::store::SessionLookup;
use lash_core_store::store::StoreError;
use lash_core_store::store::StoreMaintenance;
use lash_core_store::store::TurnInputStore;
use lash_core_store::store::WindowSelector;
use lash_kernel_value::identity::SessionId;
use lash_sqlite_store::{BlobArtifactDescriptor, SqliteStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn catalog_uri(root: &Path) -> PathBuf {
    root.join("lash.db")
}

async fn admit_store(
    catalog: &Arc<SqliteStore>,
    request: &SessionStoreCreateRequest,
) -> Result<Arc<SqliteStore>, StoreError> {
    catalog.admit_session(request).await?;
    Ok(Arc::clone(catalog))
}

fn recorded_llm_profile(id: &str) -> Option<lash_core::LlmProfileConfig> {
    Some(lash_core::testing::test_llm_profile_config(
        id,
        lash_core::testing::test_llm_profile_metadata(id),
    ))
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
        session_id: SessionId::fixture(session_id.to_string()),
        head_revision,
        ..RuntimeSessionState::fixture(SessionPolicy::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
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
    let plugin_state = PluginState {
        plugins: Default::default(),
    };
    let mut state = RuntimeSessionState {
        session_id: SessionId::from("root"),
        turn_index: 1,
        ..RuntimeSessionState::fixture(lash_core_store::session_policy::SessionPolicy::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ))
    };
    state.set_plugin_state(Some(plugin_state));
    store
        .admit_session(&SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: state.session_id.clone(),
            relation: lash_core_store::session_identity::SessionRelation::Root,
            config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
                &state.policy.clone(),
            ),
            head: SessionCreationHead::Config,
            retention: lash_core_store::store::Retention::UntilGc,
        })
        .await
        .expect("bind session to store");
    state.ensure_agent_frame_initialized();
    let stored = store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
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
    // The root is a SessionManifest node, read back through its manifest
    // rather than as an artifact blob: it survived the sweep.
    let checkpoint = store
        .get_checkpoint(&stored.checkpoint_ref)
        .await
        .expect("read checkpoint")
        .expect("checkpoint manifest");
    let plugin_ref = checkpoint
        .component_ref(lash_core_store::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        .expect("plugin snapshot ref")
        .clone();
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
async fn sqlite_factory_creates_metadata_once_and_preserves_on_reopen() {
    let root = unique_temp_dir("metadata");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );
    let request = SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("chat/alpha"),
        relation: lash_core_store::session_identity::SessionRelation::Child {
            parent_session_id: SessionId::from("preserved-parent"),
            caused_by: None,
        },
        config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
            &SessionPolicy {
                model: recorded_llm_profile("first-model"),
                ..SessionPolicy::new(
                    lash_protocol_types::session_model::TurnBudget::Unbounded,
                    lash_protocol_types::session_model::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                )
            },
        ),
        head: SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };

    let store = admit_store(&factory, &request).await.expect("create store");
    let meta = store
        .load_session_meta(&request.session_id)
        .await
        .expect("load meta")
        .expect("meta");
    assert_eq!(meta.session_id, "chat/alpha");
    assert_eq!(
        meta.parent_session_id().map(|id| id.as_str()),
        Some("preserved-parent")
    );

    let reopened = admit_store(
        &factory,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            relation: lash_core_store::session_identity::SessionRelation::Root,
            config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
                &SessionPolicy {
                    model: recorded_llm_profile("second-model"),
                    ..SessionPolicy::new(
                        lash_protocol_types::session_model::TurnBudget::Unbounded,
                        lash_protocol_types::session_model::MaxToolCalls::new(1024),
                        lash_core::NoProgressBudget::bounded(12),
                    )
                },
            ),
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
    assert_eq!(
        reopened_meta.parent_session_id().map(|id| id.as_str()),
        Some("preserved-parent")
    );
}

#[tokio::test]
async fn sqlite_factory_delete_session_removes_only_the_selected_session() {
    let root = unique_temp_dir("delete-session");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );
    let request = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id.to_string()),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
            &SessionPolicy {
                model: recorded_llm_profile("model"),
                ..SessionPolicy::new(
                    lash_protocol_types::session_model::TurnBudget::Unbounded,
                    lash_protocol_types::session_model::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                )
            },
        ),
        head: SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    let deleted_store = admit_store(&factory, &request(&SessionId::from("delete/me")))
        .await
        .expect("create deleted session");
    admit_store(&factory, &request(&SessionId::from("keep/me")))
        .await
        .expect("create retained session");
    let mut deleted_state = factory_state(&deleted_store, &SessionId::from("delete/me"), 0).await;
    deleted_state.set_execution_graph(lash_core_store::testing::store_fixtures::execution_graph(
        "deleted session's execution state",
    ));
    deleted_store
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&deleted_state))
        .await
        .expect("commit deleted session checkpoint");
    // Capture the complete surviving graph before collection, so a lost
    // descendant cannot disappear from both the expected and actual sets.
    let retained_config = {
        let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
        let mut query = conn
            .prepare(
                "WITH RECURSIVE reachable(hash) AS (
                     SELECT revision.config_ref FROM session_head AS head
                     JOIN session_revisions AS revision
                       ON revision.session_id = head.session_id
                      AND revision.head_revision = head.head_revision
                     WHERE head.session_id = 'keep/me'
                     UNION
                     SELECT edge.child_hash FROM blob_edges AS edge
                     JOIN reachable ON edge.parent_hash = reachable.hash
                 ) SELECT lower(hex(hash)) FROM reachable",
            )
            .expect("surviving config closure query");
        query
            .query_map([], |row| row.get::<_, String>(0))
            .expect("surviving config closure")
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()
            .expect("surviving config identities")
    };
    assert!(retained_config.len() > 1, "the config has descendants");
    {
        let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
        conn.execute(
            "INSERT INTO blobs (hash, content, encoding) VALUES (zeroblob(32), X'02', 0)",
            [],
        )
        .expect("insert host artifact blob");
        conn.execute(
            "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
             VALUES ('kernel_document', 'shared-module', zeroblob(32))",
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
    // The delete held its roots; the collection retires what they reach.
    factory.gc_unreachable().await.expect("collect");

    assert!(catalog_uri(&root).exists());
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
             WHERE namespace = 'kernel_document' AND artifact_ref = 'shared-module'",
            [],
            |row| row.get(0),
        )
        .expect("count host refs");
    assert_eq!(
        host_ref_count, 1,
        "factory artifact refs without session attribution remain host-owned"
    );
    let blobs = || {
        conn.prepare("SELECT lower(hex(hash)) FROM blobs ORDER BY hash")
            .expect("retained blob query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("retained blobs")
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()
            .expect("retained blob identities")
    };
    let host_artifact = std::collections::BTreeSet::from([
        "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
    ]);
    assert_eq!(
        blobs(),
        retained_config.union(&host_artifact).cloned().collect(),
        "delete retains exactly the host artifact and the surviving config's reachable closure"
    );

    factory
        .delete_session(&SessionId::from("keep/me"))
        .await
        .expect("delete final config referrer");
    factory
        .gc_unreachable()
        .await
        .expect("collect final config");
    assert_eq!(
        blobs(),
        host_artifact,
        "the last session releases every config node, including shared descendants"
    );
}

#[tokio::test]
async fn sqlite_catalog_partitions_derived_node_ids_by_session() {
    let root = unique_temp_dir("global-node-id");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );
    let store_for = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id.to_string()),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
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
        let frame_key = lash_protocol_types::FrameKey::from_caller_material("shared-frame-key")
            .expect("non-empty frame material");
        let frame_node_id = lash_core_execution::facade_support::frame_node_id(
            &state.session_id,
            frame_key.as_str(),
        );
        let node = lash_core_store::session_graph::SessionNodeRecord {
            node_id: lash_vocabulary::identity::NodeId::fixture(frame_node_id.to_string()),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: lash_core_store::session_graph::SessionNodePayload::FrameOpen {
                frame_key,
                reason: lash_core_store::session_identity::AgentFrameReason::initial(),
                assignment: lash_core_store::session_identity::AgentFrameAssignment::unconfigured(
                    SessionPolicy::new(
                        lash_protocol_types::session_model::TurnBudget::Unbounded,
                        lash_protocol_types::session_model::MaxToolCalls::new(1024),
                        lash_core::NoProgressBudget::bounded(12),
                    ),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(state);
        commit.graph = GraphAppend::Extend {
            nodes: vec![node.clone()],
        };
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

    let frame_key = lash_protocol_types::FrameKey::from_caller_material("shared-frame-key")
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
            .contains_active_ancestor(
                &first_state.session_id,
                &lash_vocabulary::identity::NodeId::fixture(first_node_id.to_string())
            )
            .await
            .unwrap()
    );
    assert!(
        second
            .contains_active_ancestor(
                &second_state.session_id,
                &lash_vocabulary::identity::NodeId::fixture(second_node_id.to_string())
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sqlite_catalog_leaf_validation_is_session_scoped() {
    let root = unique_temp_dir("leaf-scope");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );
    let request = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id.to_string()),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    };
    let first = admit_store(&factory, &request(&SessionId::from("leaf-a")))
        .await
        .expect("first store");
    let second = admit_store(&factory, &request(&SessionId::from("leaf-b")))
        .await
        .expect("second store");
    let first_state = factory_state(&first, &SessionId::from("leaf-a"), 0).await;
    let second_state = factory_state(&second, &SessionId::from("leaf-b"), 0).await;
    let frame_key = lash_protocol_types::FrameKey::from_caller_material("leaf-a-node")
        .expect("non-empty frame material");
    let frame_node_id = lash_core_execution::facade_support::frame_node_id(
        &first_state.session_id,
        frame_key.as_str(),
    );
    let node = lash_core_store::session_graph::SessionNodeRecord {
        node_id: lash_vocabulary::identity::NodeId::fixture(frame_node_id.to_string()),
        parent_node_id: None,
        timestamp: "2026-07-26T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: lash_core_store::session_graph::SessionNodePayload::FrameOpen {
            frame_key,
            reason: lash_core_store::session_identity::AgentFrameReason::initial(),
            assignment: lash_core_store::session_identity::AgentFrameAssignment::unconfigured(
                SessionPolicy::new(
                    lash_protocol_types::session_model::TurnBudget::Unbounded,
                    lash_protocol_types::session_model::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                ),
            ),
        },
    };
    let mut first_commit = RuntimeCommit::persisted_state_for_test(&first_state);
    first_commit.graph = GraphAppend::Extend {
        nodes: vec![node.clone()],
    };
    first
        .commit_runtime_state(first_commit)
        .await
        .expect("commit first session node");

    second
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&second_state))
        .await
        .expect("another session's live node must not invalidate an empty session");

    let mut second_state = second_state;
    second_state.head_revision = 1;
    let resident_leaf = second_state.session_graph.leaf_node_id.clone();
    let mut cross_session_leaf = RuntimeCommit::persisted_state_for_test(&second_state);
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
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );
    let request = |session_id: &SessionId| SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id.to_string()),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
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
            lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
                "maintenance-b",
                lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
                lash_core_store::turn_input_vocabulary::TurnInput::text("dedupe fence"),
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
            lash_core_store::turn_input_vocabulary::PendingTurnInputDraft::new(
                "maintenance-b",
                lash_core_store::turn_input_vocabulary::TurnInputIngress::NextTurn,
                lash_core_store::turn_input_vocabulary::TurnInput::text("dedupe fence"),
            )
            .with_source_key(source_key),
        )
        .await
        .expect("replay second input");
    assert_eq!(replay.input_id, cancelled.input_id);
    assert_eq!(
        replay.state.kind(),
        lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled
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
    let conn = rusqlite::Connection::open(catalog_uri(root)).expect("open catalog");
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
) -> (Arc<SqliteStore>, lash_vocabulary::identity::NodeId) {
    let store = admit_store(
        factory,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core_store::session_identity::SessionRelation::Root,
            config: lash_core_store::session_graph::PersistedSessionConfig::new(
                lash_protocol_types::session_model::TurnBudget::Unbounded,
                lash_protocol_types::session_model::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            head: SessionCreationHead::Config,
            retention: lash_core_store::store::Retention::UntilGc,
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
        .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
        .await
        .expect("commit root node");
    (store, leaf)
}

/// A pin is deleted with its session, so the owner's delete reclaims the
/// pinned leaf itself: no row is left tombstoned for a later delete to drain,
/// and no pin or revision row outlives the session.
///
/// The owner's store handle is dropped before its delete on purpose: a live
/// handle can still vacuum its own session and would mask a leak.
#[tokio::test]
async fn sqlite_delete_reclaims_a_pinned_leaf_with_its_session() {
    let root = unique_temp_dir("pinned-at-delete");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );

    let leaf = {
        let (store, leaf) =
            commit_single_root_node(&factory, &SessionId::from("orphan-owner")).await;
        drop(store);
        leaf
    };
    factory
        .pin(
            &SessionId::from("orphan-owner"),
            &lash_core_store::store::Target::Revision(1),
        )
        .await
        .expect("pin the owner's committed revision");
    factory
        .delete_session(&SessionId::from("orphan-owner"))
        .await
        .expect("delete owner session");

    assert!(
        resident_tombstoned_node_ids(&root).is_empty(),
        "the delete must not leave the pinned leaf tombstoned"
    );
    assert!(
        !resident_graph_node_ids(&root).contains(&leaf.to_string()),
        "the pinned leaf must be physically gone with its session"
    );
    let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
    for table in ["pins", "session_revisions"] {
        let rows: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id = 'orphan-owner'"),
                [],
                |row| row.get(0),
            )
            .expect("count the deleted session's rows");
        assert_eq!(rows, 0, "{table} must hold nothing of a deleted session");
    }
}

/// Fork ancestry owned by a session deleted *before* its child is the second
/// orphaning flow: the ancestry is only tombstoned when the child is deleted, by
/// which time its owner is already unbindable. The same delete must reclaim it.
#[tokio::test]
async fn sqlite_delete_reclaims_fork_ancestry_orphaned_by_earlier_owner_delete() {
    let root = unique_temp_dir("orphan-fork-ancestry");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );

    let parent_leaf = {
        let (store, leaf) =
            commit_single_root_node(&factory, &SessionId::from("orphan-fork-parent")).await;
        drop(store);
        leaf
    };
    let policy = SessionPolicy::new(
        lash_protocol_types::session_model::TurnBudget::Unbounded,
        lash_protocol_types::session_model::MaxToolCalls::new(1024),
        lash_core::NoProgressBudget::bounded(12),
    );
    factory
        .fork_session(
            &lash_core_store::session_store_factory_types::ForkSessionRequest {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from("orphan-fork-child"),
                source_session_id: SessionId::from("orphan-fork-parent"),
                head_revision: 1,
                relation: lash_core_store::session_identity::SessionRelation::Root,
                config: lash_core_store::session_graph::PersistedSessionConfig::from_policy(
                    &policy.clone(),
                ),
                retention: lash_core_store::store::Retention::UntilGc,
            },
        )
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
        let mut child_state = lash_core_store::store::window_state(read, child.fleet_format())
            .expect("adopt child window")
            .state;
        let parent_node_id = child_state.session_graph.leaf_node_id.clone();
        child_state
            .session_graph
            .apply_append(&lash_core_store::store::GraphAppend::Extend {
                nodes: vec![lash_core_store::session_graph::SessionNodeRecord {
                    node_id: lash_vocabulary::identity::NodeId::from("orphan-fork-child-node"),
                    parent_node_id,
                    timestamp: "2026-08-17T00:00:00.000000000Z"
                        .parse()
                        .expect("canonical node timestamp"),
                    payload: lash_core_store::session_graph::SessionNodePayload::Event {
                        event: lash_protocol_types::session_model::SessionHistoryRecord::Protocol(
                            lash_protocol_types::session_model::ProtocolEvent::typed(
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
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&child_state))
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

/// A delete reclaims exactly the tombstoned rows that have no live owner
/// (FIG-1519). A live session's meta row exists before its first graph node
/// commits and goes only with the delete that records its id, so a missing
/// owner is the reclaim frontier: residue stranded under an id the deleted set
/// never recorded drains, while a live session's tombstones stay resident for
/// its own vacuum.
#[tokio::test]
async fn sqlite_delete_reclaims_tombstones_with_no_live_owner() {
    let root = unique_temp_dir("ownerless-tombstones");
    let factory = std::sync::Arc::new(
        SqliteStore::open(
            &catalog_uri(&root),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open catalog"),
    );
    drop(commit_single_root_node(&factory, &SessionId::from("live-owner")).await);
    drop(commit_single_root_node(&factory, &SessionId::from("deleted-now")).await);
    {
        let conn = rusqlite::Connection::open(catalog_uri(&root)).expect("open catalog");
        for (owner, node_id) in [
            ("live-owner", "live-owner-tombstone"),
            ("stranded-owner", "stranded-tombstone"),
        ] {
            conn.execute(
                "INSERT INTO graph_nodes
                     (session_id, node_id, parent_node_id, generation, frame_node_id,
                      body_bytes, node_json, tombstoned)
                 VALUES (?1, ?2, NULL, 99, ?2, 2, '{}', 1)",
                rusqlite::params![owner, node_id],
            )
            .expect("seed a tombstoned row");
        }
        let recorded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM deleted_sessions WHERE session_id = 'stranded-owner'",
                [],
                |row| row.get(0),
            )
            .expect("probe the deleted set");
        assert_eq!(recorded, 0, "the stranded owner is outside the deleted set");
    }

    factory
        .delete_session(&SessionId::from("deleted-now"))
        .await
        .expect("delete an unrelated session");

    assert_eq!(
        resident_tombstoned_node_ids(&root),
        vec!["live-owner-tombstone".to_string()],
        "the delete drains the ownerless tombstone and leaves the live owner's to its vacuum"
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
