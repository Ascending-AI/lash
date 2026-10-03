//! FIG-4390: a run's response phase plan is recorded with its paid
//! completion, so adding or removing an assistant-response hook between a
//! run's first attempt and its replay never changes the response the run
//! serves (ADR 0105 §1).
//!
//! Each law executes one run on the Restate server double. Its first attempt
//! journals the LLM call's phases and dies before the step after them; the
//! core that ran it is dropped as it dies, and a second core over the same
//! stores, with the other hook set, installs its `SessionShifts` and replays
//! the run:
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
//! The `live_*` laws run the same run on a live `restate-server`, over the
//! live backend's SQLite memory store set: the first attempt's death is the
//! deployment dying at the checkpoint's journal frame, and the deployment
//! that comes back serves the second core. The `recorded-runs` suite
//! of `scripts/restate-suites.toml` runs them on its live and replay legs.

use super::*;

mod catalog_fork;

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

/// How the hook set changes between the run's first attempt and its replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookChange {
    /// The first attempt ran with a response hook; the replay has none.
    Removed,
    /// The first attempt ran with no response hook; the replay has one.
    Added,
    StateRetained,
    CallbackUnavailable,
}

impl HookChange {
    fn first_has_hook(self) -> bool {
        self != Self::Added
    }

    /// The response the run serves: what its first attempt recorded.
    fn served(self) -> &'static str {
        match self {
            Self::Removed | Self::StateRetained | Self::CallbackUnavailable => DERIVED,
            Self::Added => RAW,
        }
    }

    /// The response the run must never serve.
    fn not_served(self) -> &'static str {
        match self {
            Self::Removed | Self::StateRetained | Self::CallbackUnavailable => RAW,
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

/// The engine a law's run executes on.
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
    /// on the endpoint the `recorded-runs` suite binds.
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
    /// replay can meet no `SessionShifts`, since a live deployment stays down until
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
        lash_core::plugin::PluginDeclaration::initial("response-phase-replay-deriver"),
        lash_core::facade_support::PluginSpec::new().with_assistant_response(
            crate::hook_key!("derive"),
            None,
            hook,
        ),
    )
}

#[derive(Clone)]
struct StateDeriver(Arc<AtomicUsize>);

