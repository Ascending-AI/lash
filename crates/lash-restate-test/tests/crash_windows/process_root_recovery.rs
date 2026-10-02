//! A root a `SessionTurn` process runs is judged by its process (FIG-4378,
//! FIG-4403).
//!
//! A `SessionTurn` process runs its child turn inline, in its own
//! `LashProcessWorkflow` run: the drive there admits, seals and commits the
//! child root, and every root admitted ahead of it in a reused session, and
//! none of them runs as a `LashTurn` run. The lost-root recovery pass reads
//! the `LashTurn` lanes of every open root's key, so it finds no run of such
//! a root on any lane while the process is running it. That absence proves
//! nothing: the root's admission records the process's run as its executor,
//! and the lost-process pass owns that run.
//!
//! Each law holds a model call the process's drive makes, which leaves the
//! root open with its input admitted, and runs the recovery pass. The pass
//! keeps the root and its admitted input. Released, the root commits its
//! turn: it answers, its input settles with it, and the process completes.
//! - The child law holds the process's own child root.
//! - The ahead law reuses an explicit session with a row queued before the
//!   process's own, and holds the root the process's drive admits for it,
//!   which its name does not tie to the process.
//!
//! The laws run on SQLite memory, SQLite file and PostgreSQL, over the
//! server double (plain, and every attempt replayed when
//! `LASH_RESTATE_TEST_ALWAYS_REPLAY=1`) and a live `restate-server` (the
//! `crash-windows` Restate suite).

use super::*;
use lash_core::StoreSet;
use lash_core::engine::{EnginePage, RootRef};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};

#[derive(Clone, Copy, Debug)]
pub(super) enum Storage {
    Memory,
    File,
    Postgres,
}

/// The store set a law runs over, and what keeps it alive.
pub(super) struct Stores {
    stores: Arc<dyn StoreSet>,
    _directory: tempfile::TempDir,
    _database: Option<IsolatedDatabase>,
}

impl Stores {
    async fn new(storage: Storage, clock: Arc<dyn lash_core::Clock>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut database = None;
        let stores: Arc<dyn StoreSet> = match storage {
            Storage::Memory => Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                    .await
                    .unwrap(),
            ),
            Storage::File => Arc::new(
                lash_sqlite_store::SqliteStoreSet::open_with_clock(directory.path(), clock)
                    .await
                    .unwrap(),
            ),
            Storage::Postgres => {
                let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                    .expect("PostgreSQL laws require a provisioned database");
                let isolated = IsolatedDatabase::create(&url).await;
                let postgres = PostgresStorage::connect(isolated.url()).await.unwrap();
                let stores = PostgresStoreSet::with_clock(
                    &postgres,
                    Arc::new(lash::persistence::FileAttachmentStore::new(
                        directory.path(),
                    )),
                    Default::default(),
                    clock,
                );
                database = Some(isolated);
                Arc::new(stores)
            }
        };
        Self {
            stores,
            _directory: directory,
            _database: database,
        }
    }
}

pub(super) enum Harness {
    Double(RestateTestBackend<dyn StoreSet>),
    Live(LiveRestateBackend<dyn StoreSet>),
}

impl Harness {
    pub(super) async fn new(storage: Storage, live: bool) -> (Self, Stores) {
        let mut retained = None;
        if live {
            let backend = LiveRestateBackend::start_with_store_set(
                LiveConfig {
                    ingress_url: live_env("RESTATE_INGRESS_URL"),
                    admin_url: live_env("RESTATE_ADMIN_URL"),
                    endpoint_bind: live_env("CW_BIND").parse().unwrap(),
                    endpoint_url: live_env("CW_URL"),
                    run_tag: run_tag("process-root-recovery"),
                    namespace: RestateNamespace::default(),
                },
                |clock| async {
                    let stores = Stores::new(storage, clock).await;
                    let ports = Arc::clone(&stores.stores);
                    retained = Some(stores);
                    Ok(ports)
                },
            )
            .await
            .unwrap();
            (Self::Live(backend), retained.unwrap())
        } else {
            let config = ServerConfig::default().always_replay(
                std::env::var("LASH_RESTATE_TEST_ALWAYS_REPLAY").as_deref() == Ok("1"),
            );
            let backend = lash_restate_test::backend_with_store_set(
                0x4378,
                config,
                Default::default(),
                |clock| async {
                    let stores = Stores::new(storage, clock).await;
                    let ports = Arc::clone(&stores.stores);
                    retained = Some(stores);
                    Ok(ports)
                },
            )
            .await
            .unwrap();
            (Self::Double(backend), retained.unwrap())
        }
    }

