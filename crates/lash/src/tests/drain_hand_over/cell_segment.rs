//! FIG-4739: a run parked inside a code cell on a draining build hands over
//! there and resumes the cell, mid-cell, on the newest build.
//!
//! Each law sends one input whose run asks the model once. The model answers
//! with a cell that binds a value, starts a process that waits for a signal,
//! and awaits the process's handle: the cell's worker parks there, and the
//! run's turn is parked on build N on a wait nothing answers. Build N+1
//! registers, an operator marks N draining, and the recovery leader's tick
//! wakes the turn's wait.
//!
//! - **hand-over**: the cell stops at the await holding everything it held,
//!   N's turn commits at that boundary owing the run's continuation, and N
//!   holds none of the run while the process is still waiting. N+1 admits
//!   the continuation, which issues the cell again: the cell resumes at the
//!   await, and when the process is signalled it runs on from the binding it
//!   made before it stopped. The model is asked once and the process is
//!   started once.
//! - **crash**: the hand-over law again, with one invocation dying once on
//!   either side of a durable record of the hand-over: N's root while it is
//!   parked and before the wake, after the wake and before its boundary
//!   commit is recorded, and after that commit; N+1's continuation before
//!   its first step, while it is parked inside the resumed cell, and after
//!   its final commit.
//! - **cancel**: a cancel that lands after the hand-over, while the
//!   continuation is parked inside the resumed cell, ends the run.
//!
//! These run on the Restate server double over SQLite memory, SQLite file
//! and PostgreSQL. The PostgreSQL legs are ignored in ordinary runs and
//! require `LASH_POSTGRES_DATABASE_URL`.

use super::*;

/// The run's cell. `before` is bound before the cell stops and read after
/// it resumes; the process waits for `signal`.
fn cell(signal: &str) -> String {
    typescript_block(&format!(
        r#"const worker = async () => {{ await waitSignal("{signal}"); return "done"; }};
let before = 20;
const handle = await processes.start({{ definition: worker }});
const answer = await handle;
before = before + 22;
finish({{ answer, before }});"#
    ))
}

fn cell_provider(
    signal: &str,
    requests: &Arc<std::sync::Mutex<Vec<LlmRequest>>>,
) -> ProviderHandle {
    let requests = Arc::clone(requests);
    let cell = cell(signal);
    crate::testing::TestProvider::builder()
        .kind("cell-segment")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let cell = cell.clone();
            async move {
                let call = {
                    let mut requests = requests.lock_recover();
                    requests.push(request);
                    requests.len()
                };
                Ok(text_response(&if call == 1 {
                    cell
                } else {
                    typescript_block(r#"finish("asked again");"#)
                }))
            }
        })
        .build()
        .into_handle()
}

