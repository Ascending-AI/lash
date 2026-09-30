//! Recorded batch boundaries and negative controls for the cancellation oracle.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use lash_conformance::{ConformanceTurnAttempt, ConformanceTurnRunner};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy)]
enum LateMutation {
    Final,
    Accounting,
}

struct MutatingRunner {
    inner: Arc<dyn ConformanceTurnRunner>,
    server: lash_restate_test::RestateTestServer,
    stores: lash_sqlite_store::SqliteStoreSet,
    mutation: LateMutation,
    applied: AtomicBool,
}

#[async_trait::async_trait]
impl ConformanceTurnRunner for MutatingRunner {
    async fn run_turn(&self, admitted: lash_core::AdmittedScope, attempt: ConformanceTurnAttempt) {
        self.inner.run_turn(admitted, attempt).await;
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    ) {
        self.inner
            .run_crashed_then_redriven_turn(admitted, crashing, redrive)
            .await;
    }

    async fn await_group_quiescence(&self, group_keys: &[String]) {
        self.inner.await_group_quiescence(group_keys).await;
        match self.mutation {
            LateMutation::Final => {
                let key = &group_keys[0];
                let mut state = self.server.object_state("EffectGroupIndex", key);
                let bytes = state
                    .get_mut("effect-group/v1/state")
                    .expect("the durable group state exists");
                let mut record: serde_json::Value =
                    serde_json::from_slice(bytes).expect("decode the group state");
                let finals = record["body"]["lifecycle"]["live"]["settlements"]
                    .as_array_mut()
                    .expect("the group retained finals");
                let cancelled = finals
                    .iter_mut()
                    .find(|pair| pair[1]["terminal"]["type"] == "cancelled")
                    .expect("the group has a cancelled final");
                cancelled[1]["terminal"] = serde_json::json!({ "type": "failed", "error":
                    lash_core::RuntimeEffectControllerError::new(lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
                        "injected late final overwrite") });
                *bytes = serde_json::to_vec(&record).expect("encode the overwritten final");
                self.server.set_object_state("EffectGroupIndex", key, state);
            }
            LateMutation::Accounting => {
                let connection = rusqlite::Connection::open(
                    self.stores
                        .database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
                )
                .expect("open the law's durable accounting database");
                let changed = connection.execute("INSERT INTO usage_deltas (
                    session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash,
                    source, model, input_tokens, output_tokens, cache_read_input_tokens,
                    cache_write_input_tokens, reasoning_output_tokens, reconciled_call_id, reconciled_attempt_ordinal)
                    SELECT session_id, operation_storage_key || ':late-duplicate', entry_ordinal,
                    payload_encoding_version, payload_hash, source, model, input_tokens, output_tokens,
                    cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens,
                    reconciled_call_id, reconciled_attempt_ordinal FROM usage_deltas WHERE input_tokens > 0 LIMIT 1", [])
                    .expect("inject a duplicate durable charge");
                assert_eq!(
                    changed, 1,
                    "the mutation duplicated a real persisted charge"
                );
            }
        }
        self.applied.store(true, Ordering::SeqCst);
    }
}

fn factories() -> lash_conformance::BatchSugarFactories {
    lash_conformance::BatchSugarFactories {
        enabled: vec![Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::new(),
        )],
        disabled: super::tool_batch_parallelism_on_the_double::withheld_factories(),
    }
}

