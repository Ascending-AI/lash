//! FIG-4390: a root's response phase plan is recorded with its paid
//! completion, so adding or removing an assistant-response hook between a
//! root's first attempt and its replay never changes the response the root
//! serves (ADR 0105 §1).
//!
//! Each law runs one root on the Restate server double. Its first attempt
//! journals the LLM call's phases and dies before the step after them; the
//! core that ran it is dropped as it dies, and a second core over the same
//! stores, with the other hook set, installs its session driver and replays
//! the root:
//!
//! - **removed**: the first core had a response hook, so the first attempt
//!   journaled phase 2's derived response. The replay, with no hook
//!   installed, serves that derived response from the journal.
//! - **added**: the first core had none, so the first attempt journaled no
//!   phase 2. The replay, with a hook installed, serves the raw completion
//!   and never runs the hook.
//!
//! Each runs with and without forced replay (every await suspends and
//! replays the journal from the start), over SQLite memory, SQLite file and
//! PostgreSQL. The PostgreSQL legs are ignored in ordinary runs and require
//! `LASH_POSTGRES_DATABASE_URL` when selected with `--include-ignored`
//! inside a PostgreSQL gate.
//!
//! The `live_*` laws run the same root on a live `restate-server`, over the
//! live backend's SQLite memory store set: the first attempt's death is the
//! deployment dying at the checkpoint's journal frame, and the deployment
//! that comes back serves the second core. The `recorded-roots` suite
//! of `scripts/restate-suites.toml` runs them on its live and replay legs.

use super::*;

const SEED: u64 = 0x4390_0001;

/// What the provider answers every call with.
const RAW: &str = "the raw completion";
/// What the response hook derives from [`RAW`].
const DERIVED: &str = "the derived response";

#[derive(Clone, Copy, Debug)]
enum Storage {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// How the hook set changes between the root's first attempt and its replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookChange {
    /// The first attempt ran with a response hook; the replay has none.
    Removed,
    /// The first attempt ran with no response hook; the replay has one.
    Added,
}

impl HookChange {
    fn first_has_hook(self) -> bool {
        self == Self::Removed
    }

    /// The response the root serves: what its first attempt recorded.
    fn served(self) -> &'static str {
        match self {
            Self::Removed => DERIVED,
            Self::Added => RAW,
        }
    }

    /// The response the root must never serve.
    fn not_served(self) -> &'static str {
        match self {
            Self::Removed => RAW,
            Self::Added => DERIVED,
        }
    }
}

/// The PostgreSQL URL required by the PostgreSQL legs.
#[allow(
    clippy::disallowed_methods,
    reason = "the PostgreSQL legs read the required service URL"
)]
fn postgres_url() -> String {
    lash_postgres_store::testing::required_database_url()
}

