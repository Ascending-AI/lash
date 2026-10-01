//! The shared world of the replay-after-advance laws: one storage kind, one
//! engine (the Restate server double or a live `restate-server`), a core with
//! its process worker, a handler held open across a crash, and a snapshot of
//! the durable rows a replay must not write.

use std::sync::Arc;
use std::time::Duration;

use lash_core::{ProcessId, StoreSet};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{HandlerAttempt, RestateTestBackend, ServerConfig};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSet};
use serde_json::{Value, json};

/// How long any one step of a law may take before the law fails.
pub const BOUND: Duration = Duration::from_secs(120);

/// The signal every target process declares.
pub const SIGNAL: &str = "ready";

#[derive(Clone, Copy, Debug)]
pub enum StorageKind {
    Memory,
    File,
    Postgres,
}

/// The storage one law runs over; dropped with the law.
pub struct Storage {
    kind: StorageKind,
    directory: tempfile::TempDir,
    postgres: Option<(PostgresStorage, IsolatedDatabase)>,
    sqlite_uri: std::sync::Mutex<Option<String>>,
}

impl Storage {
    pub async fn new(kind: StorageKind) -> Self {
        let directory = tempfile::tempdir().expect("a storage directory");
        let postgres = if matches!(kind, StorageKind::Postgres) {
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .expect("the PostgreSQL leg runs with LASH_POSTGRES_DATABASE_URL set");
            let database = IsolatedDatabase::create(&url).await;
            let storage = PostgresStorage::connect(database.url())
                .await
                .expect("open the isolated PostgreSQL database");
            Some((storage, database))
        } else {
            None
        };
        Self {
            kind,
            directory,
            postgres,
            sqlite_uri: std::sync::Mutex::new(None),
        }
    }

    async fn stores(&self, clock: Arc<dyn lash_core::Clock>) -> Arc<dyn StoreSet> {
        let sqlite = match self.kind {
            StorageKind::Memory => SqliteStoreSet::memory_with_clock(clock)
                .await
                .expect("SQLite memory stores"),
            StorageKind::File => SqliteStoreSet::open_with_clock(self.directory.path(), clock)
                .await
                .expect("SQLite file stores"),
            StorageKind::Postgres => {
                return Arc::new(PostgresStoreSet::with_clock(
                    &self.postgres.as_ref().expect("PostgreSQL storage").0,
                    Arc::new(lash::persistence::FileAttachmentStore::new(
                        self.directory.path(),
                    )),
                    lash_core::WakeDeliveryConfig::default(),
                    clock,
                ));
            }
        };
        *self.sqlite_uri.lock().expect("uri lock") = Some(
            sqlite
                .database_uri(SqliteDatabase::ProcessRegistry)
                .to_owned(),
        );
        Arc::new(sqlite)
    }

    /// The process registry's durable row counts and change clock: a replay
    /// that writes a process, an event, a tombstone, a wake delivery or a
    /// tool-intent ledger row moves one of them.
    pub async fn snapshot(&self) -> Vec<i64> {
        const SNAPSHOT: &str = "SELECT current_seq,
            (SELECT COUNT(*) FROM processes), (SELECT COUNT(*) FROM process_events),
            (SELECT COUNT(*) FROM process_tombstones),
            (SELECT COUNT(*) FROM process_wake_deliveries),
            (SELECT COUNT(*) FROM tool_intent_submissions)
            FROM process_change_clock";
        let uri = self.sqlite_uri.lock().expect("uri lock").clone();
        if let Some(uri) = uri {
            let connection = rusqlite::Connection::open_with_flags(
                uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .expect("open the SQLite process registry read-only");
            connection
                .query_row(SNAPSHOT, [], |row| {
                    (0..6).map(|index| row.get::<_, i64>(index)).collect()
                })
                .expect("read the SQLite snapshot")
        } else {
            let sql = SNAPSHOT
                .replace("processes", "lash_processes")
                .replace("process_events", "lash_process_events")
                .replace("process_tombstones", "lash_process_tombstones")
                .replace("process_wake_deliveries", "lash_process_wake_deliveries")
                .replace("tool_intent_submissions", "lash_tool_intent_submissions")
                .replace("process_change_clock", "lash_process_change_clock");
            let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(&sql)
                .fetch_one(self.postgres.as_ref().expect("PostgreSQL storage").0.pool())
                .await
                .expect("read the PostgreSQL snapshot");
            vec![row.0, row.1, row.2, row.3, row.4, row.5]
        }
    }
}

/// The engine a law runs its handler on.
#[derive(Clone)]
pub enum Engine {
    Double(RestateTestBackend<dyn StoreSet>),
    Live(LiveRestateBackend),
}

impl Engine {
    pub async fn double(storage: &Storage, seed: u64) -> Self {
        Self::Double(
            lash_restate_test::backend_with_store_set(
                seed,
                ServerConfig::default(),
                lash_restate_test::DeploymentHooks::default(),
                |clock| async { Ok(storage.stores(clock).await) },
            )
            .await
            .expect("the Restate server double"),
        )
    }

