//! The SQLite conformance suite, registered once per substrate (ADR 0102).
//!
//! `conformance.rs` and `conformance_memory.rs` each mount this module under a
//! `SUBSTRATE`: every fixture opens its databases through a [`TestBackend`]
//! on that substrate, so the same laws hold a file backend and a named
//! in-memory one to one standard. Laws that need a database file by nature —
//! pre-seeded legacy schemas, path spellings, a second OS process, WAL
//! snapshot reads — live in `conformance.rs` alone.
// No live_replay_tests!: live replay is an in-process cache, not SQLite-backed storage.
// No queued-lane resolver macro: engine pacing belongs to Restate, not a persistence store.

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_conformance::{
    FenceIntegrityHandles, FenceIntegrityInjector, FenceIntegrityObservation, FenceIntegrityTarget,
    GraphFactObservation, GraphIntegrityCorruption, GraphIntegrityHandles, GraphIntegrityInjector,
    GraphIntegrityRead, GraphIntegrityTarget, LineageConformanceHandles,
    LineageConformanceInjector, ReopenableProcessRegistry, ReopenableRuntimePersistence,
    ReopenableTriggerStore,
};
use lash_core_execution::store::{ConformanceSessionStoreFactory, RuntimePersistenceDecorator};
use lash_core_execution::{
    ProcessCompletionAuthority, ProcessExecutionEnvStore, ProcessIdentity, ProcessInput,
    ProcessLifecycle as _, ProcessListFilter, ProcessProvenance, ProcessQuery as _,
    ProcessRegistrar as _, ProcessRegistration, ProcessRegistry, ProcessStatusFilter,
    RuntimePersistence, SessionCommitStore, SessionStoreFactory, StoreError, TriggerStore,
};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSetOptions};

use super::SUBSTRATE;
use crate::backend_fixture::{Substrate, TestBackend, sync_await};

struct MultiSessionAdmissionStore {
    inner: Arc<dyn RuntimePersistence>,
    backend: TestBackend,
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for MultiSessionAdmissionStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core_execution::SessionBinding,
    ) -> Result<lash_core_execution::SessionAdmission, StoreError> {
        self.backend
            .store()
            .await
            .admit_and_bind_session(binding)
            .await
    }
}

/// Engine promise authority for storage laws that cross a turn-control boundary.
async fn promise_authority() -> (
    lash_restate_test::RestateTestBackend,
    Arc<dyn lash_core_execution::EffectHost>,
) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let backend = lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("boot the Restate promise authority");
    let host: Arc<dyn lash_core_execution::EffectHost> = backend.restate().restate_effect_host();
    (backend, host)
}

#[path = "attachment_store.rs"]
mod attachment_store;
#[path = "claim_atomicity.rs"]
mod claim_atomicity;
#[path = "generation_drain.rs"]
mod generation_drain;
#[path = "lineage.rs"]
mod lineage;
#[path = "obligation_relay.rs"]
mod obligation_relay;
#[path = "process_prune_reclaim.rs"]
mod process_prune_reclaim;
#[path = "process_retention.rs"]
mod process_retention;
#[path = "session_delete_blob_reclaim.rs"]
mod session_delete_blob_reclaim;
#[path = "session_ingress.rs"]
mod session_ingress;
#[path = "session_meta.rs"]
mod session_meta;
#[path = "store_maintenance.rs"]
mod store_maintenance;
#[path = "trigger_occurrence_retention.rs"]
mod trigger_occurrence_retention;

include!("append_identity.rs");

/// A fixture flavor [`Retained`] opens from synchronous code.
trait BlockingFixture: Clone {
    fn open_on(substrate: Substrate) -> Self;
}

impl BlockingFixture for TestBackend {
    fn open_on(substrate: Substrate) -> Self {
        Self::blocking(substrate)
    }
}

/// Backends a fixture opened and must keep alive for its whole law.
#[derive(Clone)]
struct Retained<B = TestBackend>(Arc<Mutex<Vec<B>>>);

impl<B> Default for Retained<B> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl<B: Clone> Retained<B> {
    fn keep(&self, backend: &B) {
        self.0.lock_recover().push(backend.clone());
    }
}

impl<B: BlockingFixture> Retained<B> {
    /// A fresh backend, kept alive with the fixture.
    fn open_blocking(&self) -> B {
        let backend = B::open_on(SUBSTRATE);
        self.keep(&backend);
        backend
    }
}

