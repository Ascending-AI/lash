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
//! Three roots meet the deletion:
//!
//! - **input**: an input-headed root that journaled its admission
//!   (`drive-admit`) and died before its head inspection (`drive-head`);
//! - **command**: a command root that journaled its first read of the
//!   session's command lane (`session-command-run:0`), applied the command
//!   off the journal, and died before its next read;
//! - **follow-on** (FIG-4361): a follow-on recovery root that journaled its
//!   seal and died while its first execution was storing the step after it,
//!   its recovery decision (`drive-follow-on`). The input root before it
//!   switched agent frame and its inline follow-on failed before its commit,
//!   so the session's drive admitted the owed follow-on as a root of its own.
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
//! memory, SQLite file and PostgreSQL. The PostgreSQL legs are ignored in
//! ordinary runs and require `LASH_POSTGRES_DATABASE_URL` when selected
//! with `--include-ignored` inside a PostgreSQL gate.
//!
//! The `recovered_follow_on_*` laws are the follow-on legs' control: with its
//! session live, the recovery root records its decision between its seal and
//! its turn and commits the follow-on. The `changed_bound_*` laws change the
//! host's follow-on recovery bound between the recovery root's attempts: the
//! root decides on the bound its chain froze and keeps the decision it
//! recorded (the config-uniformity audit's D2). Both run with and without
//! forced replay: a replay after the follow-on's own commit runs its turn on
//! the head and at the turn index the decision recorded, never on the head
//! that commit moved (FIG-4380).

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
    /// A follow-on recovery root: its first attempt journals its seal and
    /// dies while the server stores the step after it, its recovery
    /// decision (`drive-follow-on`), with no result recorded.
    FollowOn,
}

/// The task the frame switch hands the follow-on; the follow-on frame's
/// context carries it and the first frame's does not.
const FOLLOW_ON_TASK: &str = "answer from the switched frame";

/// The follow-on the input root's frame switch owes.
fn follow_on_of(root: &lash_core::TurnId) -> lash_core::TurnId {
    lash_core::TurnId::from(format!("{root}:agent-frame:1"))
}

/// The root the session's drive admits the owed follow-on's first recovery
/// under.
fn follow_on_recovery_root(root: &lash_core::TurnId) -> lash_core::TurnId {
    lash_core::TurnId::from(format!("follow-on:{}#0", follow_on_of(root)))
}

/// A stateless model: the switched frame's context carries the task and
/// is answered with text; the first frame's is answered with the switch.
fn follow_on_model_reply(request: &LlmRequest) -> LlmResponse {
    if serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains(FOLLOW_ON_TASK)
    {
        return text_response("follow-on done");
    }
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: "switch-call".to_string(),
            tool_name: "switch_frame".to_string(),
            input_json: serde_json::json!({ "task": FOLLOW_ON_TASK }).to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

/// A before-turn hook that fails a turn in the frame of a follow-on the head
/// owes at recovery count zero: the inline follow-on of the root that
/// switched frame, on every execution of that root. The root's first turn,
/// replayed on the head before its switch, runs in the first frame and
/// passes. The switch commit stays, the follow-on stays owed, and the
/// session's drive admits its recovery as a root of its own, which raises
/// the count before its turn and passes the hook.
fn fail_the_inline_follow_on(
    catalog: Arc<std::sync::OnceLock<Arc<dyn lash_core::DeploymentStore>>>,
) -> crate::plugins::StaticPluginFactory {
    let hook: lash_core::plugin::BeforeTurnHook = Arc::new(move |context| {
        let catalog = catalog.get().cloned();
        Box::pin(async move {
            let Some(catalog) = catalog else {
                return Ok(Vec::new());
            };
            let owed =
                match lash_core::runtime::live_session_view(&catalog, &context.session_id).await {
                    Ok(Some(store)) => store.load_pending_follow_on().await.ok().flatten(),
                    _ => None,
                };
            let frame = context.state.to_snapshot().current_frame_node_id;
            if owed.is_some_and(|owed| owed.attempts == 0 && frame.as_ref() == Some(&owed.frame_id))
            {
                return Err(lash_core::PluginError::Invoke(
                    "the inline follow-on fails before its commit".to_owned(),
                ));
            }
            Ok(Vec::new())
        }) as lash_core::plugin::PluginFuture<_>
    });
    crate::plugins::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("deleted-session-follow-on-failure"),
        lash_core::facade_support::PluginSpec::new().with_before_turn(hook),
    )
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

/// The PostgreSQL URL required by the PostgreSQL legs.
#[allow(
    clippy::disallowed_methods,
    reason = "the PostgreSQL legs read the required service URL"
)]
fn postgres_url() -> String {
    lash_postgres_store::testing::required_database_url()
}

