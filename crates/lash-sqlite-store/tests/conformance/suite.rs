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

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use lash_conformance::{
    FenceIntegrityHandles, FenceIntegrityInjector, FenceIntegrityObservation, FenceIntegrityTarget,
    GraphFactObservation, GraphIntegrityCorruption, GraphIntegrityHandles, GraphIntegrityInjector,
    GraphIntegrityRead, GraphIntegrityTarget, LineageConformanceHandles,
    LineageConformanceInjector, ReopenableProcessRegistry, ReopenableRuntimePersistence,
    ReopenableTriggerStore, SessionExecutionLeaseRenewalZeroRowHandles,
    SessionExecutionLeaseRenewalZeroRowInjector,
};
use lash_core_execution::store::ConformanceSessionStoreFactory;
use lash_core_execution::{
    AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    ProcessCompletionAuthority, ProcessExecutionEnvStore, ProcessIdentity, ProcessInput,
    ProcessLifecycle as _, ProcessListFilter, ProcessProvenance, ProcessQuery as _,
    ProcessRegistrar as _, ProcessRegistration, ProcessRegistry, ProcessStatusFilter,
    RecoveryContract, Resolution, ResolveOutcome, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimePersistence, SessionCommitStore,
    SessionStoreFactory, TriggerStore,
};
use lash_sqlite_store::{
    SqliteBackendOptions, SqliteDatabase, SqliteEffectReplayOptions, SqliteRuntimeEffectController,
};

use super::SUBSTRATE;
use crate::backend_fixture::durable_turn_scope;
use crate::backend_fixture::{TestBackend, sync_await};

#[path = "attachment_store.rs"]
mod attachment_store;
#[path = "await_event_discovery.rs"]
mod await_event_discovery;
#[path = "cancelled_turn_withheld_input.rs"]
mod cancelled_turn_withheld_input;
#[path = "claim_atomicity.rs"]
mod claim_atomicity;
#[path = "direct_turn_acceptance.rs"]
mod direct_turn_acceptance;
#[path = "drain_end.rs"]
mod drain_end;
#[path = "effect_group.rs"]
mod effect_group;
#[path = "effect_group_drain.rs"]
mod effect_group_drain;
#[path = "lineage.rs"]
mod lineage;
#[path = "lock_order.rs"]
mod lock_order;
#[path = "pre_frame_key.rs"]
mod pre_frame_key;
#[path = "pre_sleep_spec.rs"]
mod pre_sleep_spec;
#[path = "process_prune_reclaim.rs"]
mod process_prune_reclaim;
#[path = "process_retention.rs"]
mod process_retention;
#[path = "restored_claim_cede.rs"]
mod restored_claim_cede;
#[path = "session_delete_blob_reclaim.rs"]
mod session_delete_blob_reclaim;
#[path = "session_ingress.rs"]
mod session_ingress;
#[path = "session_meta.rs"]
mod session_meta;
#[path = "session_read_view.rs"]
mod session_read_view;
#[path = "sleep_replay.rs"]
mod sleep_replay;
#[path = "store_maintenance.rs"]
mod store_maintenance;
#[path = "tool_child_invocation.rs"]
mod tool_child_invocation;
#[path = "trigger_occurrence_retention.rs"]
mod trigger_occurrence_retention;
#[path = "turn_runner.rs"]
mod turn_runner;
#[path = "wake_delivery.rs"]
mod wake_delivery;

include!("append_identity.rs");
include!("effect_lease_fencing.rs");

/// Backends a fixture opened and must keep alive for its whole law.
#[derive(Clone, Default)]
struct Retained(Arc<Mutex<Vec<TestBackend>>>);

impl Retained {
    fn keep(&self, backend: &TestBackend) {
        self.0.lock_recover().push(backend.clone());
    }

