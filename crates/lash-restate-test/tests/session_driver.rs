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
    CrashPoint, CrashRule, RestateTestBackend, SESSION_DRIVER_SERVICE, ServerConfig,
    TURN_DRIVER_SERVICE,
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
    let driver = Arc::new(ScriptedDriver::default());
    let installed = backend
        .restate()
        .session_work_engine()
        .install_session_driver(Arc::clone(&driver) as Arc<dyn SessionDriver>);
    assert!(
        installed.runs_on(driver.as_ref()),
        "the first install is the engine's driver"
    );
    (backend, driver, installed)
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
        matches!(first.stop, DriveStop::Yielded { .. }),
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
/// answers the same roots.
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

fn assert_continuation_case(
    first: &DriveOutcome,
    next: &DriveOutcome,
    driver: &ScriptedDriver,
    session: &SessionId,
) {
    assert_eq!(
        committed_roots(first),
        (0..64)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
    assert!(matches!(first.stop, DriveStop::Yielded { .. }));
    assert_eq!(committed_roots(next), ["item-64"]);
    assert_eq!(next.stop, DriveStop::Idle);
    assert_eq!(
        driver.ledger(session).consumed,
        (0..65)
            .map(|index| format!("item-{index}"))
            .collect::<Vec<_>>()
    );
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
                matches!(first.stop, DriveStop::Yielded { .. }),
                "the bounded drive yielded before its successor"
            );
            let next = tokio::time::timeout(
                Duration::from_secs(60),
                backend.attach_drive(&session, successor),
            )
            .await
            .expect("successor finishes")
            .expect("successor outcome");
            assert_continuation_case(&first, &next, &driver, &session);
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
                2,
                "one predecessor and one successor: {cut:?}, replay={replay}"
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            matches!(first.stop, DriveStop::Yielded { .. }),
            "the bounded drive yielded before its successor"
        );
        let next = tokio::time::timeout(
            Duration::from_secs(60),
            backend.attach_drive(&session, successor),
        )
        .await
        .expect("successor finishes")
        .expect("successor outcome");
        assert_continuation_case(&first, &next, &driver, &session);
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
            2,
            "one predecessor and one successor: {cut:?}"
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
