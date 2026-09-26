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

use std::collections::{BTreeMap, VecDeque};
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
use lash_restate::{
    LASH_SESSION_DRIVE_VERSION, RestateSessionDriveRequest, RestateTurnDriveRequest,
};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    CrashPoint, RestateTestBackend, SESSION_DRIVER_SERVICE, ServerConfig, TURN_DRIVER_SERVICE,
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
    /// A queued run that finds nothing it can claim: it cedes, the item
    /// stays due, and admission mints a fresh root for it every time.
    Cede,
}

#[derive(Default)]
struct ScriptedDriver {
    ledgers: Mutex<BTreeMap<SessionId, Ledger>>,
    gate: Mutex<Option<Arc<AdmissionGate>>>,
    scripts: Mutex<BTreeMap<String, RootScript>>,
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

    fn gate_for(&self, request: &DriveRequest, ordinal: u32) -> Option<Arc<AdmissionGate>> {
        self.gate
            .lock()
            .unwrap()
            .as_ref()
            .filter(|gate| gate.request == request.request.as_str() && gate.ordinal == ordinal)
            .cloned()
    }

    /// Admission's body: the oldest open item is the root, or nothing is.
    async fn admission(
        &self,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, RuntimeError> {
        let next = self.ledger(&request.session).open.front().cloned();
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
                lash_core::engine::AdmittedWork::Queued,
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
    async fn drive(&self, _request: DriveRequest) -> Result<DriveOutcome, DriveAbort> {
        Err(DriveAbort::Refused(runtime_error(
            "the scripted drive runs only split across the engine's handlers",
        )))
    }

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
    ) -> Result<RootOutcome, DriveAbort> {
        let root = admitted.root().clone();
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
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

async fn fixture(seed: u64) -> (RestateTestBackend, Arc<ScriptedDriver>) {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let driver = Arc::new(ScriptedDriver::default());
    let installed = backend
        .restate()
        .session_work_engine()
        .install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
    assert!(
        Arc::ptr_eq(&installed, &(Arc::clone(&driver) as Arc<dyn SessionDriver>)),
        "the first install is the engine's driver"
    );
    (backend, driver)
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
    let (backend, driver) = fixture(11).await;
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

/// A row committed while a drive runs, after that drive's last admission
/// read the ledger, is admitted by the next invocation: its schedule names
/// its own request, so the engine queues it behind the running drive instead
/// of deduplicating it into that drive (FIG-3600 ruling on O2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schedule_committed_while_a_drive_runs_is_admitted_by_the_next_invocation() {
    let (backend, driver) = fixture(12).await;
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

/// One request id is one drive: a repeated schedule of it attaches to the
/// first invocation, so an item committed after that drive's last admission
/// stays open until another request drives the session. This is why every
/// schedule names its own request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_request_id_is_one_drive() {
    let (backend, driver) = fixture(13).await;
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
    let (backend, driver) = fixture(14).await;
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

/// A request stamped with another generation is refused before either
/// handler journals anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_of_another_generation_is_refused_before_any_journal_command() {
    let (backend, driver) = fixture(15).await;
    let session = SessionId::from("drive-generation");
    driver.accept(&session, "a");
    let ingress = backend.ingress();
    for version in [0, LASH_SESSION_DRIVE_VERSION + 1] {
        let refused = ingress
            .call_object_json::<_, DriveOutcome>(
                SESSION_DRIVER_SERVICE,
                session.as_str(),
                "drive",
                &RestateSessionDriveRequest {
                    drive_version: version,
                    request: DriveRequest {
                        session: session.clone(),
                        request: request(&format!("generation-{version}")),
                        build_generation: lash_core::engine::BuildGeneration::for_test("any"),
                    },
                },
            )
            .await;
        assert!(refused.is_err(), "generation {version} is refused");
        let turn = ingress
            .call_workflow_json::<_, RootOutcome>(
                TURN_DRIVER_SERVICE,
                &format!("{}:{}a", session.as_str().len(), session.as_str()),
                "run",
                &RestateTurnDriveRequest {
                    drive_version: version,
                    sender_generation: Some(lash_core::engine::BuildGeneration::for_test("any")),
                    admitted: admission_body::admitted(
                        session.clone(),
                        TurnId::from("a"),
                        request("generation"),
                        lash_core::engine::AdmissionId::new("generation"),
                        0,
                        lash_core::engine::AdmittedWork::Queued,
                    ),
                },
            )
            .await;
        assert!(turn.is_err(), "generation {version} is refused by LashTurn");
    }
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
    let (backend, driver) = fixture(18).await;
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

/// HIGH-1 (b): a queued root that cedes stops the drive. Admission would
/// mint a fresh root for the still-due queue on every ordinal; the drive
/// stops `Yielded` after the first instead of starting a `LashTurn` per
/// ordinal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_root_that_cedes_stops_the_drive() {
    let (backend, driver) = fixture(19).await;
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
    let (backend, driver) = fixture(20).await;
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
    engine.install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
    let outcome = attach(&backend, &session, "r1").await;
    assert_eq!(committed_roots(&outcome), ["a"]);
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
/// answers the same roots.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_crashed_at_any_journal_point_of_either_handler_consumes_each_item_once() {
    let seed = 17;
    let session = SessionId::from("drive-crash");
    let reference = {
        let (backend, driver) = fixture(seed).await;
        driver.accept(&session, "a");
        driver.accept(&session, "b");
        backend
            .restate()
            .session_work_engine()
            .schedule_drive(&session, request("r1"));
        let outcome = attach(&backend, &session, "r1").await;
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
            let (backend, driver) = fixture(seed).await;
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
            let outcome = attach(&backend, &session, "r1").await;
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