/// One backend per scenario name: a law that asks twice for the same
/// scenario is reopening the store it wrote, so it gets a fresh store on the
/// same backend.
#[derive(Clone)]
struct ScenarioBackends {
    clock: Arc<dyn lash_core_execution::Clock>,
    by_scenario: Arc<Mutex<HashMap<String, TestBackend>>>,
}

impl ScenarioBackends {
    fn new(clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        Self {
            clock,
            by_scenario: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn store(&self, scenario: &str) -> Arc<dyn RuntimePersistence> {
        self.concrete_store(scenario) as Arc<dyn RuntimePersistence>
    }

    /// The scenario's store as its concrete type, for laws that also reach
    /// its test-support seams.
    fn concrete_store(&self, scenario: &str) -> Arc<lash_sqlite_store::Store> {
        let existing = self.by_scenario.lock_recover().get(scenario).cloned();
        let backend = existing.unwrap_or_else(|| {
            let clock = Arc::clone(&self.clock);
            let backend =
                sync_await(async move { TestBackend::open_with_clock(SUBSTRATE, clock).await });
            self.by_scenario
                .lock_recover()
                .entry(scenario.to_string())
                .or_insert(backend)
                .clone()
        });
        backend.blocking_store()
    }
}

fn root_session_request(session_id: &str) -> lash_core_execution::SessionStoreCreateRequest {
    lash_core_execution::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id),
        relation: lash_core_execution::SessionRelation::Root,
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
    }
}

lash_conformance::attachment_adoption_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory();
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    ((backend, bytes_root), factory, make_bytes)
});

lash_conformance::attachment_condemnation_recovery_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory();
    let reopen = backend.clone();
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    (
        (backend, bytes_root),
        factory,
        make_bytes,
        move || async move {
            reopen.reopen().await.session_store_factory() as Arc<dyn SessionStoreFactory>
        },
    )
});

#[tokio::test]
async fn sqlite_attachment_condemnation_enumeration_refuses_corrupt_rows() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory();
    let session_id = SessionId::from("condemnation-corruption");
    factory
        .create_store(
            &lash_core_execution::testing::store_fixtures::session_store_request(
                &session_id,
                "condemnation-corruption",
                lash_core_execution::SessionRelation::Root,
            ),
        )
        .await
        .expect("materialize catalog");
    let connection = backend.raw(SqliteDatabase::DurableCore);
    connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             INSERT INTO attachment_condemnations
                 (attachment_id, phase, write_token, write_session_id)
             VALUES ('corrupt-condemnation', 'future-phase', NULL, NULL);",
        )
        .expect("inject unknown persisted phase");
    assert!(matches!(
        lash_core_execution::AttachmentRootSet::list_condemnations(factory.as_ref()).await,
        Err(lash_core_execution::StoreError::StoredDataCorrupt { .. })
    ));
    connection
        .execute_batch(
            "DELETE FROM attachment_condemnations;
             INSERT INTO attachment_condemnations
                 (attachment_id, phase, write_token, write_session_id)
             VALUES ('corrupt-condemnation', 'deleting', 'opaque', 'session');",
        )
        .expect("inject inconsistent persisted provenance");
    assert!(matches!(
        lash_core_execution::AttachmentRootSet::list_condemnations(factory.as_ref()).await,
        Err(lash_core_execution::StoreError::StoredDataCorrupt { .. })
    ));
}

lash_conformance::abandoned_attachment_recovery_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let bytes_root = Arc::new(tempfile::tempdir().expect("attachment bytes root"));
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    ((retained.clone(), bytes_root), move || {
        let retained = retained.clone();
        let make_bytes = Arc::clone(&make_bytes);
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
            (factory, make_bytes, move || async move {
                backend.reopen().await.session_store_factory() as Arc<dyn SessionStoreFactory>
            })
        }
    })
});

fn artifact_store_handles(
    backend: &TestBackend,
) -> lash_conformance::fused_artifact_store::ArtifactStoreHandles {
    let store = backend.blocking_store();
    lash_conformance::fused_artifact_store::ArtifactStoreHandles {
        artifacts: Arc::clone(&store) as Arc<dyn lash_core::ModuleArtifactStore>,
        process_env: store as Arc<dyn ProcessExecutionEnvStore>,
    }
}

