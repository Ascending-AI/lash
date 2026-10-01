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

/// A core over `double` whose provider answers [`RAW`], counting its calls
/// in `provider_calls`, with the deriving response hook installed when
/// `with_hook`. Building it installs its session driver on the double's
/// engine, once no other core holds that installation.
fn core_over(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
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
        LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded)
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
fn session_journals(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    session: &lash_core::SessionId,
) -> Vec<String> {
    let server = double.server();
    server
        .invocations()
        .into_iter()
        .filter(|view| view.target.contains(session.as_str()))
        .map(|view| {
            let names: Vec<_> = server
                .journal(&view.id)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| format!("{:?}:{:?}", entry.ty, entry.name))
                .collect();
            format!(
                "{} {} attempts={} failure={:?} {names:?}",
                view.target, view.status, view.attempts, view.last_failure
            )
        })
        .collect()
}

/// The lash invocations of `session` the engine has not completed.
fn open_session_invocations(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    session: &lash_core::SessionId,
) -> Vec<String> {
    double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| {
            (view.target.starts_with("LashSession/") || view.target.starts_with("LashTurn/"))
                && view.target.contains(session.as_str())
                && view.status != "completed"
        })
        .map(|view| format!("{} {} {:?}", view.target, view.status, view.last_failure))
        .collect()
}

async fn a_changed_response_hook_set_does_not_change_the_served_response(
    change: HookChange,
    storage: Storage,
    always_replay: bool,
) -> Result<()> {
    let Some(world) = world(storage, always_replay).await else {
        eprintln!("skipping the {storage:?} leg: LASH_POSTGRES_DATABASE_URL is not set");
        return Ok(());
    };
    let World {
        double,
        _files,
        _database,
    } = world;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(
        &double,
        change.first_has_hook(),
        &provider_calls,
        &hook_calls,
    );
    let session_id = lash_core::SessionId::from("response-phase-replay");
    drop(
        core.session(session_id.as_str())
            .created()
            .await
            .open()
            .await?,
    );
    let root = lash_core::TurnId::from("response-phase-root");
    let server = double.server();
    // The step after the LLM call's phases: the turn's completion
    // checkpoint, which both hook sets issue.
    server.crash_on(
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
    // runs before the server starts the replay.
    let first = Arc::new(std::sync::Mutex::new(Some(core)));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(server.on_crash(Arc::new({
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
        session_journals(&double, &session_id),
    );
    let hook_calls_before_replay = hook_calls.load(Ordering::SeqCst);
    let second = core_over(
        &double,
        !change.first_has_hook(),
        &provider_calls,
        &hook_calls,
    );

    let root_run = || {
        server.invocations().into_iter().find(|view| {
            view.target.starts_with("LashTurn/") && view.target.contains(root.as_str())
        })
    };
    let settled = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            // A replay that found no driver installed may have spent its
            // retries before the second core installed one.
            for view in server.invocations() {
                if view.target.contains(session_id.as_str()) && view.status == "paused" {
                    server.resume(&view.id);
                }
            }
            if root_run().is_some_and(|run| run.status == "completed")
                && open_session_invocations(&double, &session_id).is_empty()
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
        open_session_invocations(&double, &session_id),
        session_journals(&double, &session_id),
    );
    assert!(server.stats().crashes >= 1, "the first attempt died");
    let run = root_run().expect("the root's run is an invocation");
    let outcome = server
        .outcome(&run.id)
        .map(|outcome| outcome.map(|value| String::from_utf8_lossy(&value).into_owned()));
    let evidence = format!(
        "attempts={} last failure {:?}, outcome {outcome:?}, journals {:?}",
        run.attempts,
        run.last_failure,
        session_journals(&double, &session_id),
    );
    assert!(
        !matches!(&run.last_failure, Some((570, _))),
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
                a_changed_response_hook_set_does_not_change_the_served_response(
                    $change,
                    $storage,
                    $always_replay,
                )
                .await
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
