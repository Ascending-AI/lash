//! Runs the shared `ProcessRegistry` conformance suite against SQLite.

use lash_sansio::sync::MutexExt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lash_core::runtime::RuntimeScope;
use lash_core::testing::conformance::{
    FenceIntegrityHandles, FenceIntegrityInjector, FenceIntegrityObservation, FenceIntegrityTarget,
    GraphFactObservation, GraphIntegrityCorruption, GraphIntegrityHandles, GraphIntegrityInjector,
    GraphIntegrityRead, GraphIntegrityTarget, LineageConformanceHandles,
    LineageConformanceInjector, ReopenableProcessRegistry, ReopenableRuntimePersistence,
    ReopenableTriggerStore, SessionExecutionLeaseRenewalZeroRowHandles,
    SessionExecutionLeaseRenewalZeroRowInjector,
};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    ProcessExecutionEnvStore, ProcessRegistry, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimePersistence, SessionCommitStore, SessionStoreFactory, TriggerStore,
};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteProcessRegistry,
    SqliteRuntimeEffectController, SqliteSessionStoreFactory, SqliteTriggerStore, Store,
};
use tempfile::TempDir;

#[path = "conformance/pre_frame_key.rs"]
mod pre_frame_key;
#[path = "conformance/session_delete_blob_reclaim.rs"]
mod session_delete_blob_reclaim;
#[path = "conformance/trigger_occurrence_retention.rs"]
mod trigger_occurrence_retention;

struct SqliteLineageConformanceInjector {
    path: PathBuf,
    _dir: TempDir,
}

#[async_trait::async_trait]
impl LineageConformanceInjector for SqliteLineageConformanceInjector {
    async fn force_lineage(&self, session_id: &str, ancestor_node_id: &str) {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let (ancestor_session_id, generation): (String, i64) = conn
            .query_row(
                "SELECT session_id, generation FROM graph_nodes WHERE node_id = ?1",
                rusqlite::params![ancestor_node_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read false-lineage ancestor facts");
        conn.execute(
            "INSERT OR REPLACE INTO fork_lineage
             (session_id, ancestor_session_id, fork_node_id, fork_generation)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                session_id,
                ancestor_session_id,
                ancestor_node_id,
                generation
            ],
        )
        .expect("inject false SQLite lineage");
    }

    async fn tombstone_node(&self, node_id: &str) {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        assert_eq!(
            conn.execute(
                "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                rusqlite::params![node_id],
            )
            .expect("tombstone intermediate SQLite node"),
            1
        );
    }

    async fn lineage_ancestors(
        &self,
        session_id: &str,
    ) -> Vec<lash_core::store::ForkLineageAncestor> {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let mut stmt = conn
            .prepare(
                "SELECT ancestor_session_id, fork_node_id, fork_generation FROM fork_lineage
                 WHERE session_id = ?1 ORDER BY ancestor_session_id",
            )
            .expect("prepare SQLite lineage observation");
        stmt.query_map(rusqlite::params![session_id], |row| {
            Ok(lash_core::store::ForkLineageAncestor {
                ancestor_session_id: row.get(0)?,
                fork_node_id: row.get(1)?,
                fork_generation: u64::try_from(row.get::<_, i64>(2)?)
                    .expect("non-negative fork generation"),
            })
        })
        .expect("query SQLite lineage observation")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect SQLite lineage observation")
    }

    async fn edge_path(&self, session_id: &str) -> Vec<GraphFactObservation> {
        let mut facts = self.all_graph_facts().await;
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let mut current = conn
            .query_row(
                "SELECT leaf_node_id FROM session_head WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("read SQLite lineage head");
        let mut path = Vec::new();
        while let Some(node_id) = current {
            let index = facts
                .iter()
                .position(|fact| fact.node_id == node_id)
                .expect("edge-path node exists in raw SQLite facts");
            let fact = facts.swap_remove(index);
            current = fact.parent_node_id.clone();
            path.push(fact);
        }
        path.reverse();
        path
    }

    async fn all_graph_facts(&self) -> Vec<GraphFactObservation> {
        let conn = rusqlite::Connection::open(&self.path).expect("open SQLite lineage catalog");
        let mut stmt = conn
            .prepare(
                "SELECT node.node_id, node.parent_node_id, node.session_id,
                        node.generation, node.frame_node_id,
                        json_extract(node.node_json, '$.kind') = 'frame_open'
                 FROM graph_nodes AS node
                 ORDER BY node.generation, node.node_id",
            )
            .expect("prepare SQLite graph facts");
        stmt.query_map([], |row| {
            Ok(GraphFactObservation {
                node_id: row.get(0)?,
                parent_node_id: row.get(1)?,
                owning_session_id: row.get(2)?,
                generation: u64::try_from(row.get::<_, i64>(3)?).expect("non-negative generation"),
                frame_node_id: row.get(4)?,
                is_frame: row.get(5)?,
            })
        })
        .expect("query SQLite graph facts")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect SQLite graph facts")
    }
}

fn sqlite_lineage_handles() -> LineageConformanceHandles {
    let dir = tempfile::tempdir().expect("SQLite lineage tempdir");
    let path = dir.path().join("durable-core.db");
    LineageConformanceHandles {
        factory: Arc::new(SqliteSessionStoreFactory::new(dir.path())),
        injector: Arc::new(SqliteLineageConformanceInjector { path, _dir: dir }),
    }
}

#[tokio::test]
async fn sqlite_fork_lineage_conformance() {
    lash_core::testing::conformance::fork_lineage_conformance(sqlite_lineage_handles()).await;
}

#[tokio::test]
async fn sqlite_fork_lineage_no_carrier_law() {
    lash_core::testing::conformance::fork_lineage_no_carrier_law(sqlite_lineage_handles()).await;
}

#[tokio::test]
async fn sqlite_fork_plan_matches_edge_walk_law() {
    lash_core::testing::conformance::fork_plan_matches_edge_walk_law(sqlite_lineage_handles())
        .await;
}

#[path = "../../lash-core/tests/support/cold_process_turn_parent.rs"]
mod cold_process_turn_parent;

fn fresh_db_path(dirs: &Arc<Mutex<Vec<TempDir>>>, file_name: &str) -> PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(file_name);
    dirs.lock_recover().push(dir);
    path
}

fn durable_turn_scope(session_id: impl Into<String>, turn_id: impl Into<String>) -> ExecutionScope {
    let session_id = session_id.into();
    ExecutionScope::turn(&session_id, turn_id)
}

async fn open_ephemeral_effect_controller(
    scope: ExecutionScope,
) -> (TempDir, SqliteRuntimeEffectController) {
    let dir = tempfile::tempdir().expect("effect replay tempdir");
    let controller =
        SqliteRuntimeEffectController::open(&dir.path().join("effect-replay.db"), scope)
            .await
            .expect("file-backed effect controller");
    (dir, controller)
}

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

fn open_registry(path: &Path) -> Arc<dyn ProcessRegistry> {
    let path = path.to_path_buf();
    let sessions = path.with_extension("sessions");
    Arc::new(sync_await(async move {
        SqliteProcessRegistry::open(&path, sessions)
            .await
            .expect("file registry")
    })) as Arc<dyn ProcessRegistry>
}

fn open_store(path: &Path) -> Arc<dyn RuntimePersistence> {
    let path = path.to_path_buf();
    Arc::new(sync_await(async move {
        Store::open(&path).await.expect("file store")
    })) as Arc<dyn RuntimePersistence>
}

fn open_store_with_clock(
    path: &Path,
    clock: Arc<dyn lash_core::Clock>,
) -> Arc<dyn RuntimePersistence> {
    let path = path.to_path_buf();
    Arc::new(sync_await(async move {
        Store::open_with_clock(&path, clock)
            .await
            .expect("clock-driven file store")
    })) as Arc<dyn RuntimePersistence>
}

struct SqliteSessionExecutionLeaseRenewalZeroRowInjector {
    path: PathBuf,
    _dir: TempDir,
}

#[async_trait::async_trait]
impl SessionExecutionLeaseRenewalZeroRowInjector
    for SqliteSessionExecutionLeaseRenewalZeroRowInjector
{
    async fn arm(&self, session_id: &str) {
        assert_eq!(session_id, "zero-row-session-lease-renewal");
        rusqlite::Connection::open(&self.path)
            .expect("open SQLite zero-row renewal injector")
            .execute_batch(
                "CREATE TRIGGER lash_test_session_lease_renewal_zero_row
                 BEFORE UPDATE OF lease_expires_at_ms ON session_execution_leases
                 WHEN OLD.session_id = 'zero-row-session-lease-renewal'
                 BEGIN
                     SELECT RAISE(IGNORE);
                 END;",
            )
            .expect("arm SQLite zero-row renewal trigger");
    }

    async fn disarm(&self) {
        rusqlite::Connection::open(&self.path)
            .expect("open SQLite zero-row renewal injector for cleanup")
            .execute_batch("DROP TRIGGER lash_test_session_lease_renewal_zero_row;")
            .expect("disarm SQLite zero-row renewal trigger");
    }
}

#[tokio::test]
async fn sqlite_zero_row_session_execution_lease_renewal_is_refused() {
    let dir = tempfile::tempdir().expect("SQLite zero-row renewal tempdir");
    let path = dir.path().join("zero-row-renewal.db");
    let store = Arc::new(
        Store::open(&path)
            .await
            .expect("open SQLite zero-row renewal store"),
    );
    lash_core::testing::conformance::session_execution_lease_zero_row_renewal_is_refused(
        SessionExecutionLeaseRenewalZeroRowHandles {
            store: store as Arc<dyn RuntimePersistence>,
            injector: Arc::new(SqliteSessionExecutionLeaseRenewalZeroRowInjector {
                path,
                _dir: dir,
            }),
        },
    )
    .await;
}

fn artifact_store_handles(
    path: &Path,
) -> lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
    let path = path.to_path_buf();
    let store = Arc::new(sync_await(async move {
        Store::open(&path).await.expect("file artifact store")
    }));
    lash_lashlang_runtime::testing::conformance::ArtifactStoreHandles {
        artifacts: Arc::clone(&store) as Arc<dyn lashlang::LashlangArtifactStore>,
        process_env: store as Arc<dyn ProcessExecutionEnvStore>,
    }
}