struct SqliteTriggerOccurrenceListingFaultInjector {
    backend: TestBackend,
}

#[async_trait::async_trait]
impl lash_conformance::TriggerOccurrenceListingFaultInjector
    for SqliteTriggerOccurrenceListingFaultInjector
{
    async fn insert_malformed_occurrence(&self) {
        let conn = self.backend.raw(SqliteDatabase::Triggers);
        conn.execute(
            "INSERT INTO trigger_occurrences (
                occurrence_id, idempotency_key, source_type, source_key,
                occurred_at_ms, record_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "occurrence-listing-malformed",
                "occurrence-listing-malformed",
                "ui.button.pressed",
                "occurrence-listing-malformed-source",
                0_i64,
                "{not valid json",
            ],
        )
        .expect("insert malformed SQLite occurrence");
    }

    async fn make_occurrence_query_unavailable(&self) {
        self.backend
            .raw(SqliteDatabase::Triggers)
            .execute_batch("DROP TABLE trigger_occurrences")
            .expect("make SQLite occurrence query unavailable");
    }
}

struct SqliteFenceIntegrityInjector {
    backend: TestBackend,
}

impl SqliteFenceIntegrityInjector {
    fn connection(&self, target: &FenceIntegrityTarget) -> rusqlite::Connection {
        self.backend.raw(match target {
            FenceIntegrityTarget::TriggerRevision { .. } => SqliteDatabase::Triggers,
            _ => SqliteDatabase::DurableCore,
        })
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
                rusqlite::params![value, session_id.as_str()],
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
                    [session_id.as_str()],
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
            FenceIntegrityTarget::TriggerRevision { subscription_id } => conn
                .query_row(
                    "SELECT revision, record_json, lifecycle, deleted_at_ms
                     FROM trigger_subscriptions WHERE subscription_id = ?1",
                    [subscription_id],
                    |row| {
                        let value: i64 = row.get(0)?;
                        let json: String = row.get(1)?;
                        let lifecycle: String = row.get(2)?;
                        let deleted_at_ms: Option<i64> = row.get(3)?;
                        Ok(FenceIntegrityObservation {
                            value,
                            mutation_fingerprint: format!("{json}:{lifecycle}:{deleted_at_ms:?}"),
                        })
                    },
                )
                .expect("observe SQLite trigger revision"),
        }
    }
}

lash_conformance::fence_integrity_tests!({
    ((), |_case| async move {
        let backend = TestBackend::open(SUBSTRATE).await;
        FenceIntegrityHandles {
            runtime: backend.store().await,
            triggers: backend.trigger_store(),
            injector: Arc::new(SqliteFenceIntegrityInjector { backend }),
        }
    })
});

struct SqliteGraphIntegrityInjector {
    backend: TestBackend,
    runtime: Arc<lash_sqlite_store::Store>,
}