    /// A live `restate-server` over the backend's own SQLite memory stores.
    pub async fn live(tag: &str) -> Self {
        let config = LiveConfig {
            ingress_url: std::env::var("RESTATE_INGRESS_URL").expect("RESTATE_INGRESS_URL"),
            admin_url: std::env::var("RESTATE_ADMIN_URL").expect("RESTATE_ADMIN_URL"),
            endpoint_bind: std::env::var("RAA_RESTATE_ENDPOINT_BIND")
                .expect("RAA_RESTATE_ENDPOINT_BIND")
                .parse()
                .expect("a socket address"),
            endpoint_url: std::env::var("RAA_RESTATE_ENDPOINT_URL")
                .expect("RAA_RESTATE_ENDPOINT_URL"),
            run_tag: format!(
                "{tag}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("wall clock after the epoch")
                    .as_nanos()
            ),
            namespace: lash_restate::RestateNamespace::default(),
        };
        Self::Live(
            LiveRestateBackend::start(config)
                .await
                .expect("serve the live Restate backend"),
        )
    }

    pub fn backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(backend) => backend.lash_backend(),
            Self::Live(backend) => backend.lash_backend(),
        }
    }

    fn install_process_worker(&self, worker: lash::durability::DurableProcessWorker) {
        match self {
            Self::Double(backend) => backend.install_process_worker(worker),
            Self::Live(backend) => backend.install_process_worker(worker),
        }
    }

    pub async fn run(
        &self,
        scope: lash_core::AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        match self {
            Self::Double(backend) => backend.run_in_handler(scope, attempt).await,
            Self::Live(backend) => backend.run_in_handler(scope, attempt).await,
        }
    }

    /// Lose the running handler's attempt, so the engine replays its journal
    /// into a new one: the double crashes the invocation; a live deployment
    /// dies and comes back.
    async fn replay_held_handler(&self) {
        match self {
            Self::Double(backend) => {
                let invocation = backend
                    .server()
                    .invocations()
                    .into_iter()
                    .find(|invocation| {
                        invocation.target.starts_with("LashTestHandlerHost/")
                            && invocation.status == "running"
                    })
                    .expect("the held handler is still running");
                assert!(
                    backend.server().crash(&invocation.id),
                    "crash the held handler's attempt"
                );
            }
            Self::Live(backend) => {
                backend.stop_serving(false);
                backend
                    .start_serving()
                    .await
                    .expect("the live deployment comes back");
            }
        }
    }

    /// Every invocation but the handler host's, once the ones the law's
    /// commands sent have finished: a replay that sends nothing new leaves
    /// this list unchanged.
    pub async fn deliveries(&self) -> Vec<(String, String)> {
        let mut deliveries: Vec<(String, String)> = match self {
            Self::Double(backend) => {
                tokio::time::timeout(BOUND, async {
                    while backend.server().invocations().iter().any(|invocation| {
                        !invocation.target.starts_with("LashTestHandlerHost/")
                            && !invocation.target.contains("LashDurableWait")
                            && invocation.status != "completed"
                    }) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the law's command deliveries finish");
                backend
                    .server()
                    .invocations()
                    .into_iter()
                    .map(|invocation| (invocation.id, invocation.target))
                    .collect()
            }
            Self::Live(backend) => {
                backend
                    .settle(Duration::from_secs(10), Duration::from_millis(25))
                    .await;
                backend
                    .invocations()
                    .await
                    .expect("list the live invocations")
                    .into_iter()
                    .map(|invocation| (invocation.id, invocation.target))
                    .collect()
            }
        };
        deliveries.retain(|(_, target)| {
            !target.starts_with("LashTestHandlerHost/") && !target.contains("LashDurableWait")
        });
        deliveries.sort();
        deliveries
    }
}

/// One law's world: its storage, engine, core and one live session.
pub struct World {
    pub storage: Storage,
    pub engine: Engine,
    pub core: lash::LashCore,
    pub session_id: lash::SessionId,
}

impl World {
    pub async fn new(kind: StorageKind, live: bool, tag: &str) -> Self {
        Self::with_route_restorer(kind, live, tag, None).await
    }

    pub async fn with_route_restorer(
        kind: StorageKind,
        live: bool,
        tag: &str,
        restorer: Option<Arc<dyn lash::triggers::TriggerRouteRestorer>>,
    ) -> Self {
        let storage = Storage::new(kind).await;
        let engine = if live {
            assert!(
                matches!(kind, StorageKind::Memory),
                "the live backend serves its own SQLite memory stores"
            );
            let engine = Engine::live(tag).await;
            if let Engine::Live(live) = &engine {
                *storage.sqlite_uri.lock().expect("uri lock") = Some(
                    live.stores()
                        .database_uri(SqliteDatabase::ProcessRegistry)
                        .to_owned(),
                );
            }
            engine
        } else {
            Engine::double(&storage, 4324).await
        };
        let session_work = match &engine {
            Engine::Double(backend) => backend.explicit_reconcile_session_work(),
            Engine::Live(backend) => backend.explicit_reconcile_session_work(),
        };
        // Only the law advances state between its receipts. A deployment's
        // immediate recovery tick can otherwise recover a reservation or
        // publish a terminal beside the replay being measured.
        let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(engine.backend())
            .with_session_work(session_work)
            .into_backend();
        let provider = lash_core::testing::TestProvider::builder()
            .kind("replay-after-advance")
            .complete(|_| async {
                Ok::<_, lash_core::llm::transport::LlmTransportError>(Default::default())
            })
            .build()
            .into_handle();
        let mut builder = lash::LashCore::standard_builder(backend)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .plugin(lash_core::testing::process_engine_plugin_fixture())
            .models(std::sync::Arc::new(
                lash::ModelRegistry::new()
                    .register(
                        "mock-model",
                        lash::RegisteredModel::new(
                            lash::ModelMetadata::builder("mock-model")
                                .context_window_tokens(200_000)
                                .build()
                                .expect("model metadata"),
                            provider,
                        ),
                    )
                    .expect("one key registers"),
            ));
        if let Some(restorer) = restorer {
            builder = builder.trigger_route_restorer(restorer);
        }
        let core = builder
            .build(lash_core::LeaseOwnerIdentity::opaque(
                "replay-after-advance",
                "facade",
            ))
            .expect("build the core");
        let worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker config"),
        )
        .expect("a durable process worker");
        engine.install_process_worker(worker);
        let session_id = lash::SessionId::from(format!("raa-{tag}"));
        core.session(session_id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                "mock-model",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await
            .expect("create the law's session");
        Self {
            storage,
            engine,
            core,
            session_id,
        }
    }

    pub fn registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.engine.backend().process_registry()
    }

    /// Register an externally owned process the law's session observes, able
    /// to receive the law's signal.
    pub async fn target(&self) -> ProcessId {
        self.registry()
            .register_process_with_observers(
                lash_core::ProcessRegistration::new(
                    lash_core::ProcessInput::External {
                        metadata: json!({"law": "replay-after-advance"}),
                    },
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_extra_event_types([lash_core::ProcessEventType {
                    name: format!("signal.{SIGNAL}"),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                }]),
                std::slice::from_ref(&self.session_id),
            )
            .await
            .expect("register the target process")
            .id
    }

    /// End `process_id` as its external owner, then prune it; with
    /// `compact`, compact its tombstone too, so nothing names it any more.
    pub async fn end_and_prune(&self, process_id: &ProcessId, compact: bool) {
        let registry = self.registry();
        let record = registry
            .get_process(process_id)
            .await
            .expect("read the target")
            .expect("the target is retained");
        // An externally owned row is closed by its owner; a row lash runs is
        // closed by its workflow key, the engine's single writer.
        let authority = if record.input.is_externally_owned() {
            lash_core::ProcessCompletionAuthority::external_owner()
        } else {
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.to_string())
        };
        let ended = match registry
            .complete_process(
                process_id,
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(json!({"done": true})),
                ),
                authority,
            )
            .await
        {
            Ok(ended) => ended.updated_at_ms,
            // A cancelled target the law already ended.
            Err(lash_core::PluginError::ProcessAlreadyTerminal { .. }) => {
                registry
                    .get_process(process_id)
                    .await
                    .expect("read the ended target")
                    .expect("the ended target is retained")
                    .updated_at_ms
            }
            Err(error) => panic!("end the target: {error:?}"),
        };
        self.engine.deliveries().await;
        let pruned = registry
            .prune_terminal_processes(
                ended.saturating_add(1),
                None,
                lash_core::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune the ended target");
        assert!(pruned.pruned_processes >= 1, "the target is pruned");
        assert!(
            matches!(
                registry.get_process(process_id).await,
                Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
            ),
            "the pruned target reads as no longer retained"
        );
        if compact {
            registry
                .compact_process_tombstones(
                    u64::MAX / 2,
                    lash_core::ProjectionWatermark::NoProjector,
                    None,
                )
                .await
                .expect("compact the target's tombstone");
            assert!(
                registry
                    .get_process(process_id)
                    .await
                    .expect("read a compacted id")
                    .is_none(),
                "a compacted target is unknown"
            );
        }
    }

    /// Delete the law's session from the catalog.
    pub async fn delete_session(&self) {
        lash_core::SessionCatalogStore::delete_session(
            self.engine.backend().session_store_factory().as_ref(),
            &self.session_id,
        )
        .await
        .expect("delete the law's session");
    }
}