impl lash_core::plugin::PluginFactory for StateDeriver {
    fn id(&self) -> &'static str {
        "state-replay-deriver"
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
    }

    fn build(
        &self,
        _: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::SessionPlugin for StateDeriver {
    fn id(&self) -> &'static str {
        "state-replay-deriver"
    }

    fn register(
        &self,
        registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let state = registrar.state();
        let calls = Arc::clone(&self.0);
        registrar.output().response(
            crate::hook_key!("count"),
            None,
            Arc::new(move |context| {
                let state = state.clone();
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    state.set("accepted", serde_json::json!(17))?;
                    let before = state.generation();
                    assert!(state.set("invalid key", serde_json::json!(1)).is_err());
                    assert_eq!(state.generation(), before);
                    state.set("second", serde_json::json!(23))?;
                    let mut response = context.response;
                    response.parts = vec![LlmOutputPart::Text {
                        text: DERIVED.to_owned(),
                        response_meta: None,
                    }];
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        )?;
        Ok(())
    }
}

/// A core over `engine` whose provider answers [`RAW`], counting its calls
/// in `provider_calls`, with the deriving response hook installed when
/// `with_hook`. Building it installs its `SessionShifts` on the engine, once no other core holds that installation.
fn core_over(
    engine: &Engine,
    with_hook: bool,
    stateful: bool,
    callback_available: bool,
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
    let mut builder = LashCore::standard_builder(engine.lash_backend())
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(provider, mock_llm_profile_spec());
    if stateful {
        builder = builder.plugin(Arc::new(StateDeriver(Arc::clone(hook_calls))));
    } else {
        builder = builder.plugin(Arc::new(if with_hook && callback_available {
            deriving_plugin(hook_calls)
        } else {
            StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("response-phase-replay-deriver"),
                lash_core::facade_support::PluginSpec::new(),
            )
        }));
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
        change == HookChange::StateRetained,
        true,
        &provider_calls,
        &hook_calls,
    );
    let session_id = lash_core::SessionId::fixture(session);
    drop(
        core.session(session_id.clone())
            .created()
            .await
            .open()
            .await?,
    );
    let run = lash_core::TurnId::fixture(format!("{session}-run"));
    // An owed derivation loses its result; the other cases lose the step
    // after both phases, so the replay serves their completed derivation.
    let crash = if change == HookChange::CallbackUnavailable {
        lash_restate_test::CrashPoint::BeforeRunResultEnding {
            suffix: "assistant_response_hooks".to_owned(),
        }
    } else {
        lash_restate_test::CrashPoint::BeforeRunEnding {
            suffix: CHECKPOINT_SUFFIX.to_owned(),
        }
    };
    engine.crash_on(
        lash_restate_test::CrashRule::new(crash)
            .service(lash_restate_test::TURN_DRIVER_SERVICE)
            .key(lash_restate::turn_workflow_key(&session_id, &run)),
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
            .with_source_key(run.as_str()),
        )
        .await
        .expect("enqueue the input");
    let engine_port = core.substrate_slot.ports().await.queued;
    // The first core leaves as its run's first attempt dies: the listener
    // runs before the engine starts the replay.
    let first = Arc::new(std::sync::Mutex::new(Some(core)));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let crashes = lash_restate_test::CrashCount::new();
    assert!(engine.on_crash(crashes.listener_with({
        let first = Arc::clone(&first);
        let dropped = Arc::clone(&dropped);
        move |_target: &str| {
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
    engine_port.schedule_shift(
        &session_id,
        lash_core::engine::ShiftRequestId::new("response-phase-replay"),
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
        "the run's first attempt dies: journals {:?}",
        session_journals(&engine, &session_id).await,
    );
    let hook_calls_before_replay = hook_calls.load(Ordering::SeqCst);
    let second = core_over(
        &engine,
        change == HookChange::CallbackUnavailable || !change.first_has_hook(),
        change == HookChange::StateRetained,
        change != HookChange::CallbackUnavailable,
        &provider_calls,
        &hook_calls,
    );
    engine.serve_the_replay().await;

    let run_execution = async || {
        engine.runs(&session_id).await.into_iter().find(|executed| {
            executed.target.starts_with("LashTurn/") && executed.target.contains(run.as_str())
        })
    };
    if change == HookChange::CallbackUnavailable {
        let parked = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let Some(park) = store.load_turn_park().await.expect("read the run's park") {
                    break park;
                }
                for executed in engine.runs(&session_id).await {
                    if executed.status == "paused" {
                        engine.resume(&executed);
                    }
                }
                assert!(
                    lash_core::store::RunStore::run_terminal(
                        second.store_factory.as_ref(),
                        &session_id,
                        &run,
                    )
                    .await
                    .expect("read the run's terminal")
                    .is_none(),
                    "an owed callback cannot commit the raw response"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the unavailable callback parks the run");
        let lash_core::store::ParkReason::PluginRevisionUnavailable { refusal, .. } = parked.reason
        else {
            panic!("the callback park remains typed: {parked:?}");
        };
        let callback = refusal
            .callback
            .expect("the park names the recorded callback");
        assert_eq!(callback.key, "assistant_response:derive");
        assert_eq!(callback.owner.plugin, "response-phase-replay-deriver");
        assert_eq!(callback.owner.behavior_revision.get(), 1);
        assert_eq!(
            provider_calls.load(Ordering::SeqCst),
            1,
            "the paid completion replays"
        );
        assert_eq!(
            hook_calls.load(Ordering::SeqCst),
            hook_calls_before_replay,
            "the unavailable callback never runs during replay"
        );
        assert_eq!(hook_calls_before_replay, 1, "the first result was lost");
        assert!(crashes.get() >= 1);
        drop(second);
        return Ok(());
    }
    let settled = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            // A replay that found no `SessionShifts` installed may have spent its
            // retries before the second core installed one.
            for executed in engine.runs(&session_id).await {
                if executed.status == "paused" {
                    engine.resume(&executed);
                }
            }
            if run_execution()
                .await
                .is_some_and(|executed| executed.status == "completed")
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
        "the run and its shift finish: open {:?}, journals {:?}",
        open_session_invocations(&engine, &session_id).await,
        session_journals(&engine, &session_id).await,
    );
    assert!(crashes.get() >= 1, "the first attempt died");
    let executed = run_execution()
        .await
        .expect("the run's execution is an invocation");
    let outcome = engine.outcome(&executed).await;
    let terminal =
        lash_core::store::RunStore::run_terminal(second.store_factory.as_ref(), &session_id, &run)
            .await?
            .expect("the completed run has a stored terminal");
    let lash_core::store::RunTerminalCause::Committed {
        outcome: committed, ..
    } = terminal.cause
    else {
        panic!("the run commits its recorded response: {terminal:?}");
    };
    let committed = serde_json::to_string(&committed)?;
    let evidence = format!(
        "last failure {:?}, outcome {outcome:?}, committed {committed}, journals {:?}",
        executed.last_failure,
        session_journals(&engine, &session_id).await,
    );
    assert!(
        !executed
            .last_failure
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
            if value.contains("\"run_outcome\":\"committed\""))
            && committed.contains(change.served())
            && !committed.contains(change.not_served()),
        "the run commits the response its first attempt recorded, `{}`: {evidence}",
        change.served(),
    );
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        hook_calls_before_replay,
        "the replay runs no response hook: the first attempt recorded the phase plan: {evidence}"
    );
    if change == HookChange::StateRetained {
        let loaded = lash_core::store::load_session_window_state(
            &store,
            lash_core::store::WindowSelector::Current,
        )
        .await?
        .expect("the replay committed a head");
        let namespace =
            &loaded.state.plugin_state().expect("plugin state").plugins["state-replay-deriver"];
        assert_eq!(
            namespace.values.get("accepted"),
            Some(&serde_json::json!(17))
        );
        assert_eq!(namespace.values.get("second"), Some(&serde_json::json!(23)));
        assert_eq!(namespace.generation, 2, "each accepted edit appears once");
        assert!(!namespace.values.contains_key("invalid key"));
    }
    drop(second);
    Ok(())
}

/// The suffix of the journal run of the turn's completion checkpoint, the
/// effect after its LLM call's phases: `<session>:<run>:1:0:checkpoint:3`.
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
    owed_unavailable_callback_parks_sqlite_memory: HookChange::CallbackUnavailable, Storage::SqliteMemory, false;
    owed_unavailable_callback_parks_sqlite_file: HookChange::CallbackUnavailable, Storage::SqliteFile, false;
    completed_callback_state_survives_cold_replay_sqlite_memory: HookChange::StateRetained, Storage::SqliteMemory, false;
    completed_callback_state_survives_cold_replay_sqlite_file: HookChange::StateRetained, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    completed_callback_state_survives_cold_replay_postgres: HookChange::StateRetained, Storage::Postgres, false;
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
#[ignore = "requires an isolated Restate server; run by the recorded-runs suite"]
async fn live_removed_hook() -> Result<()> {
    on_live_restate(HookChange::Removed).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the recorded-runs suite"]
async fn live_added_hook() -> Result<()> {
    on_live_restate(HookChange::Added).await
}

fn callback_session(
    mut plugins: Vec<Arc<dyn lash_core::plugin::PluginFactory>>,
) -> Arc<lash_core::facade_support::PluginSession> {
    plugins.push(Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    ));
    lash_core::facade_support::PluginHost::new(plugins)
        .build_session(lash_core::plugin::PluginSessionRequest::creation(
            "recorded-response-plan",
            Default::default(),
        ))
        .expect("materialize the callback registry")
}

fn appending_callback(
    plugin: &'static str,
    revision: u32,
    suffix: &'static str,
    calls: &Arc<AtomicUsize>,
) -> Arc<dyn lash_core::plugin::PluginFactory> {
    let calls = Arc::clone(calls);
    let mut declaration = lash_core::plugin::PluginDeclaration::initial(plugin);
    declaration.behavior_revision = lash_core::plugin::BehaviorRevision::new(revision).unwrap();
    Arc::new(StaticPluginFactory::new(
        declaration,
        lash_core::facade_support::PluginSpec::new().with_assistant_response(
            crate::hook_key!("append"),
            None,
            Arc::new(move |ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                let response = text_response(&format!("{}{suffix}", ctx.response.full_text()));
                Box::pin(async move {
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        ),
    ))
}

async fn replay_callbacks(
    recorded: &lash_core::facade_support::PluginSession,
    live: &lash_core::facade_support::PluginSession,
    states: &[lash_core::AssistantStreamHookState],
) -> std::result::Result<String, lash_core::PluginError> {
    let plan: lash_core::AssistantResponsePlan =
        serde_json::from_value(serde_json::to_value(recorded.assistant_response_plan()).unwrap())
            .unwrap();
    let transforms = live
        .transform_assistant_response(
            &lash_core::SessionId::fixture("recorded-response-plan"),
            text_response(RAW),
            &plan,
            states,
        )
        .await?;
    Ok(transforms
        .last()
        .map_or_else(|| RAW.into(), |t| t.value.response.full_text()))
}

#[tokio::test]
async fn recorded_callback_order_survives_reordered_installation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let a = appending_callback("response-a", 1, ":a", &calls);
    let b = appending_callback("response-b", 1, ":b", &calls);
    let recorded = callback_session(vec![a.clone(), b.clone()]);
    let live = callback_session(vec![b, a]);
    assert_eq!(
        replay_callbacks(&recorded, &live, &[]).await.unwrap(),
        format!("{RAW}:a:b")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_unavailable_recorded_callback_refuses_before_any_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let a = appending_callback("response-a", 1, ":a", &calls);
    let b = appending_callback("response-b", 1, ":b", &calls);
    let recorded = callback_session(vec![b.clone(), a]);
    let live = callback_session(vec![b]);
    let error = replay_callbacks(&recorded, &live, &[])
        .await
        .expect_err("the recorded callback is owed");
    let error = lash_core::RuntimeEffectControllerError::from(error).into_runtime_error();
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginRevisionUnavailable
    );
    let Some(lash_core::RuntimeErrorCause::PluginExecution { refusal }) = error.cause else {
        panic!("the missing callback remains typed");
    };
    assert_eq!(
        refusal.callback.as_ref().unwrap().owner.plugin,
        "response-a"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_recorded_callback_revision_never_runs_a_substitute() {
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = callback_session(vec![appending_callback("response-a", 1, ":old", &calls)]);
    let live = callback_session(vec![appending_callback("response-a", 2, ":new", &calls)]);
    let error = replay_callbacks(&recorded, &live, &[])
        .await
        .expect_err("revision one is owed");
    let error = lash_core::RuntimeEffectControllerError::from(error).into_runtime_error();
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginRevisionUnavailable
    );
    let Some(lash_core::RuntimeErrorCause::PluginExecution { refusal }) = error.cause else {
        panic!("the revision refusal remains typed");
    };
    let callback = refusal.callback.as_ref().unwrap();
    assert_eq!(callback.key, "assistant_response:append");
    assert_eq!(callback.owner.behavior_revision.get(), 1);
    assert_eq!(
        refusal
            .available
            .iter()
            .find(|revision| revision.plugin == "response-a")
            .unwrap()
            .behavior_revision
            .get(),
        2
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_state_pairs_multiple_callbacks_of_one_plugin_on_cold_replay() {
    let mut spec = lash_core::facade_support::PluginSpec::new();
    for state in ["first", "second"] {
        let key = lash_core::plugin::HookKey::new(state).unwrap();
        spec = spec.with_assistant_stream_finished(
            key,
            Arc::new(move |_| Box::pin(async move { Ok(Some(serde_json::json!(state))) })),
        );
        spec = spec.with_assistant_response(
            key,
            Some(key),
            Arc::new(move |ctx| {
                let state = ctx
                    .stream_state
                    .expect("this callback's recorded stream state");
                let response = text_response(&format!(
                    "{}:{}",
                    ctx.response.full_text(),
                    state.as_str().unwrap()
                ));
                Box::pin(async move {
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        );
    }
    let plugin: Arc<dyn lash_core::plugin::PluginFactory> = Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("paired-responses"),
        spec,
    ));
    let recorded = callback_session(vec![plugin.clone()]);
    let states = recorded
        .finish_assistant_stream(
            &lash_core::SessionId::fixture("recorded-response-plan"),
            lash_core::plugin::AssistantStreamFinishReason::Complete,
        )
        .await
        .unwrap();
    let states: Vec<lash_core::AssistantStreamHookState> =
        serde_json::from_value(serde_json::to_value(states).unwrap()).unwrap();
    let live = callback_session(vec![plugin]);
    assert_eq!(
        replay_callbacks(&recorded, &live, &states).await.unwrap(),
        format!("{RAW}:first:second")
    );
}