#[async_trait::async_trait]
impl GraphIntegrityInjector for SqliteGraphIntegrityInjector {
    async fn inject(&self, target: &GraphIntegrityTarget) {
        let conn = self.backend.raw(SqliteDatabase::DurableCore);
        match target.corruption {
            GraphIntegrityCorruption::OrphanLeaf => {
                let changed = conn
                    .execute(
                        "UPDATE graph_nodes SET parent_node_id = ?1 WHERE node_id = ?2",
                        rusqlite::params![
                            target.missing_node_id.as_str(),
                            target.leaf_node_id.as_str()
                        ],
                    )
                    .expect("inject orphaned SQLite graph leaf");
                assert_eq!(changed, 1);
            }
            GraphIntegrityCorruption::DuplicateNodeId => {
                conn.execute_batch(
                    "ALTER TABLE graph_nodes RENAME TO graph_nodes_valid;
                     CREATE TABLE graph_nodes (
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
                        rusqlite::params![target.leaf_node_id.as_str()],
                    )
                    .expect("inject duplicate SQLite graph node id");
                assert_eq!(changed, 1);
            }
            GraphIntegrityCorruption::DanglingLeafId => {
                let changed = conn
                    .execute(
                        "UPDATE session_head SET leaf_node_id = ?1 WHERE session_id = ?2",
                        rusqlite::params![
                            target.missing_node_id.as_str(),
                            target.session_id.as_str()
                        ],
                    )
                    .expect("inject dangling SQLite graph leaf id");
                assert_eq!(changed, 1);
            }
            GraphIntegrityCorruption::ParentCycle => {
                if target.read == GraphIntegrityRead::ActivePath {
                    let changed = conn
                        .execute(
                            "UPDATE graph_nodes SET parent_node_id = ?1 WHERE node_id = ?2",
                            rusqlite::params![
                                target.leaf_node_id.as_str(),
                                target.root_node_id.as_str()
                            ],
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
                                target.leaf_node_id.as_str(),
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
        _session_id: &SessionId,
    ) -> Result<lash_core_execution::SessionGraph, lash_core_execution::StoreError> {
        self.runtime.load_session_graph().await
    }
}

lash_conformance::graph_integrity_tests!({
    ((), |_case| async move {
        let backend = TestBackend::open(SUBSTRATE).await;
        let runtime = backend.store().await;
        GraphIntegrityHandles {
            runtime: Arc::clone(&runtime) as Arc<dyn RuntimePersistence>,
            injector: Arc::new(SqliteGraphIntegrityInjector { backend, runtime }),
        }
    })
});

#[tokio::test]
async fn sqlite_load_session_graph_accepts_healthy_non_empty_session() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let session_id = "healthy-whole-session-graph";
    let mut state = lash_core_execution::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..lash_core_execution::RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
        ))
    };
    state.ensure_agent_frame_initialized();
    state
        .session_graph
        .append_plugin("healthy-whole-graph", serde_json::json!({"second": true}));
    store
        .admit_and_bind_session(&lash_core_execution::SessionBinding::root(session_id))
        .await
        .expect("bind healthy SQLite graph session");
    store
        .commit_runtime_state(
            lash_core_execution::RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
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

lash_conformance::signed_counter_write_domain_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    (backend, store)
});

lash_conformance::artifact_store_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        lash_conformance::fused_artifact_store::ReopenableArtifactStore {
            open: artifact_store_handles(&backend),
            reopen: Arc::new(move || artifact_store_handles(&backend)),
        }
    })
});

lash_conformance::artifact_referrer_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        lash_conformance::fused_artifact_store::ReopenableArtifactStore {
            open: artifact_store_handles(&backend),
            reopen: Arc::new(move || artifact_store_handles(&backend)),
        }
    })
});

lash_conformance::process_registry_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move |_label: &str| {
        let backend = retained.open_blocking();
        let reopened = sync_await({
            let backend = backend.clone();
            async move { backend.reopen().await }
        });
        retained.keep(&reopened);
        ReopenableProcessRegistry {
            open: backend.process_registry()
                as Arc<dyn lash_core_execution::ConformanceProcessRegistry>,
            reopen: reopened.process_registry()
                as Arc<dyn lash_core_execution::ConformanceProcessRegistry>,
        }
    })
});

#[tokio::test]
async fn sqlite_recently_retired_filter_uses_the_extracted_updated_at_column() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry();
    let recent_pushdown_id = registry
        .register_process(
            ProcessRegistration::new(
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                ProcessProvenance::host(),
                lash_core_execution::Lifetime::Detached,
            )
            .with_admitted_identity(
                lash_core_execution::AdmittedProcessIdentity::for_testing(ProcessIdentity::new(
                    "recent-pushdown-kind",
                )),
            ),
        )
        .await
        .expect("register recently retired pushdown fixture")
        .id;
    let terminal = registry
        .complete_process(
            &recent_pushdown_id,
            lash_core_execution::ProcessAwaitOutput::from_tool_output(
                lash_core_execution::ToolCallOutput::success(serde_json::json!({})),
            ),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete recently retired pushdown fixture");
    let terminal = match terminal {
        lash_core_execution::ProcessCompletionOutcome::Committed(record)
        | lash_core_execution::ProcessCompletionOutcome::AlreadyApplied { stored: record }
        | lash_core_execution::ProcessCompletionOutcome::Superseded { stored: record } => record,
    };

    let conn = backend.raw(SqliteDatabase::ProcessRegistry);
    assert_eq!(
        conn.execute(
            "UPDATE processes SET updated_at_ms = 0 WHERE process_id = ?1",
            rusqlite::params![terminal.id.as_str()],
        )
        .expect("age only the extracted process timestamp"),
        1
    );
    drop(conn);

    let bounded = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_kind: Some("recent-pushdown-kind".to_string()),
            retired_since_ms: Some(terminal.updated_at_ms),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list bounded recently retired rows");
    assert!(
        bounded.is_empty(),
        "the SQL WHERE must reject the extracted old timestamp before JSON decode"
    );
    assert_eq!(
        registry
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::Any,
                identity_kind: Some("recent-pushdown-kind".to_string()),
                ..ProcessListFilter::default()
            })
            .await
            .expect("list unbounded pushdown fixture")
            .len(),
        1,
        "the unbounded list still returns the retained row"
    );
}

