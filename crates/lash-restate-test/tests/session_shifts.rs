//! The Restate `SessionShifts`'s own laws (FIG-3600): what `LashSession` and
//! `LashTurn` owe whatever kernel shift the core installs.
//!
//! The shift here is a scripted [`SessionShifts`] over an in-memory ledger:
//! its admission is a recorded `AdmitShift` step, exactly as the kernel's is,
//! and its run run consumes the admitted item idempotently. That isolates
//! the engine's part: one shift per session at a time, one `LashTurn` per
//! admitted run, schedules that are never swallowed, the generation gate,
//! and recovery from a crash at every journal point of both handlers. The
//! kernel's own laws (admission, sealing, the turn body) run on the real
//! shift elsewhere.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::engine::{
    AdmitVerdict, Admitted, RunOutcome, SealRefusal, ShiftAbort, ShiftOutcome, ShiftRequest,
    ShiftRequestId, ShiftStop, admission_body, shift_admission_replay_key,
};
use lash_core::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    RuntimeErrorCode, ScopedEffectController, SessionId, SessionShifts, SessionWorkEngine, TurnId,
};
use lash_restate::{Call, Reply, RestateRunRequest, RestateSessionShiftRequest};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    CrashCount, CrashPoint, CrashRule, DeploymentHooks, Refusal, RestateTestBackend,
    SESSION_SHIFT_SERVICE, ServerConfig, TURN_DRIVER_SERVICE,
};

// ---------------------------------------------------------------------------
// The scripted shift
// ---------------------------------------------------------------------------

/// One session's items: open ones in arrival order, and every item a run
/// run consumed, in consumption order.
#[derive(Clone, Debug, Default)]
struct Ledger {
    open: VecDeque<String>,
    consumed: Vec<String>,
    /// How many times any run run started, redrives included.
    run_executions: usize,
    refused_terminals: Vec<String>,
}

/// A gate admission `ordinal` of one request waits at, after it read the
/// ledger and before it answers: the shift is then past its last read.
struct AdmissionGate {
    request: String,
    ordinal: u32,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// A hold every run run waits at before it consumes its item: the shift
/// that called the run is then awaiting it.
#[derive(Default)]
struct RunHold {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// How the execution of a scripted item's run ends, when it does not consume
/// the item.
#[derive(Clone, Copy, Debug)]
enum RunScript {
    /// The run is refused terminally (`ShiftAbort::Refused`) and consumes
    /// nothing: the item stays open.
    Refuse,
    /// Another `SessionShifts` sealed the session after this admission: the seal is
    /// superseded and nothing runs, which is what the kernel answers a run
    /// whose shift lost the seal race.
    Supersede,
    /// A queued run that finds nothing admissible: it cedes, the item
    /// stays due, and admission mints a fresh run for it every time.
    Cede,
    /// The run's first execution ends `ShiftAbort::Refused` carrying a
    /// retryable error, the shape a shift racing a lane release leaves: the
    /// refusal's own code, not the variant it arrived in, decides whether
    /// the attempt ends. The script fires once; the redelivery runs the
    /// unscripted path.
    RefusedRetryable,
}

#[derive(Default)]
struct ScriptedShifts {
    ledgers: Mutex<BTreeMap<SessionId, Ledger>>,
    gate: Mutex<Option<Arc<AdmissionGate>>>,
    continuation_gate: Mutex<Option<Arc<AdmissionGate>>>,
    run_hold: Mutex<Option<Arc<RunHold>>>,
    scripts: Mutex<BTreeMap<String, RunScript>>,
    /// The admissions of an item that still fail their attempt, by item.
    admission_faults: Mutex<BTreeMap<String, usize>>,
}

/// The item a scripted run was admitted for: a ceded item's runs are
/// `{item}@{request}#{ordinal}`.
fn item_of(run: &TurnId) -> &str {
    run.as_str().split('@').next().unwrap_or_default()
}

impl ScriptedShifts {
    fn accept(&self, session: &SessionId, item: &str) {
        self.ledgers
            .lock()
            .unwrap()
            .entry(session.clone())
            .or_default()
            .open
            .push_back(item.to_owned());
    }

    fn script(&self, item: &str, script: RunScript) {
        self.scripts.lock().unwrap().insert(item.to_owned(), script);
    }

    /// The next `faults` admissions that would admit `item` fail their
    /// attempt retryably instead, before anything is recorded.
    fn fail_admissions(&self, item: &str, faults: usize) {
        self.admission_faults
            .lock()
            .unwrap()
            .insert(item.to_owned(), faults);
    }

    /// The admissions of `item` still owed a failure.
    fn admission_faults_left(&self, item: &str) -> usize {
        self.admission_faults
            .lock()
            .unwrap()
            .get(item)
            .copied()
            .unwrap_or_default()
    }

    /// Another `SessionShifts` answered `item`: it leaves the open items, unscripted.
    fn answer_elsewhere(&self, session: &SessionId, item: &str) {
        self.scripts.lock().unwrap().remove(item);
        let mut ledgers = self.ledgers.lock().unwrap();
        let ledger = ledgers.entry(session.clone()).or_default();
        ledger.open.retain(|open| open != item);
        ledger.consumed.push(item.to_owned());
    }

