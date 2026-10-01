//! The Restate session driver's own laws (FIG-3600): what `LashSession` and
//! `LashTurn` owe whatever kernel drive the core installs.
//!
//! The drive here is a scripted [`SessionDriver`] over an in-memory ledger:
//! its admission is a recorded `AdmitDrive` step, exactly as the kernel's is,
//! and its root run consumes the admitted item idempotently. That isolates
//! the engine's part: one drive per session at a time, one `LashTurn` per
//! admitted root, schedules that are never swallowed, the generation gate,
//! and recovery from a crash at every journal point of both handlers. The
//! kernel's own laws (admission, sealing, the turn body) run on the real
//! drive elsewhere.

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
    AdmitVerdict, Admitted, DriveAbort, DriveOutcome, DriveRequest, DriveRequestId, DriveStop,
    RootOutcome, SealVerdict, admission_body, drive_admission_replay_key,
};
use lash_core::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    RuntimeErrorCode, ScopedEffectController, SessionDriver, SessionId, SessionWorkEngine, TurnId,
};
use lash_restate::{Call, Reply, RestateSessionDriveRequest, RestateTurnDriveRequest};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    CrashPoint, CrashRule, DeploymentHooks, Refusal, RestateTestBackend, SESSION_DRIVER_SERVICE,
    ServerConfig, TURN_DRIVER_SERVICE,
};

// ---------------------------------------------------------------------------
// The scripted drive
// ---------------------------------------------------------------------------

/// One session's items: open ones in arrival order, and every item a root
/// run consumed, in consumption order.
#[derive(Clone, Debug, Default)]
struct Ledger {
    open: VecDeque<String>,
    consumed: Vec<String>,
    /// How many times any root run started, redrives included.
    root_runs: usize,
}

/// A gate admission `ordinal` of one request waits at, after it read the
/// ledger and before it answers: the drive is then past its last read.
struct AdmissionGate {
    request: String,
    ordinal: u32,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// A hold every root run waits at before it consumes its item: the drive
/// that called the root is then awaiting it.
#[derive(Default)]
struct RootHold {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// How the run of a scripted item's root ends, when it does not consume
/// the item.
#[derive(Clone, Copy, Debug)]
enum RootScript {
    /// The run is refused terminally (`DriveAbort::Refused`) and consumes
    /// nothing: the item stays open.
    Refuse,
    /// Another driver sealed the session after this admission: the seal is
    /// superseded and nothing runs, which is what the kernel answers a root
    /// whose drive lost the seal race.
    Supersede,
    /// A queued run that finds nothing admissible: it cedes, the item
    /// stays due, and admission mints a fresh root for it every time.
    Cede,
    /// The run's first execution ends `DriveAbort::Refused` carrying a
    /// retryable error, the shape a drive racing a lane release leaves: the
    /// refusal's own code, not the variant it arrived in, decides whether
    /// the attempt ends. The script fires once; the redelivery runs the
    /// unscripted path.
    RefusedRetryable,
}

#[derive(Default)]
struct ScriptedDriver {
    ledgers: Mutex<BTreeMap<SessionId, Ledger>>,
    gate: Mutex<Option<Arc<AdmissionGate>>>,
    continuation_gate: Mutex<Option<Arc<AdmissionGate>>>,
    root_hold: Mutex<Option<Arc<RootHold>>>,
    scripts: Mutex<BTreeMap<String, RootScript>>,
    /// The admissions of an item that still fail their attempt, by item.
    admission_faults: Mutex<BTreeMap<String, usize>>,
}

/// The item a scripted root was admitted for: a ceded item's roots are
/// `{item}@{request}#{ordinal}`.
fn item_of(root: &TurnId) -> &str {
    root.as_str().split('@').next().unwrap_or_default()
}

impl ScriptedDriver {
    fn accept(&self, session: &SessionId, item: &str) {
        self.ledgers
            .lock()
            .unwrap()
            .entry(session.clone())
            .or_default()
            .open
            .push_back(item.to_owned());
    }

    fn script(&self, item: &str, script: RootScript) {
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

    /// Another driver answered `item`: it leaves the open items, unscripted.
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

    /// From now on every root run waits at the returned hold.
    fn hold_roots(&self) -> Arc<RootHold> {
        let hold = Arc::new(RootHold::default());
        *self.root_hold.lock().unwrap() = Some(Arc::clone(&hold));
        hold
    }

    fn gate_for(&self, request: &DriveRequest, ordinal: u32) -> Option<Arc<AdmissionGate>> {
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
                        .starts_with(lash_core::engine::DRIVE_CONTINUATION_PREFIX))
                .then(|| self.continuation_gate.lock().unwrap().take())
                .flatten()
            })
    }

    /// Admission's body: the oldest open item is the root, or nothing is.
    async fn admission(
        &self,
        request: &DriveRequest,
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
                Some(RootScript::Cede)
            )
        });
        Ok(match next {
            Some(item) => AdmitVerdict::Admit(admission_body::admitted(
                request.session.clone(),
                TurnId::from(if ceded {
                    format!("{item}@{}#{ordinal}", request.request.as_str())
                } else {
                    item.clone()
                }),
                request.request.clone(),
                lash_core::engine::AdmissionId::new(format!(
                    "{}:{ordinal}",
                    request.request.as_str()
                )),
                0,
                request.build_generation.clone(),
                lash_core::engine::AdmittedWork::Queued {
                    head: lash_core::BatchId::from("scripted-batch"),
                },
            )),
            None => AdmitVerdict::Idle,
        })
    }
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::QueuedWork, message.into())
}