fn cell_core(
    backend: lash_core::Backend,
    work: Arc<dyn lash_core::SessionWorkEngine>,
    signal: &str,
    requests: &Arc<std::sync::Mutex<Vec<LlmRequest>>>,
) -> LashCore {
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
        .with_session_work(work)
        .into_backend();
    rlm_core_builder_over(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(cell_provider(signal, requests), mock_llm_profile_spec())
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .plugin(lash_core::testing::process_engine_plugin_fixture())
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core")
}

/// A run parked inside its cell on build N, with N+1 registered and N marked
/// draining.
struct CellRoll {
    engine: Engine,
    core: LashCore,
    requests: Arc<std::sync::Mutex<Vec<LlmRequest>>>,
    session: lash_core::SessionId,
    handle: Option<crate::SendHandle>,
    process: lash_core::ProcessId,
    signal: String,
    old: BuildGeneration,
    next: BuildGeneration,
    _keep: Keep,
}

/// The continuation's recovery root, at the recovery count its drive
/// admission recorded.
const CONTINUATION: &str = "follow-on:run-root:agent-frame:1#0";

impl CellRoll {
    async fn start(storage: Storage, session: &str) -> Result<Self> {
        let World { engine, _keep, .. } = double_world(storage).await;
        let Engine::Double(double) = &engine else {
            unreachable!("the law runs on the double");
        };
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let signal = format!("go-{session}");
        let old = engine
            .old_backend()
            .build_generation()
            .expect("the engine's generation is bound")
            .clone();
        let core = cell_core(engine.old_backend(), engine.old_work(), &signal, &requests);
        double.install_process_worker(
            lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config()?)
                .expect("the core's process worker"),
        );
        let session = lash_core::SessionId::fixture(session);
        let handle = core
            .session(session.clone())
            .created()
            .await
            .open()
            .await?
            .send(TurnInput::text("start the job and wait for it"))
            .id("run-root")
            .await?;
        let process = waiting_process(&core, &signal).await;
        let next = BuildGeneration::for_test("cell-segment-next");
        engine
            .roll(next.clone(), &Arc::new(Model::holding(0)))
            .await;
        assert!(
            engine
                .old_backend()
                .generation_drain()
                .mark_draining(&old, 1)
                .await
                .expect("mark build N draining"),
            "the law's mark is N's first"
        );
        Ok(Self {
            engine,
            core,
            requests,
            session,
            handle: Some(handle),
            process,
            signal,
            old,
            next,
            _keep,
        })
    }

    fn server(&self) -> &lash_restate_test::RestateTestServer {
        let Engine::Double(double) = &self.engine else {
            unreachable!("the law runs on the double");
        };
        double.server()
    }

    fn sent(&mut self) -> crate::SendHandle {
        self.handle.take().expect("the run's send handle")
    }

    async fn in_flight(&self, generation: &BuildGeneration) -> u64 {
        self.engine
            .old_backend()
            .generation_drain()
            .generation_work(generation)
            .await
            .expect("read the generation's work")
            .in_flight_turns
    }

    /// The root invocation whose key ends with `key`, once it is parked on
    /// the cell's await: the process's terminal has `attaches` waits armed
    /// on it, and the root waits on the server with a journal that has
    /// stopped growing.
    async fn parked_root(&self, key: &str, attaches: usize) -> lash_restate_test::InvocationView {
        let target = format!("{key}/run");
        let deadline = tokio::time::Instant::now() + WEDGE;
        let mut seen = None;
        loop {
            let invocations = self.server().invocations();
            let armed = invocations
                .iter()
                .filter(|view| view.target.starts_with("LashProcessAttach/"))
                .count();
            let parked = invocations.into_iter().find(|view| {
                view.target.ends_with(&target)
                    && view.target.contains(lash_restate_test::TURN_DRIVER_SERVICE)
                    && view.status == "running"
                    && view.blocked_on_server == Some(true)
            });
            if let Some(view) = parked.filter(|_| armed >= attaches) {
                if seen == Some(view.journal_len) {
                    return view;
                }
                seen = Some(view.journal_len);
            } else {
                seen = None;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the root `{key}` never parked: {:#?}",
                self.server().invocations()
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Run the recovery leader's tick until the run counts in N+1 and N
    /// holds none of it: the tick wakes the parked turn, and the turn's
    /// boundary commit and the continuation's admission follow it.
    async fn hand_over(&self) -> Result<()> {
        let driver = Arc::clone(&self.core._session_driver);
        let page = std::num::NonZeroUsize::new(16).expect("non-zero page");
        let deadline = tokio::time::Instant::now() + WEDGE;
        loop {
            driver
                .reconcile(&lash_core::engine::ReconcileCursor::default(), page)
                .await?;
            if (
                self.in_flight(&self.old).await,
                self.in_flight(&self.next).await,
            ) == (0, 1)
            {
                return Ok(());
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the parked run never handed over: {:#?}",
                self.server().invocations()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Signal the cell's process, which ends it.
    async fn release_process(&self) -> Result<()> {
        let Engine::Double(double) = &self.engine else {
            unreachable!("the law runs on the double");
        };
        // The host's signal enters a handler of its own, as a host
        // operation on a Restate deployment does.
        let handler = double
            .open_handler(lash_core::AdmittedScope::runtime_operation(
                "cell-segment-release",
            ))
            .await
            .unwrap_or_else(|error| panic!("open the release's handler: {error}"));
        self.core
            .processes()
            .signal(
                lash_core::ProcessSignal::new(
                    lash_core::ProcessSignalIdentity::new(
                        self.process.clone(),
                        self.signal.clone(),
                        "cell-segment-release",
                    )
                    .expect("valid signal identity"),
                    serde_json::Value::Null,
                ),
                handler.scoped(),
            )
            .await?;
        handler
            .close()
            .await
            .unwrap_or_else(|error| panic!("close the release's handler: {error}"));
        Ok(())
    }

    /// Every process the session's cells started.
    async fn started(&self) -> usize {
        self.engine
            .old_backend()
            .process_registry()
            .list_observed_by(
                &self.session,
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await
            .expect("list the session's processes")
            .len()
    }

    /// The run left nothing behind: the head owes no continuation, no cell
    /// is suspended, and the draining build holds nothing.
    async fn assert_ended(&self) -> Result<()> {
        let store = lash_core::runtime::live_session_view(&self.core.store_factory, &self.session)
            .await?
            .expect("an opened session has a store");
        assert!(
            store.load_pending_follow_on().await?.is_none(),
            "the run's last commit left nothing owed"
        );
        assert_eq!(
            (
                self.in_flight(&self.old).await,
                self.in_flight(&self.next).await
            ),
            (0, 0),
            "no build holds the run"
        );
        assert!(
            self.core
                .generation_drain_status(&self.old)
                .await?
                .drained(),
            "N's drain is complete"
        );
        Ok(())
    }
}

/// The process waiting on `signal`, once the cell started it and it reached
/// its wait.
async fn waiting_process(core: &LashCore, signal: &str) -> lash_core::ProcessId {
    let deadline = tokio::time::Instant::now() + WEDGE;
    loop {
        let waiting = core
            .processes()
            .list(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("list the processes")
            .into_iter()
            .find(|process| {
                matches!(
                    process.wait.as_ref().map(|wait| &wait.kind),
                    Some(lash_core::WaitKind::Signal { name, .. }) if name == signal
                )
            });
        if let Some(process) = waiting {
            return process.process_id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cell's process never reached its signal wait"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Where an invocation of the mid-cell hand-over dies, once.
#[derive(Clone, Copy, Debug)]
enum Crash {
    /// N's root dies while it is parked inside the cell, before the drain
    /// wakes it: its replay runs the cell to the same await and parks again.
    OldRootWhileParked,
    /// N's root dies after the wake is journaled, before the first command
    /// that follows it: its replay stops the cell at the same await and
    /// commits the boundary.
    OldRootAfterTheWake,
    /// N's root dies after its boundary commit, with its outcome unrecorded.
    OldRootAfterItsBoundaryCommit,
    /// N+1's continuation root dies before its first step is recorded.
    ContinuationBeforeItsFirstStep,
    /// N+1's continuation root dies while it is parked inside the resumed
    /// cell: its replay resumes the cell again from the committed capture.
    ContinuationWhileParked,
    /// N+1's continuation root dies after its final commit, with its outcome
    /// unrecorded.
    ContinuationAfterItsCommit,
}

impl Crash {
    /// The crash the server plans for this point, if it is a planned one.
    /// `parked_commands` is the number of commands N's root journaled before
    /// it parked.
    fn rule(self, parked_commands: usize) -> Option<lash_restate_test::CrashRule> {
        use lash_restate_test::protocol::MessageType;
        use lash_restate_test::{CrashPoint, CrashRule, TURN_DRIVER_SERVICE};
        let root_run = |point, key: &str| {
            CrashRule::new(point)
                .service(TURN_DRIVER_SERVICE)
                .handler("run")
                .key_ending(key)
        };
        let before_output = || CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        };
        match self {
            Self::OldRootWhileParked | Self::ContinuationWhileParked => None,
            Self::OldRootAfterTheWake => Some(root_run(
                CrashPoint::BeforeCommand {
                    index: parked_commands,
                },
                "run-root",
            )),
            Self::OldRootAfterItsBoundaryCommit => Some(root_run(before_output(), "run-root")),
            Self::ContinuationBeforeItsFirstStep => Some(root_run(
                CrashPoint::BeforeRunResult { name: None },
                CONTINUATION,
            )),
            Self::ContinuationAfterItsCommit => Some(root_run(before_output(), CONTINUATION)),
        }
    }
}

async fn a_run_parked_inside_a_cell_hands_over_and_resumes_mid_cell(
    storage: Storage,
    crash: Option<Crash>,
) -> Result<()> {
    let session = match crash {
        Some(crash) => format!("cell-segment-{crash:?}"),
        None => "cell-segment-hand-over".to_owned(),
    };
    let mut roll = CellRoll::start(storage, &session).await?;
    let parked = roll.parked_root("run-root", 1).await;
    let crashes = lash_restate_test::CrashCount::new();
    assert!(
        roll.server().on_crash(crashes.listener()),
        "the law's crash listener is the engine's only one"
    );
    match crash {
        Some(Crash::OldRootWhileParked) => {
            assert!(roll.server().crash(&parked.id), "crash N's parked root");
            roll.parked_root("run-root", 1).await;
        }
        Some(crash) => {
            let parked_commands = roll
                .server()
                .journal(&parked.id)
                .expect("the parked root's journal")
                .iter()
                .filter(|entry| entry.ty.is_command())
                .count();
            if let Some(rule) = crash.rule(parked_commands) {
                roll.server().crash_on(rule);
            }
        }
        None => {}
    }

    roll.hand_over().await?;
    // N holds none of the run while the cell is still waiting on its
    // process, and the model was not asked for the cell again.
    assert_eq!(roll.started().await, 1, "the cell started its process once");
    assert_eq!(roll.requests.lock_recover().len(), 1);
    let resumed = roll.parked_root(CONTINUATION, 1).await;
    if matches!(crash, Some(Crash::ContinuationWhileParked)) {
        assert!(
            roll.server().crash(&resumed.id),
            "crash the continuation's parked root"
        );
        roll.parked_root(CONTINUATION, 1).await;
    }

    roll.release_process().await?;
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::json!({ "answer": "done", "before": 42 }),
        }),
        "the resumed cell runs on from the binding it made before it stopped"
    );
    assert_eq!(
        roll.requests.lock_recover().len(),
        1,
        "the model was asked once: the cell was issued again, never asked for again"
    );
    assert_eq!(
        roll.started().await,
        1,
        "the start the first invocation dispatched is never dispatched again"
    );
    if crash.is_some() {
        assert_eq!(crashes.get(), 1, "the invocation died once");
    }
    roll.assert_ended().await
}

/// A cancel of the run that lands after the hand-over, while its
/// continuation is parked inside the resumed cell on the newest build, ends
/// the run: the cancellation is the logical run's, wherever its cell waits.
async fn a_cancel_after_the_hand_over_reaches_the_resumed_cell(
    storage: Storage,
    (): (),
) -> Result<()> {
    let mut roll = CellRoll::start(storage, "cell-segment-cancel").await?;
    roll.parked_root("run-root", 1).await;
    roll.hand_over().await?;
    roll.parked_root(CONTINUATION, 1).await;
    let receipt = roll
        .handle
        .as_ref()
        .expect("the run's send handle")
        .cancel()
        .origin("cell-segment-law")
        .await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { root, .. } if root.as_str() == "run-root"),
        "the cancel reaches the running root: {receipt:?}"
    );
    let outcome = tokio::time::timeout(WEDGE, roll.sent().outcome())
        .await
        .expect("the cancelled run answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        outcome.root().map(lash_core::TurnId::as_str),
        Some("run-root")
    );
    assert_eq!(roll.requests.lock_recover().len(), 1);
    assert_eq!(roll.started().await, 1);
    Ok(())
}

async fn hands_over(storage: Storage, crash: Option<Crash>) -> Result<()> {
    a_run_parked_inside_a_cell_hands_over_and_resumes_mid_cell(storage, crash).await
}

macro_rules! cell_segment_laws {
    ($($(#[$attr:meta])* $name:ident: $run:ident, $storage:expr, $arg:expr;)*) => {
        $(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                $run($storage, $arg).await
            }
        )*
    };
}

cell_segment_laws! {
    cell_hands_over_sqlite_memory: hands_over, Storage::SqliteMemory, None;
    cell_hands_over_sqlite_file: hands_over, Storage::SqliteFile, None;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_hands_over_postgres: hands_over, Storage::Postgres, None;
    cell_crash_old_root_while_parked_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRootWhileParked);
    cell_crash_old_root_while_parked_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRootWhileParked);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_old_root_while_parked_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRootWhileParked);
    cell_crash_old_root_after_the_wake_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRootAfterTheWake);
    cell_crash_old_root_after_the_wake_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRootAfterTheWake);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_old_root_after_the_wake_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRootAfterTheWake);
    cell_crash_old_root_after_boundary_commit_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRootAfterItsBoundaryCommit);
    cell_crash_old_root_after_boundary_commit_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRootAfterItsBoundaryCommit);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_old_root_after_boundary_commit_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRootAfterItsBoundaryCommit);
    cell_crash_continuation_before_first_step_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::ContinuationBeforeItsFirstStep);
    cell_crash_continuation_before_first_step_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::ContinuationBeforeItsFirstStep);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_continuation_before_first_step_postgres:
        hands_over, Storage::Postgres, Some(Crash::ContinuationBeforeItsFirstStep);
    cell_crash_continuation_while_parked_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::ContinuationWhileParked);
    cell_crash_continuation_while_parked_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::ContinuationWhileParked);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_continuation_while_parked_postgres:
        hands_over, Storage::Postgres, Some(Crash::ContinuationWhileParked);
    cell_crash_continuation_after_commit_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::ContinuationAfterItsCommit);
    cell_crash_continuation_after_commit_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::ContinuationAfterItsCommit);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_continuation_after_commit_postgres:
        hands_over, Storage::Postgres, Some(Crash::ContinuationAfterItsCommit);
    cell_cancel_after_sqlite_memory:
        a_cancel_after_the_hand_over_reaches_the_resumed_cell, Storage::SqliteMemory, ();
    cell_cancel_after_sqlite_file:
        a_cancel_after_the_hand_over_reaches_the_resumed_cell, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_cancel_after_postgres:
        a_cancel_after_the_hand_over_reaches_the_resumed_cell, Storage::Postgres, ();
}