    /// A fresh backend, kept alive with the fixture.
    fn open_blocking(&self) -> TestBackend {
        let backend = TestBackend::blocking(SUBSTRATE);
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
    let retained = Retained::default();
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

fn sqlite_conformance_invocation(
    controller: SqliteRuntimeEffectController,
    execution_scope: ExecutionScope,
) -> lash_conformance::ConformanceInvocation {
    let live: Arc<dyn RuntimeEffectController> = Arc::new(controller.clone());
    lash_conformance::ConformanceInvocation::new(
        live,
        execution_scope,
        || {},
        move || {
            controller.start_replay();
            Arc::new(controller.clone()) as Arc<dyn RuntimeEffectController>
        },
    )
}

/// A controller over a fresh backend's journal, and the backend that
/// keeps it alive.
async fn open_effect_controller(
    scope: ExecutionScope,
) -> (TestBackend, SqliteRuntimeEffectController) {
    let backend = TestBackend::open(SUBSTRATE).await;
    let controller = backend
        .open_effect_controller(scope)
        .await
        .expect("open the effect controller");
    (backend, controller)
}

/// Effect-replay options whose leases last `lease_timings`.
fn with_lease_timings(
    lease_timings: lash_core_execution::facade_support::LeaseTimings,
) -> impl FnOnce(SqliteBackendOptions) -> SqliteBackendOptions {
    move |options| SqliteBackendOptions {
        effect_replay: SqliteEffectReplayOptions {
            lease_timings,
            ..options.effect_replay.clone()
        },
        ..options
    }
}

struct SqliteSessionExecutionLeaseRenewalZeroRowInjector {
    backend: TestBackend,
}

#[async_trait::async_trait]
impl SessionExecutionLeaseRenewalZeroRowInjector
    for SqliteSessionExecutionLeaseRenewalZeroRowInjector
{
    async fn arm(&self, session_id: &SessionId) {
        assert_eq!(session_id, "zero-row-session-lease-renewal");
        self.backend
            .raw(SqliteDatabase::DurableCore)
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
        self.backend
            .raw(SqliteDatabase::DurableCore)
            .execute_batch("DROP TRIGGER lash_test_session_lease_renewal_zero_row;")
            .expect("disarm SQLite zero-row renewal trigger");
    }
}

lash_conformance::session_execution_lease_renewal_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let store = backend.store().await;
    (
        (),
        SessionExecutionLeaseRenewalZeroRowHandles {
            store: store as Arc<dyn RuntimePersistence>,
            injector: Arc::new(SqliteSessionExecutionLeaseRenewalZeroRowInjector { backend }),
        },
    )
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
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => conn.execute(
                "UPDATE session_execution_leases SET lease_fencing_token = ?1 WHERE session_id = ?2",
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
            FenceIntegrityTarget::SessionLeaseFencingToken { session_id } => conn
                .query_row(
                    "SELECT lease_fencing_token, lease_owner_id, lease_token,
                            lease_claimed_at_ms, lease_expires_at_ms
                     FROM session_execution_leases WHERE session_id = ?1",
                    [session_id.as_str()],
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
    let retained = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        lash_conformance::fused_artifact_store::ReopenableArtifactStore {
            open: artifact_store_handles(&backend),
            reopen: Arc::new(move || artifact_store_handles(&backend)),
        }
    })
});

fn value_envelope(
    scope: ExecutionScope,
    attribution: lash_core_execution::RuntimeAttribution,
    replay_key: &str,
    operation: &str,
) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(scope, replay_key)
                .expect("valid SQLite effect address"),
            attribution,
            replay_key,
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: operation.to_string(),
        },
    )
}

fn value_outcome(marker: &str) -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::LanguageRuntimeValue {
        value: serde_json::json!(marker),
    }
}

fn assert_value_marker(outcome: RuntimeEffectOutcome, expected: &str) {
    let RuntimeEffectOutcome::LanguageRuntimeValue { value } = outcome else {
        panic!("expected language-runtime-value outcome");
    };
    assert_eq!(value, serde_json::json!(expected));
}

fn returning_executor(marker: &'static str) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |_| async move { Ok(value_outcome(marker)) })
}