    fn ledger(&self, session: &SessionId) -> Ledger {
        self.ledgers
            .lock()
            .unwrap()
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    fn gate(&self, request: &str, ordinal: u32) -> Arc<AdmissionGate> {
        let gate = Arc::new(AdmissionGate {
            request: request.to_owned(),
            ordinal,
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    /// From now on every run run waits at the returned hold.
    fn hold_runs(&self) -> Arc<RunHold> {
        let hold = Arc::new(RunHold::default());
        *self.run_hold.lock().unwrap() = Some(Arc::clone(&hold));
        hold
    }

    fn gate_for(&self, request: &ShiftRequest, ordinal: u32) -> Option<Arc<AdmissionGate>> {
        self.gate
            .lock()
            .unwrap()
            .as_ref()
            .filter(|gate| gate.request == request.request.as_str() && gate.ordinal == ordinal)
            .cloned()
            .or_else(|| {
                (ordinal == 0
                    && request
                        .request
                        .as_str()
                        .starts_with(lash_core::engine::SHIFT_CONTINUATION_PREFIX))
                .then(|| self.continuation_gate.lock().unwrap().take())
                .flatten()
            })
    }

    /// Admission's body: the oldest open item is the run, or nothing is.
    async fn admission(
        &self,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
    ) -> Result<AdmitVerdict, RuntimeError> {
        let next = self.ledger(&request.session).open.front().cloned();
        if let Some(item) = &next
            && let Some(owed) = self.admission_faults.lock().unwrap().get_mut(item)
            && *owed > 0
        {
            *owed -= 1;
            return Err(runtime_error(format!(
                "the admission of {item} failed its attempt"
            )));
        }
        if let Some(gate) = self.gate_for(request, ordinal) {
            gate.reached.notify_one();
            gate.release.notified().await;
        }
        let ceded = next.as_ref().is_some_and(|item| {
            matches!(
                self.scripts.lock().unwrap().get(item),
                Some(RunScript::Cede)
            )
        });
        Ok(match next {
            Some(item) => AdmitVerdict::Admit({
                let receipt_session: lash_core::SessionId = request.session.clone();
                let receipt_admission = lash_core::engine::AdmissionId::new(format!(
                    "{}#{ordinal}",
                    request.request.as_str()
                ));
                admission_body::admitted(
                    receipt_session.clone(),
                    request.request.clone(),
                    receipt_admission.clone(),
                    admitting_generation.clone(),
                    lash_core::store::ShiftAdmissionReceipt {
                        selection: lash_core::store::ShiftAdmissionSelection {
                            run: TurnId::fixture(if ceded {
                                format!("{item}@{}#{ordinal}", request.request.as_str())
                            } else {
                                item.clone()
                            }),
                            work: lash_core::engine::AdmittedWork::Queued {
                                head: lash_core::BatchId::from("scripted-batch"),
                            },
                            observed_epoch: 0,
                        },
                        run_start: lash_core::store::RunStartNonce::new(receipt_admission.as_str()),
                        seal: lash_core::store::ShiftEpochSeal::Sealed(
                            lash_core::store_backend_support::sealed_shift_fence(
                                receipt_session.clone(),
                                1,
                                receipt_admission.clone(),
                            ),
                        ),
                        cancel_intent: lash_core::TurnCancelIntentSnapshot::Absent,
                        run_admission: None,
                    },
                )
            }),
            None => AdmitVerdict::Idle,
        })
    }
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::QueuedWork, message.into())
}

#[async_trait::async_trait]
impl SessionShifts for ScriptedShifts {
    async fn admit(
        &self,
        controller: ScopedEffectController<'_>,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        let address = EffectAddress::new(
            controller.execution_scope().clone(),
            shift_admission_replay_key(&request.request, ordinal),
        )
        .map_err(|error| ShiftAbort::Refused(runtime_error(error.to_string())))?;
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(address, RuntimeAttribution::default(), "admit-shift"),
            RuntimeEffectCommand::AdmitShift {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: request.session.clone(),
                    request: request.request.clone(),
                    build_generation: admitting_generation.clone(),
                    run_start: lash_core::engine::RunStartNonce::new("witness-nonce"),
                }),
            },
        );
        let verdict = self
            .admission(request, admitting_generation, ordinal)
            .await
            .map_err(ShiftAbort::Retry)?;
        controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    Ok(RuntimeEffectOutcome::AdmitShift {
                        verdict: Box::new(verdict),
                    })
                }),
            )
            .await
            .and_then(RuntimeEffectOutcome::into_admit_shift)
            .map_err(|error| ShiftAbort::Retry(error.into_runtime_error()))
    }

    async fn execute_run(
        &self,
        _controller: ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async {
                let run = admitted.run().clone();
                // The ledger retains terminal evidence as the real store does.
                // A new invocation adopts that end before entering the body.
                if self
                    .ledger(admitted.session())
                    .refused_terminals
                    .contains(&run.to_string())
                {
                    return Err(ShiftAbort::Refused(runtime_error(format!(
                        "run {run} is refused"
                    ))));
                }
                let hold = self.run_hold.lock().unwrap().clone();
                if let Some(hold) = hold {
                    hold.reached.notify_one();
                    hold.release.notified().await;
                }
                let script = self.scripts.lock().unwrap().get(item_of(&run)).copied();
                {
                    let mut ledgers = self.ledgers.lock().unwrap();
                    let ledger = ledgers.entry(admitted.session().clone()).or_default();
                    ledger.run_executions += 1;
                    match script {
                        Some(RunScript::Refuse) => {
                            ledger.refused_terminals.push(run.to_string());
                            return Err(ShiftAbort::Refused(runtime_error(format!(
                                "run {run} is refused"
                            ))));
                        }
                        Some(RunScript::Supersede) => {
                            return Ok(RunOutcome::Refused {
                                run,
                                refusal: SealRefusal::Superseded { epoch: 2 },
                            });
                        }
                        Some(RunScript::Cede) => return Ok(RunOutcome::Ceded { run }),
                        Some(RunScript::RefusedRetryable) => {
                            self.scripts.lock().unwrap().remove(item_of(&run));
                            return Err(ShiftAbort::Refused(RuntimeError::new(
                                RuntimeErrorCode::SessionExecutionLaneBusy,
                                format!("run {run} met the session lane still held"),
                            )));
                        }
                        None => {}
                    }
                    // Idempotent, like a commit fenced by its admission: a redrive of
                    // a run that already consumed its item consumes nothing.
                    if ledger.open.front().map(String::as_str) == Some(run.as_str()) {
                        ledger.open.pop_front();
                        ledger.consumed.push(run.as_str().to_owned());
                    }
                }
                Ok(RunOutcome::Committed {
                    work_remaining: true,
                    kind: lash_core::store::RunTerminalKind::Answered,
                    run,
                })
            }
            .await,
        )
    }

    async fn close_run(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &lash_core::SessionId,
        _run: &lash_core::TurnId,
    ) -> Result<(), lash_core::engine::ShiftAbort> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The backend, its scripted `SessionShifts`, and the engine's installation of that
/// `SessionShifts`, which the test keeps for as long as the `SessionShifts` serves shifts.
async fn fixture(
    seed: u64,
) -> (
    RestateTestBackend,
    Arc<ScriptedShifts>,
    Arc<dyn SessionShifts>,
) {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let (scripted, installed) = install(&backend);
    (backend, scripted, installed)
}

/// A scripted `SessionShifts` installed as `backend`'s engine `SessionShifts`.
fn install(backend: &RestateTestBackend) -> (Arc<ScriptedShifts>, Arc<dyn SessionShifts>) {
    let scripted = Arc::new(ScriptedShifts::default());
    let installed = backend
        .restate()
        .session_work_engine()
        .install_session_shifts(Arc::clone(&scripted) as Arc<dyn SessionShifts>);
    assert!(
        installed.runs_on(scripted.as_ref()),
        "the first install is the engine's `SessionShifts`"
    );
    (scripted, installed)
}

fn request(id: &str) -> ShiftRequestId {
    ShiftRequestId::new(id)
}

async fn attach(backend: &RestateTestBackend, session: &SessionId, id: &str) -> ShiftOutcome {
    tokio::time::timeout(
        Duration::from_secs(20),
        backend.attach_shift(session, request(id)),
    )
    .await
    .expect("the shift ends")
    .expect("the shift's outcome")
}

/// How `id`'s shift of `session` ended, read through every invocation it
/// handed off to.
async fn whole_shift(backend: &RestateTestBackend, session: &SessionId, id: &str) -> ShiftOutcome {
    tokio::time::timeout(
        Duration::from_secs(20),
        backend
            .restate()
            .session_work_engine()
            .await_shift(session, &request(id)),
    )
    .await
    .expect("the shift ends")
    .expect("the shift's outcome")
}

fn committed_runs(outcome: &ShiftOutcome) -> Vec<String> {
    outcome
        .ran
        .iter()
        .map(|run| match run {
            RunOutcome::Committed { run, .. } => run.as_str().to_owned(),
            RunOutcome::Refused { run, refusal } => {
                panic!("run {run} was refused: {refusal:?}")
            }
            RunOutcome::Ceded { run } => panic!("run {run} ceded"),
            RunOutcome::Applied { run } => panic!("run {run} ran no turn"),
            RunOutcome::Released { run } => panic!("run {run} was released"),
        })
        .collect()
}

/// The `LashTurn` invocations whose `handler` the engine ran.
fn turn_invocations(backend: &RestateTestBackend, handler: &str) -> usize {
    backend
        .server()
        .invocations()
        .iter()
        .filter(|view| {
            view.target.starts_with(TURN_DRIVER_SERVICE)
                && view.target.ends_with(&format!("/{handler}"))
        })
        .count()
}

/// Every `LashSession` invocation ended with an outcome, none with a
/// failure.
fn no_shift_failed(backend: &RestateTestBackend) {
    for view in backend.server().invocations() {
        if view.target.starts_with(SESSION_SHIFT_SERVICE) {
            assert_eq!(view.status, "completed", "{view:?}");
            assert!(
                backend
                    .server()
                    .outcome(&view.id)
                    .is_some_and(|outcome| outcome.is_ok()),
                "{} failed: {view:?}",
                view.target
            );
        }
    }
}

/// The `LashSession` invocations of the backend.
fn session_shifts(backend: &RestateTestBackend) -> Vec<lash_restate_test::InvocationView> {
    backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(SESSION_SHIFT_SERVICE))
        .collect()
}