/// The world over `storage`, or `None` for a PostgreSQL leg without a
/// database.
async fn world(storage: Storage, always_replay: bool, work: Work) -> Option<World> {
    world_bounded(
        storage,
        always_replay,
        work,
        lash_core::store::DEFAULT_MAX_FOLLOW_ON_RECOVERIES,
    )
    .await
}

/// [`world`], its core configured with the host follow-on recovery bound
/// `max_recoveries`.
async fn world_bounded(
    storage: Storage,
    always_replay: bool,
    work: Work,
    max_recoveries: u32,
) -> Option<World> {
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
            let url = postgres_url();
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
    let core = core_over(&double, work, max_recoveries);
    let deleter = postgres.unwrap_or_else(|| Deleter::Catalog(Arc::clone(&core.store_factory)));
    Some(World {
        core,
        double,
        deleter,
        _files: files,
        _database: database,
    })
}

/// A core over `double` for `work`, with the host follow-on recovery bound
/// `max_recoveries`. Building it installs its session driver on the double's
/// engine, once no other core holds that installation.
fn core_over(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    work: Work,
    max_recoveries: u32,
) -> LashCore {
    let provider = crate::testing::TestProvider::builder()
        .kind("deleted-session-root-replay")
        .complete(move |request| async move {
            Ok(match work {
                Work::FollowOn => follow_on_model_reply(&request),
                Work::Input | Work::Command => {
                    text_response(&format!("echo: {}", last_user_text(&request)))
                }
            })
        })
        .build()
        .into_handle();
    let catalog = Arc::new(std::sync::OnceLock::new());
    let mut builder = LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    )
    .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(
        crate::QueuedWorkBatchingConfig::new(1).with_max_follow_on_recoveries(max_recoveries),
    )
    .serve_test_model(provider, mock_model_spec());
    if work == Work::FollowOn {
        builder = builder
            .tools(Arc::new(AgentFrameSwitchTools))
            .plugin(Arc::new(fail_the_inline_follow_on(Arc::clone(&catalog))));
    }
    let core = builder
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core");
    let _ = catalog.set(Arc::clone(&core.store_factory));
    core
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