/// The double over `storage`, and what its stores need to outlive it, or
/// `None` for a PostgreSQL leg without a database.
struct World {
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    _files: Option<tempfile::TempDir>,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

/// The engine a law's root runs on.
enum Engine {
    Double(lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>),
    Live(lash_restate_test::live::LiveRestateBackend),
}

/// One invocation of the law's session, as either engine reports it.
struct Run {
    id: String,
    target: String,
    status: String,
    last_failure: Option<String>,
}

impl Engine {
    /// A live `restate-server` deployment over its own SQLite memory stores,
    /// on the endpoint the `recorded-roots` suite binds.
    #[allow(
        clippy::disallowed_methods,
        reason = "the live law reads the suite's server and endpoint addresses"
    )]
    async fn live(run_tag: &str) -> Self {
        let env = |name: &str| {
            std::env::var(name)
                .unwrap_or_else(|_| panic!("the live suite's environment sets {name}"))
        };
        Self::Live(
            lash_restate_test::live::LiveRestateBackend::start(
                lash_restate_test::live::LiveConfig {
                    ingress_url: env("RESTATE_INGRESS_URL"),
                    admin_url: env("RESTATE_ADMIN_URL"),
                    endpoint_bind: env("RR_BIND").parse().expect("a socket address"),
                    endpoint_url: env("RR_URL"),
                    run_tag: run_tag.to_owned(),
                    namespace: lash_restate::RestateNamespace::default(),
                },
            )
            .await
            .expect("serve the live Restate backend"),
        )
    }

    fn lash_backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(double) => double.lash_backend(),
            Self::Live(live) => live.lash_backend(),
        }
    }

    fn crash_on(&self, rule: lash_restate_test::CrashRule) {
        match self {
            Self::Double(double) => double.server().crash_on(rule),
            Self::Live(live) => live.crash_on(rule),
        }
    }

    fn on_crash(&self, listener: lash_restate_test::CrashListener) -> bool {
        match self {
            Self::Double(double) => double.server().on_crash(listener),
            Self::Live(live) => live.on_crash(listener),
        }
    }

    /// Serve the replay: the double replays a crashed attempt by itself; a
    /// live deployment that died at its crash listens again.
    async fn serve_the_replay(&self) {
        if let Self::Live(live) = self {
            live.start_serving()
                .await
                .expect("the live deployment comes back");
        }
    }

    /// Every invocation of `session`.
    async fn runs(&self, session: &lash_core::SessionId) -> Vec<Run> {
        let runs: Vec<Run> = match self {
            Self::Double(double) => double
                .server()
                .invocations()
                .into_iter()
                .map(|view| Run {
                    id: view.id,
                    target: view.target,
                    status: view.status.to_owned(),
                    last_failure: view
                        .last_failure
                        .map(|(code, message)| format!("{code}: {message}")),
                })
                .collect(),
            Self::Live(live) => live
                .invocations()
                .await
                .expect("list the live invocations")
                .into_iter()
                .map(|row| Run {
                    id: row.id,
                    target: row.target,
                    status: row.status,
                    last_failure: row.last_failure,
                })
                .collect(),
        };
        runs.into_iter()
            .filter(|run| run.target.contains(session.as_str()))
            .collect()
    }

    /// Resume `run`, paused after it spent its retries: only the double's
    /// replay can meet no driver, since a live deployment stays down until
    /// the second core is installed.
    fn resume(&self, run: &Run) {
        if let Self::Double(double) = self {
            double.server().resume(&run.id);
        }
    }

    /// `run`'s journal, by the names of its entries.
    async fn journal(&self, run: &Run) -> Vec<String> {
        match self {
            Self::Double(double) => double
                .server()
                .journal(&run.id)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| format!("{:?}:{:?}", entry.ty, entry.name))
                .collect(),
            Self::Live(live) => live.journal(&run.id).await.unwrap_or_default(),
        }
    }

    /// What a completed `run` answered: the double keeps the outcome; a live
    /// server keeps it as the journal's output entry.
    async fn outcome(&self, run: &Run) -> Option<std::result::Result<String, String>> {
        match self {
            Self::Double(double) => double.server().outcome(&run.id).map(|outcome| {
                outcome
                    .map(|value| String::from_utf8_lossy(&value).into_owned())
                    .map_err(|(code, message)| format!("{code}: {message}"))
            }),
            Self::Live(live) => {
                let completed = live
                    .outcome(&run.id)
                    .await
                    .expect("read the live outcome")?;
                Some(match completed {
                    Ok(()) => Ok(live
                        .journal_entries(&run.id)
                        .await
                        .expect("read the live journal")
                        .into_iter()
                        .filter(|(name, _)| name.contains("Output"))
                        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
                        .collect()),
                    Err(failure) => Err(failure),
                })
            }
        }
    }
}

async fn world(storage: Storage, always_replay: bool) -> Option<World> {
    let config = lash_restate_test::ServerConfig::default().always_replay(always_replay);
    let hooks = lash_restate_test::DeploymentHooks::default;
    Some(match storage {
        Storage::SqliteMemory => World {
            double: lash_restate_test::backend_with_store_set(
                SEED,
                config,
                hooks(),
                |clock| async {
                    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                        .await
                        .map_err(|error| {
                            lash_restate_test::BackendError::Stores(error.to_string())
                        })?;
                    Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
                },
            )
            .await
            .expect("the Restate double over SQLite memory"),
            _files: None,
            _database: None,
        },
        Storage::SqliteFile => {
            let files = tempfile::tempdir().expect("a SQLite store directory");
            let root = files.path().to_owned();
            World {
                double: lash_restate_test::backend_with_store_set(
                    SEED,
                    config,
                    hooks(),
                    |clock| async move {
                        let stores =
                            lash_sqlite_store::SqliteStoreSet::open_with_clock(root, clock)
                                .await
                                .map_err(|error| {
                                    lash_restate_test::BackendError::Stores(error.to_string())
                                })?;
                        Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
                    },
                )
                .await
                .expect("the Restate double over SQLite files"),
                _files: Some(files),
                _database: None,
            }
        }
        Storage::Postgres => {
            let url = postgres_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open the provisioned PostgreSQL storage");
            let files = tempfile::tempdir().expect("an attachment directory");
            let attachments = files.path().to_owned();
            World {
                double: lash_restate_test::backend_with_store_set(
                    SEED,
                    config,
                    hooks(),
                    |clock| async move {
                        Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                            &storage,
                            Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                                attachments,
                            )),
                            lash_core::WakeDeliveryConfig::default(),
                            clock,
                        )) as Arc<dyn lash_core::StoreSet>)
                    },
                )
                .await
                .expect("the Restate double over PostgreSQL"),
                _files: Some(files),
                _database: Some(database),
            }
        }
    })
}