/// Waits until `reached` answers true. A shift that pauses first fails the
/// law, and so does a wait that never ends.
async fn until_unpaused(
    backend: &RestateTestBackend,
    what: &str,
    mut reached: impl FnMut() -> bool,
) {
    for _ in 0..12_000 {
        if let Some(paused) = session_shifts(backend)
            .into_iter()
            .find(|view| view.status == "paused")
        {
            panic!(
                "{what}: a shift paused on the failed attempts of more than one run: {paused:?}"
            );
        }
        if reached() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{what}: never reached: {:?}", session_shifts(backend));
}

/// Waits until the engine has no invocation left running.
async fn settle(backend: &RestateTestBackend) {
    for _ in 0..500 {
        backend.server().settle().await;
        if backend
            .server()
            .invocations()
            .iter()
            .all(|view| view.status == "completed")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "invocations still open: {:?}",
        backend.server().invocations()
    );
}

// ---------------------------------------------------------------------------
// Laws
// ---------------------------------------------------------------------------

/// A schedule's shift admits every open item in arrival order, one
/// `LashTurn` per run, and stops `Idle` once admission finds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scheduled_shift_runs_every_open_item_in_arrival_order() {
    let (backend, scripted, _installation) = fixture(11).await;
    let session = SessionId::from("shift-order");
    scripted.accept(&session, "a");
    scripted.accept(&session, "b");
    backend
        .restate()
        .session_work_engine()
        .schedule_shift(&session, request("r1"));
    let outcome = attach(&backend, &session, "r1").await;
    assert_eq!(committed_runs(&outcome), ["a", "b"]);
    assert_eq!(outcome.stop, ShiftStop::Idle);
    let ledger = scripted.ledger(&session);
    assert_eq!(ledger.consumed, ["a", "b"]);
    assert!(ledger.open.is_empty());
    settle(&backend).await;
    let turns: Vec<_> = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(TURN_DRIVER_SERVICE))
        .map(|view| view.target)
        .collect();
    assert_eq!(
        turns,
        [
            "LashTurn/11:shift-orderr1#0/run",
            "LashTurn/11:shift-orderr1#1/run",
            "LashTurn/11:shift-orderr1#2/run"
        ],
        "one invocation per immutable admission, including the idle stop"
    );
}

/// A busy session yields after a bounded number of runs and schedules the
/// rest under a fresh request. The first invocation cannot grow forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_session_shift_hands_off_before_its_journal_grows_without_bound() {
    let (backend, scripted, _installation) = fixture(0x517).await;
    let session = SessionId::from("shift-bounded");
    for index in 0..65 {
        scripted.accept(&session, &format!("item-{index}"));
    }
    backend
        .restate()
        .session_work_engine()
        .schedule_shift(&session, request("bounded"));
    let first = attach(&backend, &session, "bounded").await;
    assert!(
        matches!(first.stop, ShiftStop::HandedOff { .. }),
        "the first invocation hands off at a boundary: {first:?}"
    );
    assert_eq!(
        first.ran.len(),
        lash_core::engine::MAX_RUNS_PER_SHIFT,
        "one invocation stops exactly at the run bound"
    );
    settle(&backend).await;
    assert_eq!(scripted.ledger(&session).consumed.len(), 65);
    no_shift_failed(&backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stable_drive_identity_cannot_select_a_generation_lane() {
    let (backend, _driver, _installation) = fixture(0x4795).await;
    let session = SessionId::from("typed-shift-lane");
    let id = request(&format!(
        "{}{}:g{}",
        lash_core::engine::SHIFT_CONTINUATION_PREFIX,
        "a".repeat(64),
        backend
            .restate()
            .build_generation()
            .expect("the engine generation is bound")
    ));
    let outcome = backend
        .restate()
        .session_work_engine()
        .attach_shift(&session, id)
        .await
        .unwrap();
    assert_eq!(outcome.stop, ShiftStop::Idle);
    let shifts = session_shifts(&backend);
    assert_eq!(shifts.len(), 1);
    assert!(
        shifts[0]
            .target
            .starts_with(&format!("LashSession/{session}/"))
    );
}

async fn held_generation_continuation() -> (
    RestateTestBackend,
    Arc<ScriptedShifts>,
    Arc<dyn SessionShifts>,
    SessionId,
    RestateSessionShiftRequest,
    Arc<AdmissionGate>,
) {
    let backend = lash_restate_test::backend(0x4568, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let (scripted, installation) = install(&backend);
    let session = SessionId::from("generation-continuation-attach");
    scripted.accept(&session, "first");
    scripted.accept(&session, "second");
    let gate = Arc::new(AdmissionGate {
        request: lash_core::engine::SHIFT_CONTINUATION_PREFIX.to_owned(),
        ordinal: 0,
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *scripted.continuation_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let generation = backend
        .restate()
        .build_generation()
        .expect("the engine's generation is bound");
    let initial = backend
        .restate()
        .session_work_engine()
        .send_resume(&session, request("initial"), generation)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the generation-lane continuation is running inside admission");
    let send = backend
        .server()
        .journal(initial.as_str())
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.one_way_call_command())
        .find(|send| send.handler_name == "shift")
        .expect("the first leg recorded its continuation send");
    let call: Call<RestateSessionShiftRequest> = serde_json::from_slice(&send.parameter).unwrap();
    let continuation = call.body;
    assert_eq!(
        continuation.request.intended_lane.as_ref(),
        Some(generation)
    );
    assert_eq!(
        continuation.request.request,
        lash_core::engine::shift_continuation_request(&ShiftRequest {
            session: session.clone(),
            request: request("initial"),
            intended_lane: Some(generation.clone()),
        })
    );
    assert_eq!(
        send.idempotency_key.as_deref(),
        Some(continuation.request.request.as_str())
    );
    assert_eq!(scripted.ledger(&session).consumed, ["first"]);
    (backend, scripted, installation, session, continuation, gate)
}

/// FIG-4568: an attach after a build roll joins the recorded generation
/// continuation, including its later legs, while its admission is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_joins_a_generation_lane_continuation_without_starting_a_stable_shift() {
    let (backend, scripted, _installation, session, continuation, gate) =
        held_generation_continuation().await;
    let next_generation = lash_core::engine::BuildGeneration::for_test("attach-next");
    backend
        .add_build(next_generation.clone(), "next", DeploymentHooks::default())
        .await
        .unwrap();
    let next = backend.restate().sibling_build(next_generation);
    let attached = next
        .session_work_engine()
        .await_drive_request(&continuation.request);
    tokio::pin!(attached);
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::select! {
            biased;
            outcome = &mut attached => outcome,
            () = async {
                tokio::task::yield_now().await;
                gate.release.notify_one();
            } => attached.await,
        }
    })
    .await
    .expect("the attached continuation ends")
    .expect("the attach joins the running continuation");
    assert_eq!(outcome.ran.len(), 1, "the remaining run was run");
    assert_eq!(outcome.ran[0].run().as_str(), "second");
    assert_eq!(outcome.stop, ShiftStop::Idle);
    settle(&backend).await;
    assert_eq!(scripted.ledger(&session).consumed, ["first", "second"]);
    assert!(
        session_shifts(&backend)
            .iter()
            .all(|view| !view.target.starts_with(&format!("LashSession/{session}/"))),
        "the attach starts no stable-lane shift: {:?}",
        session_shifts(&backend)
    );
    no_shift_failed(&backend);
}

