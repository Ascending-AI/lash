//! FIG-4346: a root replayed after its session was deleted follows the
//! journal its first attempt recorded and ends with the typed retirement
//! ADR 0049 prescribes.
//!
//! The first attempt of the root's `LashTurn` run journals its start marker,
//! its seal and the recorded step the replay must read back, and dies before
//! the recorded step after it. The session's storage delete commits before
//! the replay. The replay issues the recorded steps, whatever the drive can
//! read of the deleted session outside them, and the next step records the
//! retirement where the journal held nothing more: the run ends with the
//! typed `SessionDeleted` refusal, never a journal mismatch.
//!
//! Two roots meet the deletion:
//!
//! - **input**: an input-headed root that journaled its admission
//!   (`drive-admit`) and died before its head inspection (`drive-head`);
//! - **command**: a command root that journaled its first read of the
//!   session's command lane (`session-command-run:0`), applied the command
//!   off the journal, and died before its next read.
//!
//! Two drives meet the deletion:
//!
//! - **held open**: the host still holds the session, so the drive runs on the
//!   host's resident runtime, whose head refresh before the admission meets
//!   the tombstone;
//! - **closed**: no host holds it, so the engine cannot open the session at
//!   all and runs the root without a runtime.
//!
//! Each runs on the Restate server double with and without forced replay
//! (every await suspends and replays the journal from the start), over SQLite
//! memory, SQLite file and PostgreSQL. The PostgreSQL legs skip unless
//! `LASH_POSTGRES_DATABASE_URL` is set (and fail without it under
//! `LASH_REQUIRE_POSTGRES=1`).

use super::*;

const SEED: u64 = 0x4346_de1e;

#[derive(Clone, Copy, Debug)]
enum Storage {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The root the replay meets the deletion in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    /// An input-headed root: its first attempt journals its admission
    /// (`drive-admit`) and dies before its head inspection (`drive-head`).
    Input,
    /// A command root applying a queued session command: its first attempt
    /// journals its first read of the command lane (`session-command-run:0`),
    /// applies the command off the journal, and dies before its next read.
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drive {
    /// The host holds the session open: the drive runs on its resident
    /// runtime.
    HeldOpen,
    /// No host holds the session: the engine opens it for the drive, and
    /// cannot once it is deleted.
    Closed,
}

/// A core over the double's engine, and what the double's stores need to
/// outlive it.
struct World {
    core: LashCore,
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    deleter: Deleter,
    _files: Option<tempfile::TempDir>,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

/// How the crash listener commits the session's storage delete. It runs
/// under the server's lock, which the core's runtime threads block on, so
/// the delete must not need them: the SQLite stores run their connections
/// on threads of their own, and a PostgreSQL delete opens a connection on
/// the listener's own runtime instead of borrowing the core's pool.
#[derive(Clone)]
enum Deleter {
    Catalog(Arc<dyn lash_core::DeploymentStore>),
    Postgres {
        url: String,
        attachments: std::path::PathBuf,
    },
}

impl Deleter {
    async fn delete(&self, session_id: &lash_core::SessionId) {
        let catalog = match self {
            Self::Catalog(catalog) => Arc::clone(catalog),
            Self::Postgres { url, attachments } => {
                let storage = lash_postgres_store::PostgresStorage::connect(url)
                    .await
                    .expect("open the delete's PostgreSQL connection");
                let stores = lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                        attachments,
                    )),
                );
                lash_core::StoreSet::session_store_factory(&stores)
            }
        };
        // The dead attempt's writer can still hold the session for a moment:
        // a contended delete is retried until a bounded deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match catalog.delete_session(session_id).await {
                Ok(_) => return,
                Err(error)
                    if format!("{error:?}").contains("Contended")
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                Err(error) => panic!("the storage delete commits: {error:?}"),
            }
        }
    }
}

/// The PostgreSQL URL of the PostgreSQL legs, or `None` when they skip.
#[allow(
    clippy::disallowed_methods,
    reason = "the PostgreSQL legs read the optional service URL"
)]
fn postgres_url() -> Option<String> {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty());
    assert!(
        url.is_some() || std::env::var("LASH_REQUIRE_POSTGRES").as_deref() != Ok("1"),
        "LASH_POSTGRES_DATABASE_URL must be set and non-empty when LASH_REQUIRE_POSTGRES=1"
    );
    url
}

