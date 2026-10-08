//! The SQLite conformance suite, registered once per substrate (ADR 0102).
//!
//! `conformance.rs` and `conformance_memory.rs` each mount this module under a
//! `SUBSTRATE`: every fixture opens its databases through a [`TestBackend`]
//! on that substrate, so the same laws hold a file backend and a named
//! in-memory one to one standard. Laws that need a database file by nature —
//! pre-seeded legacy schemas, path spellings, a second OS process, WAL
//! snapshot reads — live in `conformance.rs` alone.
// No live_replay_tests!: live replay is an in-process cache, not SQLite-backed storage.
// No queued-lane resolver macro: engine pacing belongs to the durable engine, not a persistence store.

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_conformance::{
    FenceIntegrityHandles, FenceIntegrityInjector, FenceIntegrityObservation, FenceIntegrityTarget,
    GraphFactObservation, LineageConformanceHandles, LineageConformanceInjector,
    ReopenableProcessRegistry,
};
use lash_core_execution::store::{ConformanceDeployment, RuntimeStore};
use lash_core_execution::{
    DeploymentStore, ProcessCompletionAuthority, ProcessExecutionEnvStore, ProcessIdentity,
    ProcessLifecycle as _, ProcessListFilter, ProcessProvenance, ProcessQuery as _,
    ProcessRegistrar as _, ProcessRegistry, ProcessStatusFilter, SessionCatalogStore,
    SessionCommitStore,
};

use super::SUBSTRATE;
use crate::backend_fixture::{Substrate, TestBackend, sync_await};

#[path = "attachment_store.rs"]
mod attachment_store;
#[path = "durable_laws.rs"]
mod durable_laws;
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
#[path = "session_mail.rs"]
mod session_mail;
#[path = "store_maintenance.rs"]
mod store_maintenance;
#[path = "tool_intent_retention.rs"]
mod tool_intent_retention;

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

    fn store(&self, scenario: &str) -> Arc<dyn RuntimeStore> {
        self.concrete_store(scenario) as Arc<dyn RuntimeStore>
    }

    /// The scenario's store as its concrete type, for laws that also reach
    /// its test-support seams.
    fn concrete_store(&self, scenario: &str) -> Arc<lash_sqlite_store::SqliteStore> {
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
        let reopened = sync_await(async move { backend.reopen().await });
        reopened.blocking_store()
    }
}

fn root_session_request(session_id: &str) -> lash_core_execution::SessionStoreCreateRequest {
    lash_core_execution::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id),
        relation: lash_core_execution::SessionRelation::Root,
        config: lash_core_execution::PersistedSessionConfig::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
            lash_core_execution::SessionToolAccess::ambient(),
        ),
        head: lash_core_execution::SessionCreationHead::Config,
        retention: lash_core_execution::Retention::UntilGc,
    }
}

/// A published head must name a retained revision, including at transaction end.
#[tokio::test]
async fn session_head_pointer_requires_a_revision() {
    let backend = TestBackend::open(SUBSTRATE).await;
    backend
        .store()
        .await
        .admit_session(&root_session_request("head-pointer"))
        .await
        .expect("create a session with revision zero");
    let conn = backend.raw();
    conn.execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE")
        .expect("begin pointer publication");
    conn.execute(
        "UPDATE session_head SET head_revision = 1 WHERE session_id = 'head-pointer'",
        [],
    )
    .expect("the pointer constraint is deferred until commit");
    let error = conn
        .execute_batch("COMMIT")
        .expect_err("a dangling head pointer cannot commit");
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    );
    conn.execute_batch("ROLLBACK")
        .expect("undo invalid pointer");
    conn.execute(
        "INSERT INTO session_revisions (session_id, head_revision, head_json)
         SELECT session_id, 1, head_json FROM session_revisions
         WHERE session_id = 'head-pointer' AND head_revision = 0",
        [],
    )
    .expect("record an unpublished revision");
    let revisions = backend
        .store()
        .await
        .revisions(&SessionId::fixture("head-pointer"))
        .await
        .expect("list retained revisions");
    assert_eq!(
        revisions
            .iter()
            .map(|row| (row.head_revision, row.head))
            .collect::<Vec<_>>(),
        vec![(0, true), (1, false)],
        "the published pointer defines the head, even with a newer revision row"
    );
}