/// FIG-4568: bypassing the attach API cannot run a recorded continuation
/// on the stable lane. Its typed refusal precedes every journaled command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generation_lane_continuation_sent_to_the_stable_lane_is_refused_typed() {
    let (backend, scripted, _installation, session, continuation, gate) =
        held_generation_continuation().await;
    let error = backend
        .ingress()
        .call_workflow_json::<_, Reply<ShiftOutcome>>(
            SESSION_SHIFT_SERVICE,
            session.as_str(),
            "shift",
            &Call::new(continuation.clone()),
        )
        .await
        .expect_err("a stable-lane call cannot run the generation continuation");
    let lash_restate::RestateHttpError::Status { body, .. } = error else {
        panic!("the handler returned no typed refusal: {error}");
    };
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let message = body["message"].as_str().unwrap();
    let (_, encoded) = message.split_once("lash-shift-refused:").unwrap();
    let refusal: RuntimeError = serde_json::from_str(encoded).unwrap();
    assert_eq!(
        refusal.code,
        RuntimeErrorCode::ExecutionScopeAdmissionRefused
    );
    let stable = session_shifts(&backend)
        .into_iter()
        .find(|view| view.target == format!("LashSession/{session}/shift"))
        .unwrap();
    assert_eq!(stable.attempts, 1);
    assert!(
        backend
            .server()
            .journal(&stable.id)
            .unwrap()
            .iter()
            .all(|entry| {
                !entry.ty.is_command()
                    || matches!(
                        entry.ty,
                        MessageType::InputCommand | MessageType::OutputCommand
                    )
            })
    );
    assert_eq!(scripted.ledger(&session).consumed, ["first"]);
    gate.release.notify_one();
    settle(&backend).await;
    assert_eq!(scripted.ledger(&session).consumed, ["first", "second"]);
}

/// A shift's attempt budget is never spent on the sum of its runs'
/// (FIG-4506). Restate counts failed attempts over an invocation's whole
/// retry loop, and a busy shift that awaits one run after another never
/// suspends, so its loop never restarts. Here the deployment dies under the
/// shift once while it awaits each run of a backlog, and is still down for
/// the shift's next attempt: more failed attempts in all than the handler's
/// budget, each followed by a run that ran to its end. The shift runs every
/// run, in order, and no invocation of it pauses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_that_goes_on_after_each_failed_attempt_never_pauses_on_their_sum() {
    let runs = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).unwrap() + 4;
    let items: Vec<_> = (0..runs).map(|index| format!("item-{index}")).collect();
    // The dispatches of the shift the deployment still owes a refusal.
    let down_for = Arc::new(AtomicUsize::new(0));
    let hooks = DeploymentHooks {
        served: None,
        refuse: Some(Arc::new({
            let down_for = Arc::clone(&down_for);
            move |dispatch| {
                (dispatch.service.ends_with(SESSION_SHIFT_SERVICE)
                    && down_for
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |owed| {
                            owed.checked_sub(1)
                        })
                        .is_ok())
                .then_some(Refusal::Retryable)
            }
        })),
    };
    let backend = lash_restate_test::backend_with_build(0x4506, ServerConfig::default(), "", hooks)
        .await
        .expect("build the Restate test backend");
    let (scripted, _installation) = install(&backend);
    let session = SessionId::from("shift-backlog");
    let hold = scripted.hold_runs();
    for item in &items {
        scripted.accept(&session, item);
    }
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_shift(&session, request("backlog"));
    let shifts = || {
        backend
            .server()
            .invocations()
            .into_iter()
            .filter(|view| view.target.starts_with(SESSION_SHIFT_SERVICE))
            .collect::<Vec<_>>()
    };
    for item in &items {
        let paused = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    () = hold.reached.notified() => return None,
                    () = tokio::time::sleep(Duration::from_millis(10)) => {
                        let paused = shifts().into_iter().find(|view| view.status == "paused");
                        if paused.is_some() {
                            return paused;
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the run of {item} never ran: {:?}", shifts()));
        if let Some(paused) = paused {
            panic!(
                "the shift paused before {item} on the failed attempts of the runs it had \
                 already run: {paused:?}"
            );
        }
        // The shift that called this run awaits it: its attempt is open.
        let shift = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(shift) = shifts().into_iter().find(|view| view.status == "running") {
                    return shift;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no shift awaits {item}: {:?}", shifts()));
        down_for.store(1, Ordering::SeqCst);
        assert!(
            backend.server().crash(&shift.id),
            "the shift awaiting {item} had an attempt to crash"
        );
        hold.release.notify_one();
    }
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        engine.await_shift(&session, &request("backlog")),
    )
    .await
    .unwrap_or_else(|_| panic!("the shift never ended: {:?}", shifts()))
    .expect("the shift's outcome");
    assert_eq!(committed_runs(&outcome), items);
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(scripted.ledger(&session).consumed, items);
    settle(&backend).await;
    no_shift_failed(&backend);
    assert_eq!(
        backend.server().stats().crashes,
        runs as u64,
        "the deployment died once under every run"
    );
    assert_eq!(
        down_for.load(Ordering::SeqCst),
        0,
        "every death cost the shift a refused attempt"
    );
}

/// A failed attempt inside a leg's first run is seen at that run's boundary
/// (FIG-4523). The deployment is down for half the shift handler's budget
/// while the shift awaits the first run of its backlog, and again while it
/// awaits the second: fewer failed attempts under either run than the budget,
/// as many under both. The attempt that outlived the first run's failures did
/// not start the leg, so the shift hands off at that run's boundary and the
/// second run's failures are counted by a new invocation. No invocation
/// pauses, and the first leg ran the first run alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_that_failed_inside_a_legs_first_run_hands_off_at_that_runs_boundary() {
    let budget = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).unwrap();
    // The refused attempts under one run: with the attempt the crash ends,
    // still inside one retry loop; under two runs, all of it.
    let refusals = budget / 2;
    let items = ["item-0", "item-1", "item-2"];
    let down_for = Arc::new(AtomicUsize::new(0));
    let hooks = DeploymentHooks {
        served: None,
        refuse: Some(Arc::new({
            let down_for = Arc::clone(&down_for);
            move |dispatch| {
                (dispatch.service.ends_with(SESSION_SHIFT_SERVICE)
                    && down_for
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |owed| {
                            owed.checked_sub(1)
                        })
                        .is_ok())
                .then_some(Refusal::Retryable)
            }
        })),
    };
    let backend = lash_restate_test::backend_with_build(0x4523, ServerConfig::default(), "", hooks)
        .await
        .expect("build the Restate test backend");
    let (scripted, _installation) = install(&backend);
    let session = SessionId::from("shift-first-run");
    let hold = scripted.hold_runs();
    for item in items {
        scripted.accept(&session, item);
    }
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_shift(&session, request("first-run"));
    for (index, item) in items.into_iter().enumerate() {
        let what = format!("the run of {item}");
        tokio::select! {
            () = hold.reached.notified() => {}
            () = until_unpaused(&backend, &what, || false) => {}
        }
        assert_eq!(
            scripted.ledger(&session).run_executions,
            index,
            "the run of {item} waits at the hold"
        );
        if index < 2 {
            let mut shift = None;
            until_unpaused(&backend, &format!("the shift awaiting {item}"), || {
                shift = session_shifts(&backend)
                    .into_iter()
                    .find(|view| view.status == "running");
                shift.is_some()
            })
            .await;
            down_for.store(refusals, Ordering::SeqCst);
            assert!(
                backend.server().crash(&shift.expect("a running shift").id),
                "the shift awaiting {item} had an attempt to crash"
            );
            until_unpaused(&backend, &format!("the outage under {item}"), || {
                down_for.load(Ordering::SeqCst) == 0
            })
            .await;
        }
        hold.release.notify_one();
    }
    let first = attach(&backend, &session, "first-run").await;
    assert_eq!(
        committed_runs(&first),
        ["item-0"],
        "the leg whose first run outlived failed attempts ran that run alone"
    );
    assert!(
        matches!(first.stop, ShiftStop::HandedOff { .. }),
        "and handed off at its boundary: {first:?}"
    );
    let outcome = whole_shift(&backend, &session, "first-run").await;
    assert_eq!(committed_runs(&outcome), items);
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(scripted.ledger(&session).consumed, items);
    settle(&backend).await;
    no_shift_failed(&backend);
    assert_eq!(backend.server().stats().crashes, 2);
    assert_eq!(
        down_for.load(Ordering::SeqCst),
        0,
        "every refusal cost the shift an attempt"
    );
}