fn open_trigger_store(path: &Path) -> Arc<dyn TriggerStore> {
    let path = path.to_path_buf();
    Arc::new(sync_await(async move {
        SqliteTriggerStore::open(&path)
            .await
            .expect("file trigger store")
    })) as Arc<dyn TriggerStore>
}

struct SqliteFenceIntegrityInjector {
    _dir: TempDir,
    runtime_path: PathBuf,
    trigger_path: PathBuf,
}

impl SqliteFenceIntegrityInjector {
    fn connection(&self, target: &FenceIntegrityTarget) -> rusqlite::Connection {
        let path = match target {
            FenceIntegrityTarget::TriggerRevision { .. } => &self.trigger_path,
            _ => &self.runtime_path,
        };
        rusqlite::Connection::open(path).expect("open raw SQLite fence fixture")
    }
}

#[async_trait::async_trait]
impl FenceIntegrityInjector for SqliteFenceIntegrityInjector {
    async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64) {
        let conn = self.connection(target);
        let changed = match target {
            FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => conn.execute(
                "UPDATE queued_work_batches SET claim_fencing_token = ?1 WHERE batch_id = ?2",
                rusqlite::params![value, batch_id],
            ),
            FenceIntegrityTarget::SessionHeadRevision { session_id } => conn.execute(
                "UPDATE session_head SET head_revision = ?1 WHERE session_id = ?2",
                rusqlite::params![value, session_id],
            ),
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => conn.execute(
                "UPDATE session_execution_leases SET lease_fencing_token = ?1 WHERE session_id = ?2",
                rusqlite::params![value, session_id],
            ),
            FenceIntegrityTarget::TriggerRevision { subscription_id } => conn.execute(
                "UPDATE trigger_subscriptions
                 SET revision = ?1,
                     record_json = json_set(record_json, '$.revision', ?1)
                 WHERE subscription_id = ?2",
                rusqlite::params![value, subscription_id],
            ),
        }
        .expect("inject raw SQLite fence value");
        assert_eq!(
            changed, 1,
            "raw SQLite fence injection must target one row: {target:?}"
        );
    }

    async fn observe_raw_value(&self, target: &FenceIntegrityTarget) -> FenceIntegrityObservation {
        let conn = self.connection(target);
        match target {
            FenceIntegrityTarget::QueuedWorkClaimFence { batch_id } => conn
                .query_row(
                    "SELECT claim_fencing_token, claim_id, claim_token,
                            claim_session_lease_generation
                     FROM queued_work_batches WHERE batch_id = ?1",
                    [batch_id],
                    |row| {
                        let value: i64 = row.get(0)?;
                        let claim_id: Option<String> = row.get(1)?;
                        let claim_token: Option<String> = row.get(2)?;
                        let generation: i64 = row.get(3)?;
                        Ok(FenceIntegrityObservation {
                            value,
                            mutation_fingerprint: format!(
                                "{claim_id:?}:{claim_token:?}:{generation}"
                            ),
                        })
                    },
                )
                .expect("observe SQLite queued-work fence"),
            FenceIntegrityTarget::SessionHeadRevision { session_id } => conn
                .query_row(
                    "SELECT head_revision, head_json, leaf_node_id, checkpoint_ref
                     FROM session_head WHERE session_id = ?1",
                    [session_id],
                    |row| {
                        let value: i64 = row.get(0)?;
                        let head_json: String = row.get(1)?;
                        let leaf: Option<String> = row.get(2)?;
                        let checkpoint: Option<String> = row.get(3)?;
                        Ok(FenceIntegrityObservation {
                            value,
                            mutation_fingerprint: format!("{head_json}:{leaf:?}:{checkpoint:?}"),
                        })
                    },
                )
                .expect("observe SQLite session-head revision"),
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => conn
                .query_row(
                    "SELECT lease_fencing_token, lease_owner_id, lease_token,
                            lease_claimed_at_ms, lease_expires_at_ms
                     FROM session_execution_leases WHERE session_id = ?1",
                    [session_id],
                    |row| {
                        let value: i64 = row.get(0)?;
                        let owner: Option<String> = row.get(1)?;
                        let token: Option<String> = row.get(2)?;
                        let claimed: i64 = row.get(3)?;
                        let expires: i64 = row.get(4)?;
                        Ok(FenceIntegrityObservation {
                            value,
                            mutation_fingerprint: format!(
                                "{owner:?}:{token:?}:{claimed}:{expires}"
                            ),
                        })
                    },
                )
                .expect("observe SQLite session-lease fence"),
            FenceIntegrityTarget::TriggerRevision { subscription_id } => conn
                .query_row(
                    "SELECT revision, record_json, enabled, tombstoned
                     FROM trigger_subscriptions WHERE subscription_id = ?1",
                    [subscription_id],
                    |row| {
                        let value: i64 = row.get(0)?;
                        let json: String = row.get(1)?;
                        let enabled: i64 = row.get(2)?;
                        let tombstoned: i64 = row.get(3)?;
                        Ok(FenceIntegrityObservation {
                            value,
                            mutation_fingerprint: format!("{json}:{enabled}:{tombstoned}"),
                        })
                    },
                )
                .expect("observe SQLite trigger revision"),
        }
    }
}

#[tokio::test]
async fn sqlite_fence_integrity_conformance() {
    lash_core::testing::conformance::fence_integrity_conformance(|case| async move {
        let dir = tempfile::tempdir().expect("SQLite fence fixture tempdir");
        let runtime_path = dir.path().join(format!("{case}-runtime.db"));
        let trigger_path = dir.path().join(format!("{case}-triggers.db"));
        let runtime = Arc::new(
            Store::open(&runtime_path)
                .await
                .expect("open SQLite runtime fixture"),
        );
        let triggers = Arc::new(
            SqliteTriggerStore::open(&trigger_path)
                .await
                .expect("open SQLite trigger fixture"),
        );
        FenceIntegrityHandles {
            runtime,
            triggers,
            injector: Arc::new(SqliteFenceIntegrityInjector {
                _dir: dir,
                runtime_path,
                trigger_path,
            }),
        }
    })
    .await;
}

struct SqliteGraphIntegrityInjector {
    _dir: TempDir,
    runtime_path: PathBuf,
    runtime: Arc<Store>,
}

#[async_trait::async_trait]
impl GraphIntegrityInjector for SqliteGraphIntegrityInjector {
    async fn inject(&self, target: &GraphIntegrityTarget) {
        let conn =
            rusqlite::Connection::open(&self.runtime_path).expect("open raw SQLite graph fixture");
        match target.corruption {
            GraphIntegrityCorruption::OrphanLeaf => {
                let changed = conn
                    .execute(
                        "UPDATE graph_nodes SET parent_node_id = ?1 WHERE node_id = ?2",
                        rusqlite::params![target.missing_node_id, target.leaf_node_id],
                    )
                    .expect("inject orphaned SQLite graph leaf");
                assert_eq!(changed, 1);
            }
            GraphIntegrityCorruption::DuplicateNodeId => {
                conn.execute_batch(
                    "ALTER TABLE graph_nodes RENAME TO graph_nodes_valid;
                     CREATE TABLE graph_nodes (
                         seq INTEGER PRIMARY KEY,
                         session_id TEXT NOT NULL,
                         node_id TEXT NOT NULL,
                         parent_node_id TEXT,
                         generation INTEGER NOT NULL,
                         frame_node_id TEXT NOT NULL,
                         node_json TEXT NOT NULL,
                         tombstoned INTEGER NOT NULL DEFAULT 0
                     );
                     INSERT INTO graph_nodes SELECT * FROM graph_nodes_valid;
                     DROP TABLE graph_nodes_valid;
                     CREATE INDEX idx_graph_nodes_session_seq ON graph_nodes(session_id, seq);
                     CREATE INDEX idx_graph_nodes_parent ON graph_nodes(parent_node_id);",
                )
                .expect("remove SQLite graph-node uniqueness for corruption injection");
                let changed = conn
                    .execute(
                        "INSERT INTO graph_nodes (
                             session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned
                         )
                         SELECT session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned
                         FROM graph_nodes WHERE node_id = ?1 LIMIT 1",
                        rusqlite::params![target.leaf_node_id],
                    )
                    .expect("inject duplicate SQLite graph node id");
                assert_eq!(changed, 1);
            }
            GraphIntegrityCorruption::DanglingLeafId => {
                let changed = conn
                    .execute(
                        "UPDATE session_head SET leaf_node_id = ?1 WHERE session_id = ?2",
                        rusqlite::params![target.missing_node_id, target.session_id],
                    )
                    .expect("inject dangling SQLite graph leaf id");
                assert_eq!(changed, 1);
            }
            GraphIntegrityCorruption::ParentCycle => {
                if target.read == GraphIntegrityRead::ActivePath {
                    let changed = conn
                        .execute(
                            "UPDATE graph_nodes SET parent_node_id = ?1 WHERE node_id = ?2",
                            rusqlite::params![target.leaf_node_id, target.root_node_id],
                        )
                        .expect("inject active SQLite graph parent cycle");
                    assert_eq!(changed, 1);
                } else {
                    let node_a_id = format!("{}-a", target.missing_node_id);
                    let node_b_id = format!("{}-b", target.missing_node_id);
                    let insert = |node_id: &str, parent_node_id: &str, generation_offset: i64| {
                        conn.execute(
                            "INSERT INTO graph_nodes (
                                 session_id, node_id, parent_node_id, generation, frame_node_id, node_json, tombstoned
                             )
                             SELECT session_id, ?1, ?2, generation + ?4, frame_node_id, node_json, tombstoned
                             FROM graph_nodes WHERE node_id = ?3 LIMIT 1",
                            rusqlite::params![
                                node_id,
                                parent_node_id,
                                target.leaf_node_id,
                                generation_offset
                            ],
                        )
                        .expect("inject inactive SQLite graph cycle node")
                    };
                    assert_eq!(insert(&node_a_id, &node_b_id, 1), 1);
                    assert_eq!(insert(&node_b_id, &node_a_id, 2), 1);
                }
            }
        }
    }

    async fn load_whole_graph(
        &self,
        _session_id: &str,
    ) -> Result<lash_core::SessionGraph, lash_core::StoreError> {
        self.runtime.load_session_graph().await
    }
}