#[async_trait::async_trait]
impl SessionDriver for ScriptedDriver {
    async fn admit(
        &self,
        controller: ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        let address = EffectAddress::new(
            controller.execution_scope().clone(),
            drive_admission_replay_key(&request.request, ordinal),
        )
        .map_err(|error| DriveAbort::Refused(runtime_error(error.to_string())))?;
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(address, RuntimeAttribution::default(), "admit-drive"),
            RuntimeEffectCommand::AdmitDrive {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: request.session.clone(),
                    request: request.request.clone(),
                    build_generation: request.build_generation.clone(),
                }),
            },
        );
        let verdict = self
            .admission(request, ordinal)
            .await
            .map_err(DriveAbort::Retry)?;
        controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    Ok(RuntimeEffectOutcome::AdmitDrive {
                        verdict: Box::new(verdict),
                    })
                }),
            )
            .await
            .and_then(RuntimeEffectOutcome::into_admit_drive)
            .map_err(|error| DriveAbort::Retry(error.into_runtime_error()))
    }

    async fn run_root(
        &self,
        _controller: ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> lash_core::engine::RootRunEnd {
        lash_core::engine::RootRunEnd::owing_nothing(
            async {
                let root = admitted.root().clone();
                let hold = self.root_hold.lock().unwrap().clone();
                if let Some(hold) = hold {
                    hold.reached.notify_one();
                    hold.release.notified().await;
                }
                let script = self.scripts.lock().unwrap().get(item_of(&root)).copied();
                {
                    let mut ledgers = self.ledgers.lock().unwrap();
                    let ledger = ledgers.entry(admitted.session().clone()).or_default();
                    ledger.root_runs += 1;
                    match script {
                        Some(RootScript::Refuse) => {
                            return Err(DriveAbort::Refused(runtime_error(format!(
                                "root {root} is refused"
                            ))));
                        }
                        Some(RootScript::Supersede) => {
                            return Ok(RootOutcome::Refused {
                                root,
                                verdict: SealVerdict::Superseded { epoch: 2 },
                            });
                        }
                        Some(RootScript::Cede) => return Ok(RootOutcome::Ceded { root }),
                        Some(RootScript::RefusedRetryable) => {
                            self.scripts.lock().unwrap().remove(item_of(&root));
                            return Err(DriveAbort::Refused(RuntimeError::new(
                                RuntimeErrorCode::SessionExecutionLaneBusy,
                                format!("root {root} met the session lane still held"),
                            )));
                        }
                        None => {}
                    }
                    // Idempotent, like a commit fenced by its admission: a redrive of
                    // a root that already consumed its item consumes nothing.
                    if ledger.open.front().map(String::as_str) == Some(root.as_str()) {
                        ledger.open.pop_front();
                        ledger.consumed.push(root.as_str().to_owned());
                    }
                }
                Ok(RootOutcome::Committed {
                    outcome: lash_core::facade_support::TurnOutcome::Finished(
                        lash_core::facade_support::TurnFinish::AssistantMessage {
                            text: format!("answered {}", root.as_str()),
                        },
                    ),
                    root,
                })
            }
            .await,
        )
    }

    async fn close_root(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &lash_core::SessionId,
        _root: &lash_core::TurnId,
    ) -> Result<(), lash_core::engine::DriveAbort> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The backend, its scripted driver, and the engine's installation of that
/// driver, which the test keeps for as long as the driver serves drives.
async fn fixture(
    seed: u64,
) -> (
    RestateTestBackend,
    Arc<ScriptedDriver>,
    Arc<dyn SessionDriver>,
) {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let (driver, installed) = install(&backend);
    (backend, driver, installed)
}

/// A scripted driver installed as `backend`'s engine driver.
fn install(backend: &RestateTestBackend) -> (Arc<ScriptedDriver>, Arc<dyn SessionDriver>) {
    let driver = Arc::new(ScriptedDriver::default());
    let installed = backend
        .restate()
        .session_work_engine()
        .install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
    assert!(
        installed.runs_on(driver.as_ref()),
        "the first install is the engine's driver"
    );
    (driver, installed)
}

fn request(id: &str) -> DriveRequestId {
    DriveRequestId::new(id)
}

async fn attach(backend: &RestateTestBackend, session: &SessionId, id: &str) -> DriveOutcome {
    tokio::time::timeout(
        Duration::from_secs(20),
        backend.attach_drive(session, request(id)),
    )
    .await
    .expect("the drive ends")
    .expect("the drive's outcome")
}

/// How `id`'s drive of `session` ended, read through every invocation it
/// handed off to.
async fn whole_drive(backend: &RestateTestBackend, session: &SessionId, id: &str) -> DriveOutcome {
    tokio::time::timeout(
        Duration::from_secs(20),
        backend
            .restate()
            .session_work_engine()
            .await_drive(session, &request(id)),
    )
    .await
    .expect("the drive ends")
    .expect("the drive's outcome")
}

fn committed_roots(outcome: &DriveOutcome) -> Vec<String> {
    outcome
        .ran
        .iter()
        .map(|root| match root {
            RootOutcome::Committed { root, .. } => root.as_str().to_owned(),
            RootOutcome::Refused { root, verdict } => {
                panic!("root {root} was refused: {verdict:?}")
            }
            RootOutcome::Ceded { root } => panic!("root {root} ceded"),
            RootOutcome::Applied { root } => panic!("root {root} ran no turn"),
            RootOutcome::Released { root } => panic!("root {root} was released"),
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
fn no_drive_failed(backend: &RestateTestBackend) {
    for view in backend.server().invocations() {
        if view.target.starts_with(SESSION_DRIVER_SERVICE) {
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
fn session_drives(backend: &RestateTestBackend) -> Vec<lash_restate_test::InvocationView> {
    backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(SESSION_DRIVER_SERVICE))
        .collect()
}

/// Waits until `reached` answers true. A drive that pauses first fails the
/// law, and so does a wait that never ends.
async fn until_unpaused(
    backend: &RestateTestBackend,
    what: &str,
    mut reached: impl FnMut() -> bool,
) {
    for _ in 0..12_000 {
        if let Some(paused) = session_drives(backend)
            .into_iter()
            .find(|view| view.status == "paused")
        {
            panic!(
                "{what}: a drive paused on the failed attempts of more than one root: {paused:?}"
            );
        }
        if reached() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{what}: never reached: {:?}", session_drives(backend));
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

/// A schedule's drive admits every open item in arrival order, one
/// `LashTurn` per root, and stops `Idle` once admission finds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scheduled_drive_runs_every_open_item_in_arrival_order() {
    let (backend, driver, _installation) = fixture(11).await;
    let session = SessionId::from("drive-order");
    driver.accept(&session, "a");
    driver.accept(&session, "b");
    backend
        .restate()
        .session_work_engine()
        .schedule_drive(&session, request("r1"));
    let outcome = attach(&backend, &session, "r1").await;
    assert_eq!(committed_roots(&outcome), ["a", "b"]);
    assert_eq!(outcome.stop, DriveStop::Idle);
    let ledger = driver.ledger(&session);
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
            "LashTurn/11:drive-ordera/run",
            "LashTurn/11:drive-orderb/run"
        ],
        "one LashTurn workflow per admitted root"
    );
}

/// A busy session yields after a bounded number of roots and schedules the
/// rest under a fresh request. The first invocation cannot grow forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_session_drive_hands_off_before_its_journal_grows_without_bound() {
    let (backend, driver, _installation) = fixture(0x517).await;
    let session = SessionId::from("drive-bounded");
    for index in 0..65 {
        driver.accept(&session, &format!("item-{index}"));
    }
    backend
        .restate()
        .session_work_engine()
        .schedule_drive(&session, request("bounded"));
    let first = attach(&backend, &session, "bounded").await;
    assert!(
        matches!(first.stop, DriveStop::HandedOff { .. }),
        "the first invocation hands off at a boundary: {first:?}"
    );
    assert_eq!(
        first.ran.len(),
        lash_core::engine::MAX_ROOTS_PER_DRIVE,
        "one invocation stops exactly at the root bound"
    );
    settle(&backend).await;
    assert_eq!(driver.ledger(&session).consumed.len(), 65);
    no_drive_failed(&backend);
}

async fn held_generation_continuation() -> (
    RestateTestBackend,
    Arc<ScriptedDriver>,
    Arc<dyn SessionDriver>,
    SessionId,
    RestateSessionDriveRequest,
    Arc<AdmissionGate>,
) {
    let backend = lash_restate_test::backend(0x4568, ServerConfig::default().always_replay(true))
        .await
        .unwrap();
    let (driver, installation) = install(&backend);
    let session = SessionId::from("generation-continuation-attach");
    driver.accept(&session, "first");
    driver.accept(&session, "second");
    let gate = Arc::new(AdmissionGate {
        request: lash_core::engine::DRIVE_CONTINUATION_PREFIX.to_owned(),
        ordinal: 0,
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *driver.continuation_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let generation = backend.restate().build_generation();
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
        .find(|send| send.handler_name == "drive")
        .expect("the first leg recorded its continuation send");
    let call: Call<RestateSessionDriveRequest> = serde_json::from_slice(&send.parameter).unwrap();
    let continuation = call.body;
    assert_eq!(
        send.idempotency_key.as_deref(),
        Some(continuation.request.request.as_str())
    );
    assert_eq!(driver.ledger(&session).consumed, ["first"]);
    (backend, driver, installation, session, continuation, gate)
}

/// FIG-4568: an attach after a build roll joins the recorded generation
/// continuation, including its later legs, while its admission is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_joins_a_generation_lane_continuation_without_starting_a_stable_drive() {
    let (backend, driver, _installation, session, continuation, gate) =
        held_generation_continuation().await;
    let next_generation = lash_core::engine::BuildGeneration::for_test("attach-next");
    backend
        .add_build(next_generation.clone(), "next", DeploymentHooks::default())
        .await
        .unwrap();
    let next = backend.restate().sibling_build(next_generation);
    let attached = next
        .session_work_engine()
        .await_drive(&session, &continuation.request.request);
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
    assert_eq!(outcome.ran.len(), 1, "the remaining root was run");
    assert_eq!(outcome.ran[0].root().as_str(), "second");
    assert_eq!(outcome.stop, DriveStop::Idle);
    settle(&backend).await;
    assert_eq!(driver.ledger(&session).consumed, ["first", "second"]);
    assert!(
        session_drives(&backend)
            .iter()
            .all(|view| !view.target.starts_with(&format!("LashSession/{session}/"))),
        "the attach starts no stable-lane drive: {:?}",
        session_drives(&backend)
    );
    no_drive_failed(&backend);
}

/// FIG-4568: bypassing the attach API cannot run a recorded continuation
/// on the stable lane. Its typed refusal precedes every journaled command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generation_lane_continuation_sent_to_the_stable_lane_is_refused_typed() {
    let (backend, driver, _installation, session, continuation, gate) =
        held_generation_continuation().await;
    let error = backend
        .ingress()
        .call_workflow_json::<_, Reply<DriveOutcome>>(
            SESSION_DRIVER_SERVICE,
            session.as_str(),
            "drive",
            &Call::new(continuation.clone()),
        )
        .await
        .expect_err("a stable-lane call cannot run the generation continuation");
    let lash_restate::RestateHttpError::Status { body, .. } = error else {
        panic!("the handler returned no typed refusal: {error}");
    };
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let message = body["message"].as_str().unwrap();
    let (_, encoded) = message.split_once("lash-drive-refused:").unwrap();
    let refusal: RuntimeError = serde_json::from_str(encoded).unwrap();
    assert_eq!(
        refusal.code,
        RuntimeErrorCode::ExecutionScopeAdmissionRefused
    );
    let stable = session_drives(&backend)
        .into_iter()
        .find(|view| view.target == format!("LashSession/{session}/drive"))
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
    assert_eq!(driver.ledger(&session).consumed, ["first"]);
    gate.release.notify_one();
    settle(&backend).await;
    assert_eq!(driver.ledger(&session).consumed, ["first", "second"]);
}

/// A drive's attempt budget is never spent on the sum of its roots'
/// (FIG-4506). Restate counts failed attempts over an invocation's whole
/// retry loop, and a busy drive that awaits one root after another never
/// suspends, so its loop never restarts. Here the deployment dies under the
/// drive once while it awaits each root of a backlog, and is still down for
/// the drive's next attempt: more failed attempts in all than the handler's
/// budget, each followed by a root that ran to its end. The drive runs every
/// root, in order, and no invocation of it pauses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_that_goes_on_after_each_failed_attempt_never_pauses_on_their_sum() {
    let roots = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).unwrap() + 4;
    let items: Vec<_> = (0..roots).map(|index| format!("item-{index}")).collect();
    // The dispatches of the drive the deployment still owes a refusal.
    let down_for = Arc::new(AtomicUsize::new(0));
    let hooks = DeploymentHooks {
        served: None,
        refuse: Some(Arc::new({
            let down_for = Arc::clone(&down_for);
            move |dispatch| {
                (dispatch.service.ends_with(SESSION_DRIVER_SERVICE)
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
    let (driver, _installation) = install(&backend);
    let session = SessionId::from("drive-backlog");
    let hold = driver.hold_roots();
    for item in &items {
        driver.accept(&session, item);
    }
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_drive(&session, request("backlog"));
    let drives = || {
        backend
            .server()
            .invocations()
            .into_iter()
            .filter(|view| view.target.starts_with(SESSION_DRIVER_SERVICE))
            .collect::<Vec<_>>()
    };
    for item in &items {
        let paused = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    () = hold.reached.notified() => return None,
                    () = tokio::time::sleep(Duration::from_millis(10)) => {
                        let paused = drives().into_iter().find(|view| view.status == "paused");
                        if paused.is_some() {
                            return paused;
                        }
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the root of {item} never ran: {:?}", drives()));
        if let Some(paused) = paused {
            panic!(
                "the drive paused before {item} on the failed attempts of the roots it had \
                 already run: {paused:?}"
            );
        }
        // The drive that called this root awaits it: its attempt is open.
        let drive = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(drive) = drives().into_iter().find(|view| view.status == "running") {
                    return drive;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no drive awaits {item}: {:?}", drives()));
        down_for.store(1, Ordering::SeqCst);
        assert!(
            backend.server().crash(&drive.id),
            "the drive awaiting {item} had an attempt to crash"
        );
        hold.release.notify_one();
    }
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        engine.await_drive(&session, &request("backlog")),
    )
    .await
    .unwrap_or_else(|_| panic!("the drive never ended: {:?}", drives()))
    .expect("the drive's outcome");
    assert_eq!(committed_roots(&outcome), items);
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(driver.ledger(&session).consumed, items);
    settle(&backend).await;
    no_drive_failed(&backend);
    assert_eq!(
        backend.server().stats().crashes,
        roots as u64,
        "the deployment died once under every root"
    );
    assert_eq!(
        down_for.load(Ordering::SeqCst),
        0,
        "every death cost the drive a refused attempt"
    );
}

/// A failed attempt inside a leg's first root is seen at that root's boundary
/// (FIG-4523). The deployment is down for half the drive handler's budget
/// while the drive awaits the first root of its backlog, and again while it
/// awaits the second: fewer failed attempts under either root than the budget,
/// as many under both. The attempt that outlived the first root's failures did
/// not start the leg, so the drive hands off at that root's boundary and the
/// second root's failures are counted by a new invocation. No invocation
/// pauses, and the first leg ran the first root alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_that_failed_inside_a_legs_first_root_hands_off_at_that_roots_boundary() {
    let budget = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).unwrap();
    // The refused attempts under one root: with the attempt the crash ends,
    // still inside one retry loop; under two roots, all of it.
    let refusals = budget / 2;
    let items = ["item-0", "item-1", "item-2"];
    let down_for = Arc::new(AtomicUsize::new(0));
    let hooks = DeploymentHooks {
        served: None,
        refuse: Some(Arc::new({
            let down_for = Arc::clone(&down_for);
            move |dispatch| {
                (dispatch.service.ends_with(SESSION_DRIVER_SERVICE)
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
    let (driver, _installation) = install(&backend);
    let session = SessionId::from("drive-first-root");
    let hold = driver.hold_roots();
    for item in items {
        driver.accept(&session, item);
    }
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_drive(&session, request("first-root"));
    for (index, item) in items.into_iter().enumerate() {
        let what = format!("the root of {item}");
        tokio::select! {
            () = hold.reached.notified() => {}
            () = until_unpaused(&backend, &what, || false) => {}
        }
        assert_eq!(
            driver.ledger(&session).root_runs,
            index,
            "the root of {item} waits at the hold"
        );
        if index < 2 {
            let mut drive = None;
            until_unpaused(&backend, &format!("the drive awaiting {item}"), || {
                drive = session_drives(&backend)
                    .into_iter()
                    .find(|view| view.status == "running");
                drive.is_some()
            })
            .await;
            down_for.store(refusals, Ordering::SeqCst);
            assert!(
                backend.server().crash(&drive.expect("a running drive").id),
                "the drive awaiting {item} had an attempt to crash"
            );
            until_unpaused(&backend, &format!("the outage under {item}"), || {
                down_for.load(Ordering::SeqCst) == 0
            })
            .await;
        }
        hold.release.notify_one();
    }
    let first = attach(&backend, &session, "first-root").await;
    assert_eq!(
        committed_roots(&first),
        ["item-0"],
        "the leg whose first root outlived failed attempts ran that root alone"
    );
    assert!(
        matches!(first.stop, DriveStop::HandedOff { .. }),
        "and handed off at its boundary: {first:?}"
    );
    let outcome = whole_drive(&backend, &session, "first-root").await;
    assert_eq!(committed_roots(&outcome), items);
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(driver.ledger(&session).consumed, items);
    settle(&backend).await;
    no_drive_failed(&backend);
    assert_eq!(backend.server().stats().crashes, 2);
    assert_eq!(
        down_for.load(Ordering::SeqCst),
        0,
        "every refusal cost the drive an attempt"
    );
}

/// A failed attempt inside a drive's first admission is seen at its first
/// root's boundary (FIG-4556). The first admission of the drive fails more
/// than half the drive handler's budget of attempts before it records
/// anything, and so does the admission after the first root: fewer failed
/// attempts under either than the budget, more under both. The leg's start is
/// the drive's first command, stored before its first admission runs, so the
/// attempt that outlived that admission's failures did not start the leg: the
/// drive hands off at the first root's boundary and the next admission's
/// failures are counted by a new invocation. No invocation pauses, and the
/// first leg ran the first root alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_that_failed_inside_its_first_admission_hands_off_at_its_first_roots_boundary() {
    let budget = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).unwrap();
    // Under one admission, inside one retry loop; under two, past it.
    let faults = budget / 2 + 1;
    let items = ["item-0", "item-1", "item-2"];
    let (backend, driver, _installation) = fixture(0x4556).await;
    let session = SessionId::from("drive-first-admission");
    for item in items {
        driver.accept(&session, item);
    }
    driver.fail_admissions("item-0", faults);
    driver.fail_admissions("item-1", faults);
    backend
        .restate()
        .session_work_engine()
        .schedule_drive(&session, request("first-admission"));
    let outcome = tokio::select! {
        outcome = whole_drive(&backend, &session, "first-admission") => outcome,
        () = until_unpaused(&backend, "the drive past its first admission", || false) => {
            unreachable!("the wait ends only by panicking")
        }
    };
    assert_eq!(committed_roots(&outcome), items);
    assert_eq!(outcome.stop, DriveStop::Idle);
    let first = attach(&backend, &session, "first-admission").await;
    assert_eq!(
        committed_roots(&first),
        ["item-0"],
        "the leg whose first admission outlived failed attempts ran its first root alone"
    );
    assert!(
        matches!(first.stop, DriveStop::HandedOff { .. }),
        "and handed off at its boundary: {first:?}"
    );
    assert_eq!(driver.ledger(&session).consumed, items);
    settle(&backend).await;
    no_drive_failed(&backend);
    for item in ["item-0", "item-1"] {
        assert_eq!(
            driver.admission_faults_left(item),
            0,
            "every failed admission of {item} cost the drive an attempt"
        );
    }
    let attempts: Vec<_> = session_drives(&backend)
        .iter()
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

/// A row committed while a drive runs, after that drive's last admission
/// read the ledger, is admitted by the next invocation: its schedule names
/// its own request, so the engine queues it behind the running drive instead
/// of deduplicating it into that drive (FIG-3600 ruling on O2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_committed_while_a_drive_runs_is_admitted_by_the_next_invocation() {
    let (backend, driver, _installation) = fixture(12).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("drive-behind");
    driver.accept(&session, "a");
    // The first drive's second admission reads the ledger (only `a`
    // consumed, nothing open), then waits here.
    let gate = driver.gate("r1", 1);
    engine.schedule_drive(&session, request("r1"));
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the first drive reaches its last admission");
    driver.accept(&session, "b");
    engine.schedule_drive(&session, request("r2"));
    gate.release.notify_one();

    let first = attach(&backend, &session, "r1").await;
    assert_eq!(committed_roots(&first), ["a"]);
    assert_eq!(first.stop, DriveStop::Idle);
    let second = attach(&backend, &session, "r2").await;
    assert_eq!(
        committed_roots(&second),
        ["b"],
        "the queued invocation's first admission is the re-check"
    );
    assert_eq!(driver.ledger(&session).consumed, ["a", "b"]);
}

/// A send that lands exactly as the drive finishes — committed after that
/// drive's last admission read the ledger, asked for before it returned —
/// is still admitted (FIG-4036): its ask joins the one drive the engine
/// sends once the running drive ended, never the running drive alone, and
/// that drive's first admission takes it. Its waiter follows that drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_as_the_drive_finishes_is_admitted_by_one_drive_behind_it() {
    let (backend, driver, _installation) = fixture(0x4036).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("drive-finishing");
    driver.accept(&session, "a");
    let gate = driver.gate("ingress:a:1", 1);
    engine
        .request_drive(&session, request("ingress:a:1"))
        .await
        .expect("the first ask is accepted");
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the drive reaches its last admission");
    driver.accept(&session, "b");
    engine
        .request_drive(&session, request("ingress:b:1"))
        .await
        .expect("the ask as the drive finishes is accepted");
    engine
        .request_drive(&session, request("ingress:b:1"))
        .await
        .expect("a repeated ask is accepted");
    gate.release.notify_one();
    let waited = tokio::time::timeout(
        Duration::from_secs(20),
        engine.await_drive(&session, &request("ingress:b:1")),
    )
    .await
    .expect("the send's drive ends")
    .expect("the send's drive outcome");
    assert_eq!(waited.stop, DriveStop::Idle);
    assert_eq!(driver.ledger(&session).consumed, ["a", "b"]);
    settle(&backend).await;
    no_drive_failed(&backend);
    assert!(
        backend
            .server()
            .inbox_high_water(SESSION_DRIVER_SERVICE, "drive-finishing")
            <= 1,
        "at most one drive waited behind the running one: {:?}",
        backend.server().invocations()
    );
}

/// Asks for one session's drive, back to back, each after its item
/// committed, queue at most one drive behind the running one, and every
/// item is admitted (FIG-4036).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn back_to_back_asks_queue_at_most_one_drive() {
    const ASKS: usize = 32;
    let (backend, driver, _installation) = fixture(0x4037).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("drive-back-to-back");
    let items: Vec<String> = (0..ASKS).map(|index| format!("item-{index:02}")).collect();
    for item in &items {
        driver.accept(&session, item);
        engine
            .request_drive(&session, request(&format!("ingress:{item}:1")))
            .await
            .expect("the ask is accepted");
    }
    for item in &items {
        tokio::time::timeout(
            Duration::from_secs(20),
            engine.await_drive(&session, &request(&format!("ingress:{item}:1"))),
        )
        .await
        .expect("the ask's drive ends")
        .expect("the ask's drive outcome");
    }
    settle(&backend).await;
    assert_eq!(driver.ledger(&session).consumed, items);
    no_drive_failed(&backend);
    assert!(
        backend
            .server()
            .inbox_high_water(SESSION_DRIVER_SERVICE, "drive-back-to-back")
            <= 1,
        "at most one drive waited behind the running one: {:?}",
        backend.server().invocations()
    );
}

/// One request id is one drive: a repeated schedule of it attaches to the
/// first invocation, so an item committed after that drive's last admission
/// stays open until another request drives the session. This is why every
/// schedule names its own request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_request_id_is_one_drive() {
    let (backend, driver, _installation) = fixture(13).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("drive-dedupe");
    driver.accept(&session, "a");
    engine.schedule_drive(&session, request("r1"));
    let first = attach(&backend, &session, "r1").await;
    assert_eq!(committed_roots(&first), ["a"]);
    driver.accept(&session, "b");
    engine.schedule_drive(&session, request("r1"));
    settle(&backend).await;
    let again = attach(&backend, &session, "r1").await;
    assert_eq!(again, first, "the repeated request answers the first drive");
    assert_eq!(driver.ledger(&session).open, ["b"]);
    let drives = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(SESSION_DRIVER_SERVICE))
        .count();
    assert_eq!(drives, 1);
    engine.schedule_drive(&session, request("r2"));
    let next = attach(&backend, &session, "r2").await;
    assert_eq!(committed_roots(&next), ["b"]);
}

/// Drives of different sessions do not wait on each other; drives of one
/// session never overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_drives_one_request_at_a_time() {
    let (backend, driver, _installation) = fixture(14).await;
    let engine = Arc::clone(backend.restate().session_work_engine());
    let held = SessionId::from("drive-held");
    let free = SessionId::from("drive-free");
    driver.accept(&held, "a");
    driver.accept(&free, "x");
    let gate = driver.gate("h1", 0);
    engine.schedule_drive(&held, request("h1"));
    tokio::time::timeout(Duration::from_secs(20), gate.reached.notified())
        .await
        .expect("the held drive reaches its admission");
    engine.schedule_drive(&held, request("h2"));
    engine.schedule_drive(&free, request("f1"));
    let free_outcome = attach(&backend, &free, "f1").await;
    assert_eq!(committed_roots(&free_outcome), ["x"]);
    assert!(
        driver.ledger(&held).consumed.is_empty(),
        "the held session's second drive waits behind its first"
    );
    gate.release.notify_one();
    let first = attach(&backend, &held, "h1").await;
    assert_eq!(committed_roots(&first), ["a"]);
    let second = attach(&backend, &held, "h2").await;
    assert!(second.ran.is_empty());
    assert_eq!(second.stop, DriveStop::Idle);
}

/// A call on a wire this build does not read is refused before either
/// handler journals anything (ADR 0115 §3.1). A request carries no drain
/// stamp: only its wire range is checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_on_a_wire_this_build_does_not_read_is_refused_before_any_journal_command() {
    let (backend, driver, _installation) = fixture(15).await;
    let session = SessionId::from("drive-generation");
    driver.accept(&session, "a");
    let ingress = backend.ingress();
    let newer = lash_restate::VersionRange::new(
        lash_restate::RESTATE_WIRE_VERSION + 1,
        lash_restate::RESTATE_WIRE_VERSION + 1,
    )
    .expect("a wire range");
    let refused = ingress
        .call_object_json::<_, Reply<DriveOutcome>>(
            SESSION_DRIVER_SERVICE,
            session.as_str(),
            "drive",
            &Call {
                wire: newer,
                body: RestateSessionDriveRequest {
                    request: DriveRequest {
                        session: session.clone(),
                        request: request("newer-wire"),
                        build_generation: lash_core::engine::BuildGeneration::for_test("any"),
                    },
                    handed_off: None,
                },
            },
        )
        .await;
    let refusal = refused.expect_err("a disjoint wire is refused by LashSession");
    assert!(
        refusal.to_string().contains("lash.wire_unsupported"),
        "{refusal}"
    );
    let turn = ingress
        .call_workflow_json::<_, Reply<RootOutcome>>(
            TURN_DRIVER_SERVICE,
            &format!("{}:{}a", session.as_str().len(), session.as_str()),
            "run",
            &Call {
                wire: newer,
                body: RestateTurnDriveRequest {
                    sender_generation: Some(lash_core::engine::BuildGeneration::for_test("any")),
                    admitted: admission_body::admitted(
                        session.clone(),
                        TurnId::from("a"),
                        request("newer-wire"),
                        lash_core::engine::AdmissionId::new("newer-wire"),
                        0,
                        lash_core::engine::BuildGeneration::for_test("any"),
                        lash_core::engine::AdmittedWork::Queued {
                            head: lash_core::BatchId::from("scripted-batch"),
                        },
                    ),
                },
            },
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
    assert_eq!(driver.ledger(&session).root_runs, 0);
}

/// HIGH-1 (a): a root whose `LashTurn` already ended is admitted again, by
/// the same drive and by the next one. Neither calls its key a second time
/// into a failure: the first drive consumes the released root and stops
/// `RootAborted` when admission names it again; the next drive's call finds
/// the key already run and attaches to the outcome the run recorded. No
/// drive fails and the root runs once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_whose_turn_already_ended_is_readmitted_without_a_second_run() {
    let (backend, driver, _installation) = fixture(18).await;
    let session = SessionId::from("drive-ended-root");
    driver.accept(&session, "a");
    driver.script("a", RootScript::Refuse);
    let engine = Arc::clone(backend.restate().session_work_engine());
    let root = TurnId::from("a");
    engine.schedule_drive(&session, request("r1"));
    let first = attach(&backend, &session, "r1").await;
    assert_eq!(first.ran, [RootOutcome::Released { root: root.clone() }]);
    assert_eq!(first.stop, DriveStop::RootAborted { root: root.clone() });
    engine.schedule_drive(&session, request("r2"));
    let second = attach(&backend, &session, "r2").await;
    assert_eq!(second, first, "the next drive attaches to the recorded end");
    settle(&backend).await;
    no_drive_failed(&backend);
    assert_eq!(driver.ledger(&session).root_runs, 1, "the root ran once");
    assert_eq!(driver.ledger(&session).open, ["a"], "nothing consumed it");
    assert_eq!(turn_invocations(&backend, "run"), 1, "one LashTurn run");
    assert_eq!(
        turn_invocations(&backend, "outcome"),
        2,
        "each drive read the run's recorded end, the second after a 409"
    );
}

/// The stop rules outlive a handoff (FIG-4523). With every await suspended,
/// the attempt that reads a root's end never started its leg, so the drive
/// hands off at that root's boundary and the next admission is another
/// invocation's. That leg starts from what the one before it remembers: a
/// released root admission names again stops the drive `RootAborted` there,
/// uncalled, where a leg that forgot it would call it, consume it and hand
/// off again without end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_root_admitted_again_after_a_handoff_stops_the_drive() {
    let backend = lash_restate_test::backend(0x4524, ServerConfig::default().always_replay(true))
        .await
        .expect("build the Restate test backend");
    let (driver, _installation) = install(&backend);
    let session = SessionId::from("drive-ended-root-handoff");
    driver.accept(&session, "a");
    driver.script("a", RootScript::Refuse);
    let root = TurnId::from("a");
    backend
        .restate()
        .session_work_engine()
        .schedule_drive(&session, request("r1"));
    let first = attach(&backend, &session, "r1").await;
    assert_eq!(first.ran, [RootOutcome::Released { root: root.clone() }]);
    assert_eq!(first.stop, DriveStop::HandedOff { root: root.clone() });
    // A waiter on the whole drive is answered the released root's refusal,
    // so the law reads the leg the drive handed off to by its own request.
    let next = lash_core::engine::drive_continuation_request(&DriveRequest {
        session: session.clone(),
        request: request("r1"),
        build_generation: backend.lash_backend().build_generation().clone(),
    });
    let second = attach(&backend, &session, next.as_str()).await;
    assert_eq!(second.ran, [], "the root is not called again");
    assert_eq!(second.stop, DriveStop::RootAborted { root });
    settle(&backend).await;
    no_drive_failed(&backend);
    assert_eq!(session_drives(&backend).len(), 2, "one handoff");
    assert_eq!(driver.ledger(&session).root_runs, 1, "the root ran once");
    assert_eq!(turn_invocations(&backend, "run"), 1, "one LashTurn run");
}

/// HIGH-1 (b): a queued root that cedes stops the drive. Admission would
/// mint a fresh root for the still-due queue on every ordinal; the drive
/// stops `Yielded` after the first instead of starting a `LashTurn` per
/// ordinal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_root_that_cedes_stops_the_drive() {
    let (backend, driver, _installation) = fixture(19).await;
    let session = SessionId::from("drive-ceded-root");
    driver.accept(&session, "q");
    driver.script("q", RootScript::Cede);
    backend
        .restate()
        .session_work_engine()
        .schedule_drive(&session, request("r1"));
    let outcome = attach(&backend, &session, "r1").await;
    let root = TurnId::from("q@r1#0");
    assert_eq!(outcome.ran, [RootOutcome::Ceded { root: root.clone() }]);
    assert_eq!(outcome.stop, DriveStop::Yielded { root });
    settle(&backend).await;
    no_drive_failed(&backend);
    assert_eq!(driver.ledger(&session).root_runs, 1);
    assert_eq!(turn_invocations(&backend, "run"), 1, "one LashTurn");
}

/// HIGH-1 (c): another driver seals the session after `LashSession`
/// admitted a root, so the root's seal is superseded and nothing runs. The
/// drive stops `Yielded`, fails nothing and burns no second `LashTurn`; once
/// the other driver answered the item, the session's next drive is idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_whose_seal_another_driver_superseded_stops_cleanly() {
    let (backend, driver, _installation) = fixture(20).await;
    let session = SessionId::from("drive-superseded");
    driver.accept(&session, "a");
    driver.script("a", RootScript::Supersede);
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_drive(&session, request("r1"));
    let outcome = attach(&backend, &session, "r1").await;
    let root = TurnId::from("a");
    assert_eq!(
        outcome.ran,
        [RootOutcome::Refused {
            root: root.clone(),
            verdict: SealVerdict::Superseded { epoch: 2 },
        }]
    );
    assert_eq!(outcome.stop, DriveStop::Yielded { root });
    driver.answer_elsewhere(&session, "a");
    engine.schedule_drive(&session, request("r2"));
    let next = attach(&backend, &session, "r2").await;
    assert!(next.ran.is_empty(), "{next:?}");
    assert_eq!(next.stop, DriveStop::Idle);
    settle(&backend).await;
    no_drive_failed(&backend);
    assert_eq!(driver.ledger(&session).root_runs, 1);
    assert_eq!(turn_invocations(&backend, "run"), 1, "one LashTurn");
}

/// A drive scheduled before any core installed its driver runs once one
/// does: the attempt fails retryably and the engine retries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_scheduled_before_the_install_runs_once_a_driver_is_installed() {
    let backend = lash_restate_test::backend(16, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let engine = Arc::clone(backend.restate().session_work_engine());
    let session = SessionId::from("drive-early");
    engine.schedule_drive(&session, request("r1"));
    let failed =
        async {
            loop {
                if backend.server().invocations().iter().any(|view| {
                    view.target.starts_with(SESSION_DRIVER_SERVICE) && view.attempts >= 1
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
    tokio::time::timeout(Duration::from_secs(20), failed)
        .await
        .expect("the early drive ran and failed");
    let driver = Arc::new(ScriptedDriver::default());
    driver.accept(&session, "a");
    let _installation =
        engine.install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
    let outcome = attach(&backend, &session, "r1").await;
    assert_eq!(committed_roots(&outcome), ["a"]);
}

/// A root run refused with a retryable-typed error — the shape a drive
/// racing a lane release leaves — keeps the attempt open: `LashTurn`
/// redelivers it instead of recording the run `Released`, so the drive's
/// attacher never reads back a refusal for what is a retry (FIG-3831).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retryable_refusal_of_a_root_run_redelivers_instead_of_releasing() {
    let (backend, driver, _installation) = fixture(22).await;
    let session = SessionId::from("drive-busy-lane");
    driver.accept(&session, "a");
    driver.script("a", RootScript::RefusedRetryable);
    let engine = Arc::clone(backend.restate().session_work_engine());
    engine.schedule_drive(&session, request("r1"));
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        engine.await_drive(&session, &request("r1")),
    )
    .await
    .expect("the drive ends")
    .expect("the refusal's redrive answers, not fails");
    assert_eq!(committed_roots(&outcome), ["a"]);
    settle(&backend).await;
    no_drive_failed(&backend);
    assert!(
        driver.ledger(&session).root_runs >= 2,
        "the refused attempt was redelivered"
    );
    let runs: Vec<_> = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| {
            view.target.starts_with(TURN_DRIVER_SERVICE) && view.target.ends_with("/run")
        })
        .collect();
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

/// The journal points of both handlers in one reference drive: every
/// command each stored, and every `ctx.run` result.
fn journal_points(backend: &RestateTestBackend, service: &str) -> Vec<CrashPoint> {
    let view = backend
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with(service))
        .unwrap_or_else(|| panic!("the reference drive ran {service}"));
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
/// the reference drive: every item is consumed once, in order, and the drive
/// answers the same roots. A drive whose crashed attempt replayed may answer
/// them over two invocations (FIG-4506), so the law reads the drive through
/// its continuations.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_crashed_at_any_journal_point_of_either_handler_consumes_each_item_once() {
    let seed = 17;
    let session = SessionId::from("drive-crash");
    let reference = {
        let (backend, driver, _installation) = fixture(seed).await;
        driver.accept(&session, "a");
        driver.accept(&session, "b");
        backend
            .restate()
            .session_work_engine()
            .schedule_drive(&session, request("r1"));
        let outcome = whole_drive(&backend, &session, "r1").await;
        settle(&backend).await;
        let points = [SESSION_DRIVER_SERVICE, TURN_DRIVER_SERVICE]
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
            let (backend, driver, _installation) = fixture(seed).await;
            if service == SESSION_DRIVER_SERVICE {
                backend.crash_session_drive(point.clone());
            } else {
                backend.crash_turn_drive(point.clone());
            }
            driver.accept(&session, "a");
            driver.accept(&session, "b");
            backend
                .restate()
                .session_work_engine()
                .schedule_drive(&session, request("r1"));
            let outcome = whole_drive(&backend, &session, "r1").await;
            assert_eq!(
                outcome, reference_outcome,
                "{service} {point:?}: the redriven drive answers the reference"
            );
            assert_eq!(
                driver.ledger(&session).consumed,
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
    println!("session drive crash matrix: {cases} journal points");
}

#[derive(Clone, Copy, Debug)]
enum ContinuationCut {
    BeforeSend,
    AfterSend,
    LastRootSettlement,
}

impl ContinuationCut {
    const ALL: [Self; 3] = [Self::BeforeSend, Self::AfterSend, Self::LastRootSettlement];

    fn rule(self, session: &SessionId) -> CrashRule {
        match self {
            Self::BeforeSend => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OneWayCallCommand,
            })
            .service(SESSION_DRIVER_SERVICE)
            .handler("drive")
            .key(session.as_str()),
            Self::AfterSend => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            })
            .service(SESSION_DRIVER_SERVICE)
            .handler("drive")
            .key(session.as_str()),
            Self::LastRootSettlement => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OutputCommand,
            })
            .service(TURN_DRIVER_SERVICE)
            .handler("run")
            .key(lash_restate::turn_workflow_key(
                session,
                &TurnId::from("item-63"),
            )),
        }
    }
}