/// The receipt one run of a durable operation returned, or its refusal.
pub type Receipt = Result<Value, String>;

/// A durable operation run inside a handler: it receives the handler's
/// scoped controller and answers its receipt.
pub type Operation = Arc<
    dyn for<'a> Fn(
            lash_core::ScopedEffectController<'a>,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Receipt> + Send + 'a>>
        + Send
        + Sync,
>;

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The replay leg: run `operation` in a handler and hold the handler open,
/// move the store on with `advance`, then lose the attempt so the engine
/// replays its journal. The replay must answer the recorded receipt, write
/// no registry row and send no new invocation.
pub async fn replay_leg(
    world: &World,
    handler: &str,
    operation: Operation,
    advance: impl AsyncFnOnce(&Value),
) -> (Value, Receipt) {
    let phase = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let attempt: HandlerAttempt = {
        let phase = Arc::clone(&phase);
        Arc::new(move |scoped| {
            let operation = Arc::clone(&operation);
            let phase = Arc::clone(&phase);
            let sender = sender.clone();
            Box::pin(async move {
                let receipt = operation(scoped).await;
                let current = phase.load(std::sync::atomic::Ordering::SeqCst);
                sender.send((current, receipt)).expect("the law listens");
                if current == 0 {
                    std::future::pending::<()>().await;
                }
            })
        })
    };
    let task = {
        let engine = world.engine.clone();
        let scope = lash_core::AdmittedScope::runtime_operation(handler.to_string());
        tokio::spawn(async move { engine.run(scope, attempt).await })
    };
    let _abort = AbortOnDrop(task.abort_handle());
    let (first_phase, original) = tokio::time::timeout(BOUND, receiver.recv())
        .await
        .expect("the first attempt answers")
        .expect("the first attempt's receipt");
    assert_eq!(first_phase, 0);
    let original = original.unwrap_or_else(|error| panic!("the recorded operation: {error}"));
    world.engine.deliveries().await;
    advance(&original).await;
    let deliveries = world.engine.deliveries().await;
    let before = world.storage.snapshot().await;
    phase.store(1, std::sync::atomic::Ordering::SeqCst);
    world.engine.replay_held_handler().await;
    let (replayed_phase, replayed) = tokio::time::timeout(BOUND, receiver.recv())
        .await
        .expect("the replayed attempt answers")
        .expect("the replayed attempt's receipt");
    assert_eq!(replayed_phase, 1);
    if replayed.is_ok() {
        assert_eq!(
            world.engine.deliveries().await,
            deliveries,
            "the replay sends no new invocation"
        );
        assert_eq!(
            world.storage.snapshot().await,
            before,
            "the replay writes no registry row"
        );
    }
    tokio::time::timeout(BOUND, task)
        .await
        .expect("the replayed handler finishes")
        .expect("the handler task joins")
        .expect("the replayed handler completes");
    (original, replayed)
}

/// Run `operation` once in a handler of its own, to completion.
pub async fn run_once(world: &World, handler: &str, operation: Operation) -> Receipt {
    let result = Arc::new(std::sync::Mutex::new(None));
    let attempt: HandlerAttempt = {
        let result = Arc::clone(&result);
        Arc::new(move |scoped| {
            let operation = Arc::clone(&operation);
            let result = Arc::clone(&result);
            Box::pin(async move {
                let receipt = operation(scoped).await;
                *result.lock().expect("result lock") = Some(receipt);
            })
        })
    };
    world
        .engine
        .run(
            lash_core::AdmittedScope::runtime_operation(handler.to_string()),
            attempt,
        )
        .await
        .expect("the handler completes");
    result
        .lock()
        .expect("result lock")
        .take()
        .expect("the handler ran the operation")
}

/// The receipt of a public result, as the law compares it.
pub fn receipt<T: serde::Serialize, E: std::fmt::Debug>(result: Result<T, E>) -> Receipt {
    result
        .map(|value| serde_json::to_value(value).expect("a receipt serializes"))
        .map_err(|error| format!("{error:?}"))
}