#[tokio::test]
async fn sqlite_graph_integrity_conformance() {
    lash_core::testing::conformance::graph_integrity_conformance(|case| async move {
        let dir = tempfile::tempdir().expect("SQLite graph fixture tempdir");
        let runtime_path = dir.path().join(format!("{case}-runtime.db"));
        let runtime = Arc::new(
            Store::open(&runtime_path)
                .await
                .expect("open SQLite graph fixture"),
        );
        GraphIntegrityHandles {
            runtime: Arc::clone(&runtime) as Arc<dyn RuntimePersistence>,
            injector: Arc::new(SqliteGraphIntegrityInjector {
                _dir: dir,
                runtime_path,
                runtime,
            }),
        }
    })
    .await;
}

#[tokio::test]
async fn sqlite_load_session_graph_accepts_healthy_non_empty_session() {
    let store = Arc::new(
        Store::memory()
            .await
            .expect("open healthy SQLite graph store"),
    );
    let session_id = "healthy-whole-session-graph";
    let mut state = lash_core::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized();
    state
        .session_graph
        .append_plugin("healthy-whole-graph", serde_json::json!({"second": true}));
    store
        .admit_and_bind_session(&lash_core::SessionBinding::root(session_id))
        .await
        .expect("bind healthy SQLite graph session");
    store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &state,
            &[],
        ))
        .await
        .expect("seed healthy SQLite graph session");

    let graph = store
        .load_session_graph()
        .await
        .expect("healthy whole-session graph loads");
    assert!(graph.nodes.len() >= 2);
    let leaf_node_id = graph
        .leaf_node_id
        .as_deref()
        .expect("loaded graph has a leaf");
    assert!(graph.nodes.iter().any(|node| node.node_id == leaf_node_id));
}

#[tokio::test]
async fn sqlite_signed_counter_write_domain_conformance() {
    let store = Arc::new(
        Store::memory()
            .await
            .expect("open SQLite signed-write fixture"),
    );
    lash_core::testing::conformance::signed_counter_write_domain_conformance(store).await;
}

#[tokio::test]
async fn sqlite_artifact_store_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_lashlang_runtime::testing::conformance::artifact_store_reopenable(|| {
        let path = fresh_db_path(&dirs, "artifacts.db");
        let reopen_path = path.clone();
        lash_lashlang_runtime::testing::conformance::ReopenableArtifactStore {
            open: artifact_store_handles(&path),
            reopen: Arc::new(move || artifact_store_handles(&reopen_path)),
        }
    })
    .await;
}

fn exec_envelope(replay_key: &str, code: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("effect-session", "effect-turn", 1, 0),
            replay_key,
            RuntimeEffectKind::ExecCode,
            replay_key,
        ),
        RuntimeEffectCommand::ExecCode {
            language: "code".to_string(),
            code: code.to_string(),
        },
    )
}

fn exec_outcome(marker: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::ExecCode {
        result: Box::new(Ok(lash_core::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            terminal_finish: Some(serde_json::json!(marker)),
        })),
    }
}

fn assert_exec_marker(outcome: RuntimeEffectOutcome, expected: &str) {
    let RuntimeEffectOutcome::ExecCode { result } = outcome else {
        panic!("expected exec-code outcome");
    };
    let response = result.expect("exec-code response");
    assert_eq!(response.terminal_finish, Some(serde_json::json!(expected)));
}

fn returning_executor(marker: &'static str) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(exec_outcome(marker)) })
}

fn failing_executor() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async move {
        Err(RuntimeEffectControllerError::foreign(
            "test_local_executor_called",
            "replay must not invoke the local executor",
        ))
    })
}