fn schedule_continuation_case(
    backend: &lash_core::Backend,
    driver: &ScriptedDriver,
    session: &SessionId,
) -> (DriveRequestId, DriveRequestId) {
    for index in 0..65 {
        driver.accept(session, &format!("item-{index}"));
    }
    let initial = DriveRequest {
        session: session.clone(),
        request: request("bounded-crash"),
        build_generation: backend.build_generation().clone(),
    };
    let successor = lash_core::engine::drive_continuation_request(&initial);
    backend
        .session_work()
        .schedule_drive(session, initial.request.clone());
    (initial.request, successor)
}

/// `first` handed off after `first_roots` roots, and `next`, the rest of the
/// drive, ran the others.
fn assert_continuation_case(
    first: &DriveOutcome,
    first_roots: usize,
    next: &DriveOutcome,
    driver: &ScriptedDriver,
    session: &SessionId,
) {
    assert_eq!(
        committed_roots(first),
        (0..first_roots)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
    assert!(matches!(first.stop, DriveStop::HandedOff { .. }));
    assert_eq!(
        committed_roots(next),
        (first_roots..65)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(next.stop, DriveStop::Idle);
    assert_eq!(
        driver.ledger(session).consumed,
        (0..65)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
}

/// The roots a leg of the continuation case runs before it hands off. Under
/// always-replay no attempt past a leg's first await started the leg, so each
/// invocation hands off at its first boundary instead of its root bound
/// (FIG-4506, FIG-4523): the drive runs in legs of one root.
fn leg_roots(replay: bool) -> usize {
    if replay {
        1
    } else {
        lash_core::engine::MAX_ROOTS_PER_DRIVE
    }
}

/// The `LashSession` invocations the continuation case's 65 roots take: the
/// root-bound leg and its successor, or under always-replay one leg per root
/// and the leg the last of them hands off to, which admits nothing.
fn continuation_case_legs(replay: bool) -> usize {
    if replay { 66 } else { 2 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drive_continuation_crash_redrives_one_successor() {
    for replay in [false, true] {
        for cut in ContinuationCut::ALL {
            let backend =
                lash_restate_test::backend(0x4145, ServerConfig::default().always_replay(replay))
                    .await
                    .expect("build the backend");
            let driver = Arc::new(ScriptedDriver::default());
            let installed = backend
                .lash_backend()
                .session_work()
                .install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
            let session = SessionId::from("drive-continuation-crash");
            backend.server().crash_on(cut.rule(&session));
            let (initial, successor) =
                schedule_continuation_case(&backend.lash_backend(), &driver, &session);
            let first = tokio::time::timeout(
                Duration::from_secs(60),
                backend.attach_drive(&session, initial),
            )
            .await
            .expect("predecessor finishes")
            .expect("predecessor outcome");
            assert!(
                matches!(first.stop, DriveStop::HandedOff { .. }),
                "the bounded drive yielded before its successor"
            );
            let leg_roots = leg_roots(replay);
            let next = tokio::time::timeout(
                Duration::from_secs(60),
                backend
                    .lash_backend()
                    .session_work()
                    .await_drive(&session, &successor),
            )
            .await
            .expect("successor finishes")
            .expect("successor outcome");
            assert_continuation_case(&first, leg_roots, &next, &driver, &session);
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
                        == format!("{SESSION_DRIVER_SERVICE}/{session}/drive"))
                    .count(),
                continuation_case_legs(replay),
                "one invocation per leg, none sent twice: {cut:?}, replay={replay}"
            );
            assert_eq!(turn_invocations(&backend, "run"), 65);
            no_drive_failed(&backend);
            drop(installed);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the session-driver Restate suite runs it"]
async fn live_restate_drive_continuation_crash_redrives_one_successor() {
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
        let driver = Arc::new(ScriptedDriver::default());
        let installed = backend
            .lash_backend()
            .session_work()
            .install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
        let crashes = Arc::new(AtomicUsize::new(0));
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let counted = Arc::clone(&crashes);
        assert!(backend.on_crash(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            sender.send(()).expect("restart listener");
        })));
        let restart = {
            let backend = backend.clone();
            tokio::spawn(async move {
                while receiver.recv().await.is_some() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    backend.start_serving().await.expect("restart the endpoint");
                }
            })
        };
        let session = SessionId::from(tag);
        backend.crash_on(cut.rule(&session));
        let (initial, successor) =
            schedule_continuation_case(&backend.lash_backend(), &driver, &session);
        let first = tokio::time::timeout(
            Duration::from_secs(60),
            backend.attach_drive(&session, initial),
        )
        .await
        .expect("predecessor finishes")
        .expect("predecessor outcome");
        assert!(
            matches!(first.stop, DriveStop::HandedOff { .. }),
            "the bounded drive yielded before its successor"
        );
        let next = tokio::time::timeout(
            Duration::from_secs(120),
            backend
                .lash_backend()
                .session_work()
                .await_drive(&session, &successor),
        )
        .await
        .expect("successor finishes")
        .expect("successor outcome");
        assert_continuation_case(&first, leg_roots(replay), &next, &driver, &session);
        backend
            .settle(Duration::from_secs(20), Duration::from_millis(100))
            .await;
        assert_eq!(crashes.load(Ordering::SeqCst), 1, "{cut:?}");
        let invocations = backend.invocations().await.expect("read invocations");
        assert_eq!(
            invocations
                .iter()
                .filter(|invocation| invocation.target
                    == format!("{SESSION_DRIVER_SERVICE}/{session}/drive"))
                .count(),
            continuation_case_legs(replay),
            "one invocation per leg, none sent twice: {cut:?}, replay={replay}"
        );
        let roots: Vec<_> = invocations
            .iter()
            .filter(|invocation| {
                invocation.target.starts_with(&format!(
                    "{TURN_DRIVER_SERVICE}/{}",
                    lash_restate::turn_workflow_key(&session, &TurnId::from(""))
                )) && invocation.target.ends_with("/run")
            })
            .collect();
        assert_eq!(roots.len(), 65, "one invocation per root: {cut:?}");
        assert!(
            roots
                .iter()
                .all(|invocation| invocation.status == "completed")
        );
        restart.abort();
        backend.finish().await;
        drop(installed);
    }
}