/// The world over `storage`, or `None` for a PostgreSQL leg without a
/// database.
async fn world(storage: Storage, always_replay: bool) -> Option<World> {
    let config = lash_restate_test::ServerConfig::default().always_replay(always_replay);
    let hooks = lash_restate_test::DeploymentHooks::default;
    let (double, files, database, postgres) = match storage {
        Storage::SqliteMemory => {
            let double =
                lash_restate_test::backend_with_store_set(SEED, config, hooks(), |clock| async {
                    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                        .await
                        .map_err(|error| {
                            lash_restate_test::BackendError::Stores(error.to_string())
                        })?;
                    Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
                })
                .await
                .expect("the Restate double over SQLite memory");
            (double, None, None, None)
        }
        Storage::SqliteFile => {
            let files = tempfile::tempdir().expect("a SQLite store directory");
            let root = files.path().to_owned();
            let double = lash_restate_test::backend_with_store_set(
                SEED,
                config,
                hooks(),
                |clock| async move {
                    let stores = lash_sqlite_store::SqliteStoreSet::open_with_clock(root, clock)
                        .await
                        .map_err(|error| {
                            lash_restate_test::BackendError::Stores(error.to_string())
                        })?;
                    Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
                },
            )
            .await
            .expect("the Restate double over SQLite files");
            (double, Some(files), None, None)
        }
        Storage::Postgres => {
            let url = postgres_url()?;
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open the provisioned PostgreSQL storage");
            let files = tempfile::tempdir().expect("an attachment directory");
            let attachments = files.path().to_owned();
            let deleter = Deleter::Postgres {
                url: database.url().to_owned(),
                attachments: attachments.clone(),
            };
            let double = lash_restate_test::backend_with_store_set(
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
            .expect("the Restate double over PostgreSQL");
            (double, Some(files), Some(database), Some(deleter))
        }
    };
    let provider = crate::testing::TestProvider::builder()
        .kind("deleted-session-root-replay")
        .complete(|request| async move {
            Ok(text_response(&format!(
                "echo: {}",
                last_user_text(&request)
            )))
        })
        .build()
        .into_handle();
    let core = LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core");
    let deleter = postgres.unwrap_or_else(|| Deleter::Catalog(Arc::clone(&core.store_factory)));
    Some(World {
        core,
        double,
        deleter,
        _files: files,
        _database: database,
    })
}

/// The lash invocations of `session` the engine has not completed.
fn open_session_invocations(world: &World, session: &lash_core::SessionId) -> Vec<String> {
    world
        .double
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

/// Every journal of `session`'s invocations, by the names of its entries.
fn session_journals(world: &World, session: &lash_core::SessionId) -> Vec<String> {
    let server = world.double.server();
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

async fn a_root_replayed_after_its_session_was_deleted_ends_typed(
    work: Work,
    storage: Storage,
    always_replay: bool,
    drive: Drive,
) -> Result<()> {
    let Some(world) = world(storage, always_replay).await else {
        eprintln!("skipping the {storage:?} leg: LASH_POSTGRES_DATABASE_URL is not set");
        return Ok(());
    };
    let session_id = lash_core::SessionId::from("deleted-root-replay");
    let session = world
        .core
        .session(session_id.as_str())
        .created()
        .await
        .open()
        .await?;
    let root = lash_core::TurnId::from("deleted-replay-root");
    let server = world.double.server();
    // The first attempt dies before the recorded step after the one the
    // replay must read back.
    let (crash, run_names, steps) = match work {
        Work::Input => (
            lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRun {
                name: format!("lash:drive-head:{root}"),
            })
            .key(lash_restate::turn_workflow_key(&session_id, &root)),
            root.to_string(),
            ["drive-admit:", "drive-head:"],
        ),
        Work::Command => (
            lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRun {
                name: "lash:session-command-run:1".to_owned(),
            }),
            "drive-commands:".to_owned(),
            ["session-command-run:0", "session-command-run:1"],
        ),
    };
    server.crash_on(crash.service(lash_restate_test::TURN_DRIVER_SERVICE));
    // The storage delete commits while the dead attempt's replay has not
    // started: the listener runs before the server starts it, and the delete
    // is the store's alone, so it needs nothing from the server.
    let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(server.on_crash(Arc::new({
        let deleted = Arc::clone(&deleted);
        let deleter = world.deleter.clone();
        let session_id = session_id.clone();
        move |_target: &str| {
            let deleter = deleter.clone();
            let session_id = session_id.clone();
            let delete = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime for the delete")
                    .block_on(deleter.delete(&session_id));
            })
            .join();
            deleted.store(delete.is_ok(), Ordering::SeqCst);
        }
    })));

    if work == Work::Command {
        Box::pin(
            session
                .admin()
                .commands()
                .refresh_tool_catalog("delete me mid-root", "deleted-replay"),
        )
        .await?;
    }
    let held = match drive {
        Drive::HeldOpen => Some(session),
        Drive::Closed => {
            drop(session);
            None
        }
    };
    if work == Work::Input {
        let store = lash_core::runtime::live_session_view(&world.core.store_factory, &session_id)
            .await?
            .expect("an opened session has a store");
        store
            .enqueue_pending_turn_input(
                lash_core::PendingTurnInputDraft::new(
                    session_id.clone(),
                    lash_core::TurnInputIngress::NextTurn,
                    TurnInput::text("delete me mid-root"),
                )
                .with_source_key(root.as_str()),
            )
            .await
            .expect("enqueue the input");
        let engine_port = world.core.substrate_slot.ports().await.queued;
        engine_port.schedule_drive(
            &session_id,
            lash_core::engine::DriveRequestId::new("deleted-replay"),
        );
    }

    let settled = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            if deleted.load(Ordering::SeqCst)
                && open_session_invocations(&world, &session_id).is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the replayed root and its drive finish: deleted={}, crashes={}, open {:?}, journals {:?}",
        deleted.load(Ordering::SeqCst),
        server.stats().crashes,
        open_session_invocations(&world, &session_id),
        session_journals(&world, &session_id),
    );
    assert!(server.stats().crashes >= 1, "the first attempt died");

    let run = server
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTurn/") && view.target.contains(&run_names))
        .expect("the root's run is an invocation");
    let names: Vec<_> = server
        .journal(&run.id)
        .expect("the run's journal")
        .into_iter()
        .filter_map(|entry| entry.name)
        .collect();
    let outcome = server.outcome(&run.id);
    let evidence = format!(
        "attempts={} last failure {:?}, outcome {outcome:?}, journal {names:?}",
        run.attempts, run.last_failure
    );
    assert!(
        !matches!(&run.last_failure, Some((570, _))),
        "the replay followed its journal: {evidence}"
    );
    for step in ["drive-root-start:", "drive-seal:"]
        .into_iter()
        .chain(steps)
    {
        assert!(
            names.iter().any(|name| name.contains(step)),
            "the replay issued the recorded steps and the one after them, \
             missing `{step}`: {evidence}"
        );
    }
    assert!(
        matches!(&outcome, Some(Err((_, message))) if message.contains("session_deleted")),
        "the root's run ends with the typed retirement: {evidence}"
    );
    drop(held);
    Ok(())
}