#[tokio::test]
async fn fork_session_rejects_a_malformed_target_session_id() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let request = lash_core_execution::ForkSessionRequest {
        session_id: SessionId::from("bad\0session"),
        source_session_id: SessionId::from("missing-fork-source"),
        head_revision: 0,
        relation: lash_core_execution::SessionRelation::Root,
        pending_observer_intents: Vec::new(),
        config: lash_core_execution::PersistedSessionConfig::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
            lash_core_execution::SessionToolAccess::ambient(),
        ),
        retention: lash_core_execution::Retention::UntilGc,
    };
    assert!(matches!(
        store.fork_session(&request).await,
        Err(lash_core_execution::StoreError::InvalidSessionId { .. })
    ));
}

lash_conformance::attachment_adoption_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await;
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    ((backend, bytes_root), factory, make_bytes)
});

lash_conformance::attachment_condemnation_recovery_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await;
    let reopen = backend.clone();
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    (
        (backend, bytes_root),
        factory,
        make_bytes,
        move || async move { reopen.reopen().await.store().await as Arc<dyn DeploymentStore> },
    )
});

#[tokio::test]
async fn sqlite_attachment_condemnation_enumeration_refuses_corrupt_rows() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await;
    let session_id = SessionId::from("condemnation-corruption");
    factory
        .admit_session(
            &lash_core_execution::testing::store_fixtures::session_store_request(
                &session_id,
                "condemnation-corruption",
                lash_core_execution::SessionRelation::Root,
            ),
        )
        .await
        .expect("materialize catalog");
    let connection = backend.raw();
    connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             INSERT INTO attachment_condemnations
                 (attachment_id, phase, write_token, sweep_generation)
             VALUES ('corrupt-condemnation', 'future-phase', NULL, 1);",
        )
        .expect("inject unknown persisted phase");
    assert!(matches!(
        lash_core_execution::AttachmentRootSet::list_condemnations(factory.as_ref()).await,
        Err(lash_core_execution::StoreError::StoredDataCorrupt { .. })
    ));
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM attachment_condemnations;
             INSERT INTO attachment_condemnations
                 (attachment_id, phase, write_token, sweep_generation)
             VALUES ('corrupt-condemnation', 'deleting', '0123456789abcdef0123456789abcdef', 1);",
        )
        .expect("inject inconsistent persisted provenance");
    assert!(matches!(
        lash_core_execution::AttachmentRootSet::list_condemnations(factory.as_ref()).await,
        Err(lash_core_execution::StoreError::StoredDataCorrupt { .. })
    ));
    connection
        .execute_batch(
            "DELETE FROM attachment_condemnations;
             INSERT INTO attachment_condemnations
                 (attachment_id, phase, sweep_generation, delete_attempts,
                  last_delete_error, stall_reason)
             VALUES ('corrupt-condemnation', 'condemned', 1, 1, 'failed', 'future-reason');",
        )
        .expect("inject an unknown persisted stall reason");
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
            let factory = backend.store().await as Arc<dyn DeploymentStore>;
            (factory, make_bytes, move || async move {
                backend.reopen().await.store().await as Arc<dyn DeploymentStore>
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
        process_env: Arc::clone(&store) as Arc<dyn ProcessExecutionEnvStore>,
        turn_preludes: store as Arc<dyn lash_core::TurnPreludeStore>,
    }
}

struct SqliteFenceIntegrityInjector {
    backend: TestBackend,
}

impl SqliteFenceIntegrityInjector {
    fn connection(&self, _target: &FenceIntegrityTarget) -> rusqlite::Connection {
        self.backend.raw()
    }
}