/// Admission failures spend only their immutable intent's retry budget.
/// Two admissions each fail beyond half the budget, while their parent shift
/// stays on its fresh leg and no invocation pauses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_failures_spend_only_their_intents_retry_budget() {
    let budget = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).unwrap();
    // Under one admission, inside one retry loop; under two, past it.
    let faults = budget / 2 + 1;
    let items = ["item-0", "item-1", "item-2"];
    let (backend, scripted, _installation) = fixture(0x4556).await;
    let session = SessionId::from("shift-first-admission");
    for item in items {
        scripted.accept(&session, item);
    }
    scripted.fail_admissions("item-0", faults);
    scripted.fail_admissions("item-1", faults);
    backend
        .restate()
        .session_work_engine()
        .schedule_shift(&session, request("first-admission"));
    let outcome = tokio::select! {
        outcome = whole_shift(&backend, &session, "first-admission") => outcome,
        () = until_unpaused(&backend, "the shift past its first admission", || false) => {
            unreachable!("the wait ends only by panicking")
        }
    };
    assert_eq!(committed_runs(&outcome), items);
    assert_eq!(outcome.stop, ShiftStop::Idle);
    let first = attach(&backend, &session, "first-admission").await;
    assert_eq!(
        committed_runs(&first),
        items,
        "admission failures are isolated in child invocations"
    );
    assert_eq!(
        first.stop,
        ShiftStop::Idle,
        "the fresh parent leg stays live"
    );
    assert_eq!(scripted.ledger(&session).consumed, items);
    settle(&backend).await;
    no_shift_failed(&backend);
    for item in ["item-0", "item-1"] {
        assert_eq!(
            scripted.admission_faults_left(item),
            0,
            "every failed admission of {item} cost its intent an attempt"
        );
    }
    let attempts: Vec<_> = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| {
            view.target.starts_with(TURN_DRIVER_SERVICE) && view.target.ends_with("/run")
        })
        .map(|view| view.attempts)
        .collect();
    assert_eq!(
        attempts
            .iter()
            .filter(|attempts| usize::try_from(**attempts).unwrap() > faults)
            .count(),
        2,
        "each admission's failed attempts were spent by an invocation of its own: {attempts:?}"
    );
}

/// A row committed while a shift runs, after that shift's last admission
/// read the ledger, is admitted by the next invocation: its schedule names
/// its own request, so the engine queues it behind the running shift instead
/// of deduplicating it into that shift (FIG-3600 ruling on O2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_committed_while_a_shift_runs_is_admitted_by_the_next_invocation() {
    let (backend, scripted, _installation) = fixture(12).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("shift-behind");
    scripted.accept(&session, "a");
    // The first shift's second admission reads the ledger (only `a`
    // consumed, nothing open), then waits here.
    let gate = scripted.gate("r1", 1);
    engine.schedule_shift(&session, request("r1"));
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the first shift reaches its last admission");
    scripted.accept(&session, "b");
    engine.schedule_shift(&session, request("r2"));
    gate.release.notify_one();

    let first = attach(&backend, &session, "r1").await;
    assert_eq!(committed_runs(&first), ["a"]);
    assert_eq!(first.stop, ShiftStop::Idle);
    let second = attach(&backend, &session, "r2").await;
    assert_eq!(
        committed_runs(&second),
        ["b"],
        "the queued invocation's first admission is the re-check"
    );
    assert_eq!(scripted.ledger(&session).consumed, ["a", "b"]);
}

/// A send that lands exactly as the shift finishes — committed after that
/// shift's last admission read the ledger, asked for before it returned —
/// is still admitted (FIG-4036): its ask joins the one shift the engine
/// sends once the running shift ended, never the running shift alone, and
/// that shift's first admission takes it. Its waiter follows that shift.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_as_the_shift_finishes_is_admitted_by_one_shift_behind_it() {
    let (backend, scripted, _installation) = fixture(0x4036).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("shift-finishing");
    scripted.accept(&session, "a");
    let gate = scripted.gate("ingress:a:1", 1);
    engine
        .request_shift(&session, request("ingress:a:1"))
        .await
        .expect("the first ask is accepted");
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the shift reaches its last admission");
    scripted.accept(&session, "b");
    engine
        .request_shift(&session, request("ingress:b:1"))
        .await
        .expect("the ask as the shift finishes is accepted");
    engine
        .request_shift(&session, request("ingress:b:1"))
        .await
        .expect("a repeated ask is accepted");
    gate.release.notify_one();
    let waited = tokio::time::timeout(
        Duration::from_secs(20),
        engine.await_shift(&session, &request("ingress:b:1")),
    )
    .await
    .expect("the send's shift ends")
    .expect("the send's shift outcome");
    assert_eq!(waited.stop, ShiftStop::Idle);
    assert_eq!(scripted.ledger(&session).consumed, ["a", "b"]);
    settle(&backend).await;
    no_shift_failed(&backend);
    assert!(
        backend
            .server()
            .inbox_high_water(SESSION_SHIFT_SERVICE, "shift-finishing")
            <= 1,
        "at most one shift waited behind the running one: {:?}",
        backend.server().invocations()
    );
}

/// Asks for one session's shift, back to back, each after its item
/// committed, queue at most one shift behind the running one, and every
/// item is admitted (FIG-4036).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_asks_queue_at_most_one_shift() {
    const ASKS: usize = 32;
    let (backend, scripted, _installation) = fixture(0x4037).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("shift-back-to-back");
    let items: Vec<String> = (0..ASKS).map(|index| format!("item-{index:02}")).collect();
    for item in &items {
        scripted.accept(&session, item);
        engine
            .request_shift(&session, request(&format!("ingress:{item}:1")))
            .await
            .expect("the ask is accepted");
    }
    for item in &items {
        tokio::time::timeout(
            Duration::from_secs(20),
            engine.await_shift(&session, &request(&format!("ingress:{item}:1"))),
        )
        .await
        .expect("the ask's shift ends")
        .expect("the ask's shift outcome");
    }
    settle(&backend).await;
    assert_eq!(scripted.ledger(&session).consumed, items);
    no_shift_failed(&backend);
    assert!(
        backend
            .server()
            .inbox_high_water(SESSION_SHIFT_SERVICE, "shift-back-to-back")
            <= 1,
        "at most one shift waited behind the running one: {:?}",
        backend.server().invocations()
    );
}

/// One request id is one shift: a repeated schedule of it attaches to the
/// first invocation, so an item committed after that shift's last admission
/// stays open until another request works the session. This is why every
/// schedule names its own request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_request_id_is_one_shift() {
    let (backend, scripted, _installation) = fixture(13).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("shift-dedupe");
    scripted.accept(&session, "a");
    engine.schedule_shift(&session, request("r1"));
    let first = attach(&backend, &session, "r1").await;
    assert_eq!(committed_runs(&first), ["a"]);
    scripted.accept(&session, "b");
    engine.schedule_shift(&session, request("r1"));
    settle(&backend).await;
    let again = attach(&backend, &session, "r1").await;
    assert_eq!(again, first, "the repeated request answers the first shift");
    assert_eq!(scripted.ledger(&session).open, ["b"]);
    let shifts = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(SESSION_SHIFT_SERVICE))
        .count();
    assert_eq!(shifts, 1);
    engine.schedule_shift(&session, request("r2"));
    let next = attach(&backend, &session, "r2").await;
    assert_eq!(committed_runs(&next), ["b"]);
}