lash_conformance::process_projection_repair_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry();
    let corruption = backend.clone();
    (
        backend,
        registry as Arc<dyn ProcessRegistry>,
        move |stale: lash_core_execution::ProcessRecord| async move {
            let conn = corruption.raw(SqliteDatabase::ProcessRegistry);
            let changed = conn
                .execute(
                    "UPDATE processes SET record_json = ?2 WHERE process_id = ?1",
                    rusqlite::params![
                        stale.id.as_str(),
                        serde_json::to_string(&stale).expect("encode stale process projection")
                    ],
                )
                .expect("corrupt SQLite process projection");
            assert_eq!(changed, 1);
        },
    )
});

lash_conformance::store_contract_state_machine_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    ((), "sqlite", move |_seed, _| {
        let retained = retained.clone();
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            lash_conformance::StoreContractHandles {
                registry: backend.process_registry() as Arc<dyn ProcessRegistry>,
                runtime: backend.store().await as Arc<dyn RuntimePersistence>,
            }
        }
    })
});

lash_conformance::runtime_persistence_state_machine_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    ((), "sqlite", move |_| {
        let retained = retained.clone();
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            lash_conformance::RuntimePersistenceStateMachineHandles::create(
                backend.session_store_factory(),
                backend.attachment_store(),
                true,
            )
            .await
            .expect("create SQLite runtime-persistence property handles")
        }
    })
});

lash_conformance::session_graph_state_machine_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    ((), "sqlite", move |_| {
        let retained = retained.clone();
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            backend.session_store_factory() as Arc<dyn SessionStoreFactory>
        }
    })
});

lash_conformance::process_continuation_store_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let storage = backend.process_registry();
    let registry = Arc::clone(&storage) as Arc<dyn lash_core_execution::ProcessRegistry>;
    let store = storage as Arc<dyn lash_core_execution::ProcessContinuationStore>;
    (backend, registry, store)
});

lash_conformance::session_store_factory_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let unbound_backend = TestBackend::open(SUBSTRATE).await;
    retained.keep(&unbound_backend);
    let unbound =
        Some(unbound_backend.store().await as Arc<dyn lash_core_execution::StoreMaintenance>);
    let make_retained = retained.clone();
    let make = move || {
        make_retained.open_blocking().session_store_factory()
            as Arc<dyn ConformanceSessionStoreFactory>
    };
    let attached_retained = retained.clone();
    let make_attached = move || {
        let backend = attached_retained.open_blocking();
        (
            backend.session_store_factory() as Arc<dyn ConformanceSessionStoreFactory>,
            backend.attachment_store() as Arc<dyn lash_core_execution::AttachmentStore>,
        )
    };
    let (engine, effect_host) = promise_authority().await;
    (
        (retained, engine),
        "sqlite",
        unbound,
        make,
        make_attached,
        effect_host,
    )
});

// The settlement laws run a facade runtime over a fresh backend per law: an
// engine backend keeps its substrate alive and supplies the durable ports the
// runtime takes from it.
lash_conformance::session_config_settlement_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let make = {
        let retained = retained.clone();
        move || {
            let backend = retained.open_blocking();
            async move { backend.as_backend() }
        }
    };
    (retained, make)
});

lash_conformance::fresh_session_admission_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move |_session_id: &str| {
        retained.open_blocking().blocking_store() as Arc<dyn RuntimePersistence>
    })
});

lash_conformance::observer_intent_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law_backend = backend.as_backend();
    (backend, law_backend)
});

lash_conformance::session_graph_append_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    (backend, factory)
});

lash_conformance::process_prune_session_store_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    let (engine, effect_host) = promise_authority().await;
    ((backend, engine), factory, registry, effect_host)
});