/// A response hook that replaces the completion with [`DERIVED`], counting
/// its calls in `calls`.
fn deriving_plugin(calls: &Arc<AtomicUsize>) -> StaticPluginFactory {
    let calls = Arc::clone(calls);
    let hook: lash_core::plugin::AssistantResponseHook = Arc::new(move |context| {
        calls.fetch_add(1, Ordering::SeqCst);
        let mut response = context.response;
        response.parts = vec![LlmOutputPart::Text {
            text: DERIVED.to_owned(),
            response_meta: None,
        }];
        Box::pin(async move {
            Ok(lash_core::facade_support::AssistantResponseTransform {
                response,
                events: Vec::new(),
            })
        })
    });
    StaticPluginFactory::new(
        "response-phase-replay-deriver",
        lash_core::facade_support::PluginSpec::new().with_assistant_response(hook),
    )
}

/// A core over `engine` whose provider answers [`RAW`], counting its calls
/// in `provider_calls`, with the deriving response hook installed when
/// `with_hook`. Building it installs its session driver on the engine, once no other core holds that installation.
fn core_over(
    engine: &Engine,
    with_hook: bool,
    provider_calls: &Arc<AtomicUsize>,
    hook_calls: &Arc<AtomicUsize>,
) -> LashCore {
    let provider_calls = Arc::clone(provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("response-phase-replay")
        .complete(move |_request| {
            provider_calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(text_response(RAW)) }
        })
        .build()
        .into_handle();
    let mut builder =
        LashCore::standard_builder(engine.lash_backend(), crate::TurnBudget::Unbounded)
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .serve_test_model(provider, mock_model_spec());
    if with_hook {
        builder = builder.plugin(Arc::new(deriving_plugin(hook_calls)));
    }
    builder
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core")
}

/// Every journal of `session`'s invocations, by the names of its entries.
async fn session_journals(engine: &Engine, session: &lash_core::SessionId) -> Vec<String> {
    let mut journals = Vec::new();
    for run in engine.runs(session).await {
        journals.push(format!(
            "{} {} failure={:?} {:?}",
            run.target,
            run.status,
            run.last_failure,
            engine.journal(&run).await
        ));
    }
    journals
}

/// The lash invocations of `session` the engine has not completed.
async fn open_session_invocations(engine: &Engine, session: &lash_core::SessionId) -> Vec<String> {
    engine
        .runs(session)
        .await
        .into_iter()
        .filter(|run| {
            (run.target.starts_with("LashSession/") || run.target.starts_with("LashTurn/"))
                && run.status != "completed"
        })
        .map(|run| format!("{} {} {:?}", run.target, run.status, run.last_failure))
        .collect()
}

/// The law on the server double over `storage`.
async fn on_the_double(change: HookChange, storage: Storage, always_replay: bool) -> Result<()> {
    let Some(world) = world(storage, always_replay).await else {
        eprintln!("skipping the {storage:?} leg: LASH_POSTGRES_DATABASE_URL is not set");
        return Ok(());
    };
    let World {
        double,
        _files,
        _database,
    } = world;
    a_changed_response_hook_set_does_not_change_the_served_response(
        change,
        Engine::Double(double),
        "response-phase-replay",
    )
    .await
}

/// The law on a live `restate-server`. Its state outlives a run, so each run
/// names its own session.
async fn on_live_restate(change: HookChange) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after the epoch")
        .as_nanos();
    let tag = format!("response-phase-replay-{change:?}-{nonce}").to_lowercase();
    let engine = Engine::live(&tag).await;
    let Engine::Live(live) = &engine else {
        unreachable!("Engine::live builds the live engine");
    };
    let live = live.clone();
    let result =
        a_changed_response_hook_set_does_not_change_the_served_response(change, engine, &tag).await;
    live.finish().await;
    result
}