    pub(super) fn backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(backend) => backend.lash_backend(),
            Self::Live(backend) => backend.lash_backend(),
        }
    }

    /// The session work whose recovery pass only the law runs: a wall-clock
    /// reconciliation would run it at a time the law does not choose.
    pub(super) fn session_work(&self) -> Arc<dyn lash_core::SessionWorkEngine> {
        match self {
            Self::Double(backend) => backend.explicit_reconcile_session_work(),
            Self::Live(backend) => backend.explicit_reconcile_session_work(),
        }
    }

    fn install_process_worker(&self, worker: lash_core_worker::DurableProcessWorker) {
        match self {
            Self::Double(backend) => backend.install_process_worker(worker),
            Self::Live(backend) => backend.install_process_worker(worker),
        }
    }

    async fn run(&self, scope: lash_core::AdmittedScope, attempt: HandlerAttempt) {
        tokio::time::timeout(BOUND, async {
            match self {
                Self::Double(backend) => backend.run_in_handler(scope, attempt).await,
                Self::Live(backend) => backend.run_in_handler(scope, attempt).await,
            }
        })
        .await
        .expect("the starting handler finishes")
        .unwrap();
    }

    pub(super) async fn finish(self) {
        if let Self::Live(backend) = self {
            backend.finish().await;
            backend.stop_serving(true);
        }
    }
}

/// The child's one model call: it reports that it started, then answers
/// once the law releases it.
pub(super) struct HeldModelCall {
    pub(super) started: tokio::sync::Notify,
    pub(super) release: tokio::sync::Semaphore,
}

impl HeldModelCall {
    pub(super) fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

pub(super) fn core(harness: &Harness, call: Arc<HeldModelCall>) -> lash::LashCore {
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(harness.backend())
        .with_session_work(harness.session_work())
        .into_backend();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("process-root-recovery")
        .complete(move |_| {
            let call = Arc::clone(&call);
            async move {
                call.started.notify_one();
                call.release
                    .acquire()
                    .await
                    .expect("the law releases the model call")
                    .forget();
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "child settled".into(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("process-root-recovery")
                .context_window_tokens(100_000)
                .build()
                .unwrap(),
        )
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "process-root-recovery",
            "test",
        ))
        .unwrap();
    harness.install_process_worker(
        lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap(),
    );
    core
}

/// Start one detached `SessionTurn` child in a host handler and answer its
/// process id. The child runs in `session`, or in a session derived from
/// its process id when none is named.
pub(super) async fn start_child(
    harness: &Harness,
    core: &lash::LashCore,
    session: Option<&lash_core::SessionId>,
    start_key: &str,
) -> ProcessId {
    let mut create_request = lash_core::SessionCreateRequest::root(
        lash_core::SessionStartPoint::Empty,
        Default::default(),
    )
    .with_spec(&lash::SessionSpec::new(
        "process-root-recovery",
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    ))
    .expect("a root spec");
    create_request.session_id = session.cloned();
    let request = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "process-root-recovery-session-turn".into(),
            create_request: Box::new(create_request),
            turn_input: Box::new(lash::TurnInput::text("run the child turn")),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_host_start_key(run_tag(start_key));
    let started = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let started = Arc::clone(&started);
            Box::pin(async move {
                let receipt = core.processes().start(request, scoped).await;
                *started.lock().unwrap() = Some(receipt);
            })
        })
    };
    harness
        .run(
            lash_core::AdmittedScope::runtime_operation(run_tag("starter")),
            attempt,
        )
        .await;
    let receipt = started
        .lock()
        .unwrap()
        .take()
        .expect("the handler ran the start")
        .expect("the child is registered");
    receipt.process_id
}

async fn child_inputs(
    harness: &Harness,
    session: &lash_core::SessionId,
) -> Vec<lash_core::PendingTurnInputRead> {
    harness
        .backend()
        .session_store_factory()
        .list_pending_turn_inputs(session)
        .await
        .expect("read the child session's inputs")
}