/// Shifts of different sessions do not wait on each other; shifts of one
/// session never overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_works_one_request_at_a_time() {
    let (backend, scripted, _installation) = fixture(14).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let held = SessionId::from("shift-held");
    let free = SessionId::from("shift-free");
    scripted.accept(&held, "a");
    scripted.accept(&free, "x");
    let gate = scripted.gate("h1", 0);
    engine.schedule_shift(&held, request("h1"));
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the held shift reaches its admission");
    engine.schedule_shift(&held, request("h2"));
    engine.schedule_shift(&free, request("f1"));
    let free_outcome = attach(&backend, &free, "f1").await;
    assert_eq!(committed_runs(&free_outcome), ["x"]);
    assert!(
        scripted.ledger(&held).consumed.is_empty(),
        "the held session's second shift waits behind its first"
    );
    gate.release.notify_one();
    let first = attach(&backend, &held, "h1").await;
    assert_eq!(committed_runs(&first), ["a"]);
    let second = attach(&backend, &held, "h2").await;
    assert!(second.ran.is_empty());
    assert_eq!(second.stop, ShiftStop::Idle);
}

/// A call on a wire this build does not read is refused before either
/// handler journals anything (ADR 0115 §3.1). A request carries no drain
/// stamp: only its wire range is checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_on_a_wire_this_build_does_not_read_is_refused_before_any_journal_command() {
    let (backend, scripted, _installation) = fixture(15).await;
    let session = SessionId::from("shift-generation");
    scripted.accept(&session, "a");
    let ingress = backend.ingress();
    let newer = lash_restate::VersionRange::new(
        lash_restate::RESTATE_WIRE_VERSION + 1,
        lash_restate::RESTATE_WIRE_VERSION + 1,
    )
    .expect("a wire range");
    let refused = ingress
        .call_object_json::<_, Reply<ShiftOutcome>>(
            SESSION_SHIFT_SERVICE,
            session.as_str(),
            "shift",
            &Call::stating(
                newer,
                RestateSessionShiftRequest {
                    request: ShiftRequest {
                        session: session.clone(),
                        request: request("newer-wire"),
                        intended_lane: None,
                    },
                    handed_off: None,
                },
            ),
        )
        .await;
    let refusal = refused.expect_err("a disjoint wire is refused by LashSession");
    assert!(
        refusal.to_string().contains("lash.wire_unsupported"),
        "{refusal}"
    );
    let turn = ingress
        .call_workflow_json::<_, Reply<lash_restate::RestateRunOutcome>>(
            TURN_DRIVER_SERVICE,
            &lash_restate::turn_invocation_key(
                &ShiftRequest {
                    session: session.clone(),
                    request: request("newer-wire"),
                    intended_lane: None,
                },
                0,
            ),
            "run",
            &Call::stating(
                newer,
                RestateRunRequest {
                    sender_generation: Some(lash_core::engine::BuildGeneration::for_test("any")),
                    request: lash_core::engine::ShiftRequest {
                        session: session.clone(),
                        request: request("newer-wire"),
                        intended_lane: None,
                    },
                    ordinal: 0,
                    rules: Default::default(),
                    draining: None,
                },
            ),
        )
        .await;
    let refusal = turn.expect_err("a disjoint wire is refused by LashTurn");
    assert!(
        refusal.to_string().contains("lash.wire_unsupported"),
        "{refusal}"
    );
    settle(&backend).await;
    for view in backend.server().invocations() {
        let journaled: Vec<_> = backend
            .server()
            .journal(&view.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| {
                entry.ty.is_command()
                    && !matches!(
                        entry.ty,
                        MessageType::InputCommand | MessageType::OutputCommand
                    )
            })
            .map(|entry| entry.ty)
            .collect();
        assert!(
            journaled.is_empty(),
            "{} journaled nothing between its input and its refusal: {journaled:?}",
            view.target
        );
    }
    assert_eq!(scripted.ledger(&session).run_executions, 0);
}

/// HIGH-1 (a): a run whose `LashTurn` already ended is admitted again, by
/// the same shift and by the next one. Neither calls its key a second time
/// into a failure: the first shift consumes the released run and stops
/// `RunAborted` when admission names it again; the next shift's call finds
/// the key already run and attaches to the outcome the run recorded. No
/// shift fails and the run executes once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_whose_turn_already_ended_is_readmitted_without_a_second_execution() {
    let (backend, scripted, _installation) = fixture(18).await;
    let session = SessionId::from("shift-ended-run");
    scripted.accept(&session, "a");
    scripted.script("a", RunScript::Refuse);
    let engine = Arc::clone(backend.restate().session_work_engine());
    let run = TurnId::from("a");
    engine.schedule_shift(&session, request("r1"));
    let first = attach(&backend, &session, "r1").await;
    assert_eq!(first.ran, [RunOutcome::Released { run: run.clone() }]);
    assert_eq!(first.stop, ShiftStop::RunAborted { run: run.clone() });
    engine.schedule_shift(&session, request("r2"));
    let second = attach(&backend, &session, "r2").await;
    assert_eq!(second, first, "the next shift attaches to the recorded end");
    settle(&backend).await;
    no_shift_failed(&backend);
    assert_eq!(
        scripted.ledger(&session).run_executions,
        1,
        "the run ran once"
    );
    assert_eq!(scripted.ledger(&session).open, ["a"], "nothing consumed it");
    assert_eq!(
        turn_invocations(&backend, "run"),
        4,
        "two intents each retain their execution and stop admission"
    );
    assert_eq!(
        turn_invocations(&backend, "outcome"),
        2,
        "each intent reads the recorded terminal it adopted"
    );
}

/// The stop rules outlive a handoff (FIG-4523). With every await suspended,
/// the attempt that reads a run's end never started its leg, so the shift
/// hands off at that run's boundary and the next admission is another
/// invocation's. That leg starts from what the one before it remembers: a
/// released run admission names again stops the shift `RunAborted` there,
/// uncalled, where a leg that forgot it would call it, consume it and hand
/// off again without end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_run_admitted_again_after_a_handoff_stops_the_shift() {
    let backend = lash_restate_test::backend(0x4524, ServerConfig::default().always_replay(true))
        .await
        .expect("build the Restate test backend");
    let (scripted, _installation) = install(&backend);
    let session = SessionId::from("shift-ended-run-handoff");
    scripted.accept(&session, "a");
    scripted.script("a", RunScript::Refuse);
    let run = TurnId::from("a");
    backend
        .restate()
        .session_work_engine()
        .schedule_shift(&session, request("r1"));
    let first = attach(&backend, &session, "r1").await;
    assert_eq!(first.ran, [RunOutcome::Released { run: run.clone() }]);
    assert_eq!(first.stop, ShiftStop::HandedOff { run: run.clone() });
    // A waiter on the whole shift is answered the released run's refusal,
    // so the law reads the leg the shift handed off to by its own request.
    let next = lash_core::engine::shift_continuation_request(&ShiftRequest {
        session: session.clone(),
        request: request("r1"),
        intended_lane: None,
    });
    let second = attach(&backend, &session, next.as_str()).await;
    assert_eq!(second.ran, [], "the run is not called again");
    assert_eq!(second.stop, ShiftStop::RunAborted { run });
    settle(&backend).await;
    no_shift_failed(&backend);
    assert_eq!(session_shifts(&backend).len(), 2, "one handoff");
    assert_eq!(
        scripted.ledger(&session).run_executions,
        1,
        "the run ran once"
    );
    assert_eq!(
        turn_invocations(&backend, "run"),
        2,
        "the successor intent records its stop without executing the run"
    );
}

/// HIGH-1 (b): a queued run that cedes stops the shift. Admission would
/// mint a fresh run for the still-due queue on every ordinal; the shift
/// stops `Yielded` after the first instead of starting a `LashTurn` per
/// ordinal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_run_that_cedes_stops_the_shift() {
    let (backend, scripted, _installation) = fixture(19).await;
    let session = SessionId::from("shift-ceded-run");
    scripted.accept(&session, "q");
    scripted.script("q", RunScript::Cede);
    backend
        .restate()
        .session_work_engine()
        .schedule_shift(&session, request("r1"));
    let outcome = attach(&backend, &session, "r1").await;
    let run = TurnId::from("q@r1#0");
    assert_eq!(outcome.ran, [RunOutcome::Ceded { run: run.clone() }]);
    assert_eq!(outcome.stop, ShiftStop::Yielded { run });
    settle(&backend).await;
    no_shift_failed(&backend);
    assert_eq!(scripted.ledger(&session).run_executions, 1);
    assert_eq!(turn_invocations(&backend, "run"), 1, "one LashTurn");
}

