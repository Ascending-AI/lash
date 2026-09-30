//! Public process commands retain their journaled receipts across retention.

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "acceptance test assertions"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "service tests read the gate's environment"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::{ProcessId, StoreSet};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{HandlerAttempt, RestateTestBackend, ServerConfig};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSet};
use serde_json::{Value, json};

const BOUND: Duration = Duration::from_secs(120);

#[path = "host_uploaded_start_input.rs"]
mod host_uploaded_start_input;

#[derive(Clone, Copy)]
enum Method {
    Signal,
    Cancel,
}

impl Method {
    async fn call(
        self,
        core: &lash::LashCore,
        id: &ProcessId,
        scoped: lash_core::ScopedEffectController<'_>,
    ) -> Result<Value, lash::EmbedError> {
        match self {
            Self::Signal => core
                .processes()
                .signal(
                    lash_core::ProcessSignal::new(
                        lash_core::ProcessSignalIdentity::new(id.clone(), "ready", "receipt-law")
                            .expect("valid signal identity"),
                        json!({"ready": 7}),
                    ),
                    scoped,
                )
                .await
                .map(|receipt| serde_json::to_value(receipt).unwrap()),
            Self::Cancel => core
                .processes()
                .cancel(id, scoped)
                .await
                .map(|receipt| serde_json::to_value(receipt).unwrap()),
        }
    }
}

#[derive(Clone, Copy)]
enum StorageKind {
    Memory,
    File,
    Postgres,
}

struct Storage {
    directory: tempfile::TempDir,
    postgres: Option<(PostgresStorage, IsolatedDatabase)>,
}

impl Storage {
    async fn new(kind: StorageKind) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let postgres = if matches!(kind, StorageKind::Postgres) {
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .expect("PostgreSQL gate supplies its URL");
            let database = IsolatedDatabase::create(&url).await;
            let storage = PostgresStorage::connect(database.url()).await.unwrap();
            Some((storage, database))
        } else {
            None
        };
        Self {
            directory,
            postgres,
        }
    }

    async fn stores(
        &self,
        kind: StorageKind,
        clock: Arc<dyn lash_core::Clock>,
        sqlite_uri: &mut Option<String>,
    ) -> Arc<dyn StoreSet> {
        match kind {
            StorageKind::Memory | StorageKind::File => {
                let stores = match kind {
                    StorageKind::Memory => SqliteStoreSet::memory_with_clock(clock).await.unwrap(),
                    StorageKind::File => {
                        SqliteStoreSet::open_with_clock(self.directory.path(), clock)
                            .await
                            .unwrap()
                    }
                    StorageKind::Postgres => unreachable!(),
                };
                *sqlite_uri = Some(
                    stores
                        .database_uri(SqliteDatabase::ProcessRegistry)
                        .to_owned(),
                );
                Arc::new(stores)
            }
            StorageKind::Postgres => Arc::new(PostgresStoreSet::with_clock(
                &self.postgres.as_ref().unwrap().0,
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    self.directory.path(),
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock,
            )),
        }
    }
}

#[derive(Clone)]
enum Engine {
    Double(RestateTestBackend<dyn StoreSet>),
    Live(LiveRestateBackend<dyn StoreSet>),
}