async fn a_root_replayed_after_its_session_was_deleted_ends_typed(
    work: Work,
    storage: Storage,
    always_replay: bool,
    drive: Drive,
) -> Result<()> {
    let Some(world) = world(storage, always_replay, work).await else {
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
            vec!["drive-admit:", "drive-head:"],
        ),
        Work::Command => (
            lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRun {
                name: "lash:session-command-run:1".to_owned(),
            }),
            "drive-commands:".to_owned(),
            vec!["session-command-run:0", "session-command-run:1"],
        ),
        // The command after the seal (the input is command 0, the start
        // marker 1 and the seal 2) is stored and its result is not: the
        // replay issues it again. Named by index, it is the same point
        // whatever step the root journals after its seal.
        Work::FollowOn => {
            let recovery = follow_on_recovery_root(&root);
            (
                lash_restate_test::CrashRule::new(
                    lash_restate_test::CrashPoint::BeforeRunResultAt { index: 3 },
                )
                .key(lash_restate::turn_workflow_key(&session_id, &recovery)),
                recovery.to_string(),
                vec!["drive-follow-on:"],
            )
        }
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
    if matches!(work, Work::Input | Work::FollowOn) {
        let store = lash_core::runtime::live_session_view(&world.core.store_factory, &session_id)
            .await?
            .expect("an opened session has a store");
        store
            .enqueue_pending_turn_input(
                lash_core::PendingTurnInputDraft::new(
                    session_id.clone(),
                    lash_core::TurnInputIngress::NextTurn,
                    TurnInput::text(match work {
                        Work::FollowOn => "hand this off, then delete me mid-recovery",
                        Work::Input | Work::Command => "delete me mid-root",
                    }),
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
                && open_session_invocations(&world.double, &session_id).is_empty()
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
        open_session_invocations(&world.double, &session_id),
        session_journals(&world.double, &session_id),
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
        .chain(steps.iter().copied())
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

/// The control of the follow-on legs (FIG-4361): with its session live, a
/// follow-on recovery root records its decision after its seal, raising the
/// recovery count in its body, and drives the recorded answer to the
/// follow-on's terminal commit. Under forced replay the root is replayed
/// after that commit moved the head: it runs its turn on the head and at the
/// turn index its decision recorded, so it follows its journal (FIG-4380).
async fn a_follow_on_recovery_root_drives_its_recorded_decision(
    storage: Storage,
    always_replay: bool,
) -> Result<()> {
    let Some(world) = world(storage, always_replay, Work::FollowOn).await else {
        eprintln!("skipping the {storage:?} leg: LASH_POSTGRES_DATABASE_URL is not set");
        return Ok(());
    };
    let session_id = lash_core::SessionId::from("recovered-follow-on");
    let _session = world
        .core
        .session(session_id.as_str())
        .created()
        .await
        .open()
        .await?;
    let root = lash_core::TurnId::from("recovered-root");
    let recovery = follow_on_recovery_root(&root);
    let store = lash_core::runtime::live_session_view(&world.core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("hand this off"),
            )
            .with_source_key(root.as_str()),
        )
        .await
        .expect("enqueue the input");
    let engine_port = world.core.substrate_slot.ports().await.queued;
    engine_port.schedule_drive(
        &session_id,
        lash_core::engine::DriveRequestId::new("recovered-follow-on"),
    );

    let server = world.double.server();
    let recovery_run = || {
        server.invocations().into_iter().find(|view| {
            view.target.starts_with("LashTurn/") && view.target.contains(recovery.as_str())
        })
    };
    let settled = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            if recovery_run().is_some_and(|run| run.status == "completed")
                && open_session_invocations(&world.double, &session_id).is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the recovery root and its drive finish: open {:?}, journals {:?}",
        open_session_invocations(&world.double, &session_id),
        session_journals(&world.double, &session_id),
    );
    let run = recovery_run().expect("the recovery root's run is an invocation");
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
    assert!(
        matches!(&outcome, Some(Ok(_))),
        "the recovery root's run ends with its answer: {evidence}"
    );
    let position = |step: &str| {
        names
            .iter()
            .position(|name| name.contains(step))
            .unwrap_or_else(|| panic!("the journal holds `{step}`: {evidence}"))
    };
    let follow_on = follow_on_of(&root);
    assert!(
        position("drive-seal:") < position("drive-follow-on:")
            && position("drive-follow-on:") < position(&format!("turn-config:{follow_on}")),
        "the decision is recorded between the seal and the follow-on's turn: {evidence}"
    );
    let head = store
        .load_session_head_meta()
        .await?
        .expect("the session has a head");
    assert_eq!(
        head.pending_follow_on, None,
        "the follow-on's terminal commit cleared its fact: {evidence}"
    );
    assert!(
        store.committed_turn_exists(&follow_on).await?,
        "the follow-on committed: {evidence}"
    );
    let answers = store
        .load_session_window(lash_core::store::WindowSelector::Current)
        .await?
        .expect("the session has a committed head")
        .window
        .read_model()
        .messages
        .iter()
        .filter(|message| {
            serde_json::to_string(message)
                .unwrap_or_default()
                .contains("follow-on done")
        })
        .count();
    assert_eq!(answers, 1, "the follow-on committed once: {evidence}");
    Ok(())
}

/// Where a follow-on recovery root's first attempt dies before the host it
/// replays on is configured with another recovery bound (FIG-4361, the
/// config-uniformity audit's D2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoundChange {
    /// The chain froze bound 0; the root dies before its decision is
    /// recorded, and replays on a host with bound 3. The raised count 1 is
    /// past the frozen bound: the follow-on commits exhausted.
    BeforeDecision,
    /// The chain froze bound 3; the root records `Run` and dies before its
    /// turn's first step, and replays on a host with bound 0. The recorded
    /// `Run` stands: the follow-on runs and answers.
    AfterDecision,
}