fn failing_executor() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async move {
        Err(RuntimeEffectControllerError::foreign(
            "test_local_executor_called",
            lash_core::TurnFailureCause::Outcome,
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

lash_conformance::process_registry_reopenable_tests!({
    let retained = Retained::default();
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
    registry
        .register_process(
            ProcessRegistration::new(
                "recent-pushdown",
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
                lash_core_execution::ProcessLifecyclePolicy::new(
                    lash_core_execution::ParentScope::Host,
                    lash_core_execution::OnParentEnd::Abandon,
                ),
            )
            .with_admitted_identity(
                lash_core_execution::AdmittedProcessIdentity::for_testing(ProcessIdentity::new(
                    "recent-pushdown-kind",
                )),
            ),
        )
        .await
        .expect("register recently retired pushdown fixture");
    let terminal = registry
        .complete_process(
            &ProcessId::from("recent-pushdown"),
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
    let retained = Retained::default();
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
    let retained = Retained::default();
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
    let retained = Retained::default();
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
    let retained = Retained::default();
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
    let effect_host = unbound_backend.effect_host() as Arc<dyn EffectHost>;
    (
        retained,
        "sqlite",
        unbound,
        make,
        make_attached,
        effect_host,
    )
});

lash_conformance::fresh_session_admission_tests!({
    let retained = Retained::default();
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

lash_conformance::attachment_owner_cold_replay_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        current_epoch_ms_for_test().saturating_sub(100_000),
    ));
    let backend = TestBackend::open_with_clock(
        SUBSTRATE,
        clock.clone() as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    let scope = durable_turn_scope("attachment-owner-cold-replay", "attachment-owner-turn");
    let first = Arc::new(
        backend
            .open_effect_controller(scope.clone())
            .await
            .expect("first effect controller"),
    ) as Arc<dyn RuntimeEffectController>;
    let reopen_effect_controller = {
        let backend = backend.clone();
        Arc::new(move || {
            let backend = backend.clone();
            let scope = scope.clone();
            Box::pin(async move {
                Arc::new(
                    backend
                        .reopen()
                        .await
                        .open_effect_controller(scope)
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

    let attachment_store = backend.attachment_store();
    (
        backend,
        lash_conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: factory,
            process_registry: registry,
            attachment_store,
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock,
            advance_clock,
        },
    )
});

lash_conformance::process_prune_session_store_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    let effect_host = backend.effect_host() as Arc<dyn EffectHost>;
    (backend, factory, registry, effect_host)
});

lash_conformance::runtime_persistence_clock_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(20_000));
    let advance_clock = Arc::clone(&clock);
    let verify_clock = Arc::clone(&clock);
    let backend = TestBackend::open_with_clock(
        SUBSTRATE,
        clock.clone() as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let store = backend.store().await as Arc<dyn RuntimePersistence>;
    (
        backend,
        store,
        move |duration_ms| advance_clock.advance(duration_ms),
        move |store: Arc<dyn RuntimePersistence>| async move {
            let observation = store
                .get_session_execution_lease(&SessionId::from("sqlite-injected-clock-diagnostic"))
                .await
                .expect("read SQLite session-lease diagnostics");
            assert_eq!(
                observation.observed_at_epoch_ms,
                lash_core_execution::ClockWallTime::timestamp_ms(verify_clock.as_ref()),
                "SQLite diagnostics must return the same injected clock that authors lease timestamps"
            );
        },
    )
});

lash_conformance::trigger_store_reopenable_tests!({
    let retained = Retained::default();
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
    let retained = Retained::default();
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(10_000));
    let store_clock = Arc::clone(&clock);
    (
        retained.clone(),
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
            let effect_host = backend.effect_host() as Arc<dyn EffectHost>;
            retained.keep(&backend);
            ReopenableRuntimePersistence {
                open,
                reopen,
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
    (Retained::default(), move |_admission_state| async move {
        let backend = TestBackend::open(SUBSTRATE).await;
        let factory = backend.session_store_factory();
        lash_conformance::UnboundSessionResolutionHandles {
            backend_name: "SQLite",
            factory,
            open_unbound: Arc::new(move || backend.blocking_store() as Arc<dyn RuntimePersistence>),
        }
    })
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

/// One journal for a crash law's scenarios: every turn a law drives, a
/// drain turn included, binds the same turn-control authority, which is this
/// journal's. Its effect leases lapse on the recovery timings, so a successor
/// controller reclaims what a crashed one held.
fn crash_journal() -> TestBackend {
    sync_await(async move {
        TestBackend::open_with(
            SUBSTRATE,
            with_lease_timings(
                lash_core_execution::facade_support::LeaseTimings::new(
                    std::time::Duration::from_millis(600),
                    std::time::Duration::from_millis(100),
                )
                .expect("crash-law effect lease timings"),
            ),
            crate::backend_fixture::system_clock(),
        )
        .await
    })
}

/// A journaled invocation over `journal`. Its redrive opens a successor
/// controller over the same journal, the way a restarted process does: its
/// own group-executor registration, the journal's completed effects replayed.
fn journaled_crash_invocation(
    journal: &TestBackend,
    scope: ExecutionScope,
) -> lash_conformance::ConformanceInvocation {
    let open = {
        let journal = journal.clone();
        let scope = scope.clone();
        move || {
            let journal = journal.clone();
            let scope = scope.clone();
            sync_await(async move {
                journal
                    .open_effect_controller(scope)
                    .await
                    .expect("journaled crash controller")
            })
        }
    };
    let controller = open();
    let faults = controller.effect_journal_faults();
    lash_conformance::ConformanceInvocation::new(
        Arc::new(controller) as Arc<dyn RuntimeEffectController>,
        scope,
        || {},
        move || Arc::new(open()) as Arc<dyn RuntimeEffectController>,
    )
    .with_effect_journal_faults(faults)
}

/// The error-return sweep's journal (FIG-3524): its short renew interval lets
/// a `renew` fault fire while the parked tool attempt is still open.
fn error_return_journal() -> TestBackend {
    sync_await(async move {
        TestBackend::open_with(
            SUBSTRATE,
            with_lease_timings(
                lash_core_execution::facade_support::LeaseTimings::new(
                    std::time::Duration::from_secs(60),
                    std::time::Duration::from_millis(50),
                )
                .expect("error-return effect lease timings"),
            ),
            crate::backend_fixture::system_clock(),
        )
        .await
    })
}

/// The `make` element of a turn-crash runner fixture: opens a crash-law
/// scenario's session store over the fixture's substrate.
type JournalStoreOpener =
    Box<dyn Fn(&str) -> Arc<lash_sqlite_store::Store> + Send + Sync + 'static>;

/// The `(guard, stores, make, host, runner)` tuple the turn-crash runner
/// macros destructure: `guard` keeps the fixture's backends alive, `host` is
/// the journal's own effect host and `runner` cuts its turns.
type JournalRunnerFixture = (
    Retained,
    Arc<dyn lash_core_execution::StoreSet>,
    JournalStoreOpener,
    Arc<dyn EffectHost>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
);

/// A turn-crash runner fixture over `journal`'s own effect host: the runner
/// cuts turns with that journal's fault injector.
fn journal_runner_fixture(journal: TestBackend) -> JournalRunnerFixture {
    let scenarios = ScenarioBackends::new(crate::backend_fixture::system_clock());
    let retained = Retained::default();
    let stores = retained.open_blocking().as_stores();
    retained.keep(&journal);
    let host = journal.effect_host();
    let faults = host.effect_journal_faults();
    let host = host as Arc<dyn EffectHost>;
    (
        retained,
        stores,
        Box::new(move |scenario: &str| scenarios.concrete_store(scenario)),
        Arc::clone(&host),
        lash_conformance::HostTurnRunner::with_journal_faults(host, faults),
    )
}

lash_conformance::turn_crash_matrix_tests!({ journal_runner_fixture(error_return_journal()) });

// The level-one matrix crashes each turn in process. On the journaled SQLite
// engine the crashed attempt's group child keeps running and renewing its
// effect lease, which nothing in the process can stop, so the successor waits
// on it forever. The real SIGKILL matrix covers this engine's crash recovery.
lash_conformance::turn_crash_level_1_tests!(
    #[ignore = "parked: an in-process crash cannot stop the journaled engine's attempt (FIG-3668)"]
    {
        journal_runner_fixture(crash_journal())
    }
);

// The turn crash laws that run their turns on a turn runner: the FIG-3571
// generation-refusal pair, the direct-acceptance crash and the cancel-closure
// cuts, on the crash journal's own host. A crash drops the turn's task, and
// the recovery is a fresh runtime over the same stores and journal.
lash_conformance::turn_crash_runner_tests!({ journal_runner_fixture(crash_journal()) });

lash_conformance::effect_layer_group_child_tests!({ journal_runner_fixture(crash_journal()) });

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_held_turn_input_visibility_survives_claim_holder_crash() {
    let scenarios = ScenarioBackends::new(crate::backend_fixture::system_clock());
    let retained = Retained::default();
    let stores = retained.open_blocking().as_stores();
    let journal = crash_journal();
    retained.keep(&journal);
    Box::pin(
        lash_conformance::held_turn_input_visibility_survives_claim_holder_crash(
            stores,
            |scenario| scenarios.store(scenario),
            |_, scope| journaled_crash_invocation(&journal, scope),
        ),
    )
    .await;
}

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
                move |options| SqliteBackendOptions {
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
            // Pause the graph append after lease-fenced admission's write transaction.
            let pause = injector.pause_after(SqliteFaultPoint::BeforeCommit, 2);
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

fn raw_count(conn: &rusqlite::Connection, sql: &str, name: &str) -> i64 {
    conn.query_row(sql, rusqlite::params![name], |row| row.get::<_, i64>(0))
        .expect("query sqlite_master")
}

lash_conformance::effect_host_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let reopen = backend.clone();
    (backend, move || {
        let backend = reopen.clone();
        sync_await(async move { backend.reopen().await.effect_host() }) as Arc<dyn EffectHost>
    })
});

lash_conformance::turn_work_driver_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let host = backend.effect_host() as Arc<dyn EffectHost>;
    let law_backend = backend.as_stores();
    (
        backend,
        host,
        law_backend,
        lash_conformance::await_event_registration_observed,
    )
});

lash_conformance::effect_host_await_event_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let reopen = backend.clone();
    let foreign = Retained::default();
    (
        (backend, foreign.clone()),
        move || {
            let backend = reopen.clone();
            sync_await(async move { backend.reopen().await.effect_host() }) as Arc<dyn EffectHost>
        },
        lash_conformance::effect_host_journaled_wait_registration_witness,
        // Another backend is another registry.
        move || foreign.open_blocking().effect_host() as Arc<dyn EffectHost>,
    )
});

lash_conformance::tool_batch_parallelism_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let host = backend.effect_host() as Arc<dyn EffectHost>;
    let law_backend = backend.as_stores();
    (
        backend,
        "sqlite",
        Arc::clone(&host),
        law_backend,
        // The producers this crate reaches. `Promise.all` on the RLM bridge and
        // the Lashlang aggregate on the process bridge register the same law
        // from the crates that own them.
        vec![lash_conformance::parallel_model_tool_calls_producer()],
        lash_conformance::HostTurnRunner::shared(host),
    )
});