fn current_epoch_ms_for_test() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_registry_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::process_registry_reopenable(|| {
        let path = fresh_db_path(&dirs, "processes.db");
        ReopenableProcessRegistry {
            open: open_registry(&path),
            reopen: open_registry(&path),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_registry_pagination_satisfies_conformance() {
    let dir = tempfile::tempdir().expect("pagination tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open pagination registry"),
    ) as Arc<dyn ProcessRegistry>;
    lash_core::testing::conformance::process_registry_pagination(registry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_prune_batch_tombstones_are_ordered() {
    let dir = tempfile::tempdir().expect("batch prune tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open batch prune registry"),
    );
    lash_core::testing::conformance::process_prune_batch_tombstones(registry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_prune_scopes_to_the_retention_filter() {
    let dir = tempfile::tempdir().expect("scoped prune tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open scoped prune registry"),
    );
    lash_core::testing::conformance::process_prune_scoped_by_originator(registry).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_leased_completion_replay_repairs_projection() {
    let dir = tempfile::tempdir().expect("leased replay repair tempdir");
    let path = dir.path().join("processes.db");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&path, dir.path().join("sessions"))
            .await
            .expect("open leased replay repair registry"),
    );
    let corruption_path = path.clone();
    lash_core::testing::conformance::leased_completion_replay_repairs_projection(
        registry as Arc<dyn ProcessRegistry>,
        move |stale| async move {
            let conn = rusqlite::Connection::open(corruption_path)
                .expect("open projection corruption connection");
            let changed = conn
                .execute(
                    "UPDATE processes SET record_json = ?2 WHERE process_id = ?1",
                    rusqlite::params![
                        stale.id,
                        serde_json::to_string(&stale).expect("encode stale process projection")
                    ],
                )
                .expect("corrupt SQLite process projection");
            assert_eq!(changed, 1);
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_store_contract_state_machine_properties() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::store_contract_state_machine("sqlite", move |seed, _| {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("store-contract tempdir");
            let registry_path = dir.path().join(format!("processes-{seed}.db"));
            let runtime_path = dir.path().join(format!("runtime-{seed}.db"));
            let sessions = dir.path().join("sessions");
            let registry = Arc::new(
                SqliteProcessRegistry::open(&registry_path, sessions)
                    .await
                    .expect("open property process registry"),
            ) as Arc<dyn ProcessRegistry>;
            let runtime = Arc::new(
                Store::open(&runtime_path)
                    .await
                    .expect("open property runtime store"),
            ) as Arc<dyn RuntimePersistence>;
            dirs.lock_recover().push(dir);
            lash_core::testing::conformance::StoreContractHandles { registry, runtime }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runtime_persistence_state_machine_properties() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::runtime_persistence_state_machine("sqlite", move |_| {
        let dirs = Arc::clone(&dirs);
        async move {
            let dir = tempfile::tempdir().expect("runtime-persistence property tempdir");
            let process_registry_path = dir.path().join("processes.db");
            SqliteProcessRegistry::open(&process_registry_path, dir.path().join("sessions"))
                .await
                .expect("open property process registry");
            let handles =
                lash_core::testing::conformance::RuntimePersistenceStateMachineHandles::create(
                    Arc::new(SqliteSessionStoreFactory::new_with_process_registry(
                        dir.path(),
                        process_registry_path,
                    )),
                    true,
                )
                .await
                .expect("create SQLite runtime-persistence property handles");
            dirs.lock_recover().push(dir);
            handles
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_session_graph_state_machine_properties() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    Box::pin(
        lash_core::testing::conformance::session_graph_state_machine("sqlite", move |_| {
            let dirs = Arc::clone(&dirs);
            async move {
                let dir = tempfile::tempdir().expect("session-graph property tempdir");
                let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()))
                    as Arc<dyn SessionStoreFactory>;
                dirs.lock_recover().push(dir);
                factory
            }
        }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_process_continuation_store_satisfies_conformance() {
    let storage = Arc::new(
        SqliteProcessRegistry::memory()
            .await
            .expect("open continuation store"),
    );
    let registry = Arc::clone(&storage) as Arc<dyn lash_core::ProcessRegistry>;
    let store = storage as Arc<dyn lash_core::ProcessContinuationStore>;
    lash_core::testing::conformance::process_continuation_store(registry, store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_wake_delivery_crash_matrix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_registry_path = dir.path().join("processes.db");
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &process_registry_path,
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
            dir.path().join("sessions"),
        )
        .await
        .expect("open process registry")
        .with_wake_delivery_config(
            lash_core::WakeDeliveryConfig::new(10_000)
                .expect("valid test retention")
                .with_enqueuing_stale_after_ms(25)
                .expect("valid short stale-claim age"),
        ),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(
        SqliteSessionStoreFactory::new_with_process_registry(dir.path(), process_registry_path)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn SessionStoreFactory>;
    Box::pin(lash_core::testing::conformance::wake_delivery_crash_matrix(
        factory, registry, clock,
    ))
    .await;
}

#[tokio::test]
async fn sqlite_process_registry_rejects_pre_unit_external_owner_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-unit-external-owner-processes.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy process db");
    conn.pragma_update(None, "user_version", 12)
        .expect("stamp legacy process schema");
    drop(conn);

    let error = match SqliteProcessRegistry::open(&path, dir.path().join("sessions")).await {
        Ok(_) => panic!("pre-unit-external-owner process stores must be recreated"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash process registry schema"));
    assert!(message.contains("supports schema version 24"));
    assert!(message.contains("delete the process registry database and start fresh"));
}

#[tokio::test]
async fn sqlite_session_store_factory_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let unbound = Store::memory().await.expect("unbound durable-core store");
    let unbound = Some(Arc::new(unbound) as Arc<dyn lash_core::StoreMaintenance>);
    lash_core::testing::conformance::session_store_factory("sqlite", unbound, || {
        let dir = tempfile::tempdir().expect("tempdir");
        let factory =
            Arc::new(SqliteSessionStoreFactory::new(dir.path())) as Arc<dyn SessionStoreFactory>;
        dirs.lock_recover().push(dir);
        factory
    })
    .await;
}

#[tokio::test]
async fn sqlite_fresh_session_admission_returns_created() {
    lash_core::testing::conformance::fresh_session_admission_returns_created(|_| {
        Arc::new(sync_await(Store::memory()).expect("in-memory SQLite store"))
            as Arc<dyn RuntimePersistence>
    })
    .await;
}

#[tokio::test]
async fn sqlite_fork_observer_intent_transient_failure_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    lash_core::testing::conformance::fork_observer_intent_transient_failure(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    ))
    .await;
}

#[tokio::test]
async fn sqlite_session_graph_append_branch_liveness_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    lash_core::testing::conformance::session_graph_append_branch_liveness(Arc::new(
        SqliteSessionStoreFactory::new(dir.path()),
    )
        as Arc<dyn SessionStoreFactory>)
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_attachment_owner_cold_replay_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(lash_core::testing::TestClock::new(
        current_epoch_ms_for_test().saturating_sub(100_000),
    ));
    let process_path = dir.path().join("processes.db");
    let registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &process_path,
            clock.clone(),
            dir.path().join("sessions"),
        )
        .await
        .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(
        SqliteSessionStoreFactory::new_with_process_registry(
            dir.path().join("sessions"),
            &process_path,
        )
        .with_clock(clock.clone()),
    ) as Arc<dyn SessionStoreFactory>;
    let effect_path = dir.path().join("effects.db");
    let scope = durable_turn_scope("attachment-owner-cold-replay", "attachment-owner-turn");
    let first = Arc::new(
        SqliteRuntimeEffectController::open_with_clock(&effect_path, scope.clone(), clock.clone())
            .await
            .expect("first effect controller"),
    ) as Arc<dyn RuntimeEffectController>;
    let reopen_effect_controller = {
        let effect_path = effect_path.clone();
        let clock = clock.clone();
        Arc::new(move || {
            let effect_path = effect_path.clone();
            let scope = scope.clone();
            let clock = clock.clone();
            Box::pin(async move {
                Arc::new(
                    SqliteRuntimeEffectController::open_with_clock(&effect_path, scope, clock)
                        .await
                        .expect("cold replay effect controller"),
                ) as Arc<dyn RuntimeEffectController>
            })
                as std::pin::Pin<Box<dyn Future<Output = Arc<dyn RuntimeEffectController>> + Send>>
        })
    };
    let advance_clock = {
        let clock = clock.clone();
        Arc::new(move |duration_ms| clock.advance(duration_ms)) as Arc<dyn Fn(u64) + Send + Sync>
    };

    lash_core::testing::conformance::attachment_owner_cold_replay(
        lash_core::testing::conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store: Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_process_prune_deletes_owned_session_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_path = dir.path().join("processes.db");
    let sessions = dir.path().join("sessions");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&process_path, &sessions)
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(SqliteSessionStoreFactory::new_with_process_registry(
        &sessions,
        &process_path,
    )) as Arc<dyn SessionStoreFactory>;

    lash_core::testing::conformance::process_prune_deletes_owned_session_stores(factory, registry)
        .await;
}

#[tokio::test]
async fn sqlite_store_uses_injected_clock_for_expiry() {
    let clock = Arc::new(lash_core::testing::TestClock::new(20_000));
    let store = Arc::new(
        Store::memory_with_clock(clock.clone())
            .await
            .expect("clock-driven sqlite store"),
    ) as Arc<dyn RuntimePersistence>;
    lash_core::testing::conformance::runtime_persistence_clock_expiry(store, |duration_ms| {
        clock.advance(duration_ms);
    })
    .await;
}

#[tokio::test]
async fn sqlite_trigger_store_satisfies_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    lash_core::testing::conformance::trigger_store_reopenable(|| {
        let path = fresh_db_path(&dirs, "triggers.db");
        ReopenableTriggerStore {
            open: open_trigger_store(&path),
            reopen: open_trigger_store(&path),
        }
    })
    .await;
}

#[tokio::test]
async fn sqlite_trigger_store_rejects_pre_keyed_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-keyed-triggers.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy trigger db");
    conn.pragma_update(None, "user_version", 1)
        .expect("stamp legacy trigger schema");
    drop(conn);

    let error = match SqliteTriggerStore::open(&path).await {
        Ok(_) => panic!("pre-keyed trigger stores must be recreated"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash trigger store schema"));
    assert!(message.contains("supports schema version 6"));
    assert!(message.contains("delete the trigger store database and start fresh"));
}

#[tokio::test]
async fn sqlite_effect_controller_rejects_pre_intent_journal_schema_before_serving() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-canonical-envelope-effects.db");
    let conn = rusqlite::Connection::open(&path).expect("open legacy effect db");
    conn.pragma_update(None, "user_version", 8)
        .expect("stamp legacy effect schema");
    drop(conn);

    let error =
        match SqliteRuntimeEffectController::open(&path, durable_turn_scope("session", "turn"))
            .await
        {
            Ok(_) => panic!("pre-intent effect stores must be recreated"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(message.contains("Unsupported lash effect replay schema"));
    assert!(message.contains("supports schema version 11"));
    assert!(message.contains(
        "drain affected sessions and recreate the whole Lash trust domain with this version"
    ));
}

#[tokio::test]
async fn sqlite_trigger_ingress_skips_malformed_matching_subscription() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("malformed-trigger.db");
    let source_type = "ui.button.pressed";
    let source_key =
        lash_core::facade_support::empty_trigger_source_key(source_type).expect("source key");
    let store = SqliteTriggerStore::open(&path)
        .await
        .expect("open trigger store");
    let register = |owner: &str, key: &str| lash_core::TriggerCommand::Register {
        owner_scope: lash_core::TriggerOwnerScope::session(owner),
        actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(owner)),
        draft: lash_core::TriggerSubscriptionDraft::for_process(
            key,
            lash_core::ProcessExecutionEnvRef::new(format!("process-env:{owner}")),
            source_type,
            source_key.clone(),
            lash_core::ProcessInput::Engine {
                kind: "test".to_string(),
                payload: serde_json::json!({ "owner": owner }),
            },
            lash_core::ProcessIdentity::new("test"),
        )
        .with_payload_schema(lash_core::LashSchema::any()),
    };
    let malformed = store
        .execute_command("register-malformed", register("malformed", "malformed-key"))
        .await
        .expect("execute malformed registration")
        .expect("register malformed row");
    let current = store
        .execute_command("register-current", register("current", "current-key"))
        .await
        .expect("execute current registration")
        .expect("register current row");
    let lash_core::TriggerCommandOutcome::Mutation { receipt: malformed } = malformed else {
        panic!("expected malformed registration receipt")
    };
    let lash_core::TriggerCommandOutcome::Mutation { receipt: current } = current else {
        panic!("expected current registration receipt")
    };
    drop(store);

    let conn = rusqlite::Connection::open(&path).expect("open raw trigger db");
    conn.execute(
        "UPDATE trigger_subscriptions SET record_json = ?2 WHERE subscription_id = ?1",
        rusqlite::params![malformed.subscription_id.as_str(), "{not valid json"],
    )
    .expect("poison trigger row");
    drop(conn);

    let reopened = SqliteTriggerStore::open(&path)
        .await
        .expect("reopen trigger store");
    let ingress = reopened
        .ingest_occurrence(lash_core::TriggerOccurrenceRequest::new(
            source_type,
            source_key,
            serde_json::json!({ "button": "Blue" }),
            "malformed-row-occurrence",
        ))
        .await
        .expect("one malformed row must not halt trigger ingress");
    assert_eq!(ingress.reservations.len(), 1);
    assert_eq!(
        ingress.reservations[0].subscription.subscription_id,
        current.subscription_id
    );
}

#[tokio::test]
async fn sqlite_store_satisfies_runtime_persistence_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let clock = Arc::new(lash_core::testing::TestClock::new(10_000));
    let store_clock = Arc::clone(&clock);
    lash_core::testing::conformance::runtime_persistence_reopenable(
        move |session_id| {
            let dir = tempfile::tempdir().expect("runtime-persistence conformance tempdir");
            let factory_dir = dir.path().to_path_buf();
            let session_id = session_id.to_string();
            let clock = store_clock.clone();
            let (open, reopen) = sync_await(async move {
                let factory = SqliteSessionStoreFactory::new(factory_dir)
                    .with_clock(clock as Arc<dyn lash_core::Clock>);
                let request = lash_core::SessionStoreCreateRequest {
                    session_id,
                    relation: lash_core::SessionRelation::Root,
                    policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                };
                let open = factory
                    .create_store(&request)
                    .await
                    .expect("create explicitly bound SQLite conformance store");
                let reopen = factory
                    .open_existing_store(&request)
                    .await
                    .expect("open explicit SQLite conformance store")
                    .expect("created SQLite conformance store exists");
                (open, reopen)
            });
            dirs.lock_recover().push(dir);
            ReopenableRuntimePersistence { open, reopen }
        },
        lash_core::testing::conformance::RuntimePersistenceLeaseTiming::controlled({
            let clock = Arc::clone(&clock);
            move |duration_ms| clock.advance(duration_ms)
        }),
    )
    .await;
}

#[tokio::test]
async fn sqlite_unbound_session_reads_resolve_the_same_session() {
    let dir = tempfile::tempdir().expect("unbound-read tempdir");
    let root = dir.path().to_path_buf();
    lash_core::testing::conformance::unbound_session_reads_resolve_the_same_session(
        move |admission_state| {
            let axis_root = root.join(format!("{admission_state:?}"));
            async move {
                std::fs::create_dir_all(&axis_root).expect("create unbound-read axis directory");
                let factory = Arc::new(SqliteSessionStoreFactory::new(&axis_root));
                let path = factory.catalog_path();
                lash_core::testing::conformance::UnboundSessionResolutionHandles {
                    backend_name: "SQLite",
                    factory,
                    open_unbound: Arc::new(move || open_store(&path)),
                }
            }
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_store_enforces_core_lease_fence_authority() {
    let store = Store::memory().await.expect("in-memory SQLite store");
    lash_core::testing::conformance::session_execution_lease_fence_authority(&store).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_runtime_persistence_recovery_laws() {
    let dir = tempfile::tempdir().expect("store-recovery tempdir");
    let clock = Arc::new(lash_core::testing::TestClock::new(10_000));
    let store_clock = Arc::clone(&clock);
    lash_core::testing::conformance::runtime_persistence_recovery_laws(
        move |scenario| {
            open_store_with_clock(
                &dir.path().join(format!("store-recovery-{scenario}.db")),
                Arc::clone(&store_clock) as Arc<dyn lash_core::Clock>,
            )
        },
        lash_core::testing::conformance::StoreRecoveryLeaseTiming::controlled(move |duration_ms| {
            clock.advance(duration_ms)
        }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_real_turn_crash_matrix() {
    let dir = tempfile::tempdir().expect("real-turn crash matrix tempdir");
    Box::pin(lash_core::testing::conformance::turn_crash_matrix_level_1(
        |scenario| open_store(&dir.path().join(format!("turn-crash-matrix-{scenario}.db"))),
    ))
    .await;
}

#[tokio::test]
async fn sqlite_complete_runtime_checkpoint_component_set_survives_cold_reopens() {
    let dir = tempfile::tempdir().expect("checkpoint-component tempdir");
    let path = dir.path().join("checkpoint-components.db");
    lash_core::testing::conformance::complete_runtime_checkpoint_component_set_survives_cold_reopens(|| {
        open_store(&path)
    })
    .await;
}

#[tokio::test]
async fn sqlite_append_receipt_replays_after_ancestor_superseded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-receipt-ancestor.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    let mutation_path = path.clone();
    lash_core::testing::conformance::append_request_receipt_replays_after_ancestor_superseded(
        store as Arc<dyn RuntimePersistence>,
        move |leaf_node_id| async move {
            let conn = rusqlite::Connection::open(mutation_path).expect("open raw sqlite");
            conn.execute(
                "UPDATE session_head
                 SET leaf_node_id = ?1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
                rusqlite::params![leaf_node_id],
            )
            .expect("switch sqlite active branch");
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_inactive_append_ancestor_precedes_stale_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-precedence.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    let mutation_path = path.clone();
    lash_core::testing::conformance::inactive_append_ancestor_precedes_stale_head(
        store as Arc<dyn RuntimePersistence>,
        move |leaf_node_id| async move {
            let conn = rusqlite::Connection::open(mutation_path).expect("open raw sqlite");
            conn.execute(
                "UPDATE session_head
                 SET leaf_node_id = ?1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
                rusqlite::params![leaf_node_id],
            )
            .expect("switch sqlite active branch");
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_tombstoned_old_leaf_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tombstoned-old-leaf.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    let mutation_path = path.clone();
    lash_core::testing::conformance::tombstoned_old_leaf_is_rejected(
        store as Arc<dyn RuntimePersistence>,
        move |node_id| async move {
            let conn = rusqlite::Connection::open(mutation_path).expect("open raw sqlite");
            conn.execute(
                "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                rusqlite::params![node_id],
            )
            .expect("tombstone sqlite old leaf");
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_append_receipt_restores_mixed_usage_envelope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(&dir.path().join("append-receipt-mixed-envelope.db"))
            .await
            .expect("open store"),
    );
    lash_core::testing::conformance::append_receipt_mixed_usage_envelope(store).await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn sqlite_cancelled_queued_append_publishes_usage_exactly_once() {
    use lash_sqlite_store::testing::{SqliteFaultInjector, SqliteFaultPoint};

    let dir = tempfile::tempdir().expect("tempdir");
    let injector = SqliteFaultInjector::default();
    let factory = SqliteSessionStoreFactory::new(dir.path()).with_fault_injector(injector.clone());
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "root".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create cancellation store");
    lash_core::testing::conformance::append_usage_cancellation_publishes_exactly_once(
        store,
        move || {
            let pause = injector.pause_after(SqliteFaultPoint::BeforeCommit, 1);
            async move {
                pause.wait_until_reached().await;
                move || pause.release()
            }
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_old_format_append_receipt_returns_public_leaf() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("append-receipt-old-format.db");
    let store = Arc::new(Store::open(&path).await.expect("open store"));
    lash_core::testing::conformance::old_format_append_receipt_returns_public_leaf(
        store,
        move || async move {
            let conn = rusqlite::Connection::open(path).expect("open raw SQLite receipt fixture");
            let result_json: String = conn
                .query_row(
                    "SELECT result_json FROM runtime_turn_commits
                     WHERE turn_id LIKE '%old-format-append-receipt%'
                       AND turn_id NOT LIKE '%old-format-append-receipt-seed%'",
                    [],
                    |row| row.get(0),
                )
                .expect("read runtime receipt JSON");
            let mut result: serde_json::Value =
                serde_json::from_str(&result_json).expect("decode runtime receipt JSON");
            let fields = result.as_object_mut().expect("receipt result object");
            fields.remove("committed_leaf_node_id");
            fields.remove("receipt_replayed");
            conn.execute(
                "UPDATE runtime_turn_commits
                 SET result_json = ?1
                 WHERE turn_id LIKE '%old-format-append-receipt%'
                   AND turn_id NOT LIKE '%old-format-append-receipt-seed%'",
                rusqlite::params![serde_json::to_string(&result).expect("encode old receipt")],
            )
            .expect("install raw pre-upgrade receipt fixture");
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_store_schema_excludes_embedded_turn_replay_tables() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("schema.db");
    drop(Store::open(&path).await.expect("open store"));
    let conn = rusqlite::Connection::open(&path).expect("open raw sqlite");
    for removed in [
        concat!("runtime_", "turn_", "checkpoints"),
        concat!("runtime_", "effect_", "journal"),
    ] {
        let count = raw_count(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            removed,
        );
        assert_eq!(count, 0, "{removed} table must not exist");
    }
    let turn_commits = raw_count(
        &conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        "runtime_turn_commits",
    );
    assert_eq!(turn_commits, 1);
}

#[tokio::test]
async fn sqlite_runtime_turn_receipt_identity_columns_are_nullable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("receipt-schema.db");
    drop(Store::open(&path).await.expect("open store"));
    let conn = rusqlite::Connection::open(path).expect("open raw sqlite");
    let mut stmt = conn
        .prepare("PRAGMA table_info(runtime_turn_commits)")
        .expect("prepare receipt schema query");
    let columns = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
        })
        .expect("query receipt schema")
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()
        .expect("collect receipt schema");
    for column in [
        "request_identity_hash",
        "requested_node_count",
        "requested_ancestor_node_id",
        "identity_encoding_version",
    ] {
        assert_eq!(columns.get(column), Some(&0), "{column} must allow NULL");
    }
}

fn raw_count(conn: &rusqlite::Connection, sql: &str, name: &str) -> i64 {
    conn.query_row(sql, rusqlite::params![name], |row| row.get::<_, i64>(0))
        .expect("query sqlite_master")
}

#[tokio::test]
async fn sqlite_effect_host_satisfies_scope_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("effect-host.db");
    lash_core::testing::conformance::effect_host(move || {
        let path = path.clone();
        Arc::new(sync_await(async move {
            SqliteEffectHost::open(&path).await.expect("effect host")
        })) as Arc<dyn EffectHost>
    })
    .await;
}

#[tokio::test]
async fn sqlite_public_signal_intent_wakes_a_parked_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let effect_host = Arc::new(
        SqliteEffectHost::open(&dir.path().join("signal-intent-effects.db"))
            .await
            .expect("open SQLite signal-intent effect host"),
    ) as Arc<dyn EffectHost>;
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("signal-intent-processes.db"),
            dir.path().join("signal-intent-sessions"),
        )
        .await
        .expect("open SQLite signal-intent process registry"),
    ) as Arc<dyn ProcessRegistry>;
    lash_core::testing::conformance::public_signal_intent_wakes_parked_process(
        "sqlite-public-signal-intent",
        effect_host,
        registry,
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_host_and_controller_reject_non_file_backed_path_spellings() {
    for path in [
        "",
        ":memory:",
        "file::memory:?cache=shared",
        "file:temporary",
    ] {
        let error = match SqliteEffectHost::open(Path::new(path)).await {
            Ok(_) => panic!("effect hosts must reject non-file-backed path {path:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a file-backed database path"),
            "unexpected error for {path:?}: {error}"
        );

        let error = match SqliteRuntimeEffectController::open(
            Path::new(path),
            durable_turn_scope("guard-session", "guard-turn"),
        )
        .await
        {
            Ok(_) => panic!("effect controllers must reject non-file-backed path {path:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("requires a file-backed database path"),
            "unexpected controller error for {path:?}: {error}"
        );
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn sqlite_completion_key_permission_tracks_backing_not_replay_ownership() {
    let memory =
        SqliteRuntimeEffectController::memory(durable_turn_scope("memory-session", "memory-turn"))
            .await
            .expect("testing-only memory controller");
    assert_eq!(
        memory.replay_ownership(),
        lash_core::EffectReplayOwnership::Controller
    );
    assert!(!memory.allows_process_lifetime_completion_keys());

    let (_file_dir, file) =
        open_ephemeral_effect_controller(durable_turn_scope("file-session", "file-turn")).await;
    assert_eq!(
        file.replay_ownership(),
        lash_core::EffectReplayOwnership::Controller
    );
    assert!(file.allows_process_lifetime_completion_keys());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_effect_host_satisfies_cold_instance_await_event_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold-await-event.db");
    lash_core::testing::conformance::effect_host_await_events_cold_instance(|| {
        let path = path.clone();
        Arc::new(sync_await(async move {
            SqliteEffectHost::open(&path)
                .await
                .expect("cold SQLite effect host")
        })) as Arc<dyn EffectHost>
    })
    .await;
}

#[tokio::test]
async fn sqlite_await_event_key_mint_is_pure_and_store_secret_is_stable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pure-await-event-key.db");
    let scope = durable_turn_scope("pure-key-session", "pure-key-turn");
    let wait = AwaitEventWaitIdentity::tool_completion("pure-key-call");

    let (first, second) = tokio::join!(
        async {
            SqliteEffectHost::open(&path)
                .await
                .expect("first concurrent host")
                .await_event_key(&scope, wait.clone())
                .await
                .expect("first concurrent key")
        },
        async {
            SqliteEffectHost::open(&path)
                .await
                .expect("second concurrent host")
                .await_event_key(&scope, wait.clone())
                .await
                .expect("second concurrent key")
        },
    );
    assert_eq!(
        first, second,
        "concurrent openers must read one store secret"
    );

    let connection = rusqlite::Connection::open(&path).expect("open raw effect database");
    let wait_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM await_event_waits", [], |row| {
            row.get(0)
        })
        .expect("count await-event waits");
    assert_eq!(wait_count, 0, "key mint must not register a promise row");
    let secret_shape: (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), length(MAX(signing_secret)) FROM await_event_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("inspect await-event signer");
    assert_eq!(secret_shape, (1, 32));
}

/// Every promise path reports one decode vocabulary.
///
/// The terminal used to be decoded twice in two places: inside the SQL layer on
/// the resolve path, where a corrupt row surfaced as `sqlite_await_event_store`,
/// and above it on the observe paths as `sqlite_await_event_decode`. One
/// coordinator decodes once, so a corrupt row is a decode failure everywhere —
/// which is also what PostgreSQL always reported.
#[tokio::test]
async fn sqlite_await_event_terminal_decode_failures_report_the_decode_vocabulary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt-await-event-terminal.db");
    let scope = durable_turn_scope("corrupt-terminal-session", "corrupt-terminal-turn");
    let host = SqliteEffectHost::open(&path)
        .await
        .expect("SQLite effect host");
    let key = host
        .await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("call"))
        .await
        .expect("mint key");
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("winner")))
            .await
            .expect("resolve promise"),
        ResolveOutcome::Accepted
    );

    let connection = rusqlite::Connection::open(&path).expect("open raw effect database");
    connection
        .execute(
            "UPDATE await_event_waits SET terminal_json = ?2 WHERE key_id = ?1",
            rusqlite::params![key.key_id.as_str(), "not-json"],
        )
        .expect("corrupt the persisted terminal");
    drop(connection);

    let peek_error = host
        .peek_await_event(&key)
        .await
        .expect_err("corrupt terminal must fail the peek");
    let resolve_error = host
        .resolve_await_event(&key, Resolution::Cancelled)
        .await
        .expect_err("corrupt terminal must fail the duplicate resolve");
    for error in [peek_error, resolve_error] {
        assert_eq!(error.code.as_str(), "sqlite_await_event_decode");
    }
}

/// SQLite promise rows are stamped by the host's injected clock.
///
/// The store runs in its host's clock domain, so durable await-event records are
/// reproducible under an injected clock rather than reading the OS clock behind
/// the host's back.
#[tokio::test]
async fn sqlite_await_event_rows_are_stamped_by_the_injected_clock() {
    const INJECTED_MS: u64 = 1_234_567_890_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("injected-clock-await-event.db");
    let clock = Arc::new(lash_core::testing::TestClock::new(INJECTED_MS));
    let host =
        SqliteEffectHost::open_with_clock(&path, Arc::clone(&clock) as Arc<dyn lash_core::Clock>)
            .await
            .expect("SQLite effect host on an injected clock");
    let key = host
        .await_event_key(
            &durable_turn_scope("injected-clock-session", "injected-clock-turn"),
            AwaitEventWaitIdentity::tool_completion("call"),
        )
        .await
        .expect("mint key");
    assert_eq!(
        host.resolve_await_event(&key, Resolution::Ok(serde_json::json!("stamped")))
            .await
            .expect("resolve promise"),
        ResolveOutcome::Accepted
    );

    let connection = rusqlite::Connection::open(&path).expect("open raw effect database");
    let stamps: (i64, i64) = connection
        .query_row(
            "SELECT created_at_ms, resolved_at_ms FROM await_event_waits WHERE key_id = ?1",
            rusqlite::params![key.key_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read promise stamps");
    assert_eq!(stamps, (INJECTED_MS as i64, INJECTED_MS as i64));
}

/// SQLite's authoritative effect-lease clock is the host's injected `Clock`,
/// because this store shares its host's clock domain. PostgreSQL — the other
/// implementor of the same shared `EffectReplayDriver` — deliberately reads the
/// *server* clock instead (the `Clock` contract's database-authoritative lease
/// boundary, fenced by `postgres_clock_contract`), so
/// each half of that split needs its own referee now that one driver drives
/// both. The driver's own clock only sleeps; if it ever stamped a row, the
/// stamps below would come from the OS clock instead.
#[tokio::test]
async fn sqlite_effect_replay_rows_are_stamped_by_the_injected_clock() {
    const INJECTED_MS: u64 = 1_234_567_890_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("injected-clock-effect-replay.db");
    let clock = Arc::new(lash_core::testing::TestClock::new(INJECTED_MS));
    let controller = SqliteRuntimeEffectController::open_with_clock(
        &path,
        durable_turn_scope(
            "injected-clock-effect-session",
            "injected-clock-effect-turn",
        ),
        Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
    )
    .await
    .expect("SQLite effect controller on an injected clock");

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let executor_release = Arc::clone(&release);
    let executing = tokio::spawn({
        let controller = controller.clone();
        async move {
            controller
                .execute_effect(
                    exec_envelope("injected-clock-effect", "first"),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        let _ = entered_tx.send(());
                        executor_release.notified().await;
                        Ok(exec_outcome("stamped"))
                    }),
                )
                .await
        }
    });
    entered_rx.await.expect("executor entered under the claim");

    let claim_path = path.clone();
    let (created_at_ms, lease_expires_at_ms) = tokio::task::spawn_blocking(move || {
        let connection = rusqlite::Connection::open(&claim_path).expect("open raw effect journal");
        connection
            .query_row(
                "SELECT created_at_ms, lease_expires_at_ms
                 FROM runtime_effect_replay WHERE replay_key = ?1",
                rusqlite::params!["injected-clock-effect"],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("read claimed lease stamps")
    })
    .await
    .expect("read the in-progress claim");
    assert_eq!(
        created_at_ms, INJECTED_MS as i64,
        "the claim stamp must come from the injected clock"
    );
    assert_eq!(
        lease_expires_at_ms,
        (INJECTED_MS + lash_core::facade_support::LeaseTimings::default().ttl_ms()) as i64,
        "the lease expiry must be derived from the injected claim instant"
    );

    release.notify_waiters();
    assert_exec_marker(
        executing
            .await
            .expect("execution task joins")
            .expect("finalize the claimed effect"),
        "stamped",
    );

    let connection = rusqlite::Connection::open(&path).expect("reopen raw effect journal");
    let (updated_at_ms, released_lease): (i64, i64) = connection
        .query_row(
            "SELECT updated_at_ms, lease_expires_at_ms
             FROM runtime_effect_replay WHERE replay_key = ?1",
            rusqlite::params!["injected-clock-effect"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read finalized stamps");
    assert_eq!(
        updated_at_ms, INJECTED_MS as i64,
        "the finalize stamp must come from the injected clock"
    );
    assert_eq!(released_lease, 0, "finalizing releases the lease");
}

#[tokio::test]
async fn sqlite_effect_host_satisfies_cold_process_await_event_conformance() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::process::Command;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold-process-await-event.db");
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(env!("CARGO_BIN_EXE_sqlite-await-event-helper"))
            .arg(&path)
            .arg(identity)
            .arg(&nonce)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn cold-process helper for {identity}: {error}"));
        let stdout = child.stdout.take().expect("helper stdout pipe");
        let mut lines = BufReader::new(stdout).lines();
        let encoded_key =
            tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("helper did not mint {identity} key"))
                .expect("read helper key")
                .unwrap_or_else(|| panic!("helper exited before printing {identity} key"));
        let key: AwaitEventKey = serde_json::from_str(&encoded_key)
            .unwrap_or_else(|error| panic!("decode helper {identity} key: {error}"));

        child
            .kill()
            .await
            .unwrap_or_else(|error| panic!("kill parked {identity} helper: {error}"));
        let status = child
            .wait()
            .await
            .unwrap_or_else(|error| panic!("reap parked {identity} helper: {error}"));
        assert!(
            !status.success(),
            "killed {identity} helper exited successfully"
        );

        let resolver = Arc::new(
            SqliteEffectHost::open(&path)
                .await
                .expect("cold-process resolver"),
        );
        let terminal = if identity == "turn_cancel_gate" {
            let address = lash_core::runtime::TurnAddress::new(
                format!("cold-process-{nonce}-session"),
                format!("cold-process-{nonce}-turn"),
            );
            let receipt = lash_core::runtime::TurnWorkDriver::new(
                Arc::clone(&resolver) as Arc<dyn EffectHost>
            )
            .request_cancel(lash_core::runtime::TurnCancelRequest::new(
                address,
                format!("cold-process-{nonce}-cancel"),
                None,
            ))
            .await
            .expect("request cancellation through a successor owner");
            assert!(matches!(
                receipt.outcome,
                lash_core::runtime::TurnCancelOutcome::Requested(_)
            ));
            resolver
                .peek_await_event(&key)
                .await
                .expect("peek successor cancellation")
                .expect("successor cancellation resolves the killed owner's gate")
        } else {
            let terminal = Resolution::Ok(serde_json::json!({
                "cold_process": true,
                "identity": identity,
                "nonce": nonce,
            }));
            assert_eq!(
                resolver
                    .resolve_await_event(&key, terminal.clone())
                    .await
                    .unwrap_or_else(|error| panic!(
                        "resolve killed-helper {identity} key: {error}"
                    )),
                ResolveOutcome::Accepted
            );
            terminal
        };
        drop(resolver);

        let observer = SqliteEffectHost::open(&path)
            .await
            .expect("cold-process observer");
        assert_eq!(
            observer
                .peek_await_event(&key)
                .await
                .unwrap_or_else(|error| panic!("peek killed-helper {identity} key: {error}")),
            Some(terminal.clone())
        );
        assert_eq!(
            observer
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .unwrap_or_else(|error| panic!("observe killed-helper {identity} key: {error}")),
            terminal
        );
    }
}

#[tokio::test]
async fn sqlite_effect_replay_satisfies_cold_process_crash_conformance() {
    use tokio::process::Command;

    let dir = tempfile::tempdir().expect("cold-process effect replay tempdir");
    let database = dir.path().join("cold-process-effect-replay.db");
    let marker = dir.path().join("external-effect.log");
    let nonce = uuid::Uuid::new_v4().to_string();
    let run = |action: &'static str| {
        let database = database.clone();
        let marker = marker.clone();
        let nonce = nonce.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Command::new(env!("CARGO_BIN_EXE_sqlite-await-event-helper"))
                    .arg(database)
                    .arg(action)
                    .arg(nonce)
                    .arg(marker)
                    .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("{action} helper timed out"))
            .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"))
        }
    };

    let crashed = run("effect_crash").await;
    assert_eq!(crashed.status.code(), Some(86));
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read crashed effect marker")
            .lines()
            .count(),
        1,
        "the external effect ran before the owner crashed"
    );

    let completed = run("effect_complete").await;
    assert!(
        completed.status.success(),
        "successor helper failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read re-executed effect marker")
            .lines()
            .count(),
        2,
        "an unrecorded external effect is honestly re-executed"
    );

    let replayed = run("effect_replay").await;
    assert!(
        replayed.status.success(),
        "replay helper failed: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read replay effect marker")
            .lines()
            .count(),
        2,
        "a recorded outcome replays without another external effect"
    );
}

#[tokio::test]
async fn sqlite_real_turn_satisfies_cold_process_crash_matrix() {
    let dir = tempfile::tempdir().expect("SQLite cold-process real-turn tempdir");
    let database = dir.path().join("cold-process-real-turn.db");
    cold_process_turn_parent::assert_real_turn_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command =
                tokio::process::Command::new(env!("CARGO_BIN_EXE_sqlite-await-event-helper"));
            command.arg(&database).arg(action).arg(nonce).arg(marker);
            command
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_controller_satisfies_replay_conformance() {
    let (_controller_dir, controller) = open_ephemeral_effect_controller(durable_turn_scope(
        "effect-conformance-session",
        "effect-conformance-turn",
    ))
    .await;

    lash_core::testing::conformance::effect_controller_concurrent_replay_deterministic(
        &controller,
        || controller.start_replay(),
    )
    .await;

    let (_tool_controller_dir, tool_controller) =
        open_ephemeral_effect_controller(durable_turn_scope(
            "tool-attempt-conformance-session",
            "tool-attempt-conformance-turn",
        ))
        .await;
    lash_core::testing::conformance::effect_controller_tool_attempt_fanout_replay_deterministic(
        &tool_controller,
        || tool_controller.start_replay(),
    )
    .await;

    let (_durable_controller_dir, durable_controller) = open_ephemeral_effect_controller(
        durable_turn_scope("durable-step-session", "durable-step-turn"),
    )
    .await;
    lash_core::testing::conformance::effect_controller_journaled_effect_replay(
        &durable_controller,
        || durable_controller.start_replay(),
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_controller_replays_without_local_executor() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    let envelope = exec_envelope("exec-replay", "first");
    let first = controller
        .execute_effect(envelope.clone(), returning_executor("recorded"))
        .await
        .expect("first effect");
    assert_exec_marker(first, "recorded");

    controller.start_replay();
    let replayed = controller
        .execute_effect(envelope, failing_executor())
        .await
        .expect("replayed effect");
    assert_exec_marker(replayed, "recorded");
}

#[tokio::test]
async fn sqlite_effect_controller_replays_a_non_empty_recorded_intent_batch() {
    let dir = tempfile::tempdir().expect("intent replay tempdir");
    let path = dir.path().join("recorded-intent-effect.db");
    let scope = durable_turn_scope("sqlite-intent-session", "sqlite-intent-turn");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("sqlite-intent-session", "sqlite-intent-turn", 0, 0),
            "sqlite-recorded-intent-attempt",
            RuntimeEffectKind::ToolAttempt,
            "sqlite-recorded-intent-attempt",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core::PreparedToolCall::from_parts(
                "sqlite-intent-call",
                "tool:sqlite_intent_leaf",
                "sqlite_intent_leaf",
                serde_json::json!({"value": "record"}),
                None,
                serde_json::Value::Null,
            ),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    );
    let expected = RuntimeEffectOutcome::ToolAttempt {
        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
            record: Box::new(lash_core::ToolCallRecord {
                call_id: Some("sqlite-intent-call".to_string()),
                tool: "sqlite_intent_leaf".to_string(),
                args: serde_json::json!({"value": "record"}),
                output: lash_core::ToolCallOutput::success(serde_json::json!({
                    "provider": "done"
                })),
                duration_ms: 7,
            }),
            intents: lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::EmitProcessEvent(
                lash_core::EmitProcessEventIntent {
                    session_id: "sqlite-intent-session".to_string(),
                    process_id: "sqlite-intent-target".to_string(),
                    event_type: "sqlite.intent.recorded".to_string(),
                    payload: serde_json::json!({"literal": true}),
                },
            )]),
        }),
        triggers: Vec::new(),
    };
    let expected_bytes = serde_json::to_vec(&expected).expect("serialize literal intent outcome");
    let first_controller = SqliteRuntimeEffectController::open(&path, scope.clone())
        .await
        .expect("open first SQLite intent controller");
    let first = first_controller
        .execute_effect(
            envelope.clone(),
            RuntimeEffectLocalExecutor::testing({
                let expected = expected.clone();
                move |_| async move { Ok(expected) }
            }),
        )
        .await
        .expect("record non-empty intent carrier");
    assert_eq!(
        serde_json::to_vec(&first).expect("serialize first SQLite intent outcome"),
        expected_bytes
    );
    drop(first_controller);

    let replay_controller = SqliteRuntimeEffectController::open(&path, scope)
        .await
        .expect("reopen SQLite intent controller");
    replay_controller.start_replay();
    let replayed = replay_controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(|_| async {
                panic!("SQLite replay must not rerun the recorded attempt body")
            }),
        )
        .await
        .expect("replay non-empty intent carrier");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replayed SQLite intent outcome"),
        expected_bytes,
        "SQLite replays the literal non-empty intent carrier byte-for-byte"
    );
}

#[tokio::test]
async fn sqlite_effect_host_retires_session_journal_rows() {
    let dir = tempfile::tempdir().expect("effect replay tempdir");
    let path = dir.path().join("effect-replay.db");
    let host = SqliteEffectHost::open(&path)
        .await
        .expect("open SQLite effect host");

    lash_core::testing::conformance::effect_host_retires_session_journal(&host).await;
    lash_core::testing::conformance::effect_host_retires_process_journal(&host).await;

    let conn = rusqlite::Connection::open(path).expect("open effect journal for row count");
    let retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_effect_replay WHERE session_id = ?1",
            ["retired-journal-session"],
            |row| row.get(0),
        )
        .expect("count retained session journal rows");
    assert_eq!(retained, 0);
    let process_retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_effect_replay WHERE session_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count retained process journal rows");
    assert_eq!(process_retained, 0);
}

