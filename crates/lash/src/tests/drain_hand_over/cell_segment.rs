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
//!   either side of a durable record of the hand-over: N's run while it is
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

#[path = "opener_groups.rs"]
mod opener_groups;

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
    cell: String,
    requests: &Arc<std::sync::Mutex<Vec<LlmRequest>>>,
) -> ProviderHandle {
    let requests = Arc::clone(requests);
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
                Ok(text_response(&if call <= 2 {
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
    cell: String,
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
        .serve_test_llm_profile(cell_provider(cell, requests), mock_llm_profile_spec())
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
    script: Arc<lash_core::testing::Script>,
}

/// The continuation's recovery run, at the recovery count its shift
/// admission recorded.
const CONTINUATION: &str = "follow-on:run-run:agent-frame:1#0";

impl CellRoll {
    async fn start(storage: Storage, session: &str) -> Result<Self> {
        Self::start_with_cell(storage, session, cell(&format!("go-{session}"))).await
    }

    async fn start_with_cell(storage: Storage, session: &str, code: String) -> Result<Self> {
        Self::start_with_cell_setup(storage, session, code, |_| {}).await
    }

    async fn start_with_cell_setup(
        storage: Storage,
        session: &str,
        code: String,
        setup: impl FnOnce(&lash_restate_test::RestateTestServer),
    ) -> Result<Self> {
        let World { engine, _keep, .. } = double_world(storage).await;
        let Engine::Double(double) = &engine else {
            unreachable!("the law runs on the double");
        };
        setup(double.server());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let signal = format!("go-{session}");
        let old = engine
            .old_backend()
            .build_generation()
            .expect("the engine's generation is bound")
            .clone();
        let script = Arc::new(lash_core::testing::Script::new());
        let scripted = Arc::clone(&script);
        let backend =
            lash_core::testing::runtime_helpers::LayeredBackend::over(engine.old_backend())
                .map_session_store_factory(move |inner| scripted.wrap("cell-run", inner))
                .into_backend();
        let core = cell_core(backend, engine.old_work(), code, &requests);
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
            .id("run-run")
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
            script,
        })
    }

    async fn start_external(storage: Storage, label: &str) -> Result<Self> {
        let World { engine, _keep, .. } = double_world(storage).await;
        let session = lash_core::SessionId::fixture(label);
        let process = engine
            .old_backend()
            .process_registry()
            .register_process_with_observers(
                lash_core::ProcessRegistration::new(
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                ),
                std::slice::from_ref(&session),
            )
            .await?
            .id;
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let source = typescript_block(
            r#"let before = 20;
const handles = await processes.list({});
const answer = await handles[0];
before = before + 22;
finish({ answer, before });"#,
        );
        let old = engine
            .old_backend()
            .build_generation()
            .expect("a bound build")
            .clone();
        let core = cell_core(engine.old_backend(), engine.old_work(), source, &requests);
        let handle = core
            .session(session.clone())
            .created()
            .await
            .open()
            .await?
            .send(TurnInput::text("await the external job"))
            .id("run-run")
            .await?;
        let next = BuildGeneration::for_test("cell-external-next");
        // Register N+1 after the caller has armed its subscription on N.
        let roll = Self {
            engine,
            core,
            requests,
            session,
            handle: Some(handle),
            process,
            signal: String::new(),
            old,
            next,
            _keep,
            script: Arc::new(lash_core::testing::Script::new()),
        };
        roll.parked_run("run-run", 1).await;
        roll.engine
            .roll(roll.next.clone(), &Arc::new(Model::holding(0)))
            .await;
        assert!(
            roll.engine
                .old_backend()
                .generation_drain()
                .mark_draining(&roll.old, 1)
                .await?
        );
        Ok(roll)
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

    /// The run invocation whose key ends with `key`, once it is parked on
    /// the cell's await: the process's terminal has `attaches` waits armed
    /// on it, and the run waits on the server with a journal that has
    /// stopped growing.
    async fn parked_run(&self, key: &str, attaches: usize) -> lash_restate_test::InvocationView {
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
                "the run `{key}` never parked: {:#?}",
                self.server().invocations()
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Run the recovery leader's tick until the run counts in N+1 and N
    /// holds none of it: the tick wakes the parked turn, and the turn's
    /// boundary commit and the continuation's admission follow it.
    async fn hand_over(&self) -> Result<()> {
        let driver = Arc::clone(&self.core._session_shifts);
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
    /// N's run dies while it is parked inside the cell, before the drain
    /// wakes it: its replay runs the cell to the same await and parks again.
    OldRunWhileParked,
    /// N's run dies after the wake is journaled, before the first command
    /// that follows it: its replay stops the cell at the same await and
    /// commits the boundary.
    OldRunAfterTheWake,
    /// N's run dies after its boundary commit, with its outcome unrecorded.
    OldRunAfterItsBoundaryCommit,
    /// N+1's continuation run dies before its first step is recorded.
    ContinuationBeforeItsFirstStep,
    /// N+1's continuation run dies while it is parked inside the resumed
    /// cell: its replay resumes the cell again from the committed capture.
    ContinuationWhileParked,
    /// N+1's continuation run dies after its final commit, with its outcome
    /// unrecorded.
    ContinuationAfterItsCommit,
}

impl Crash {
    /// The crash the server plans for this point, if it is a planned one.
    /// `parked_commands` is the number of commands N's run journaled before
    /// it parked.
    fn rule(self, parked_commands: usize) -> Option<lash_restate_test::CrashRule> {
        use lash_restate_test::protocol::MessageType;
        use lash_restate_test::{CrashPoint, CrashRule, TURN_DRIVER_SERVICE};
        let run_execution = |point, key: &str| {
            CrashRule::new(point)
                .service(TURN_DRIVER_SERVICE)
                .handler("run")
                .key_ending(key)
        };
        let before_output = || CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        };
        match self {
            Self::OldRunWhileParked | Self::ContinuationWhileParked => None,
            Self::OldRunAfterTheWake => Some(run_execution(
                CrashPoint::BeforeCommand {
                    index: parked_commands,
                },
                "run-run",
            )),
            Self::OldRunAfterItsBoundaryCommit => Some(run_execution(before_output(), "run-run")),
            Self::ContinuationBeforeItsFirstStep => Some(run_execution(
                CrashPoint::BeforeRunResult { name: None },
                CONTINUATION,
            )),
            Self::ContinuationAfterItsCommit => Some(run_execution(before_output(), CONTINUATION)),
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
    let parked = roll.parked_run("run-run", 1).await;
    let crashes = lash_restate_test::CrashCount::new();
    assert!(
        roll.server().on_crash(crashes.listener()),
        "the law's crash listener is the engine's only one"
    );
    match crash {
        Some(Crash::OldRunWhileParked) => {
            assert!(roll.server().crash(&parked.id), "crash N's parked run");
            roll.parked_run("run-run", 1).await;
        }
        Some(crash) => {
            let parked_commands = roll
                .server()
                .journal(&parked.id)
                .expect("the parked run's journal")
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
    let resumed = roll.parked_run(CONTINUATION, 1).await;
    if matches!(crash, Some(Crash::ContinuationWhileParked)) {
        assert!(
            roll.server().crash(&resumed.id),
            "crash the continuation's parked run"
        );
        roll.parked_run(CONTINUATION, 1).await;
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

/// Cancellation after capture and cancellation in the successor both end
/// continuation ownership. Store gates fix each crash window precisely.
#[derive(Clone, Copy, Debug)]
enum CancelAt {
    Captured,
    Successor,
}

#[derive(Clone, Copy, Debug)]
enum CancelCrash {
    BeforeAuthorization,
    AfterAuthorization,
    BeforeCommit,
    AfterCommit,
}

async fn cancellation_discards_the_cell(
    storage: Storage,
    (at, crash): (CancelAt, Option<CancelCrash>),
) -> Result<()> {
    use lash_core::testing::StoreOp;
    let crash_name = crash.map_or_else(|| "None".to_owned(), |point| format!("{point:?}"));
    let session = format!("cell-cancel-{at:?}-{crash_name}");
    let mut roll = CellRoll::start(storage, &session).await?;
    let old_run = roll.parked_run("run-run", 1).await;
    let capture_gate = matches!(at, CancelAt::Captured).then(|| {
        roll.script
            .on(StoreOp::authorize_turn_cancel_closure)
            .nth(roll.script.calls(StoreOp::authorize_turn_cancel_closure) + 1)
            .before()
            .pause()
    });
    let invocation = if let Some(gate) = &capture_gate {
        let mut wake = Box::pin(async {
            loop {
                roll.core
                    ._session_shifts
                    .reconcile(
                        &lash_core::engine::ReconcileCursor::default(),
                        std::num::NonZeroUsize::new(16).expect("non-zero page"),
                    )
                    .await
                    .expect("wake the cell for handover");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
        gate.reached_by(&mut wake, 1).await;
        old_run
    } else {
        roll.hand_over().await?;
        roll.parked_run(CONTINUATION, 1).await
    };
    let settlement_gate = capture_gate.clone().unwrap_or_else(|| {
        roll.script
            .on(StoreOp::authorize_turn_cancel_closure)
            .nth(roll.script.calls(StoreOp::authorize_turn_cancel_closure) + 1)
            .before()
            .pause()
    });
    let receipt = roll
        .handle
        .as_ref()
        .expect("the Run's send handle")
        .cancel()
        .origin("cell-segment-law")
        .await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { run, .. } if run.as_str() == "run-run"),
        "the cancel reaches the logical Run: {receipt:?}"
    );
    settlement_gate.reached(1).await;
    let crashes = lash_restate_test::CrashCount::new();
    assert!(roll.server().on_crash(crashes.listener()));
    let crash_gate = crash.map(|point| {
        if matches!(point, CancelCrash::BeforeAuthorization) {
            return Arc::clone(&settlement_gate);
        }
        let (op, after) = match point {
            CancelCrash::BeforeAuthorization => (StoreOp::authorize_turn_cancel_closure, false),
            CancelCrash::AfterAuthorization => (StoreOp::authorize_turn_cancel_closure, true),
            CancelCrash::BeforeCommit => (StoreOp::commit_runtime_state, false),
            CancelCrash::AfterCommit => (StoreOp::commit_runtime_state, true),
        };
        // The capture gate intercepted the first authorization. Its stale
        // observed intent is refused, and the retry authorizes the cancel.
        let next = roll.script.calls(op)
            + usize::from(
                !matches!(point, CancelCrash::AfterAuthorization) || capture_gate.is_some(),
            );
        let rule = roll.script.on(op).nth(next);
        if after {
            rule.after().pause()
        } else {
            rule.before().pause()
        }
    });
    if !matches!(crash, Some(CancelCrash::BeforeAuthorization)) {
        settlement_gate.open_all();
    }
    if let Some(gate) = crash_gate {
        gate.reached(1).await;
        assert!(
            roll.server().crash(&invocation.id),
            "crash the cancelling Run at {crash:?}"
        );
        gate.open_all();
    }
    let outcome = tokio::time::timeout(WEDGE, roll.sent().outcome())
        .await
        .expect("the cancelled Run answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        outcome.run().map(lash_core::TurnId::as_str),
        Some("run-run")
    );
    assert_eq!(roll.requests.lock_recover().len(), 1);
    assert_eq!(roll.started().await, 1);
    assert_eq!(
        crashes.get(),
        u64::from(crash.is_some()),
        "exactly the planned crash occurred"
    );
    let store = lash_core::runtime::live_session_view(&roll.core.store_factory, &roll.session)
        .await?
        .expect("the cancelled session has a store");
    let terminal = store
        .run_terminal(&lash_core::TurnId::fixture("run-run"))
        .await?
        .expect("cancellation has durable logical-Run terminal evidence");
    assert_eq!(
        terminal.kind(),
        lash_core::store::RunTerminalKind::Cancelled
    );
    assert!(
        store.load_pending_follow_on().await?.is_none(),
        "no continuation is owed"
    );
    let head = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the cancelled session has a head");
    let execution = head
        .state
        .execution_state_hydration()?
        .expect("the cancelled session retains its ordinary globals");
    #[derive(serde::Deserialize)]
    struct SuspensionProbe {
        suspended_cell: Option<serde::de::IgnoredAny>,
    }
    let root: SuspensionProbe =
        rmp_serde::from_slice(&execution.root).expect("decode the persisted RLM root");
    assert!(
        root.suspended_cell.is_none(),
        "terminal cancellation must discard the suspended cell"
    );

    // An independent deployment has no resident interpreter or SessionShifts
    // from the cancelled Run. It reopens the durable session on a fresh engine.
    let Engine::Double(double) = &roll.engine else {
        unreachable!("the law runs on the double");
    };
    let fresh_build = double
        .add_separate_build(
            roll.next.clone(),
            "reopened",
            lash_restate_test::DeploymentHooks::default(),
        )
        .await
        .expect("start a fresh deployment over the persisted session");
    let reopened = cell_core(
        fresh_build.lash_backend(),
        fresh_build.explicit_reconcile_session_work(),
        cell(&roll.signal),
        &roll.requests,
    );
    fresh_build.processes().install(
        lash_core_worker::DurableProcessWorker::new(reopened.durable_process_worker_config()?)
            .expect("the reopened deployment's process worker"),
    );
    let fresh = reopened
        .session(roll.session.clone())
        .open()
        .await?
        .send(TurnInput::text("start the same cell in a new Run"))
        .id("fresh-run")
        .await?;
    let deadline = tokio::time::Instant::now() + WEDGE;
    loop {
        if let Some(process) = reopened
            .processes()
            .list(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await?
            .into_iter()
            .find(|process| {
                process.process_id != roll.process
                    && matches!(
                        process.wait.as_ref().map(|wait| &wait.kind),
                        Some(lash_core::WaitKind::Signal { name, .. }) if name == &roll.signal
                    )
            })
        {
            roll.process = process.process_id;
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "identical source did not execute its process-start prefix again"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        roll.started().await,
        2,
        "the new Run starts its own process"
    );
    roll.release_process().await?;
    let output = tokio::time::timeout(WEDGE, fresh.output())
        .await
        .expect("the fresh Run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::json!({ "answer": "done", "before": 42 }),
        }),
        "the fresh Run executes its own bindings and prefix"
    );
    assert_eq!(
        roll.requests.lock_recover().len(),
        2,
        "each Run asked for the identical cell once"
    );
    Ok(())
}

async fn a_cancel_after_the_hand_over_reaches_the_resumed_cell(
    storage: Storage,
    (): (),
) -> Result<()> {
    cancellation_discards_the_cell(storage, (CancelAt::Successor, None)).await
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
    cell_crash_old_run_while_parked_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRunWhileParked);
    cell_crash_old_run_while_parked_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRunWhileParked);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_old_run_while_parked_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRunWhileParked);
    cell_crash_old_run_after_the_wake_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRunAfterTheWake);
    cell_crash_old_run_after_the_wake_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRunAfterTheWake);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_old_run_after_the_wake_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRunAfterTheWake);
    cell_crash_old_run_after_boundary_commit_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRunAfterItsBoundaryCommit);
    cell_crash_old_run_after_boundary_commit_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRunAfterItsBoundaryCommit);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_crash_old_run_after_boundary_commit_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRunAfterItsBoundaryCommit);
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
    cell_cancel_capture_before_authorization_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Captured, Some(CancelCrash::BeforeAuthorization));
    cell_cancel_capture_after_authorization_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Captured, Some(CancelCrash::AfterAuthorization));
    cell_cancel_capture_before_commit_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Captured, Some(CancelCrash::BeforeCommit));
    cell_cancel_capture_after_commit_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Captured, Some(CancelCrash::AfterCommit));
    cell_cancel_successor_before_authorization_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Successor, Some(CancelCrash::BeforeAuthorization));
    cell_cancel_successor_after_authorization_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Successor, Some(CancelCrash::AfterAuthorization));
    cell_cancel_successor_before_commit_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Successor, Some(CancelCrash::BeforeCommit));
    cell_cancel_successor_after_commit_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Successor, Some(CancelCrash::AfterCommit));
    cell_cancel_capture_sqlite_memory:
        cancellation_discards_the_cell, Storage::SqliteMemory, (CancelAt::Captured, None);
    cell_cancel_capture_sqlite_file:
        cancellation_discards_the_cell, Storage::SqliteFile, (CancelAt::Captured, None);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    cell_cancel_capture_postgres:
        cancellation_discards_the_cell, Storage::Postgres, (CancelAt::Captured, None);
}