lash_conformance::trigger_store_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        let reopened = sync_await({
            let backend = backend.clone();
            async move { backend.reopen().await }
        });
        retained.keep(&reopened);
        ReopenableTriggerStore {
            open: backend.trigger_store() as Arc<dyn TriggerStore>,
            reopen: reopened.trigger_store() as Arc<dyn TriggerStore>,
        }
    })
});

lash_conformance::trigger_occurrence_listing_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.trigger_store() as Arc<dyn TriggerStore>;
    let injector = Arc::new(SqliteTriggerOccurrenceListingFaultInjector {
        backend: backend.clone(),
    });
    (backend, store, injector)
});

#[tokio::test]
async fn sqlite_trigger_ingress_skips_malformed_matching_subscription() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let source_type = "ui.button.pressed";
    let source_key = lash_core_execution::facade_support::empty_trigger_source_key(source_type)
        .expect("source key");
    let store = backend.trigger_store();
    let register = |owner: &str, key: &str| lash_core_execution::TriggerCommand::Register {
        owner_scope: lash_core_execution::TriggerOwnerScope::session(owner),
        actor: lash_core_execution::ProcessOriginator::session(
            lash_core_execution::SessionScope::new(owner),
        ),
        draft: lash_core_execution::TriggerSubscriptionDraft::for_process(
            key,
            lash_core_execution::ProcessExecutionEnvRef::new(format!("process-env:{owner}")),
            source_type,
            source_key.clone(),
            lash_core_execution::ProcessInput::Engine {
                kind: "test".to_string(),
                payload: serde_json::json!({ "owner": owner }),
            },
            lash_core_execution::ProcessIdentity::new("test"),
        )
        .with_payload_schema(lash_core_execution::LashSchema::any()),
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
    let lash_core_execution::TriggerCommandOutcome::Mutation { receipt: malformed } = malformed
    else {
        panic!("expected malformed registration receipt")
    };
    let lash_core_execution::TriggerCommandOutcome::Mutation { receipt: current } = current else {
        panic!("expected current registration receipt")
    };
    drop(store);

    let conn = backend.raw(SqliteDatabase::Triggers);
    conn.execute(
        "UPDATE trigger_subscriptions SET record_json = ?2 WHERE subscription_id = ?1",
        rusqlite::params![malformed.subscription_id.as_str(), "{not valid json"],
    )
    .expect("poison trigger row");
    drop(conn);

    let reopened = backend.reopen().await.trigger_store();
    let ingress = reopened
        .ingest_occurrence(lash_core_execution::TriggerOccurrenceRequest::new(
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

lash_conformance::runtime_persistence_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let (engine, effect_host) = promise_authority().await;
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
    let store_clock = Arc::clone(&clock);
    (
        (retained.clone(), engine),
        move |session_id: &str| {
            let request = root_session_request(session_id);
            let clock = store_clock.clone() as Arc<dyn lash_core_execution::Clock>;
            let (backend, open, reopen) = sync_await(async move {
                let backend = TestBackend::open_with_clock(SUBSTRATE, clock).await;
                let factory = backend.session_store_factory();
                let open = factory
                    .create_store(&request)
                    .await
                    .expect("create explicitly bound SQLite conformance store");
                let reopen = factory
                    .open_existing_store(&request)
                    .await
                    .expect("open explicit SQLite conformance store")
                    .expect("created SQLite conformance store exists");
                (backend, open, reopen)
            });
            let effect_host = Arc::clone(&effect_host);
            retained.keep(&backend);
            ReopenableRuntimePersistence {
                open: Arc::new(MultiSessionAdmissionStore {
                    inner: open,
                    backend: backend.clone(),
                }),
                reopen: Arc::new(MultiSessionAdmissionStore {
                    inner: reopen,
                    backend,
                }),
                effect_host,
            }
        },
        lash_conformance::RuntimePersistenceLeaseTiming::controlled({
            let clock = Arc::clone(&clock);
            move |duration_ms| clock.advance(duration_ms)
        }),
    )
});

lash_conformance::unbound_session_read_tests!({
    (
        Retained::<TestBackend>::default(),
        move |_admission_state| async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            let factory = backend.session_store_factory();
            lash_conformance::UnboundSessionResolutionHandles {
                backend_name: "SQLite",
                factory,
                open_unbound: Arc::new(move || {
                    backend.blocking_store() as Arc<dyn RuntimePersistence>
                }),
            }
        },
    )
});

