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
    GraphFactObservation, LineageConformanceHandles, LineageConformanceInjector,
    ReopenableProcessRegistry, ReopenableRuntimeStore, ReopenableTriggerStore,
};
use lash_core_execution::store::{ConformanceDeployment, RuntimeStore};
use lash_core_execution::{
    DeploymentStore, ProcessCompletionAuthority, ProcessExecutionEnvStore, ProcessIdentity,
    ProcessInput, ProcessLifecycle as _, ProcessListFilter, ProcessProvenance, ProcessQuery as _,
    ProcessRegistrar as _, ProcessRegistration, ProcessRegistry, ProcessStatusFilter,
    SessionCatalogStore, SessionCommitStore, TriggerStore,
};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSetOptions};

use super::SUBSTRATE;
use crate::backend_fixture::{Substrate, TestBackend, sync_await};

struct ScopeLawTurnRunner(lash_restate_test::RestateTestBackend);

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for ScopeLawTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core_execution::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.0
            .run_in_handler(
                admitted,
                Arc::new(move |scoped| {
                    let attempt = Arc::clone(&attempt);
                    Box::pin(async move {
                        attempt(scoped).await;
                    })
                }),
            )
            .await
            .expect("the law's turn runs inside a handler");
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core_execution::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        let handler = |attempt: lash_conformance::ConformanceTurnAttempt| -> lash_restate_test::HandlerAttempt {
            Arc::new(move |scoped| {
                let attempt = Arc::clone(&attempt);
                Box::pin(async move {
                    attempt(scoped).await;
                })
            })
        };
        self.0
            .run_crashed_then_redriven(admitted, handler(crashing), handler(redrive))
            .await
            .unwrap_or_else(|error| {
                panic!("the law's crashed turn did not redrive in its handler: {error}")
            });
    }
}

// FIG-4110: every frame open (a context-pressure frame, a pressure frame
// followed by `continue_as`, an administrative compaction) killed at each
// crash point and redriven opens once, chained in order, with one summarizer
// call. Each turn runs inside a handler of the Restate double over this
// substrate's stores.
lash_conformance::frame_open_redrive_tests!({
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let backend = TestBackend::open(SUBSTRATE).await;
    let stores = backend.as_stores();
    let double_stores = Arc::clone(&stores);
    let double = lash_restate_test::backend_with(
        4110 + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        lash_restate_test::ServerConfig::default(),
        move |_| Arc::clone(&double_stores),
    )
    .await
    .expect("boot the frame-open law's handler");
    let effect_host = double.restate().restate_effect_host();
    let runner = Arc::new(ScopeLawTurnRunner(double.clone()))
        as Arc<dyn lash_conformance::ConformanceTurnRunner>;
    (
        (backend, double),
        "sqlite-frame-open",
        effect_host,
        stores,
        runner,
    )
});

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joined_inputs_turn_scope_closes_with_its_admitting_root() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let stores = backend.as_stores();
    let double_stores = Arc::clone(&stores);
    let double = lash_restate_test::backend_with(
        4023,
        lash_restate_test::ServerConfig::default(),
        move |_| Arc::clone(&double_stores),
    )
    .await
    .expect("boot the joined-scope law's handler");
    let effect_host = double.restate().restate_effect_host();
    let runner =
        Arc::new(ScopeLawTurnRunner(double)) as Arc<dyn lash_conformance::ConformanceTurnRunner>;
    lash_conformance::registration_macro_support::a_joined_inputs_turn_scope_closes_with_its_admitting_root(
        "sqlite-joined-scope", effect_host, stores, runner,
    ).await;
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

#[path = "admission_atomicity.rs"]
mod admission_atomicity;
#[path = "attachment_store.rs"]
mod attachment_store;
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
        session_id: SessionId::from(session_id),
        relation: lash_core_execution::SessionRelation::Root,
        config: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded)
            .into(),
        head: lash_core_execution::SessionCreationHead::CommittedByCreator,
    }
}