#[async_trait::async_trait]
impl FenceIntegrityInjector for SqliteFenceIntegrityInjector {
    async fn inject_raw_value(&self, target: &FenceIntegrityTarget, value: i64) {
        let conn = self.connection(target);
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .expect("enable pointer fault injection");
        let changed = match target {
            FenceIntegrityTarget::SessionHeadRevision { session_id } => conn.execute(
                "UPDATE session_head SET head_revision = ?1 WHERE session_id = ?2",
                rusqlite::params![value, session_id.as_str()],
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
            FenceIntegrityTarget::SessionHeadRevision { session_id } => conn
                .query_row(
                    "SELECT head.head_revision, revision.head_json, revision.leaf_node_id, revision.checkpoint_ref
                     FROM session_head AS head JOIN session_revisions AS revision USING (session_id)
                     WHERE head.session_id = ?1 ORDER BY revision.head_revision DESC LIMIT 1",
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
        }
    }
}

lash_conformance::fence_integrity_tests!({
    ((), |_case| async move {
        let backend = TestBackend::open(SUBSTRATE).await;
        FenceIntegrityHandles {
            runtime: backend.store().await,
            injector: Arc::new(SqliteFenceIntegrityInjector { backend }),
        }
    })
});

lash_conformance::artifact_store_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        let keep_reopen = retained.clone();
        lash_conformance::fused_artifact_store::ReopenableArtifactStore {
            open: artifact_store_handles(&backend),
            reopen: Arc::new(move || {
                let source = backend.clone();
                let reopened = sync_await(async move { source.reopen().await });
                keep_reopen.keep(&reopened);
                artifact_store_handles(&reopened)
            }),
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

lash_conformance::tool_material_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        let keep_reopen = retained.clone();
        lash_conformance::material_retention::ReopenableToolMaterialStore {
            open: backend.blocking_store(),
            reopen: Arc::new(move || {
                let source = backend.clone();
                let reopened = sync_await(async move { source.reopen().await });
                keep_reopen.keep(&reopened);
                reopened.blocking_store() as Arc<dyn lash_core::store::ToolMaterialStore>
            }),
        }
    })
});

lash_conformance::process_registry_reopenable_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move |_label: &str| {
        let backend = retained.open_blocking();
        sync_await({
            let backend = backend.clone();
            async move {
                lash_conformance::publish_process_registry_fixture_environments(
                    backend.process_env_store().as_ref(),
                )
                .await;
            }
        });
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
    lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    let registry = backend.process_registry();
    let recent_pushdown_id = registry
        .register_process(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
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
            ProcessCompletionAuthority::workflow_key(&recent_pushdown_id),
        )
        .await
        .expect("complete recently retired pushdown fixture");
    let terminal = match terminal {
        lash_core_execution::ProcessCompletionOutcome::Committed(record)
        | lash_core_execution::ProcessCompletionOutcome::AlreadyApplied { stored: record }
        | lash_core_execution::ProcessCompletionOutcome::Superseded { stored: record } => record,
    };

    let conn = backend.raw();
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
    lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    let registry = backend.process_registry();
    let corruption = backend.clone();
    (
        backend,
        registry as Arc<dyn ProcessRegistry>,
        move |stale: lash_core_execution::ProcessRecord| async move {
            let conn = corruption.raw();
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
            lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref())
                .await;
            lash_conformance::StoreContractHandles {
                registry: backend.process_registry() as Arc<dyn ProcessRegistry>,
                runtime: backend.store().await as Arc<dyn RuntimeStore>,
            }
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
            backend.store().await as Arc<dyn ConformanceDeployment>
        }
    })
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
        retained.open_blocking().blocking_store() as Arc<dyn RuntimeStore>
    })
});

lash_conformance::observer_intent_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law_backend = backend.as_backend();
    (backend, law_backend)
});

lash_conformance::queue_observation_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law_backend = backend.as_backend();
    (backend, law_backend)
});

lash_conformance::session_graph_append_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
    (backend, factory)
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
        let reopen = reopen.clone();
        sync_await(async move { reopen.reopen().await }).blocking_store() as Arc<dyn RuntimeStore>
    })
});