lash_conformance::store_recovery_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
    let scenarios =
        ScenarioBackends::new(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>);
    (
        (),
        move |scenario: &str| scenarios.store(scenario),
        lash_conformance::StoreRecoveryLeaseTiming::controlled(move |duration_ms| {
            clock.advance(duration_ms)
        }),
    )
});

lash_conformance::checkpoint_component_reopen_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let reopen = backend.clone();
    (backend, move || {
        reopen.blocking_store() as Arc<dyn RuntimePersistence>
    })
});

lash_conformance::append_head_switch_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let mutation = backend.clone();
    (
        backend,
        store as Arc<dyn RuntimePersistence>,
        move |leaf_node_id: lash_core_execution::NodeId| async move {
            let conn = mutation.raw(SqliteDatabase::DurableCore);
            conn.execute(
                "UPDATE session_head
                 SET leaf_node_id = ?1, head_revision = head_revision + 1
                 WHERE session_id = 'root'",
                rusqlite::params![leaf_node_id.as_str()],
            )
            .expect("switch sqlite active branch");
        },
    )
});

lash_conformance::append_tombstone_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let mutation = backend.clone();
    (
        backend,
        store as Arc<dyn RuntimePersistence>,
        move |node_id: lash_core_execution::NodeId| async move {
            let conn = mutation.raw(SqliteDatabase::DurableCore);
            conn.execute(
                "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                rusqlite::params![node_id.as_str()],
            )
            .expect("tombstone sqlite old leaf");
        },
    )
});

lash_conformance::append_receipt_envelope_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    (backend, store as Arc<dyn RuntimePersistence>)
});

// The commit-seam pause needs the fault injector, which only the `testing`
// feature builds.
#[cfg(feature = "testing")]
mod cancelled_queued_append {
    use super::*;
    use lash_sqlite_store::testing::{SqliteFaultInjector, SqliteFaultPoint};

    lash_conformance::append_usage_cancellation_tests!({
        let injector = SqliteFaultInjector::default();
        let backend = TestBackend::open_with(
            SUBSTRATE,
            {
                let injector = injector.clone();
                move |options| SqliteStoreSetOptions {
                    fault_injector: Some(injector),
                    ..options
                }
            },
            crate::backend_fixture::system_clock(),
        )
        .await;
        let store = backend
            .session_store_factory()
            .create_store(&root_session_request("root"))
            .await
            .expect("create cancellation store");
        (backend, store, move || {
            // The append commit is the first write after the pause is armed.
            let pause = injector.pause(SqliteFaultPoint::BeforeCommit);
            async move {
                pause.wait_until_reached().await;
                move || pause.release()
            }
        })
    });
}

lash_conformance::append_receipt_rewrite_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let mutation = backend.clone();
    (
        backend,
        store as Arc<dyn RuntimePersistence>,
        move || async move {
            let conn = mutation.raw(SqliteDatabase::DurableCore);
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
});

fn raw_count(conn: &rusqlite::Connection, sql: &str, name: &str) -> i64 {
    conn.query_row(sql, rusqlite::params![name], |row| row.get::<_, i64>(0))
        .expect("query sqlite_master")
}

#[tokio::test]
async fn sqlite_store_schema_excludes_embedded_turn_replay_tables() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let conn = backend.raw(SqliteDatabase::DurableCore);
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
    let backend = TestBackend::open(SUBSTRATE).await;
    let conn = backend.raw(SqliteDatabase::DurableCore);
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
        "identity_encoding_version",
    ] {
        assert_eq!(columns.get(column), Some(&0), "{column} must allow NULL");
    }
}

#[tokio::test]
async fn sqlite_runtime_turn_receipt_rejects_half_populated_append_identity() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let conn = backend.raw(SqliteDatabase::DurableCore);
    let error = conn
        .execute(
            "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                request_identity_hash
             ) VALUES ('half-identity', 'half-identity', 'hash', '{}', 0, 'request-hash')",
            [],
        )
        .expect_err("a half-populated append identity must violate the schema CHECK");
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    );
}

lash_conformance::retention_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    (backend, factory)
});