#[tokio::test]
async fn fork_session_rejects_a_malformed_target_session_id() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    let request = lash_core_execution::ForkSessionRequest {
        session_id: SessionId::from("bad\0session"),
        node_id: lash_core_execution::NodeId::from("missing-fork-point"),
        relation: lash_core_execution::SessionRelation::Root,
        pending_observer_intents: Vec::new(),
        policy: lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
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
    let connection = backend.raw(SqliteDatabase::DurableCore);
    connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             INSERT INTO attachment_condemnations
                 (attachment_id, phase, write_token, write_session_id, sweep_generation)
             VALUES ('corrupt-condemnation', 'future-phase', NULL, NULL, 1);",
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
                 (attachment_id, phase, write_token, write_session_id, sweep_generation)
             VALUES ('corrupt-condemnation', 'deleting', 'opaque', 'session', 1);",
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
                runtime: backend.store().await as Arc<dyn RuntimeStore>,
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
                backend.store().await,
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
            backend.store().await as Arc<dyn ConformanceDeployment>
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
    let make_retained = retained.clone();
    let make =
        move || make_retained.open_blocking().blocking_store() as Arc<dyn ConformanceDeployment>;
    let attached_retained = retained.clone();
    let make_attached = move || {
        let backend = attached_retained.open_blocking();
        (
            backend.blocking_store() as Arc<dyn ConformanceDeployment>,
            backend.attachment_store() as Arc<dyn lash_core_execution::AttachmentStore>,
        )
    };
    let (engine, effect_host) = promise_authority().await;
    ((retained, engine), make, make_attached, effect_host)
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

lash_conformance::session_graph_append_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
    (backend, factory)
});

lash_conformance::process_prune_session_store_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
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
                let open = backend.store().await;
                open.admit_session(&request)
                    .await
                    .expect("admit SQLite conformance session");
                let reopen = backend.reopen().await.store().await;
                (backend, open, reopen)
            });
            let effect_host = Arc::clone(&effect_host);
            retained.keep(&backend);
            ReopenableRuntimeStore {
                open: open as Arc<dyn RuntimeStore>,
                reopen: reopen as Arc<dyn RuntimeStore>,
                effect_host,
            }
        },
        lash_conformance::RuntimePersistenceLeaseTiming::controlled({
            let clock = Arc::clone(&clock);
            move |duration_ms| clock.advance(duration_ms)
        }),
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
        store as Arc<dyn RuntimeStore>,
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
    store
        .admit_session(&root_session_request("root"))
        .await
        .expect("admit receipt envelope session");
    (backend, store as Arc<dyn RuntimeStore>)
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
        let store = backend.store().await;
        store
            .admit_session(&root_session_request("root"))
            .await
            .expect("admit cancellation session");
        let committed_store = Arc::clone(&store);
        (backend, store, move || {
            // The append commit is the first write after the pause is armed.
            let pause = injector.pause(SqliteFaultPoint::BeforeCommit);
            async move {
                pause.wait_until_reached().await;
                move || {
                    pause.release();
                    sync_await(async move {
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                match lash_core_execution::SessionHistoryStore::load_session_window(
                                    committed_store.as_ref(),
                                    &SessionId::from("root"),
                                    lash_core_execution::store::WindowSelector::Current,
                                )
                                .await
                                .expect("read cancelled append after release")
                                {
                                    Some(_) => break,
                                    None => {
                                        tokio::time::sleep(std::time::Duration::from_millis(1))
                                            .await
                                    }
                                }
                            }
                        })
                        .await
                        .expect("cancelled append commits after seam release");
                    });
                }
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
        store as Arc<dyn RuntimeStore>,
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

lash_conformance::usage_ledger_store_tests!({
    use lash_core_execution::StoreSet as _;
    let backend = TestBackend::open(SUBSTRATE).await;
    let snapshot_backend = backend.clone();
    let snapshot: lash_conformance::UsageLedgerSnapshot = Arc::new(move || {
        let backend = snapshot_backend.clone();
        Box::pin(async move {
            let connection = backend.raw(SqliteDatabase::DurableCore);
            let tables = connection.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'usage_%' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap()
                .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
            let mut snapshot = Vec::new();
            for table in tables {
                let mut statement = connection
                    .prepare(&format!("SELECT * FROM \"{table}\""))
                    .unwrap();
                let count = statement.column_count();
                let mut rows = statement
                    .query_map([], |row| {
                        (0..count)
                            .map(|index| row.get::<_, rusqlite::types::Value>(index))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .map(|row| format!("{:?}", row.unwrap()))
                    .collect::<Vec<_>>();
                rows.sort();
                snapshot.push((table, rows.join("\n")));
            }
            snapshot
        })
    });
    let fixture = lash_conformance::UsageLedgerStoreFixture {
        accounting: backend.usage_accounting(),
        factory: backend.session_store_factory(),
        snapshot,
    };
    (backend, fixture)
});