/// Every store-backed backend issues completion keys: the promise rows
/// live as long as the backend, file or memory, and a key resolves for
/// that whole lifetime (ADR 0102).
#[tokio::test]
async fn sqlite_backends_issue_completion_keys() {
    let scope = durable_turn_scope("completion-key-session", "completion-key-turn");
    let (_backend, controller) = open_effect_controller(scope.clone()).await;
    assert!(matches!(
        controller
            .prepare_completion_key(
                &scope,
                lash_core_execution::AwaitEventWaitIdentity::tool_completion("completion-call"),
                true,
            )
            .await
            .expect("completion-key preparation"),
        lash_core_execution::CompletionKeyPreparation::Issued(_)
    ));
}

lash_conformance::effect_host_cold_await_event_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    // The parked-owner vector abandons an in-progress effect, so its cold
    // successor must wait one lease TTL before reclaiming it. A one-second
    // test policy preserves that semantic wait without spending the
    // production-default 30 seconds; the derived renewal interval also starts
    // after the vector's 250ms proof that the original owner remains parked.
    let lease_timings = lash_core_execution::facade_support::LeaseTimings::from_ttl(
        std::time::Duration::from_secs(1),
    )
    .expect("cold-instance conformance lease timings");
    let reopen = backend.clone();
    let catalog = backend.clone();
    let make = move || {
        let backend = reopen.clone();
        sync_await(async move {
            backend
                .reopen_with(
                    with_lease_timings(lease_timings),
                    crate::backend_fixture::system_clock(),
                )
                .await
                .effect_host()
        }) as Arc<dyn EffectHost>
    };
    let make_catalog = move || {
        let backend = catalog.clone();
        sync_await(async move { backend.reopen().await.session_store_factory() })
            as Arc<dyn lash_core_execution::SessionStoreFactory>
    };
    (backend, make, make_catalog)
});