async fn rejects_late_mutation(mutation: LateMutation, expected: &str) {
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite fixture");
    let runner = Arc::new(MutatingRunner {
        inner: harness.turn_runner(),
        server: harness.server_double().expect("double"),
        stores: stores.clone(),
        mutation,
        applied: AtomicBool::new(false),
    });
    let prefix = format!("cancel-oracle-{}", harness.run_nonce());
    let host = harness.endpoint_host();
    let driving = runner.clone();
    let law = tokio::spawn(async move {
        lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
            &prefix,
            host,
            Arc::new(stores),
            driving,
            factories(),
        )
        .await;
    });
    let panic = law
        .await
        .expect_err("the mutation must turn the law red")
        .into_panic();
    assert!(
        runner.applied.load(Ordering::SeqCst),
        "the durable mutation actually ran"
    );
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(
        message.contains(expected),
        "the intended oracle rejected the mutation: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cancellation_oracle_rejects_late_final_overwrite() {
    rejects_late_mutation(
        LateMutation::Final,
        "late settlements preserve durable finals",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cancellation_oracle_rejects_duplicate_accounting() {
    rejects_late_mutation(
        LateMutation::Accounting,
        "late settlements preserve durable finals",
    )
    .await;
}

async fn recorded_boundaries(
    harness: &LiveConformanceHarness,
    stores: Arc<dyn lash_core::StoreSet>,
    storage: &str,
) {
    let prefix = format!("recorded-batch-{storage}-{}", harness.run_nonce());
    lash_conformance::registration_macro_support::batch_redrive_reuses_children(
        &prefix,
        harness.endpoint_host(),
        stores.clone(),
        harness.turn_runner(),
        factories(),
    )
    .await;
    lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
        &prefix,
        harness.endpoint_host(),
        stores,
        harness.turn_runner(),
        factories(),
    )
    .await;
}

async fn sqlite_double_fixture(
    always_replay: bool,
    file: bool,
) -> (
    LiveConformanceHarness,
    tempfile::TempDir,
    Arc<dyn lash_core::StoreSet>,
) {
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("the fixture selects the double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await;
    let directory = tempfile::tempdir().expect("SQLite file fixture directory");
    let stores = if file {
        lash_sqlite_store::SqliteStoreSet::open(directory.path())
            .await
            .expect("SQLite file fixture")
    } else {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory fixture")
    };
    (harness, directory, Arc::new(stores))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn recorded_batch_boundaries_on_sqlite_double() {
    for always_replay in [false, true] {
        for file in [false, true] {
            let (harness, _directory, stores) = sqlite_double_fixture(always_replay, file).await;
            if always_replay && file {
                let prefix = format!("recorded-batch-sqlite-file-{}", harness.run_nonce());
                lash_conformance::registration_macro_support::batch_redrive_reuses_children(
                    &prefix,
                    harness.endpoint_host(),
                    stores,
                    harness.turn_runner(),
                    factories(),
                )
                .await;
            } else {
                recorded_boundaries(
                    &harness,
                    stores,
                    if file { "sqlite-file" } else { "sqlite-memory" },
                )
                .await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "FIG-4364: assembled batch row contradicts durable final under cancel+replay"]
async fn cancellation_on_sqlite_file_with_forced_replay() {
    let (harness, _directory, stores) = sqlite_double_fixture(true, true).await;
    let prefix = format!("recorded-batch-sqlite-file-{}", harness.run_nonce());
    lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
        &prefix,
        harness.endpoint_host(),
        stores,
        harness.turn_runner(),
        factories(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires isolated Restate and PostgreSQL; run through the effect-group suite with pg16"]
async fn live_recorded_batch_boundaries_on_current_stores() {
    let directory = tempfile::tempdir().expect("file and attachment fixture directory");
    let postgres = postgres_fixture().await;
    let stores: Vec<(&str, Arc<dyn lash_core::StoreSet>)> = vec![
        (
            "sqlite-memory",
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("SQLite memory fixture"),
            ),
        ),
        (
            "sqlite-file",
            Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(directory.path().join("sqlite"))
                    .await
                    .expect("SQLite file fixture"),
            ),
        ),
        (
            "postgres",
            Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &postgres,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    directory.path().join("attachments"),
                )),
            )),
        ),
    ];
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::Live).await;
    for (storage, stores) in stores {
        recorded_boundaries(&harness, stores, storage).await;
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "service fixture host reads the isolated PostgreSQL gate configuration"
)]
async fn postgres_fixture() -> lash_postgres_store::PostgresStorage {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("the gate provides PostgreSQL");
    let pool = sqlx::PgPool::connect(&url)
        .await
        .expect("connect PostgreSQL provisioner");
    sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
        .execute(&pool)
        .await
        .expect("provision the isolated PostgreSQL fixture");
    pool.close().await;
    lash_postgres_store::PostgresStorage::connect(&url)
        .await
        .expect("PostgreSQL fixture")
}

async fn postgres_double_fixture(
    always_replay: bool,
) -> (
    LiveConformanceHarness,
    tempfile::TempDir,
    Arc<dyn lash_core::StoreSet>,
) {
    let directory = tempfile::tempdir().expect("attachment fixture directory");
    let postgres = postgres_fixture().await;
    let stores: Arc<dyn lash_core::StoreSet> =
        Arc::new(lash_postgres_store::PostgresStoreSet::new(
            &postgres,
            Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                directory.path(),
            )),
        ));
    let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
        unreachable!("the fixture selects the double");
    };
    let harness = LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
        seed,
        always_replay,
    })
    .await;
    (harness, directory, stores)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires isolated PostgreSQL; run through the effect-group suite with pg16"]
async fn postgres_recorded_batch_boundaries_on_double() {
    for always_replay in [false, true] {
        let (harness, _directory, stores) = postgres_double_fixture(always_replay).await;
        if always_replay {
            let prefix = format!("recorded-batch-postgres-{}", harness.run_nonce());
            lash_conformance::registration_macro_support::batch_redrive_reuses_children(
                &prefix,
                harness.endpoint_host(),
                stores,
                harness.turn_runner(),
                factories(),
            )
            .await;
        } else {
            recorded_boundaries(&harness, stores, "postgres").await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "FIG-4364: assembled batch row contradicts durable final under cancel+replay"]
async fn cancellation_on_postgres_with_forced_replay() {
    let (harness, _directory, stores) = postgres_double_fixture(true).await;
    let prefix = format!("recorded-batch-postgres-{}", harness.run_nonce());
    lash_conformance::registration_macro_support::batch_cancel_preserves_committed_drains(
        &prefix,
        harness.endpoint_host(),
        stores,
        harness.turn_runner(),
        factories(),
    )
    .await;
}