fn predecessor_subscriptions(roll: &CellRoll) -> (String, Vec<lash_restate_test::InvocationView>) {
    let views = roll.server().invocations();
    let attach = views
        .iter()
        .find(|view| view.target.starts_with("LashProcessAttach/"))
        .expect("the predecessor's attach");
    let key = attach
        .target
        .split('/')
        .nth(1)
        .expect("the attach's wait key")
        .to_owned();
    let wait = format!("LashDurableWaitWorkflow/{key}/await_resolution");
    let read = format!("LashProcessWorkflow/{}/await_terminal", roll.process);
    let subscriptions: Vec<_> = views
        .iter()
        .filter(|view| view.target == attach.target || view.target == wait || view.target == read)
        .cloned()
        .collect();
    assert_eq!(
        subscriptions.len(),
        3,
        "the wait, attach and terminal read are armed on N"
    );
    assert!(subscriptions.iter().all(|view| view.status != "completed"));
    (key, subscriptions)
}

async fn complete_external(roll: &CellRoll) -> Result<()> {
    let output = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!("done")),
    );
    roll.engine
        .old_backend()
        .process_registry()
        .complete_process(
            &roll.process,
            output.clone(),
            lash_core::ProcessCompletionAuthority::ExternalOwner,
        )
        .await?;
    let Engine::Double(double) = &roll.engine else {
        unreachable!("the law's double")
    };
    lash_restate::RestateIngressClient::new(double.connection())
        .call_workflow_json::<_, lash_restate::Reply<()>>(
            "LashProcessWorkflow",
            roll.process.as_str(),
            "complete_terminal",
            &lash_restate::Call::new(lash_restate::RestateProcessCompleteRequest {
                process_id: roll.process.clone(),
                output,
            }),
        )
        .await
        .expect("publish the external process's terminal");
    Ok(())
}