#[tokio::test]
async fn sqlite_effect_controller_reports_envelope_divergent_paths() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    lash_core::testing::conformance::effect_controller_replay_mismatch_diagnostics(
        &controller,
        "sqlite_effect_replay_hash_conflict",
    )
    .await;
}

#[tokio::test]
async fn sqlite_effect_controller_satisfies_lease_fencing_conformance() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let path = fresh_db_path(&dirs, "effect-lease-fencing.db");
    let make_path = path.clone();
    let steal_path = path.clone();
    let expire_path = path.clone();
    lash_core::testing::conformance::effect_controller_lease_fencing(
        lash_core::testing::conformance::EffectLeaseFencingBackend {
            make_controller: Box::new(move |ttl| {
                let path = make_path.clone();
                Box::pin(async move {
                    let controller = SqliteRuntimeEffectController::open_with_options(
                        &path,
                        durable_turn_scope("session", "turn"),
                        SqliteEffectReplayOptions {
                            lease_timings: lash_core::facade_support::LeaseTimings::from_ttl(ttl)
                                .expect("conformance lease timings"),
                        },
                    )
                    .await
                    .expect("controller");
                    let for_replay = controller.clone();
                    lash_core::testing::conformance::LeaseFencingController {
                        controller: Arc::new(controller),
                        start_replay: Box::new(move || for_replay.start_replay()),
                    }
                })
            }),
            steal_lease: Box::new(move |replay_key| {
                let path = steal_path.clone();
                Box::pin(async move {
                    let stolen_until = current_epoch_ms_for_test().saturating_add(10_000);
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    let changed = conn
                        .execute(
                            "UPDATE runtime_effect_replay
                             SET lease_owner_id = 'stolen-owner',
                                 lease_token = 'stolen-token',
                                 lease_expires_at_ms = ?1
                             WHERE replay_key = ?2",
                            rusqlite::params![stolen_until as i64, replay_key],
                        )
                        .expect("steal lease row");
                    assert_eq!(changed, 1);
                })
            }),
            expire_lease: Box::new(move |replay_key| {
                let path = expire_path.clone();
                Box::pin(async move {
                    let conn = rusqlite::Connection::open(&path).expect("open sqlite");
                    let changed = conn
                        .execute(
                            "UPDATE runtime_effect_replay
                             SET lease_expires_at_ms = 0
                             WHERE replay_key = ?1",
                            rusqlite::params![replay_key],
                        )
                        .expect("expire lease row");
                    assert_eq!(changed, 1);
                })
            }),
        },
    )
    .await;
}