impl Engine {
    fn backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(b) => b.lash_backend(),
            Self::Live(b) => b.lash_backend(),
        }
    }

    async fn run(
        &self,
        scope: lash_core::AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        match self {
            Self::Double(b) => b.run_in_handler(scope, attempt).await,
            Self::Live(b) => b.run_in_handler(scope, attempt).await,
        }
    }

    async fn replay(&self) {
        match self {
            Self::Double(b) => {
                let invocation = b
                    .server()
                    .invocations()
                    .into_iter()
                    .find(|v| v.target.starts_with("LashTestHandlerHost/") && v.status == "running")
                    .expect("the enclosing handler has not finished");
                assert!(
                    b.server().crash(&invocation.id),
                    "crash the unsettled handler"
                );
            }
            Self::Live(b) => {
                b.stop_serving(false);
                b.start_serving().await.unwrap();
            }
        }
    }

    async fn deliveries(&self) -> Vec<(String, String)> {
        let mut deliveries: Vec<_> = match self {
            Self::Double(b) => {
                tokio::time::timeout(BOUND, async {
                    while b.server().invocations().iter().any(|v| {
                        !v.target.starts_with("LashTestHandlerHost/") && v.status != "completed"
                    }) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("all command deliveries finish while the enclosing handler stays open");
                b.server()
                    .invocations()
                    .into_iter()
                    .map(|v| (v.id, v.target))
                    .collect()
            }
            Self::Live(b) => {
                b.settle(Duration::from_secs(10), Duration::from_millis(25))
                    .await;
                b.invocations()
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|v| (v.id, v.target))
                    .collect()
            }
        };
        deliveries.retain(|(_, target)| !target.starts_with("LashTestHandlerHost/"));
        deliveries.sort();
        deliveries
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// Row counts detect new appends/outbox writes; the change clock detects registry mutations.
const SNAPSHOT: &str = "SELECT current_seq,
    (SELECT COUNT(*) FROM processes), (SELECT COUNT(*) FROM process_events),
    (SELECT COUNT(*) FROM process_tombstones), (SELECT COUNT(*) FROM process_wake_deliveries)
    FROM process_change_clock";

async fn snapshot(storage: &Storage, sqlite_uri: Option<&str>) -> (i64, i64, i64, i64, i64) {
    if let Some(uri) = sqlite_uri {
        let connection = rusqlite::Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        connection
            .query_row(SNAPSHOT, [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
    } else {
        let sql = SNAPSHOT
            .replace("processes", "lash_processes")
            .replace("process_events", "lash_process_events")
            .replace("process_tombstones", "lash_process_tombstones")
            .replace("process_wake_deliveries", "lash_process_wake_deliveries")
            .replace("process_change_clock", "lash_process_change_clock");
        sqlx::query_as(&sql)
            .fetch_one(storage.postgres.as_ref().unwrap().0.pool())
            .await
            .unwrap()
    }
}

async fn law(kind: StorageKind, method: Method, live: bool) {
    let storage = Storage::new(kind).await;
    let mut sqlite_uri = None;
    let engine = if live {
        let config = LiveConfig {
            ingress_url: std::env::var("RESTATE_INGRESS_URL").unwrap(),
            admin_url: std::env::var("RESTATE_ADMIN_URL").unwrap(),
            endpoint_bind: std::env::var("PC_BIND").unwrap().parse().unwrap(),
            endpoint_url: std::env::var("PC_URL").unwrap(),
            run_tag: format!(
                "public-receipt-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ),
            namespace: lash_restate::RestateNamespace::default(),
        };
        Engine::Live(
            LiveRestateBackend::start_with_store_set(config, |clock| async {
                let stores = storage.stores(kind, clock, &mut sqlite_uri).await;
                Ok(stores)
            })
            .await
            .unwrap(),
        )
    } else {
        Engine::Double(
            lash_restate_test::backend_with_store_set(
                4300,
                ServerConfig::default(),
                lash_restate_test::DeploymentHooks::default(),
                |clock| async { Ok(storage.stores(kind, clock, &mut sqlite_uri).await) },
            )
            .await
            .unwrap(),
        )
    };
    let backend = engine.backend();
    let registry = backend.process_registry();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("public-receipt")
        .complete(|_| async {
            Ok::<_, lash_core::llm::transport::LlmTransportError>(Default::default())
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .models(std::sync::Arc::new(
            lash_core::ModelRegistry::new()
                .register(
                    "mock-model",
                    lash_core::RegisteredModel::new(
                        lash_core::ModelMetadata::builder("mock-model")
                            .context_window_tokens(200_000)
                            .build()
                            .unwrap(),
                        provider,
                    ),
                )
                .expect("register the test model"),
        ))
        .model("mock-model")
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "receipt-law",
            "facade",
        ))
        .unwrap();
    let worker =
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap();
    match &engine {
        Engine::Double(b) => b.install_process_worker(worker),
        Engine::Live(b) => b.install_process_worker(worker),
    }
    let record = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: json!({"owner": "receipt-law"}),
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "signal.ready".into(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
        )
        .await
        .unwrap();
    let phase = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let id = record.id.clone();
        let phase = Arc::clone(&phase);
        Arc::new(move |scoped| {
            let core = core.clone();
            let id = id.clone();
            let phase = Arc::clone(&phase);
            let tx = tx.clone();
            Box::pin(async move {
                let receipt = method
                    .call(&core, &id, scoped)
                    .await
                    .map_err(|e| format!("{e:?}"));
                let current = phase.load(Ordering::SeqCst);
                tx.send((current, receipt)).unwrap();
                if current < 2 {
                    std::future::pending::<()>().await;
                }
            })
        })
    };
    let task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .run(
                    lash_core::AdmittedScope::runtime_operation("public-command-receipt"),
                    attempt,
                )
                .await
        })
    };
    let _abort = AbortOnDrop(task.abort_handle());
    let (first_phase, original) = tokio::time::timeout(BOUND, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_phase, 0);
    let original = original.expect("the original public command is admitted");
    engine.deliveries().await;
    let ended = registry
        .complete_process(
            &record.id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                json!({"done": true}),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .unwrap();
    engine.deliveries().await;
    let pruned = registry
        .prune_terminal_processes(
            ended.updated_at_ms + 1,
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .unwrap();
    assert_eq!(pruned.pruned_processes, 1);
    assert!(matches!(
        registry.get_process(&record.id).await,
        Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
    ));
    let mut replays = Vec::new();
    for current in 1..=2 {
        if current == 2 {
            assert_eq!(
                registry
                    .compact_process_tombstones(
                        u64::MAX / 2,
                        lash_core::ProjectionWatermark::NoProjector,
                        None
                    )
                    .await
                    .unwrap(),
                1
            );
            assert!(registry.get_process(&record.id).await.unwrap().is_none());
        }
        let deliveries = engine.deliveries().await;
        let before = snapshot(&storage, sqlite_uri.as_deref()).await;
        phase.store(current, Ordering::SeqCst);
        engine.replay().await;
        let (replayed_phase, receipt) = tokio::time::timeout(BOUND, rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replayed_phase, current);
        if let Err(error) = &receipt {
            eprintln!("retention phase {current}: {error}");
        }
        replays.push((current, receipt));
        assert_eq!(
            engine.deliveries().await,
            deliveries,
            "replay creates no new deliveries"
        );
        assert_eq!(
            snapshot(&storage, sqlite_uri.as_deref()).await,
            before,
            "replay writes no registry rows"
        );
        for id in [&record.id, &ProcessId::fixture("never-registered")] {
            let result = Arc::new(std::sync::Mutex::new(None));
            let fresh: HandlerAttempt = {
                let result = Arc::clone(&result);
                let core = core.clone();
                let id = id.clone();
                Arc::new(move |scoped| {
                    let result = Arc::clone(&result);
                    let core = core.clone();
                    let id = id.clone();
                    Box::pin(async move {
                        *result.lock().unwrap() = Some(method.call(&core, &id, scoped).await);
                    })
                })
            };
            engine
                .run(
                    lash_core::AdmittedScope::runtime_operation(format!("fresh-{current}-{id}")),
                    fresh,
                )
                .await
                .unwrap();
            let error = result
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .expect_err("fresh commands refuse absent processes");
            match error {
                lash::EmbedError::Plugin(
                    lash_core::PluginError::ProcessUnknown { .. }
                    | lash_core::PluginError::ProcessNoLongerRetained { .. },
                ) => {}
                lash::EmbedError::Plugin(lash_core::PluginError::RuntimeEffectController(
                    error,
                )) => {
                    if current == 1 && id == record.id {
                        assert_eq!(
                            error.code,
                            lash_core::RuntimeErrorCode::ProcessNoLongerRetained
                        );
                    } else {
                        assert_eq!(error.code, lash_core::RuntimeErrorCode::Plugin);
                        assert!(
                            error.message.contains(id.as_str()),
                            "unknown process refusal names its id"
                        );
                    }
                }
                error => panic!("fresh command must return a retention refusal: {error:?}"),
            }
        }
    }
    for (phase, replay) in replays {
        assert_eq!(
            replay.expect("public replay returns its settled receipt after retention"),
            original,
            "retention phase {phase}"
        );
    }
    tokio::time::timeout(BOUND, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

macro_rules! backend_laws {
    ($name:ident, $kind:ident, $live:expr $(, $ignore:literal)?) => {
        mod $name {
            use super::*;
            mod signal {
                use super::*;
                #[tokio::test]
                $(#[ignore = $ignore])?
                async fn public_process_command_replay_after_prune_returns_settled_receipt() { law(StorageKind::$kind, Method::Signal, $live).await; }
            }
            mod cancel {
                use super::*;
                #[tokio::test]
                $(#[ignore = $ignore])?
                async fn public_process_command_replay_after_prune_returns_settled_receipt() { law(StorageKind::$kind, Method::Cancel, $live).await; }
            }
        }
    };
}
backend_laws!(double_sqlite_memory, Memory, false);
backend_laws!(double_sqlite_file, File, false);
backend_laws!(double_postgres, Postgres, false, "requires PostgreSQL");
backend_laws!(live_sqlite_memory, Memory, true, "requires live Restate");
backend_laws!(live_sqlite_file, File, true, "requires live Restate");
backend_laws!(
    live_postgres,
    Postgres,
    true,
    "requires PostgreSQL and live Restate"
);