/// A changed host recovery bound between the attempts of a follow-on
/// recovery root does not change its decision: the root decides on the
/// bound its chain froze, and replays the decision it recorded. The first
/// core, whose bound the switch froze, is dropped as the root's first
/// attempt dies, so the replay finds no session driver installed until the
/// second core, with the other bound, installs its own. Under forced replay
/// the root is replayed after the follow-on's terminal commit, run or
/// exhausted, moved the head: it runs that terminal on the head and at the
/// turn index its decision recorded (FIG-4380).
async fn a_changed_host_bound_does_not_change_the_recovery_decision(
    change: BoundChange,
    storage: Storage,
    always_replay: bool,
) -> Result<()> {
    let (frozen, host) = match change {
        BoundChange::BeforeDecision => (0, 3),
        BoundChange::AfterDecision => (3, 0),
    };
    let Some(world) = world_bounded(storage, always_replay, Work::FollowOn, frozen).await else {
        eprintln!("skipping the {storage:?} leg: LASH_POSTGRES_DATABASE_URL is not set");
        return Ok(());
    };
    let World {
        core,
        double,
        deleter: _,
        _files,
        _database,
    } = world;
    let session_id = lash_core::SessionId::from("changed-recovery-bound");
    drop(
        core.session(session_id.as_str())
            .created()
            .await
            .open()
            .await?,
    );
    let root = lash_core::TurnId::from("bound-root");
    let recovery = follow_on_recovery_root(&root);
    let follow_on = follow_on_of(&root);
    let server = double.server();
    let crash = match change {
        BoundChange::BeforeDecision => lash_restate_test::CrashPoint::BeforeRun {
            name: format!("lash:drive-follow-on:{recovery}"),
        },
        BoundChange::AfterDecision => lash_restate_test::CrashPoint::BeforeRun {
            name: format!("lash:turn-config:{follow_on}"),
        },
    };
    server.crash_on(
        lash_restate_test::CrashRule::new(crash)
            .service(lash_restate_test::TURN_DRIVER_SERVICE)
            .key(lash_restate::turn_workflow_key(&session_id, &recovery)),
    );
    let store = lash_core::runtime::live_session_view(&core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text("hand this off"),
            )
            .with_source_key(root.as_str()),
        )
        .await
        .expect("enqueue the input");
    let engine_port = core.substrate_slot.ports().await.queued;
    // The first core leaves as its recovery root's first attempt dies: the
    // listener runs before the server starts the replay.
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
        lash_core::engine::DriveRequestId::new("changed-recovery-bound"),
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
        "the recovery root's first attempt dies: journals {:?}",
        session_journals(&double, &session_id),
    );
    let second = core_over(&double, Work::FollowOn, host);

    let recovery_run = || {
        server.invocations().into_iter().find(|view| {
            view.target.starts_with("LashTurn/") && view.target.contains(recovery.as_str())
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
            if recovery_run().is_some_and(|run| run.status == "completed")
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
        "the recovery root and its drive finish: open {:?}, journals {:?}",
        open_session_invocations(&double, &session_id),
        session_journals(&double, &session_id),
    );
    assert!(server.stats().crashes >= 1, "the first attempt died");
    let run = recovery_run().expect("the recovery root's run is an invocation");
    let journal = server.journal(&run.id).expect("the run's journal");
    let decisions: Vec<String> = journal
        .iter()
        .filter_map(|entry| entry.run_completion()?.ok())
        .map(|value| String::from_utf8_lossy(value.as_ref()).into_owned())
        .filter(|value| value.contains("\"answer\":{\"decision\""))
        .collect();
    let outcome = server
        .outcome(&run.id)
        .map(|outcome| outcome.map(|value| String::from_utf8_lossy(&value).into_owned()));
    let evidence = format!(
        "attempts={} last failure {:?}, outcome {outcome:?}, decisions {decisions:?}",
        run.attempts, run.last_failure
    );
    assert!(
        !matches!(&run.last_failure, Some((570, _))),
        "the replay followed its journal: {evidence}"
    );
    let [decision] = decisions.as_slice() else {
        panic!("the root recorded one recovery decision: {evidence}");
    };
    assert!(
        decision.contains(&format!("\"follow_on_recoveries\":{frozen}")),
        "the recorded decision carries the bound the chain froze: {evidence}"
    );
    match change {
        BoundChange::BeforeDecision => {
            assert!(
                decision.contains("\"decision\":\"exhausted\""),
                "the frozen bound decided exhaustion: {evidence}"
            );
            // An exhausted follow-on commits as its failed terminal before
            // any execution: no model call answers it.
            assert!(
                matches!(&outcome, Some(Ok(value))
                    if value.contains("\"root_outcome\":\"committed\"")
                        && value.contains("\"stopped\"")
                        && !value.contains("follow-on done")),
                "the follow-on committed exhausted: {evidence}"
            );
        }
        BoundChange::AfterDecision => {
            assert!(
                decision.contains("\"decision\":\"run\""),
                "the recorded decision ran the follow-on: {evidence}"
            );
            assert!(
                matches!(&outcome, Some(Ok(value)) if value.contains("follow-on done")),
                "the follow-on ran and answered: {evidence}"
            );
        }
    }
    let head = store
        .load_session_head_meta()
        .await?
        .expect("the session has a head");
    assert_eq!(
        head.pending_follow_on, None,
        "the follow-on's terminal commit cleared its fact: {evidence}"
    );
    drop(second);
    Ok(())
}

macro_rules! changed_recovery_bound_laws {
    ($($(#[$attr:meta])* $name:ident: $change:expr, $storage:expr, $always_replay:expr;)*) => {
        $(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                a_changed_host_bound_does_not_change_the_recovery_decision(
                    $change,
                    $storage,
                    $always_replay,
                )
                .await
            }
        )*
    };
}

changed_recovery_bound_laws! {
    changed_bound_before_decision_sqlite_memory: BoundChange::BeforeDecision, Storage::SqliteMemory, false;
    changed_bound_before_decision_sqlite_file: BoundChange::BeforeDecision, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    changed_bound_before_decision_postgres: BoundChange::BeforeDecision, Storage::Postgres, false;
    changed_bound_after_decision_sqlite_memory: BoundChange::AfterDecision, Storage::SqliteMemory, false;
    changed_bound_after_decision_sqlite_file: BoundChange::AfterDecision, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    changed_bound_after_decision_postgres: BoundChange::AfterDecision, Storage::Postgres, false;
    changed_bound_before_decision_sqlite_memory_always_replay: BoundChange::BeforeDecision, Storage::SqliteMemory, true;
    changed_bound_before_decision_sqlite_file_always_replay: BoundChange::BeforeDecision, Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    changed_bound_before_decision_postgres_always_replay: BoundChange::BeforeDecision, Storage::Postgres, true;
    changed_bound_after_decision_sqlite_memory_always_replay: BoundChange::AfterDecision, Storage::SqliteMemory, true;
    changed_bound_after_decision_sqlite_file_always_replay: BoundChange::AfterDecision, Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    changed_bound_after_decision_postgres_always_replay: BoundChange::AfterDecision, Storage::Postgres, true;
}

macro_rules! follow_on_recovery_laws {
    ($($(#[$attr:meta])* $name:ident: $storage:expr, $always_replay:expr;)*) => {
        $(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                a_follow_on_recovery_root_drives_its_recorded_decision($storage, $always_replay)
                    .await
            }
        )*
    };
}

follow_on_recovery_laws! {
    recovered_follow_on_sqlite_memory: Storage::SqliteMemory, false;
    recovered_follow_on_sqlite_file: Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    recovered_follow_on_postgres: Storage::Postgres, false;
    recovered_follow_on_sqlite_memory_always_replay: Storage::SqliteMemory, true;
    recovered_follow_on_sqlite_file_always_replay: Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    recovered_follow_on_postgres_always_replay: Storage::Postgres, true;
}

macro_rules! deleted_session_root_replay_laws {
    ($($(#[$attr:meta])* $name:ident: $work:expr, $storage:expr, $always_replay:expr, $drive:expr;)*) => {
        $(
            $(#[$attr])*
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
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    input_postgres_held_open: Work::Input, Storage::Postgres, false, Drive::HeldOpen;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    input_postgres_closed: Work::Input, Storage::Postgres, false, Drive::Closed;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    input_postgres_always_replay_held_open: Work::Input, Storage::Postgres, true, Drive::HeldOpen;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    input_postgres_always_replay_closed: Work::Input, Storage::Postgres, true, Drive::Closed;
    command_sqlite_memory_held_open: Work::Command, Storage::SqliteMemory, false, Drive::HeldOpen;
    command_sqlite_memory_closed: Work::Command, Storage::SqliteMemory, false, Drive::Closed;
    command_sqlite_memory_always_replay_held_open: Work::Command, Storage::SqliteMemory, true, Drive::HeldOpen;
    command_sqlite_memory_always_replay_closed: Work::Command, Storage::SqliteMemory, true, Drive::Closed;
    command_sqlite_file_held_open: Work::Command, Storage::SqliteFile, false, Drive::HeldOpen;
    command_sqlite_file_closed: Work::Command, Storage::SqliteFile, false, Drive::Closed;
    command_sqlite_file_always_replay_held_open: Work::Command, Storage::SqliteFile, true, Drive::HeldOpen;
    command_sqlite_file_always_replay_closed: Work::Command, Storage::SqliteFile, true, Drive::Closed;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    command_postgres_held_open: Work::Command, Storage::Postgres, false, Drive::HeldOpen;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    command_postgres_closed: Work::Command, Storage::Postgres, false, Drive::Closed;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    command_postgres_always_replay_held_open: Work::Command, Storage::Postgres, true, Drive::HeldOpen;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    command_postgres_always_replay_closed: Work::Command, Storage::Postgres, true, Drive::Closed;
    follow_on_sqlite_memory_held_open: Work::FollowOn, Storage::SqliteMemory, false, Drive::HeldOpen;
    follow_on_sqlite_memory_closed: Work::FollowOn, Storage::SqliteMemory, false, Drive::Closed;
    follow_on_sqlite_memory_always_replay_held_open: Work::FollowOn, Storage::SqliteMemory, true, Drive::HeldOpen;
    follow_on_sqlite_memory_always_replay_closed: Work::FollowOn, Storage::SqliteMemory, true, Drive::Closed;
    follow_on_sqlite_file_held_open: Work::FollowOn, Storage::SqliteFile, false, Drive::HeldOpen;
    follow_on_sqlite_file_closed: Work::FollowOn, Storage::SqliteFile, false, Drive::Closed;
    follow_on_sqlite_file_always_replay_held_open: Work::FollowOn, Storage::SqliteFile, true, Drive::HeldOpen;
    follow_on_sqlite_file_always_replay_closed: Work::FollowOn, Storage::SqliteFile, true, Drive::Closed;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    follow_on_postgres_held_open: Work::FollowOn, Storage::Postgres, false, Drive::HeldOpen;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    follow_on_postgres_closed: Work::FollowOn, Storage::Postgres, false, Drive::Closed;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    follow_on_postgres_always_replay_held_open: Work::FollowOn, Storage::Postgres, true, Drive::HeldOpen;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    follow_on_postgres_always_replay_closed: Work::FollowOn, Storage::Postgres, true, Drive::Closed;
}