#[tokio::test]
async fn sqlite_sleep_replay_returns_after_recorded_due_time() {
    let (_controller_dir, controller) =
        open_ephemeral_effect_controller(durable_turn_scope("session", "turn")).await;
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::for_turn("session", "turn", 1, 0),
            "sleep",
            RuntimeEffectKind::Sleep,
            "sleep-key",
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 120 },
    );

    let started = std::time::Instant::now();
    let first = controller
        .execute_effect(envelope.clone(), RuntimeEffectLocalExecutor::unavailable())
        .await
        .expect("first sleep");
    assert!(matches!(first, RuntimeEffectOutcome::Sleep));
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(100),
        "first sleep must wait until the recorded due_at"
    );

    controller.start_replay();
    let replayed = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        controller.execute_effect(envelope, failing_executor()),
    )
    .await
    .expect("replay must not sleep the full original duration")
    .expect("sleep replay");
    assert!(matches!(replayed, RuntimeEffectOutcome::Sleep));
}

/// Drive one process into `waiting` and assert the retention contract: live rows
/// are listed as non-terminal and are never prune candidates.
async fn assert_waiting_process_is_live_not_prunable(
    registry: &dyn ProcessRegistry,
    process_id: &str,
) {
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register waiting retention process");
    let authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "waiting-retention-run")
            .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("invocation authority carries its start fact");
    registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .expect("start waiting retention process");
    let waiting = registry
        .set_process_wait_with_authority(
            process_id,
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "retention".to_string(),
                    event_type: "retention.signal".to_string(),
                    key: format!("{process_id}:wait"),
                    ordinal: 1,
                },
            },
            &authority,
        )
        .await
        .expect("enter wait");
    assert_eq!(
        waiting.status.label(),
        "waiting",
        "the wait must land in the persisted status label the retention SQL reads"
    );
    assert!(!waiting.is_terminal(), "a waiting process is not terminal");

    let live = registry
        .list_non_terminal_page(
            std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
            None,
        )
        .await
        .expect("list non-terminal processes")
        .records;
    assert!(
        live.iter().any(|record| record.id == process_id),
        "a waiting process must be listed as live"
    );

    let report = registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune terminal processes");
    assert_eq!(
        report.pruned_processes, 0,
        "a waiting process must never be a prune candidate, whatever the cutoff"
    );
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("read waiting retention process")
            .is_some(),
        "the waiting process row must survive the prune"
    );
}