lash_conformance::append_head_switch_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let mutation = backend.clone();
    (
        backend,
        store as Arc<dyn RuntimeStore>,
        move |leaf_node_id: lash_core_execution::NodeId| async move {
            let conn = mutation.raw();
            conn.execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE")
                .expect("begin branch publication");
            conn.execute(
                "INSERT INTO session_revisions (session_id, head_revision, leaf_node_id, checkpoint_ref, head_json)
                 SELECT session_id, head_revision + 1, ?1, checkpoint_ref, head_json
                 FROM session_head JOIN session_revisions USING (session_id, head_revision)
                 WHERE session_id = 'root'",
                rusqlite::params![leaf_node_id.as_str()],
            )
            .expect("record branch revision");
            conn.execute("UPDATE session_head SET head_revision = head_revision + 1 WHERE session_id = 'root'", [])
                .expect("switch sqlite active branch");
            conn.execute_batch("COMMIT")
                .expect("commit branch publication");
        },
    )
});

lash_conformance::append_tombstone_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let mutation = backend.clone();
    (
        backend,
        store as Arc<dyn RuntimeStore>,
        move |node_id: lash_core_execution::NodeId| async move {
            let conn = mutation.raw();
            conn.execute(
                "UPDATE graph_nodes SET tombstoned = 1 WHERE node_id = ?1",
                rusqlite::params![node_id.as_str()],
            )
            .expect("tombstone sqlite old leaf");
        },
    )
});

lash_conformance::append_receipt_rewrite_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    store
        .admit_session(&root_session_request("root"))
        .await
        .expect("admit old-format receipt run");
    let mutation = backend.clone();
    (
        backend,
        store as Arc<dyn RuntimeStore>,
        move || async move {
            let conn = mutation.raw();
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
    let conn = backend.raw();
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
    let conn = backend.raw();
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
    let conn = backend.raw();
    let error = conn
        .execute(
            "INSERT INTO runtime_turn_commits (
                session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                request_identity_hash, failure_evidence, change_seq, head_revision
             ) VALUES ('half-identity', 'half-identity', 'hash', '{}', 0, 'request-hash', 0, 1, 1)",
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
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
    (backend, factory)
});

lash_conformance::attachment_stalled_retry_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
    let backend = TestBackend::open_with_clock(SUBSTRATE, clock.clone()).await;
    let factory = backend.store().await;
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    ((backend, bytes_root), factory, make_bytes, clock)
});

lash_conformance::attachment_referrer_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await;
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    let injector = backend.clone();
    let insert_edge: lash_conformance::InsertAttachmentEdge = Arc::new(move |id, kind, key| {
        let injector = injector.clone();
        Box::pin(async move {
            injector.raw()
                .execute("INSERT INTO attachment_referrer_edges (attachment_id, referrer_kind, referrer_id) VALUES (?1, ?2, ?3)", rusqlite::params![id.as_str(), kind, key])
                .map(|_| ()).map_err(|error| lash_core_execution::StoreError::Backend(error.to_string()))
        })
    });
    let handles = lash_conformance::AttachmentReferrerHandles {
        factory,
        cleanup: backend.as_stores().artifact_cleanup(),
        bytes: make_bytes,
        insert_edge,
    };
    ((backend, bytes_root), handles)
});

lash_conformance::checkpoint_profile_tests!({
    let first = TestBackend::open_with(
        SUBSTRATE,
        |mut options| {
            options.store.blob_profile = lash_sqlite_store::BuiltinBlobProfile::LowLatency;
            options
        },
        Arc::new(lash_core_execution::testing::TestClock::new(10_000)),
    )
    .await;
    let balanced = first
        .reopen_with(
            |mut options| {
                options.store.blob_profile = lash_sqlite_store::BuiltinBlobProfile::Balanced;
                options
            },
            Arc::new(lash_core_execution::testing::TestClock::new(10_000)),
        )
        .await;
    let compact = first
        .reopen_with(
            |mut options| {
                options.store.blob_profile = lash_sqlite_store::BuiltinBlobProfile::Compact;
                options
            },
            Arc::new(lash_core_execution::testing::TestClock::new(10_000)),
        )
        .await;
    let stores = vec![
        first.store().await as Arc<dyn RuntimeStore>,
        balanced.store().await,
        compact.store().await,
    ];
    ((first, balanced, compact), stores)
});

#[tokio::test]
async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms() {
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_lashlang_runtime::testing::nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(artifact_store_handles(&backend).artifacts).await;
}