async fn a_changed_response_hook_set_does_not_change_the_served_response(
    change: HookChange,
    engine: Engine,
    session: &str,
) -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(
        &engine,
        change.first_has_hook(),
        &provider_calls,
        &hook_calls,
    );
    let session_id = lash_core::SessionId::from(session);
    drop(
        core.session(session_id.as_str())
            .created()
            .await
            .open()
            .await?,
    );
    let root = lash_core::TurnId::from(format!("{session}-root"));
    // The step after the LLM call's phases: the turn's completion
    // checkpoint, which both hook sets issue.
    engine.crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRunEnding {
            suffix: CHECKPOINT_SUFFIX.to_owned(),
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .key(lash_restate::turn_workflow_key(&session_id, &root)),
    );
    let store = lash_core::runtime::live_session_view(&core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("answer me"),
            )
            .with_source_key(root.as_str()),
        )
        .await
        .expect("enqueue the input");
    let engine_port = core.substrate_slot.ports().await.queued;
    // The first core leaves as its root's first attempt dies: the listener
    // runs before the engine starts the replay.
    let first = Arc::new(std::sync::Mutex::new(Some(core)));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let crashes = Arc::new(AtomicUsize::new(0));
    assert!(engine.on_crash(Arc::new({
        let first = Arc::clone(&first);
        let dropped = Arc::clone(&dropped);
        let crashes = Arc::clone(&crashes);
        move |_target: &str| {
            crashes.fetch_add(1, Ordering::SeqCst);
            let core = first
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let drop_core = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime for the drop")
                    .block_on(async move { drop(core) });
            })
            .join();
            dropped.store(drop_core.is_ok(), Ordering::SeqCst);
        }
    })));
    engine_port.schedule_drive(
        &session_id,
        lash_core::engine::DriveRequestId::new("response-phase-replay"),
    );
    drop(engine_port);
    let crashed = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        crashed.is_ok(),
        "the root's first attempt dies: journals {:?}",
        session_journals(&engine, &session_id).await,
    );
    let hook_calls_before_replay = hook_calls.load(Ordering::SeqCst);
    let second = core_over(
        &engine,
        !change.first_has_hook(),
        &provider_calls,
        &hook_calls,
    );
    engine.serve_the_replay().await;

    let root_run =
        async || {
            engine.runs(&session_id).await.into_iter().find(|run| {
                run.target.starts_with("LashTurn/") && run.target.contains(root.as_str())
            })
        };
    let settled = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            // A replay that found no driver installed may have spent its
            // retries before the second core installed one.
            for run in engine.runs(&session_id).await {
                if run.status == "paused" {
                    engine.resume(&run);
                }
            }
            if root_run()
                .await
                .is_some_and(|run| run.status == "completed")
                && open_session_invocations(&engine, &session_id)
                    .await
                    .is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the root and its drive finish: open {:?}, journals {:?}",
        open_session_invocations(&engine, &session_id).await,
        session_journals(&engine, &session_id).await,
    );
    assert!(
        crashes.load(Ordering::SeqCst) >= 1,
        "the first attempt died"
    );
    let run = root_run().await.expect("the root's run is an invocation");
    let outcome = engine.outcome(&run).await;
    let evidence = format!(
        "last failure {:?}, outcome {outcome:?}, journals {:?}",
        run.last_failure,
        session_journals(&engine, &session_id).await,
    );
    assert!(
        !run.last_failure
            .as_deref()
            .is_some_and(|failure| failure.starts_with("570")),
        "the replay followed its journal: {evidence}"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        1,
        "the replay serves the recorded completion: {evidence}"
    );
    assert!(
        matches!(&outcome, Some(Ok(value))
            if value.contains("\"root_outcome\":\"committed\"")
                && value.contains(change.served())
                && !value.contains(change.not_served())),
        "the root commits the response its first attempt recorded, `{}`: {evidence}",
        change.served(),
    );
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        hook_calls_before_replay,
        "the replay runs no response hook: the first attempt recorded the phase plan: {evidence}"
    );
    drop(second);
    Ok(())
}

/// The suffix of the journal run of the turn's completion checkpoint, the
/// effect after its LLM call's phases: `<session>:<root>:1:0:checkpoint:3`.
const CHECKPOINT_SUFFIX: &str = ":checkpoint:3";

macro_rules! response_phase_replay_laws {
    ($($(#[$attr:meta])* $name:ident: $change:expr, $storage:expr, $always_replay:expr;)*) => {
        $(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                on_the_double($change, $storage, $always_replay).await
            }
        )*
    };
}

response_phase_replay_laws! {
    removed_hook_sqlite_memory: HookChange::Removed, Storage::SqliteMemory, false;
    removed_hook_sqlite_file: HookChange::Removed, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    removed_hook_postgres: HookChange::Removed, Storage::Postgres, false;
    added_hook_sqlite_memory: HookChange::Added, Storage::SqliteMemory, false;
    added_hook_sqlite_file: HookChange::Added, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    added_hook_postgres: HookChange::Added, Storage::Postgres, false;
    removed_hook_sqlite_memory_always_replay: HookChange::Removed, Storage::SqliteMemory, true;
    removed_hook_sqlite_file_always_replay: HookChange::Removed, Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    removed_hook_postgres_always_replay: HookChange::Removed, Storage::Postgres, true;
    added_hook_sqlite_memory_always_replay: HookChange::Added, Storage::SqliteMemory, true;
    added_hook_sqlite_file_always_replay: HookChange::Added, Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    added_hook_postgres_always_replay: HookChange::Added, Storage::Postgres, true;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the recorded-roots suite"]
async fn live_removed_hook() -> Result<()> {
    on_live_restate(HookChange::Removed).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the recorded-roots suite"]
async fn live_added_hook() -> Result<()> {
    on_live_restate(HookChange::Added).await
}