/// A waiting process is live, not prunable.
///
/// `lash_core::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS`
/// is the shared retention contract, but this backend's SQL spells the label set
/// out as `status IN ('running', 'waiting')` and `status NOT IN (…)`. The law test
/// in core proves the constant partitions `ProcessStatus`; this is the
/// behavioural half, which is what fails if the SQL literals stop agreeing with
/// it and a live waiting process becomes prune-eligible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_waiting_processes_are_live_not_prunable() {
    let dir = tempfile::tempdir().expect("waiting retention tempdir");
    let registry = SqliteProcessRegistry::open(
        &dir.path().join("processes.db"),
        dir.path().join("sessions"),
    )
    .await
    .expect("open waiting retention registry");
    let process_id = format!("waiting-retention:{}", uuid::Uuid::new_v4());
    assert_waiting_process_is_live_not_prunable(&registry, &process_id).await;
}

/// Lexical half of the retention contract: every `status IN`/`status NOT IN`
/// literal in this backend's SQL must spell exactly the label list rendered
/// from `LIVE_PROCESS_STATUS_LABELS`. The partition law proves the
/// constant tracks `ProcessStatus`; the behavioural referee above proves
/// today's labels retain; this closes the remaining gap where a future label
/// grows the constant while a stale SQL literal silently prunes live rows.
#[test]
fn sqlite_status_list_literals_derive_from_the_shared_constant() {
    let expected = format!(
        "({})",
        lash_core::facade_support::registry_transitions::LIVE_PROCESS_STATUS_LABELS
            .iter()
            .map(|label| format!("'{label}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let sources = [
        (
            "process_registry.rs",
            include_str!("../src/process_registry.rs"),
        ),
        (
            "process_registry_change.rs",
            include_str!("../src/process_registry_change.rs"),
        ),
    ];
    let mut total = 0usize;
    for (name, source) in sources {
        for delimiter in ["status IN ", "status NOT IN "] {
            for site in source.split(delimiter).skip(1) {
                assert!(
                    site.starts_with(&expected),
                    "{name}: a `{delimiter}` list literal diverged from \
                     LIVE_PROCESS_STATUS_LABELS: expected {expected}, found {}",
                    &site[..site.len().min(40)]
                );
                total += 1;
            }
        }
    }
    assert_eq!(
        total, 1,
        "expected exactly one status-list literal site in the SQLite backend; \
         update this count (and the derivation check) when adding one"
    );
}