/// HIGH-1 (c): another `SessionShifts` seals the session after `LashSession`
/// admitted a run, so the run's seal is superseded and nothing runs. The
/// shift stops `Yielded`, fails nothing and burns no second `LashTurn`; once
/// the other `SessionShifts` answered the item, the session's next shift is idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_whose_seal_another_shift_superseded_stops_cleanly() {
    let (backend, scripted, _installation) = fixture(20).await;
    let session = SessionId::from("shift-superseded");
    scripted.accept(&session, "a");
    scripted.script("a", RunScript::Supersede);
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_shift(&session, request("r1"));
    let outcome = attach(&backend, &session, "r1").await;
    let run = TurnId::from("a");
    assert_eq!(
        outcome.ran,
        [RunOutcome::Refused {
            run: run.clone(),
            refusal: SealRefusal::Superseded { epoch: 2 },
        }]
    );
    assert_eq!(outcome.stop, ShiftStop::Yielded { run });
    scripted.answer_elsewhere(&session, "a");
    engine.schedule_shift(&session, request("r2"));
    let next = attach(&backend, &session, "r2").await;
    assert!(next.ran.is_empty(), "{next:?}");
    assert_eq!(next.stop, ShiftStop::Idle);
    settle(&backend).await;
    no_shift_failed(&backend);
    assert_eq!(scripted.ledger(&session).run_executions, 1);
    assert_eq!(
        turn_invocations(&backend, "run"),
        2,
        "the next intent records its idle stop"
    );
}

/// A shift scheduled before any core installed its `SessionShifts` runs once one
/// does: the attempt fails retryably and the engine retries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_scheduled_before_the_install_runs_once_session_shifts_are_installed() {
    let backend = lash_restate_test::backend(16, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("shift-early");
    engine.schedule_shift(&session, request("r1"));
    let failed =
        async {
            loop {
                if backend.server().invocations().iter().any(|view| {
                    view.target.starts_with(SESSION_SHIFT_SERVICE) && view.attempts >= 1
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
    tokio::time::timeout(Duration::from_secs(20), failed)
        .await
        .expect("the early shift ran and failed");
    let scripted = Arc::new(ScriptedShifts::default());
    scripted.accept(&session, "a");
    let _installation =
        engine.install_session_shifts(Arc::clone(&scripted) as Arc<dyn SessionShifts>);
    let outcome = attach(&backend, &session, "r1").await;
    assert_eq!(committed_runs(&outcome), ["a"]);
}

/// A run run refused with a retryable-typed error — the shape a shift
/// racing a lane release leaves — keeps the attempt open: `LashTurn`
/// redelivers it instead of recording the run `Released`, so the shift's
/// attacher never reads back a refusal for what is a retry (FIG-3831).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retryable_refusal_of_a_run_execution_redelivers_instead_of_releasing() {
    let (backend, scripted, _installation) = fixture(22).await;
    let session = SessionId::from("shift-busy-lane");
    scripted.accept(&session, "a");
    scripted.script("a", RunScript::RefusedRetryable);
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_shift(&session, request("r1"));
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        engine.await_shift(&session, &request("r1")),
    )
    .await
    .expect("the shift ends")
    .expect("the refusal's redrive answers, not fails");
    assert_eq!(committed_runs(&outcome), ["a"]);
    settle(&backend).await;
    no_shift_failed(&backend);
    assert!(
        scripted.ledger(&session).run_executions >= 2,
        "the refused attempt was redelivered"
    );
    let runs = backend
        .server()
        .turn_invocations(&session, &TurnId::from("a"));
    assert_eq!(runs.len(), 1, "one LashTurn run invocation");
    assert!(
        runs[0].attempts >= 2,
        "the same invocation retried: {:?}",
        runs[0]
    );
    assert!(
        backend
            .server()
            .outcome(&runs[0].id)
            .is_some_and(|outcome| outcome.is_ok()),
        "the turn workflow completed, not failed: {:?}",
        runs[0]
    );
}

/// The journal points of both handlers in one reference shift: every
/// command each stored, and every `ctx.run` result.
fn journal_points(backend: &RestateTestBackend, service: &str) -> Vec<CrashPoint> {
    let view = backend
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with(service))
        .unwrap_or_else(|| panic!("the reference shift ran {service}"));
    let mut points = Vec::new();
    let entries = backend.server().journal(&view.id).unwrap_or_default();
    for (index, entry) in entries
        .iter()
        .filter(|entry| entry.ty.is_command())
        .enumerate()
        .skip(1)
    {
        points.push(CrashPoint::BeforeCommand { index });
        if entry.ty == MessageType::RunCommand {
            points.push(CrashPoint::BeforeRunResult {
                name: entry.name.clone(),
            });
        }
    }
    points
}

/// A crash at any journal point of `LashSession` or `LashTurn` recovers to
/// the reference shift: every item is consumed once, in order, and the shift
/// answers the same runs. A shift whose crashed attempt replayed may answer
/// them over two invocations (FIG-4506), so the law reads the shift through
/// its continuations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_crashed_at_any_journal_point_of_either_handler_consumes_each_item_once() {
    let seed = 17;
    let session = SessionId::from("shift-crash");
    let reference = {
        let (backend, scripted, _installation) = fixture(seed).await;
        scripted.accept(&session, "a");
        scripted.accept(&session, "b");
        backend
            .restate()
            .session_work_engine()
            .schedule_shift(&session, request("r1"));
        let outcome = whole_shift(&backend, &session, "r1").await;
        settle(&backend).await;
        let points = [SESSION_SHIFT_SERVICE, TURN_DRIVER_SERVICE]
            .into_iter()
            .map(|service| (service, journal_points(&backend, service)))
            .collect::<Vec<_>>();
        (outcome, points)
    };
    let (reference_outcome, points) = reference;
    let mut cases = 0;
    for (service, service_points) in points {
        assert!(!service_points.is_empty(), "{service} has journal points");
        for point in service_points {
            let (backend, scripted, _installation) = fixture(seed).await;
            if service == SESSION_SHIFT_SERVICE {
                backend.crash_session_shift(point.clone());
            } else {
                backend.crash_run_execution(point.clone());
            }
            scripted.accept(&session, "a");
            scripted.accept(&session, "b");
            backend
                .restate()
                .session_work_engine()
                .schedule_shift(&session, request("r1"));
            let outcome = whole_shift(&backend, &session, "r1").await;
            assert_eq!(
                outcome, reference_outcome,
                "{service} {point:?}: the redriven shift answers the reference"
            );
            assert_eq!(
                scripted.ledger(&session).consumed,
                ["a", "b"],
                "{service} {point:?}: each item consumed once, in order"
            );
            assert_eq!(
                backend.server().stats().crashes,
                1,
                "{service} {point:?}: the crash fired"
            );
            cases += 1;
        }
    }
    println!("session shift crash matrix: {cases} journal points");
}

#[derive(Clone, Copy, Debug)]
enum ContinuationCut {
    BeforeSend,
    AfterSend,
    LastRunSettlement,
}

impl ContinuationCut {
    const ALL: [Self; 3] = [Self::BeforeSend, Self::AfterSend, Self::LastRunSettlement];