async fn assert_subscription_retired(
    roll: &CellRoll,
    subscriptions: &[lash_restate_test::InvocationView],
) {
    let deadline = tokio::time::Instant::now() + WEDGE;
    loop {
        let views = roll.server().invocations();
        if subscriptions.iter().all(|old| {
            views
                .iter()
                .any(|view| view.id == old.id && view.status == "completed")
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "predecessor subscription remains pinned: {views:#?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn assert_external_answer(roll: &mut CellRoll) -> Result<()> {
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the resumed run answers")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::json!({"answer": "done", "before": 42}),
        })
    );
    assert_eq!(
        roll.requests.lock_recover().len(),
        1,
        "the captured cell resumes without another model call"
    );
    roll.assert_ended().await
}

#[derive(Clone, Copy, Debug)]
enum RetirementCrash {
    BeforePromise,
    AfterPromise,
    BeforeIndex,
    AfterIndex,
}

async fn retires_subscription(crash: Option<RetirementCrash>, hold_attach: bool) -> Result<()> {
    use lash_restate_test::{CrashPoint, CrashRule, protocol::MessageType};
    let mut roll = CellRoll::start_external(
        Storage::SqliteMemory,
        &format!("wait-retirement-{crash:?}-{hold_attach}"),
    )
    .await?;
    let predecessor = roll.parked_run("run-run", 1).await;
    let old_deployment = roll
        .server()
        .pinned_deployment(&predecessor.id)
        .expect("N is pinned");
    let (key, subscriptions) = predecessor_subscriptions(&roll);
    let crashes = lash_restate_test::CrashCount::new();
    if let Some(crash) = crash {
        assert!(roll.server().on_crash(crashes.listener()));
        let (service, key, ty) = match crash {
            RetirementCrash::BeforePromise => (
                "LashDurableWaitWorkflow",
                key.clone(),
                MessageType::CompletePromiseCommand,
            ),
            RetirementCrash::AfterPromise => (
                "LashDurableWaitWorkflow",
                key.clone(),
                MessageType::OutputCommand,
            ),
            RetirementCrash::BeforeIndex => (
                "LashDurableWaitIndex",
                roll.session.to_string(),
                MessageType::SetStateCommand,
            ),
            RetirementCrash::AfterIndex => (
                "LashDurableWaitIndex",
                roll.session.to_string(),
                MessageType::OutputCommand,
            ),
        };
        roll.server().crash_on(
            CrashRule::new(CrashPoint::BeforeFrame { ty })
                .service(service)
                .key(key)
                .handler("resolve"),
        );
    }
    let held = if hold_attach {
        Some(roll.server().hold("LashProcessAttach", &key).await)
    } else {
        None
    };
    roll.hand_over().await?;
    roll.parked_run(CONTINUATION, 1).await;
    if let Some(held) = held {
        let status = roll.core.generation_drain_status(&roll.old).await?;
        assert_eq!(status.in_flight_turns, 0, "the run already belongs to N+1");
        assert!(
            status.unfinished_invocations >= 2,
            "the held attach and read still need N: {status:?}"
        );
        assert!(
            !status.drained(),
            "engine work holds the drain after SQL work transfers"
        );
        assert!(
            roll.server()
                .remove_deployment(&old_deployment, false)
                .is_err()
        );
        held.release();
    }
    assert_subscription_retired(&roll, &subscriptions).await;
    roll.server().settle().await;
    let successor: Vec<_> = roll
        .server()
        .invocations()
        .into_iter()
        .filter(|view| {
            view.pinned_deployment_id != predecessor.pinned_deployment_id
                && view.status != "completed"
                && (view.target.starts_with("LashProcessAttach/")
                    || view.target.ends_with("/await_terminal")
                    || view.target.starts_with("LashDurableWaitWorkflow/")
                        && view.target.ends_with("/await_resolution"))
        })
        .collect();
    assert_eq!(
        successor.len(),
        3,
        "N+1 owns its own wait, attach and read: {successor:?}"
    );
    let process = roll
        .engine
        .old_backend()
        .process_registry()
        .get_process(&roll.process)
        .await?
        .expect("the awaited process");
    assert!(
        process.terminal().is_none() && process.cancel_request.is_none(),
        "retiring the subscription leaves the process live"
    );
    let status = roll.core.generation_drain_status(&roll.old).await?;
    assert!(status.drained(), "N has no remaining holds: {status:?}");
    roll.server()
        .remove_deployment(&old_deployment, false)
        .expect("remove N before completing the process");
    complete_external(&roll).await?;
    if crash.is_some() {
        assert_eq!(crashes.get(), 1, "the chosen record boundary crashed once");
    }
    assert_external_answer(&mut roll).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_retires_predecessor_subscription_before_process_completion() -> Result<()> {
    retires_subscription(None, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_signal_handover_retires_only_the_predecessors_read() -> Result<()> {
    let mut roll = CellRoll::start(Storage::SqliteMemory, "signal-read-retirement").await?;
    let key = roll
        .engine
        .old_backend()
        .effect_host()
        .await_event_key(
            &lash_core::ExecutionScope::process(roll.process.clone()),
            lash_core::AwaitEventWaitIdentity::process_signal(
                roll.process.clone(),
                &roll.signal,
                1,
            ),
        )
        .await
        .expect("the process's signal key");
    let address = lash_restate::RestateDurableWaitAddress::for_key(&key);
    let target = format!(
        "LashDurableWaitWorkflow/{}/await_resolution",
        address.workflow_key
    );
    let predecessor = roll
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target == target)
        .expect("the predecessor's signal read");
    roll.engine
        .old_backend()
        .process_work()
        .port()
        .deliver_hand_over(&roll.process, &roll.old)
        .await?;
    assert_subscription_retired(&roll, std::slice::from_ref(&predecessor)).await;
    let deadline = tokio::time::Instant::now() + WEDGE;
    while !roll.server().invocations().iter().any(|view| {
        view.target == target
            && view.pinned_deployment_id != predecessor.pinned_deployment_id
            && view.status != "completed"
    }) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the successor never awaited the same signal key"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let record = roll
        .engine
        .old_backend()
        .process_registry()
        .get_process(&roll.process)
        .await?
        .expect("the process");
    assert!(record.terminal().is_none() && record.cancel_request.is_none());
    roll.hand_over().await?;
    roll.parked_run(CONTINUATION, 1).await;
    roll.release_process().await?;
    assert_external_answer(&mut roll).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_retirement_crash_before_promise() -> Result<()> {
    retires_subscription(Some(RetirementCrash::BeforePromise), false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_retirement_crash_after_promise() -> Result<()> {
    retires_subscription(Some(RetirementCrash::AfterPromise), false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_retirement_crash_before_index() -> Result<()> {
    retires_subscription(Some(RetirementCrash::BeforeIndex), false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_retirement_crash_after_index() -> Result<()> {
    retires_subscription(Some(RetirementCrash::AfterIndex), false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_drain_counts_an_attach_until_it_returns() -> Result<()> {
    retires_subscription(None, true).await
}

async fn terminal_or_cancel_during_retirement(cancel: bool) -> Result<()> {
    let mut roll = CellRoll::start_external(
        Storage::SqliteMemory,
        &format!("wait-retirement-race-{cancel}"),
    )
    .await?;
    let predecessor = roll.parked_run("run-run", 1).await;
    let old_deployment = roll
        .server()
        .pinned_deployment(&predecessor.id)
        .expect("N is pinned");
    let (key, subscriptions) = predecessor_subscriptions(&roll);
    let held = roll.server().hold("LashDurableWaitWorkflow", &key).await;
    let driver = Arc::clone(&roll.core._session_shifts);
    let reconcile = tokio::spawn(async move {
        driver
            .reconcile(
                &lash_core::engine::ReconcileCursor::default(),
                std::num::NonZeroUsize::new(16).expect("page"),
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + WEDGE;
    while !roll
        .server()
        .journal(&predecessor.id)
        .expect("the predecessor's journal")
        .iter()
        .any(|entry| entry.ty == lash_restate_test::protocol::MessageType::AttachInvocationCommand)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "retirement never awaited its physical read"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if cancel {
        // The store accepts cancellation while retirement cannot complete.
        // Its engine delivery queues behind the exclusive retirement call.
        let request = roll
            .handle
            .as_ref()
            .expect("the sent input")
            .cancel()
            .origin("retirement-race")
            .into_future();
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => {
                result?;
                held.release();
            },
            result = async {
                loop {
                    let store = lash_core::runtime::live_session_view(&roll.core.store_factory, &roll.session).await?.expect("the session");
                    if store.turn_cancel_request(&lash_core::facade_support::TurnAddress::new(roll.session.clone(), lash_core::TurnId::fixture("run-run"))).await?.is_some() { break; }
                    assert!(tokio::time::Instant::now() < deadline, "the cancel was never recorded");
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Ok::<(), crate::EmbedError>(())
            } => {
                result?;
                held.release();
                request.await?;
            }
        }
    } else {
        complete_external(&roll).await?;
        held.release();
    }
    reconcile.await.expect("the recovery task finishes")?;
    assert_subscription_retired(&roll, &subscriptions).await;
    if cancel {
        let outcome = tokio::time::timeout(WEDGE, roll.sent().outcome())
            .await
            .expect("the cancelled run answers")?;
        assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
        assert!(
            roll.engine
                .old_backend()
                .process_registry()
                .get_process(&roll.process)
                .await?
                .expect("the process")
                .terminal()
                .is_none(),
            "a detached externally owned process remains live after its waiting Run cancels"
        );
        assert_eq!(roll.requests.lock_recover().len(), 1);
    } else {
        assert_external_answer(&mut roll).await?;
    }
    roll.server().settle().await;
    roll.server()
        .remove_deployment(&old_deployment, false)
        .expect("the retirement race releases N");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_observes_completion_during_subscription_retirement() -> Result<()> {
    terminal_or_cancel_during_retirement(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handover_honours_cancel_during_subscription_retirement() -> Result<()> {
    terminal_or_cancel_during_retirement(true).await
}

mod waits;