/// The recovery pass, run until one pass reads the engine without a
/// failure: a loaded server's admin query can time out, and the pass then
/// ends nothing and is retried, as the recovery interval does.
async fn recovery_passes(
    harness: &Harness,
) -> Vec<Result<lash_core::engine::ParkReconcileReport, lash_core::engine::EngineRefusal>> {
    let sessions = harness.backend().session_store_factory();
    let clock = harness.backend().clock();
    let writer = lash_core::drive::StoreParkRecovery::new(sessions.as_ref(), clock.as_ref());
    let control = harness.session_work().control();
    let mut passes = Vec::new();
    for _ in 0..20 {
        let pass = control
            .reconcile_parks(
                &writer,
                EnginePage {
                    after: None,
                    limit: std::num::NonZeroUsize::new(16).unwrap(),
                    budget: Duration::from_secs(5),
                },
            )
            .await;
        let read = pass.as_ref().is_ok_and(|report| report.failed.is_empty());
        passes.push(pass);
        if read {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        matches!(passes.last(), Some(Ok(report)) if report.failed.is_empty()),
        "a recovery pass read the engine: {passes:?}"
    );
    passes
}

async fn process_root_law(storage: Storage, live: bool) {
    let (harness, _stores) = Harness::new(storage, live).await;
    let call = Arc::new(HeldModelCall::new());
    let core = core(&harness, Arc::clone(&call));
    let process_id = start_child(&harness, &core, None, "process-root-start").await;
    tokio::time::timeout(BOUND, call.started.notified())
        .await
        .expect("the child's turn reaches its model call");

    let target = RootRef {
        session: lash_core::facade_support::process_child_session_id(&process_id),
        root: lash_core::TurnId::fixture(process_id.as_str()),
    };
    let sessions = harness.backend().session_store_factory();
    assert!(
        sessions
            .root_terminal(&target.session, &target.root)
            .await
            .expect("read the child root's terminal")
            .is_none(),
        "the child root is open while its model call is held"
    );
    let admitted = child_inputs(&harness, &target.session).await;
    assert!(
        matches!(
            admitted.as_slice(),
            [row] if row.status == lash_core::PendingTurnInputReadStatus::Admitted {
                root: target.root.clone(),
            }
        ),
        "the child root holds its start input: {admitted:?}"
    );

    let passes = recovery_passes(&harness).await;
    assert!(
        passes.iter().all(|pass| pass
            .as_ref()
            .is_ok_and(|report| !report.ended_roots.contains(&target))),
        "no recovery pass ends a root its live process runs: {passes:?}"
    );
    assert_eq!(
        sessions
            .root_terminal(&target.session, &target.root)
            .await
            .expect("read the child root's terminal"),
        None,
        "the child root stays open for its process"
    );
    let kept = child_inputs(&harness, &target.session).await;
    assert!(
        matches!(
            kept.as_slice(),
            [row] if row.status == lash_core::PendingTurnInputReadStatus::Admitted {
                root: target.root.clone(),
            }
        ),
        "the child root still holds its start input: {kept:?}"
    );

    call.release.add_permits(1);
    let output = tokio::time::timeout(BOUND, core.processes().await_output(&process_id))
        .await
        .expect("the child process reaches its terminal")
        .expect("read the child's terminal");
    assert!(
        matches!(
            &output,
            lash_core::ProcessAwaitOutput::Settled { output }
                if matches!(output.outcome, lash_core::ToolCallOutcome::Success(_))
        ),
        "the child process completes: {output:?}"
    );
    let terminal = sessions
        .root_terminal(&target.session, &target.root)
        .await
        .expect("read the child root's terminal")
        .expect("the child root has its terminal");
    assert_eq!(
        terminal.kind(),
        lash_core::store::RootTerminalKind::Answered,
        "the child root answers with its committed turn: {terminal:?}"
    );
    let settled = child_inputs(&harness, &target.session).await;
    assert!(
        settled.is_empty(),
        "the start input settles with the root's commit: {settled:?}"
    );
    drop(core);
    harness.finish().await;
}

/// The ahead law: a process that runs its turn in an explicit session the
/// host created drives, inline in its own run, the root of a row queued
/// there before its own; the recovery pass leaves that root to the process
/// while it runs it.
async fn ahead_root_law(storage: Storage, live: bool) {
    let (harness, _stores) = Harness::new(storage, live).await;
    let call = Arc::new(HeldModelCall::new());
    let core = core(&harness, Arc::clone(&call));
    let session = lash_core::SessionId::fixture(run_tag("process-root-ahead"));
    let sessions = harness.backend().session_store_factory();

    // The host creates the explicit session; no turn runs in it yet, so no
    // execution has bound its turn cancellation.
    core.session(session.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "process-root-recovery",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create the explicit session");

    // A row queued in the session through the store alone: no drive is
    // asked for it, so the process's drive admits it ahead of its own.
    let target = RootRef {
        session: session.clone(),
        root: lash_core::TurnId::fixture(run_tag("ahead-turn")),
    };
    let ahead = sessions
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session.clone(),
                lash_core::TurnInputIngress::next_turn(),
                lash::TurnInput::text("queued ahead of the process"),
            )
            .with_source_key(target.root.as_str()),
        )
        .await
        .expect("queue the row ahead")
        .input_id;
    let process_id = start_child(&harness, &core, Some(&session), "process-root-ahead").await;
    tokio::time::timeout(BOUND, call.started.notified())
        .await
        .expect("the root admitted ahead reaches its model call");
    assert!(
        sessions
            .root_terminal(&target.session, &target.root)
            .await
            .expect("read the ahead root's terminal")
            .is_none(),
        "the root admitted ahead is open while its model call is held"
    );
    let admitted_ahead = |rows: &[lash_core::PendingTurnInputRead]| {
        rows.iter().any(|row| {
            row.input.input_id == ahead
                && row.status
                    == lash_core::PendingTurnInputReadStatus::Admitted {
                        root: target.root.clone(),
                    }
        })
    };
    let admitted = child_inputs(&harness, &session).await;
    assert!(
        admitted_ahead(&admitted),
        "the process's drive admitted the queued row to its own root: {admitted:?}"
    );

    let passes = recovery_passes(&harness).await;
    assert!(
        passes.iter().all(|pass| pass
            .as_ref()
            .is_ok_and(|report| !report.ended_roots.contains(&target))),
        "no recovery pass ends a root its live process admitted ahead of its own: {passes:?}"
    );
    assert_eq!(
        sessions
            .root_terminal(&target.session, &target.root)
            .await
            .expect("read the ahead root's terminal"),
        None,
        "the root admitted ahead stays open for its process"
    );
    let kept = child_inputs(&harness, &session).await;
    assert!(
        admitted_ahead(&kept),
        "the root admitted ahead still holds its input: {kept:?}"
    );

    call.release.add_permits(2);
    let output = tokio::time::timeout(BOUND, core.processes().await_output(&process_id))
        .await
        .expect("the child process reaches its terminal")
        .expect("read the child's terminal");
    assert!(
        matches!(
            &output,
            lash_core::ProcessAwaitOutput::Settled { output }
                if matches!(output.outcome, lash_core::ToolCallOutcome::Success(_))
        ),
        "the child process completes: {output:?}"
    );
    let terminal = sessions
        .root_terminal(&target.session, &target.root)
        .await
        .expect("read the ahead root's terminal")
        .expect("the root admitted ahead has its terminal");
    assert_eq!(
        terminal.kind(),
        lash_core::store::RootTerminalKind::Answered,
        "the root admitted ahead answers with its committed turn: {terminal:?}"
    );
    let settled = child_inputs(&harness, &session).await;
    assert!(
        settled.is_empty(),
        "every row settles with the root that drove it: {settled:?}"
    );
    drop(core);
    harness.finish().await;
}

macro_rules! laws {
    ($module:ident, $storage:expr $(, $service:literal)?) => {
        mod $module {
            use super::*;
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_recovery_pass_keeps_the_child_root_its_live_process_runs() {
                process_root_law($storage, false).await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "live Restate; crash-windows suite"]
            async fn live_restate_a_recovery_pass_keeps_the_child_root_its_live_process_runs() {
                process_root_law($storage, true).await;
            }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_recovery_pass_keeps_a_root_its_live_process_admitted_ahead_of_its_own() {
                ahead_root_law($storage, false).await;
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "live Restate; crash-windows suite"]
            async fn live_restate_a_recovery_pass_keeps_a_root_its_live_process_admitted_ahead_of_its_own()
            {
                ahead_root_law($storage, true).await;
            }
        }
    };
}

laws!(sqlite_memory, Storage::Memory);
laws!(sqlite_file, Storage::File);
laws!(
    postgres,
    Storage::Postgres,
    "requires PostgreSQL; run through the pg16 service gate"
);