    fn rule(self, session: &SessionId, replay: bool) -> CrashRule {
        match self {
            Self::BeforeSend => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OneWayCallCommand,
            })
            .service(SESSION_SHIFT_SERVICE)
            .handler("shift")
            .key(session.as_str()),
            Self::AfterSend => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            })
            .service(SESSION_SHIFT_SERVICE)
            .handler("shift")
            .key(session.as_str()),
            Self::LastRunSettlement => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            })
            .service(TURN_DRIVER_SERVICE)
            .handler("run")
            .key({
                let mut request = ShiftRequest {
                    session: session.clone(),
                    request: request("bounded-crash"),
                    intended_lane: None,
                };
                let runs = leg_runs(replay);
                for _ in 0..63 / runs {
                    request.request = lash_core::engine::shift_continuation_request(&request);
                }
                lash_restate::turn_invocation_key(&request, (63 % runs) as u32)
            }),
        }
    }
}

fn schedule_continuation_case(
    backend: &lash_core::Backend,
    scripted: &ScriptedShifts,
    session: &SessionId,
) -> (ShiftRequestId, ShiftRequestId) {
    for index in 0..65 {
        scripted.accept(session, &format!("item-{index}"));
    }
    let initial = ShiftRequest {
        session: session.clone(),
        request: request("bounded-crash"),
        intended_lane: None,
    };
    let successor = lash_core::engine::shift_continuation_request(&initial);
    backend
        .session_work()
        .schedule_shift(session, initial.request.clone());
    (initial.request, successor)
}

/// `first` handed off after `first_runs` runs, and `next`, the rest of the
/// shift, ran the others.
fn assert_continuation_case(
    first: &ShiftOutcome,
    first_runs: usize,
    next: &ShiftOutcome,
    scripted: &ScriptedShifts,
    session: &SessionId,
) {
    assert_eq!(
        committed_runs(first),
        (0..first_runs)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
    assert!(matches!(first.stop, ShiftStop::HandedOff { .. }));
    assert_eq!(
        committed_runs(next),
        (first_runs..65)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(next.stop, ShiftStop::Idle);
    assert_eq!(
        scripted.ledger(session).consumed,
        (0..65)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
}

/// The runs a leg of the continuation case runs before it hands off. Under
/// always-replay no attempt past a leg's first await started the leg, so each
/// invocation hands off at its first boundary instead of its run bound
/// (FIG-4506, FIG-4523): the shift runs in legs of one run.
fn leg_runs(replay: bool) -> usize {
    if replay {
        1
    } else {
        lash_core::engine::MAX_RUNS_PER_SHIFT
    }
}

/// The `LashSession` invocations the continuation case's 65 runs take: the
/// run-bound leg and its successor, or under always-replay one leg per run
/// and the leg the last of them hands off to, which admits nothing.
fn continuation_case_legs(replay: bool) -> usize {
    if replay { 66 } else { 2 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shift_continuation_crash_redrives_one_successor() {
    for replay in [false, true] {
        for cut in ContinuationCut::ALL {
            let backend =
                lash_restate_test::backend(0x4145, ServerConfig::default().always_replay(replay))
                    .await
                    .expect("build the backend");
            let scripted = Arc::new(ScriptedShifts::default());
            let installed = backend
                .lash_backend()
                .session_work()
                .install_session_shifts(Arc::clone(&scripted) as Arc<dyn SessionShifts>);
            let session = SessionId::from("shift-continuation-crash");
            backend.server().crash_on(cut.rule(&session, replay));
            let (initial, successor) =
                schedule_continuation_case(&backend.lash_backend(), &scripted, &session);
            let first = tokio::time::timeout(
                Duration::from_secs(60),
                backend.attach_shift(&session, initial),
            )
            .await
            .expect("predecessor finishes")
            .expect("predecessor outcome");
            assert!(
                matches!(first.stop, ShiftStop::HandedOff { .. }),
                "the bounded shift yielded before its successor"
            );
            let leg_runs = leg_runs(replay);
            let next = tokio::time::timeout(
                Duration::from_secs(60),
                backend
                    .lash_backend()
                    .session_work()
                    .await_shift(&session, &successor),
            )
            .await
            .expect("successor finishes")
            .expect("successor outcome");
            assert_continuation_case(&first, leg_runs, &next, &scripted, &session);
            settle(&backend).await;
            assert_eq!(
                backend.server().stats().crashes,
                1,
                "{cut:?}, replay={replay}"
            );
            assert_eq!(
                backend
                    .server()
                    .invocations()
                    .iter()
                    .filter(|invocation| invocation.target
                        == format!("{SESSION_SHIFT_SERVICE}/{session}/shift"))
                    .count(),
                continuation_case_legs(replay),
                "one invocation per leg, none sent twice: {cut:?}, replay={replay}"
            );
            assert_eq!(
                turn_invocations(&backend, "run"),
                66,
                "65 selected runs and the final idle admission"
            );
            no_shift_failed(&backend);
            drop(installed);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the SessionShifts Restate suite runs it"]
async fn live_restate_shift_continuation_crash_redrives_one_successor() {
    use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
    // The suite's replay leg suspends every await, as the double's
    // always-replay does.
    let replay = std::env::var("LASH_RESTATE_SUITE_LEG").is_ok_and(|leg| leg == "replay");
    for cut in ContinuationCut::ALL {
        let tag = format!(
            "continuation-{:?}-{}",
            cut,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let env = |name| std::env::var(name).expect("the live suite's endpoint environment");
        let backend = LiveRestateBackend::start(LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("CW_BIND").parse().expect("endpoint bind"),
            endpoint_url: env("CW_URL"),
            run_tag: tag.clone(),
            namespace: lash_restate::RestateNamespace::default(),
        })
        .await
        .expect("start the live backend");
        let scripted = Arc::new(ScriptedShifts::default());
        let installed = backend
            .lash_backend()
            .session_work()
            .install_session_shifts(Arc::clone(&scripted) as Arc<dyn SessionShifts>);
        let crashes = CrashCount::new();
        assert!(backend.on_crash(crashes.listener()));
        let restart = {
            let backend = backend.clone();
            let mut on_restart = crashes.clone();
            tokio::spawn(async move {
                let mut seen = 0;
                while let Ok(count) = on_restart.wait_until(seen + 1).await {
                    seen = count;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    backend.start_serving().await.expect("restart the endpoint");
                }
            })
        };
        let session = SessionId::fixture(tag);
        backend.crash_on(cut.rule(&session, replay));
        let (initial, successor) =
            schedule_continuation_case(&backend.lash_backend(), &scripted, &session);
        let first = tokio::time::timeout(
            Duration::from_secs(60),
            backend.attach_shift(&session, initial),
        )
        .await
        .expect("predecessor finishes")
        .expect("predecessor outcome");
        assert!(
            matches!(first.stop, ShiftStop::HandedOff { .. }),
            "the bounded shift yielded before its successor"
        );
        let next = tokio::time::timeout(
            Duration::from_secs(120),
            backend
                .lash_backend()
                .session_work()
                .await_shift(&session, &successor),
        )
        .await
        .expect("successor finishes")
        .expect("successor outcome");
        assert_continuation_case(&first, leg_runs(replay), &next, &scripted, &session);
        backend
            .settle(Duration::from_secs(20), Duration::from_millis(100))
            .await;
        assert_eq!(crashes.get(), 1, "{cut:?}");
        let invocations = backend.invocations().await.expect("read invocations");
        assert_eq!(
            invocations
                .iter()
                .filter(|invocation| invocation.target
                    == format!("{SESSION_SHIFT_SERVICE}/{session}/shift"))
                .count(),
            continuation_case_legs(replay),
            "one invocation per leg, none sent twice: {cut:?}, replay={replay}"
        );
        let runs: Vec<_> = invocations
            .iter()
            .filter(|invocation| {
                invocation.target.starts_with(&format!(
                    "{TURN_DRIVER_SERVICE}/{}:{}",
                    session.as_str().len(),
                    session.as_str()
                )) && invocation.target.ends_with("/run")
            })
            .collect();
        assert_eq!(
            runs.len(),
            66,
            "65 selected runs and the final idle admission: {cut:?}"
        );
        assert!(
            runs.iter()
                .all(|invocation| invocation.status == "completed")
        );
        restart.abort();
        backend.finish().await;
        drop(installed);
    }
}
