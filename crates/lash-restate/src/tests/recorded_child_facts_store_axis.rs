//! FIG-4396's recorded-child-facts law across the store axis on the server
//! double: a spawned child and the process that runs it run under the facts
//! their parent recorded, on a worker whose plugin set has other defaults.
//!
//! lash-subagents registers the declared-start laws on the double over
//! SQLite memory, in process and replaying every await; the live endpoint
//! runs them under `just effect-group-conformance-e2e`. These tiers run the
//! same law with the double's every store a SQLite file set or a PostgreSQL
//! set (the latter registered in `postgres_ingress`), so the captured environment, the child's creation head and its
//! process row are written and read back by each durable store.

use super::*;

/// The double's handler runs each attempt of the law's turn, the double
/// replays a crashed one into its redrive, and children's segments run in
/// the double's process workflow on the law's worker.
struct DoubleTurnRunner {
    backend: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
}

fn into_handler_attempt(
    attempt: lash_conformance::ConformanceTurnAttempt,
) -> lash_restate_test::HandlerAttempt {
    Arc::new(
        move |controller| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            let attempt = Arc::clone(&attempt);
            Box::pin(async move {
                attempt(controller).await;
            })
        },
    )
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for DoubleTurnRunner {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_in_handler(admitted, into_handler_attempt(attempt))
            .await
            .expect("the double's handler runs the law's turn");
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_crashed_then_redriven(
                admitted,
                into_handler_attempt(crashing),
                into_handler_attempt(redrive),
            )
            .await
            .expect("the double crashes and redrives the law's turn");
    }

    fn process_work(
        &self,
        watched: lash_core::WatchedRegistry,
        worker: lash_core_worker::DurableProcessWorker,
    ) -> lash_core::ProcessWorkWiring {
        self.backend.install_process_worker(worker);
        let port = Arc::new(lash_core::NoProcessWork::new(&watched));
        lash_core::ProcessWorkWiring::new(watched, port)
    }
}

/// The double's server config: a pending deadline is a wall-clock instant
/// the engine's durable wait measures against the system clock, so virtual
/// time starts at wall time.
fn server_config() -> lash_restate_test::ServerConfig {
    lash_restate_test::ServerConfig {
        start_time_ms: u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0),
        )
        .unwrap_or(0),
        ..lash_restate_test::ServerConfig::default()
    }
}

/// The declared-start tier over `double`, whose stores are the law's.
fn tier(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    prefix: &str,
) -> lash_conformance::DeclaredStartTier {
    let backend = double.lash_backend();
    lash_conformance::DeclaredStartTier {
        prefix: format!("{prefix}-{}", uuid::Uuid::new_v4().simple()),
        effect_host: backend.effect_host() as Arc<dyn lash_core::EffectHost>,
        stores: Arc::clone(double.engine_stores()),
        runner: Arc::new(DoubleTurnRunner {
            backend: double.clone(),
        }),
        rlm: vec![Arc::new(
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
                &backend,
            )
            .with_process_lifecycle(true),
        )],
        subagents: Arc::new(|timeout| {
            let factory = lash_subagents::SubagentsPluginFactory::new(
                Arc::new(lash_subagents::CapabilityRegistry::new().with(Arc::new(
                    lash_subagents::StaticCapability::new(
                        "default",
                        lash_core::facade_support::SessionSpec::inherit(),
                    ),
                ))),
                lash_core::lifetime::starter,
            );
            Arc::new(match timeout {
                Some(timeout) => factory.with_timeout(timeout),
                None => factory,
            })
        }),
        delivery: Arc::clone(backend.process_work().port()),
    }
}

/// A double whose every store is a SQLite file set under a fresh directory.
pub(super) async fn sqlite_file_tier() -> (
    (
        tempfile::TempDir,
        lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    ),
    lash_conformance::DeclaredStartTier,
) {
    let directory = tempfile::tempdir().expect("SQLite file directory");
    let root = directory.path().to_path_buf();
    let double = lash_restate_test::backend_with_store_set(
        0x4396_0001,
        server_config(),
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            Ok(Arc::new(
                lash_sqlite_store::SqliteStoreSet::open_with_clock(root, clock)
                    .await
                    .expect("SQLite file stores"),
            ) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("the Restate double over SQLite file stores");
    let tier = tier(&double, "declared-start-sqlite-file");
    ((directory, double), tier)
}

/// A double whose every store is a PostgreSQL set in an isolated database,
/// requiring the configured service URL. The PostgreSQL law is
/// registered under `postgres_ingress`, the selection the pg-store service
/// leg runs.
pub(super) async fn postgres_tier() -> Option<(
    (
        lash_postgres_store::testing::IsolatedDatabase,
        tempfile::TempDir,
        lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    ),
    lash_conformance::DeclaredStartTier,
)> {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("PostgreSQL storage");
    let attachments = tempfile::tempdir().expect("attachment directory");
    let attachment_root = attachments.path().to_path_buf();
    let double = lash_restate_test::backend_with_store_set(
        0x4396_0002,
        server_config(),
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    attachment_root,
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock,
            )) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("the Restate double over PostgreSQL stores");
    let tier = tier(&double, "declared-start-postgres");
    Some(((database, attachments, double), tier))
}

mod sqlite_file {
    lash_conformance::declared_start_tests!(@law [] {
        super::sqlite_file_tier().await
    }; declared_start_child_runs_under_recorded_facts_on_a_worker_with_other_defaults);
    lash_conformance::declared_start_tests!(@law [] {
        super::sqlite_file_tier().await
    }; declared_start_refusal_settles_the_call);
}