#[tokio::test]
async fn sqlite_await_event_key_mint_is_pure_and_store_secret_is_stable() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let scope = durable_turn_scope("pure-key-session", "pure-key-turn");
    let wait = AwaitEventWaitIdentity::tool_completion("pure-key-call");

    let (first, second) = tokio::join!(
        async {
            backend
                .reopen()
                .await
                .effect_host()
                .await_event_key(&scope, wait.clone())
                .await
                .expect("first concurrent key")
        },
        async {
            backend
                .reopen()
                .await
                .effect_host()
                .await_event_key(&scope, wait.clone())
                .await
                .expect("second concurrent key")
        },
    );
    assert_eq!(
        first, second,
        "concurrent openers must read one store secret"
    );

    let observer = backend.reopen().await.effect_host();
    assert!(
        observer
            .list_outstanding_await_event_keys(&SessionId::from("unknown-pure-key-session"))
            .await
            .expect("unknown session read")
            .is_empty()
    );
    assert!(
        observer
            .list_outstanding_await_event_keys(&SessionId::from("pure-key-session"))
            .await
            .expect("minted-only session read")
            .is_empty()
    );

    let connection = backend.raw(SqliteDatabase::EffectReplay);
    let wait_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM await_event_waits", [], |row| {
            row.get(0)
        })
        .expect("count await-event waits");
    assert_eq!(
        wait_count, 0,
        "key mint and administrative reads must not register a promise row"
    );
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
    let backend = TestBackend::open(SUBSTRATE).await;
    let scope = durable_turn_scope("corrupt-terminal-session", "corrupt-terminal-turn");
    let host = backend.effect_host();
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

    let connection = backend.raw(SqliteDatabase::EffectReplay);
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
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(INJECTED_MS));
    let backend = TestBackend::open_with_clock(
        SUBSTRATE,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let host = backend.effect_host();
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

    let connection = backend.raw(SqliteDatabase::EffectReplay);
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
/// implementor of the same shared `StoreEffectReplayDriver` — deliberately reads the
/// *server* clock instead (the `Clock` contract's database-authoritative lease
/// boundary, fenced by `postgres_clock_contract`), so
/// each half of that split needs its own referee now that one driver drives
/// both. The driver's own clock only sleeps; if it ever stamped a row, the
/// stamps below would come from the OS clock instead.
#[tokio::test]
async fn sqlite_effect_replay_rows_are_stamped_by_the_injected_clock() {
    const INJECTED_MS: u64 = 1_234_567_890_000;
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(INJECTED_MS));
    let backend = TestBackend::open_with_clock(
        SUBSTRATE,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let controller = backend
        .open_effect_controller(durable_turn_scope(
            "injected-clock-effect-session",
            "injected-clock-effect-turn",
        ))
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
                    value_envelope(
                        durable_turn_scope(
                            "injected-clock-effect-session",
                            "injected-clock-effect-turn",
                        ),
                        lash_core_execution::RuntimeAttribution::for_turn(
                            "injected-clock-effect-session",
                            "injected-clock-effect-turn",
                            1,
                            0,
                        ),
                        "injected-clock-effect",
                        "first",
                    ),
                    RuntimeEffectLocalExecutor::testing(move |_| async move {
                        let _ = entered_tx.send(());
                        executor_release.notified().await;
                        Ok(value_outcome("stamped"))
                    }),
                )
                .await
        }
    });
    entered_rx.await.expect("executor entered under the claim");

    let claim_backend = backend.clone();
    let (created_at_ms, lease_expires_at_ms) = tokio::task::spawn_blocking(move || {
        let connection = claim_backend.raw(SqliteDatabase::EffectReplay);
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
        (INJECTED_MS + lash_core_execution::facade_support::LeaseTimings::default().ttl_ms())
            as i64,
        "the lease expiry must be derived from the injected claim instant"
    );

    release.notify_waiters();
    assert_value_marker(
        executing
            .await
            .expect("execution task joins")
            .expect("finalize the claimed effect"),
        "stamped",
    );

    let connection = backend.raw(SqliteDatabase::EffectReplay);
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

lash_conformance::effect_controller_replay_tests!({
    let scope = durable_turn_scope("effect-conformance-session", "effect-conformance-turn");
    let (backend, controller) = open_effect_controller(scope.clone()).await;
    (backend, move || {
        sqlite_conformance_invocation(controller.clone(), scope.clone())
    })
});

lash_conformance::effect_controller_response_derivation_tests!({
    let scope = durable_turn_scope("effect-conformance-session", "effect-conformance-turn");
    let (backend, controller) = open_effect_controller(scope.clone()).await;
    (backend, move || {
        sqlite_conformance_invocation(controller.clone(), scope.clone())
    })
});

#[tokio::test]
async fn sqlite_effect_controller_replays_without_local_executor() {
    let scope = durable_turn_scope("session", "turn");
    let (_backend, controller) = open_effect_controller(scope.clone()).await;
    let envelope = value_envelope(
        scope,
        lash_core_execution::RuntimeAttribution::for_turn("session", "turn", 1, 0),
        "exec-replay",
        "first",
    );
    let first = controller
        .execute_effect(envelope.clone(), returning_executor("recorded"))
        .await
        .expect("first effect");
    assert_value_marker(first, "recorded");

    controller.start_replay();
    let replayed = controller
        .execute_effect(envelope, failing_executor())
        .await
        .expect("replayed effect");
    assert_value_marker(replayed, "recorded");
}

#[tokio::test]
async fn sqlite_effect_controller_replays_a_non_empty_recorded_intent_batch() {
    let backend = TestBackend::open(SUBSTRATE).await;
    let scope = durable_turn_scope("sqlite-intent-session", "sqlite-intent-turn");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            lash_core_execution::EffectAddress::new(
                scope.clone(),
                "sqlite-recorded-intent-attempt",
            )
            .expect("valid SQLite intent address"),
            lash_core_execution::RuntimeAttribution::for_turn(
                "sqlite-intent-session",
                "sqlite-intent-turn",
                0,
                0,
            ),
            "sqlite-recorded-intent-attempt",
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: lash_core_execution::PreparedToolCall::from_parts(
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
        launch: Box::new(lash_core_execution::ToolAttemptLaunch::Done {
            record: Box::new(lash_core_execution::ToolCallRecord {
                call_id: Some("sqlite-intent-call".to_string()),
                tool: "sqlite_intent_leaf".to_string(),
                args: serde_json::json!({"value": "record"}),
                output: lash_core_execution::ToolCallOutput::success(serde_json::json!({
                    "provider": "done"
                })),
                duration_ms: 7,
            }),
            intents: lash_core_execution::ToolIntents::v3(vec![
                lash_core_execution::ToolIntent::EmitProcessEvent(
                    lash_core_execution::EmitProcessEventIntent {
                        session_id: SessionId::from("sqlite-intent-session"),
                        process_id: ProcessId::from("sqlite-intent-target"),
                        event_type: "sqlite.intent.recorded".to_string(),
                        payload: serde_json::json!({"literal": true}),
                    },
                ),
            ]),
        }),
        triggers: Vec::new(),
        capture: None,
    };
    let expected_bytes = serde_json::to_vec(&expected).expect("serialize literal intent outcome");
    let first_controller = backend
        .open_effect_controller(scope.clone())
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

    let replay_controller = backend
        .reopen()
        .await
        .open_effect_controller(scope)
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

lash_conformance::effect_host_retirement_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let host = backend.effect_host() as Arc<dyn EffectHost>;
    (backend, host)
});

lash_conformance::effect_controller_replay_mismatch_tests!({
    let scope = durable_turn_scope("session", "turn");
    let (backend, controller) = open_effect_controller(scope.clone()).await;
    (
        backend,
        move || sqlite_conformance_invocation(controller.clone(), scope.clone()),
        "sqlite_effect_replay_hash_conflict",
    )
});

lash_conformance::retention_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    (backend, factory)
});