macro_rules! deleted_session_root_replay_laws {
    ($($name:ident: $work:expr, $storage:expr, $always_replay:expr, $drive:expr;)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                a_root_replayed_after_its_session_was_deleted_ends_typed(
                    $work,
                    $storage,
                    $always_replay,
                    $drive,
                )
                .await
            }
        )*
    };
}

deleted_session_root_replay_laws! {
    input_sqlite_memory_held_open: Work::Input, Storage::SqliteMemory, false, Drive::HeldOpen;
    input_sqlite_memory_closed: Work::Input, Storage::SqliteMemory, false, Drive::Closed;
    input_sqlite_memory_always_replay_held_open: Work::Input, Storage::SqliteMemory, true, Drive::HeldOpen;
    input_sqlite_memory_always_replay_closed: Work::Input, Storage::SqliteMemory, true, Drive::Closed;
    input_sqlite_file_held_open: Work::Input, Storage::SqliteFile, false, Drive::HeldOpen;
    input_sqlite_file_closed: Work::Input, Storage::SqliteFile, false, Drive::Closed;
    input_sqlite_file_always_replay_held_open: Work::Input, Storage::SqliteFile, true, Drive::HeldOpen;
    input_sqlite_file_always_replay_closed: Work::Input, Storage::SqliteFile, true, Drive::Closed;
    input_postgres_held_open: Work::Input, Storage::Postgres, false, Drive::HeldOpen;
    input_postgres_closed: Work::Input, Storage::Postgres, false, Drive::Closed;
    input_postgres_always_replay_held_open: Work::Input, Storage::Postgres, true, Drive::HeldOpen;
    input_postgres_always_replay_closed: Work::Input, Storage::Postgres, true, Drive::Closed;
    command_sqlite_memory_held_open: Work::Command, Storage::SqliteMemory, false, Drive::HeldOpen;
    command_sqlite_memory_closed: Work::Command, Storage::SqliteMemory, false, Drive::Closed;
    command_sqlite_memory_always_replay_held_open: Work::Command, Storage::SqliteMemory, true, Drive::HeldOpen;
    command_sqlite_memory_always_replay_closed: Work::Command, Storage::SqliteMemory, true, Drive::Closed;
    command_sqlite_file_held_open: Work::Command, Storage::SqliteFile, false, Drive::HeldOpen;
    command_sqlite_file_closed: Work::Command, Storage::SqliteFile, false, Drive::Closed;
    command_sqlite_file_always_replay_held_open: Work::Command, Storage::SqliteFile, true, Drive::HeldOpen;
    command_sqlite_file_always_replay_closed: Work::Command, Storage::SqliteFile, true, Drive::Closed;
    command_postgres_held_open: Work::Command, Storage::Postgres, false, Drive::HeldOpen;
    command_postgres_closed: Work::Command, Storage::Postgres, false, Drive::Closed;
    command_postgres_always_replay_held_open: Work::Command, Storage::Postgres, true, Drive::HeldOpen;
    command_postgres_always_replay_closed: Work::Command, Storage::Postgres, true, Drive::Closed;
}
