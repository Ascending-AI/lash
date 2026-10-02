//! L9 (FIG-3795 §5.2): the session shift across a build roll, on the
//! multi-deployment server double.
//!
//! Two builds serve one `SessionShifts` — the double's stand-in for the one
//! store both deployments admit against: build N (drain generation `G_N`),
//! then build N+1 (`G_N+1`), each binding `LashSession` and `LashTurn`
//! under their stable name and their own `_g<G>` lane. The law: every
//! `ShiftRequestId` sent during the roll is admitted once across the
//! stable object and the `_g` resume handler — the item its admission
//! admits is consumed once, its run executes once, and a request a lane does
//! not serve is refused before it journals anything.

use super::*;
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    AttemptDispatch, CrashPoint, CrashRule, DeploymentHooks, DeploymentId, RestateTestServer,
    ServerConfig,
};

use crate::session_shifts::{RestateRunRequest, RestateSessionShiftRequest, turn_workflow_key};
use lash_core::SessionShifts;
use lash_core::engine::{
    AdmitVerdict, RunOutcome, ShiftAbort, ShiftOutcome, ShiftRequest, ShiftRequestId, ShiftStop,
    admission_body, shift_admission_replay_key,
};

pub(super) fn generation(build: &'static str) -> lash_core::engine::BuildGeneration {
    lash_core::engine::BuildGeneration::for_test(build)
}

pub(super) fn stable_session() -> crate::services::ServiceRoute {
    crate::services::DEFAULT_NAMESPACE.stable(LashService::SessionShifts)
}

fn session_lane(build: &'static str) -> crate::services::ServiceRoute {
    crate::services::DEFAULT_NAMESPACE.generation(LashService::SessionShifts, generation(build))
}

fn turn_lane(build: &'static str) -> crate::services::ServiceRoute {
    crate::services::DEFAULT_NAMESPACE.generation(LashService::TurnDriver, generation(build))
}

/// One session's items: open ones in arrival order, and every item a run
/// run consumed, in consumption order.
#[derive(Clone, Debug, Default)]
pub(super) struct Ledger {
    pub(super) open: VecDeque<String>,
    pub(super) consumed: Vec<String>,
}

/// A gate one `(request, ordinal)` admission waits at, after the `SessionShifts`'s
/// ledger read and before it answers: the shift is then in flight inside
/// its first admission, where the roll finds it.
pub(super) struct AdmissionGate {
    request: String,
    ordinal: u32,
    pub(super) reached: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

/// The shift both deployments install: one ledger for both builds, as the
/// one store is. `admissions` counts each admission body's executions — a
/// replay that re-ran its unjournaled body included — and `stamps` records
/// the generation each request carried when its admission first ran, which
/// the S9 stamp persists as `admitted_generation`.
#[derive(Default)]
pub(super) struct RollShifts {
    ledgers: Mutex<BTreeMap<SessionId, Ledger>>,
    admissions: Mutex<BTreeMap<(String, u32), usize>>,
    stamps: Mutex<BTreeMap<String, lash_core::engine::BuildGeneration>>,
    run_executions: Mutex<BTreeMap<String, usize>>,
    gate: Mutex<Option<Arc<AdmissionGate>>>,
}

impl RollShifts {
    pub(super) fn accept(&self, session: &SessionId, item: &str) {
        self.ledgers
            .lock_recover()
            .entry(session.clone())
            .or_default()
            .open
            .push_back(item.to_owned());
    }

    pub(super) fn ledger(&self, session: &SessionId) -> Ledger {
        self.ledgers
            .lock_recover()
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    fn admission_runs(&self, request: &str, ordinal: u32) -> usize {
        self.admissions
            .lock_recover()
            .get(&(request.to_owned(), ordinal))
            .copied()
            .unwrap_or_default()
    }

    fn stamp(&self, request: &str) -> Option<lash_core::engine::BuildGeneration> {
        self.stamps.lock_recover().get(request).cloned()
    }

    pub(super) fn runs_of(&self, run: &str) -> usize {
        self.run_executions
            .lock_recover()
            .get(run)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn gate(&self, request: &str, ordinal: u32) -> Arc<AdmissionGate> {
        let gate = Arc::new(AdmissionGate {
            request: request.to_owned(),
            ordinal,
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.gate.lock_recover() = Some(Arc::clone(&gate));
        gate
    }

    fn gate_for(&self, request: &ShiftRequest, ordinal: u32) -> Option<Arc<AdmissionGate>> {
        self.gate
            .lock_recover()
            .as_ref()
            .filter(|gate| gate.request == request.request.as_str() && gate.ordinal == ordinal)
            .cloned()
    }

    /// The admission body the journaled `AdmitShift` step wraps: the oldest
    /// open item is the run, or nothing is.
    async fn admission(
        &self,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
    ) -> AdmitVerdict {
        if let Some(gate) = self.gate_for(request, ordinal) {
            gate.reached.notify_one();
            gate.release.notified().await;
        }
        {
            *self
                .admissions
                .lock_recover()
                .entry((request.request.as_str().to_owned(), ordinal))
                .or_default() += 1;
            self.stamps
                .lock_recover()
                .entry(request.request.as_str().to_owned())
                .or_insert_with(|| admitting_generation.clone());
        }
        let next = self
            .ledgers
            .lock_recover()
            .get(&request.session)
            .and_then(|ledger| ledger.open.front().cloned());
        match next {
            Some(item) => AdmitVerdict::Admit(admission_body::admitted(
                request.session.clone(),
                TurnId::fixture(item),
                request.request.clone(),
                lash_core::engine::AdmissionId::new(format!(
                    "{}:{ordinal}",
                    request.request.as_str()
                )),
                0,
                admitting_generation.clone(),
                lash_core::engine::AdmittedWork::Queued {
                    head: lash_core::BatchId::from("scripted-batch"),
                },
            )),
            None => AdmitVerdict::Idle,
        }
    }
}

fn runtime_error(message: impl Into<String>) -> lash_core::RuntimeError {
    lash_core::RuntimeError::new(lash_core::RuntimeErrorCode::QueuedWork, message.into())
}

#[async_trait::async_trait]
impl SessionShifts for RollShifts {
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
                }),
            },
        );
        let verdict = self.admission(request, admitting_generation, ordinal).await;
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
        admitted: lash_core::engine::Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async {
                let run = admitted.run().clone();
                *self
                    .run_executions
                    .lock_recover()
                    .entry(run.as_str().to_owned())
                    .or_default() += 1;
                // Idempotent, like a commit fenced by its admission: a redrive of a
                // run that already consumed its item consumes nothing.
                let mut ledgers = self.ledgers.lock_recover();
                let ledger = ledgers.entry(admitted.session().clone()).or_default();
                if ledger.open.front().map(String::as_str) == Some(run.as_str()) {
                    ledger.open.pop_front();
                    ledger.consumed.push(run.as_str().to_owned());
                }
                Ok(RunOutcome::Committed {
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

/// The runner the roll's endpoints carry for the services L9 never sends
/// work to.
struct NoProcesses;

#[async_trait::async_trait]
impl RestateProcessRunner for NoProcesses {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &crate::SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        panic!("L9 sends no process segment")
    }
}

/// The endpoint URI build N registers at: its own, as every build's is
/// (ADR 0115 §3.5).
pub(super) const BUILD_N_URI: &str = "http://lash-roll.test/lash/n/build-n";

/// The two builds over one server, one `SessionShifts` and one store set.
pub(super) struct SessionRoll {
    pub(super) server: RestateTestServer,
    pub(super) ingress: RestateIngressClient,
    pub(super) shifts: Arc<RollShifts>,
    /// The slot's installation of `shifts`, kept for the roll's life.
    _installation: Arc<dyn SessionShifts>,
    endpoint_next: Mutex<Option<Endpoint>>,
    pub(super) deployment_n: DeploymentId,
    deployment_next: Mutex<Option<DeploymentId>>,
    served: Arc<Mutex<Vec<(String, AttemptDispatch)>>>,
}

impl SessionRoll {
    /// Build N registered and serving; build N+1 built, not yet registered.
    pub(super) async fn start(seed: u64) -> Self {
        let server = RestateTestServer::new(ServerConfig::default().with_seed(seed))
            .expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let ingress = RestateIngressClient::new(connection.clone());
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the shared SQLite memory store set");
        let registry = stores.process_registry();
        let continuations: Arc<dyn lash_core::ProcessContinuationStore> = registry.clone();
        let registry: Arc<dyn ProcessRegistry> = registry;
        let sessions = stores.session_store_factory() as Arc<dyn lash_core::DeploymentStore>;
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));

        let shifts = Arc::new(RollShifts::default());
        // One slot for both deployments, installed once: each build's
        // `LashSession`/`LashTurn` finds the same `SessionShifts`, as both builds of
        // a deployment family shift the same sessions' store.
        let slot = RestateSessionShiftsSlot::new();
        let installation = slot.install(Arc::clone(&shifts) as Arc<dyn SessionShifts>);
        let endpoint = |build: &'static str| {
            crate::services::bind_lash_services(
                Endpoint::builder(),
                crate::services::LashServiceParts {
                    effect_host: &host,
                    ingress: ingress.clone(),
                    admin: crate::RestateAdminClient::new(connection.clone()),
                    attachments: Arc::clone(&sessions) as Arc<dyn lash_core::AttachmentReferrers>,
                    sessions: Arc::clone(&sessions),
                    process_workflow: LashProcessWorkflowImpl::new(
                        Arc::new(NoProcesses),
                        Arc::clone(&registry),
                        continuations.clone(),
                        ingress.clone(),
                        Arc::new(lash_core::attachments::NoopAttachmentReferrers),
                        test_restate_authority_id(),
                        generation(build),
                        &crate::services::DEFAULT_NAMESPACE,
                    ),
                    session_shifts: slot.clone(),
                    build_generation: generation(build),
                    namespace: crate::RestateNamespace::default(),
                    fleet: crate::object_state::FleetView::default(),
                },
            )
            .build()
        };
        let served: Arc<Mutex<Vec<(String, AttemptDispatch)>>> = Arc::default();
        let deployment_n = server
            .register_at(
                endpoint("N"),
                BUILD_N_URI,
                "build-N",
                Self::recording("N", &served),
            )
            .await
            .expect("register build N");
        let endpoint_next = endpoint("N+1");
        Self {
            server,
            ingress,
            shifts,
            _installation: installation,
            endpoint_next: Mutex::new(Some(endpoint_next)),
            deployment_n,
            deployment_next: Mutex::default(),
            served,
        }
    }

    fn recording(
        build: &'static str,
        served: &Arc<Mutex<Vec<(String, AttemptDispatch)>>>,
    ) -> DeploymentHooks {
        let served = Arc::clone(served);
        DeploymentHooks {
            served: Some(Arc::new(move |dispatch: &AttemptDispatch| {
                served
                    .lock_recover()
                    .push((build.to_string(), dispatch.clone()));
            })),
            refuse: None,
        }
    }

    /// Register build N+1, once.
    pub(super) async fn register_next(&self) {
        let Some(endpoint) = self.endpoint_next.lock_recover().take() else {
            return;
        };
        let served = Arc::clone(&self.served);
        let id = self
            .server
            .register_with(endpoint, "build-N+1", Self::recording("N+1", &served))
            .await
            .expect("register build N+1");
        *self.deployment_next.lock_recover() = Some(id);
    }

    pub(super) fn deployment_next(&self) -> DeploymentId {
        self.deployment_next
            .lock_recover()
            .clone()
            .expect("build N+1 registered")
    }

    /// The request body `request`'s shift carries under `stamp`.
    pub(super) fn shift_body(
        session: &SessionId,
        request: &str,
        stamp: Option<&lash_core::engine::BuildGeneration>,
    ) -> RestateSessionShiftRequest {
        RestateSessionShiftRequest {
            request: ShiftRequest {
                session: session.clone(),
                request: ShiftRequestId::new(request),
                intended_lane: stamp.cloned(),
            },
            handed_off: None,
        }
    }

    /// Send `request`'s shift to `route`, keyed by the request id as the
    /// scheduler's sends are.
    pub(super) async fn send(
        &self,
        session: &SessionId,
        request: &str,
        stamp: &lash_core::engine::BuildGeneration,
        route: &crate::services::ServiceRoute,
    ) -> RestateInvocationId {
        self.ingress
            .send_object_json_idempotent(
                &route.name(),
                session.as_str(),
                "shift",
                &crate::Call::new(Self::shift_body(
                    session,
                    request,
                    matches!(route.lane(), crate::services::Lane::Generation(_)).then_some(stamp),
                )),
                request,
            )
            .await
            .expect("the shift send is accepted")
    }

    /// Attach to `request`'s shift under `route` and wait for its outcome.
    pub(super) async fn attach(
        &self,
        session: &SessionId,
        request: &str,
        stamp: &lash_core::engine::BuildGeneration,
        route: &crate::services::ServiceRoute,
    ) -> ShiftOutcome {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.ingress
                .call_object_json_idempotent::<_, crate::Reply<ShiftOutcome>>(
                    &route.name(),
                    session.as_str(),
                    "shift",
                    &crate::Call::new(Self::shift_body(
                        session,
                        request,
                        matches!(route.lane(), crate::services::Lane::Generation(_))
                            .then_some(stamp),
                    )),
                    request,
                ),
        )
        .await
        .expect("the shift ends")
        .expect("the shift's outcome")
        .into_body()
    }

    /// Every invocation of `target` the double saw.
    pub(super) fn invocations_of(&self, target: &str) -> Vec<lash_restate_test::InvocationView> {
        self.server
            .invocations()
            .into_iter()
            .filter(|view| view.target == target)
            .collect()
    }

    /// Wait (bounded) for `done`, letting the double settle between looks.
    async fn wait_for(&self, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the roll did not reach its expected state: {:#?}",
                self.server.invocations()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub(super) async fn settle(&self) {
        self.server.settle().await;
    }
}

/// The commands an invocation journaled past its input, by `(type, name)`.
fn journaled_commands(
    roll: &SessionRoll,
    invocation_id: &str,
) -> Vec<(MessageType, Option<String>)> {
    roll.server
        .journal(invocation_id)
        .expect("the invocation's journal")
        .iter()
        .filter(|entry| {
            !matches!(
                entry.ty,
                MessageType::InputCommand
                    | MessageType::RunCompletionNotification
                    | MessageType::OutputCommand
                    | MessageType::End
            )
        })
        .map(|entry| (entry.ty, entry.name.clone()))
        .collect()
}

/// What a misroute owes: a first-attempt refusal naming the misroute, with
/// nothing journaled (the generation sentinel rides the first admission,
/// FIG-3980).
fn assert_misrouted(roll: &SessionRoll, view: &lash_restate_test::InvocationView, case: &str) {
    assert_eq!(view.status, "completed", "{case}: {view:?}");
    assert_eq!(view.attempts, 1, "{case}: refused on the first attempt");
    let message = match roll.server.outcome(&view.id) {
        Some(Err((_, message))) => message,
        other => panic!("{case}: the misroute completed with a refusal: {other:?}"),
    };
    assert!(
        message.contains("misrouted"),
        "{case}: the refusal is typed: {message}"
    );
    assert_eq!(
        journaled_commands(roll, &view.id),
        Vec::new(),
        "{case}: nothing is journaled"
    );
}

/// L9: a shift request sent during the roll is admitted once, whichever
/// lane it lands on — stable on either build, or the resume-only `_g`
/// lane of the build its stamp names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l9_every_request_sent_during_the_roll_is_admitted_once() {
    let seed = 0x3795_e900;
    let gn = generation("N");
    let gn1 = generation("N+1");
    let roll = SessionRoll::start(seed).await;

    // A shift sent before the roll finishes it: pinned to N even though
    // N+1 registers while its first admission is open, and a re-send of
    // the same request id attaches to the live invocation instead of
    // executing twice.
    let session_held = SessionId::from("l9-held");
    roll.shifts.accept(&session_held, "h1");
    let gate = roll.shifts.gate("r-held", 0);
    let held_target = format!("LashSession/{session_held}/shift");
    let first = roll
        .send(&session_held, "r-held", &gn, &stable_session())
        .await;
    tokio::time::timeout(Duration::from_secs(60), gate.reached.notified())
        .await
        .expect("the held shift reached its gated admission");
    roll.register_next().await;
    let again = roll
        .send(&session_held, "r-held", &gn, &stable_session())
        .await;
    assert_eq!(
        again.as_str(),
        first.as_str(),
        "the re-send attached to the held invocation"
    );
    gate.release.notify_one();
    let outcome = roll
        .attach(&session_held, "r-held", &gn, &stable_session())
        .await;
    assert_eq!(
        outcome.ran.len(),
        1,
        "the held shift ran its one item: {outcome:?}"
    );
    let held = roll.invocations_of(&held_target);
    assert_eq!(held.len(), 1, "one invocation for one request id");
    assert_eq!(
        held[0].pinned_deployment_id,
        roll.deployment_n.as_str(),
        "a shift sent before the roll stays on the build it pinned"
    );

    // New work after the roll lands on the newest build, even a request a
    // stale sender stamped `G_N` — the stable lane takes every stamp. The
    // admission records the generation of the build that admits (S9,
    // FIG-4742): the run executes on N+1, so it is N+1's work, not N's.
    let session_new = SessionId::from("l9-new");
    roll.shifts.accept(&session_new, "n1");
    roll.shifts.accept(&session_new, "n2");
    roll.send(&session_new, "r-new", &gn, &stable_session())
        .await;
    let outcome = roll
        .attach(&session_new, "r-new", &gn, &stable_session())
        .await;
    assert_eq!(
        outcome.ran.len(),
        2,
        "the new shift ran both items: {outcome:?}"
    );
    let new = roll.invocations_of(&format!("LashSession/{session_new}/shift"));
    assert_eq!(new.len(), 1, "one invocation for the new shift");
    assert_eq!(
        new[0].pinned_deployment_id,
        roll.deployment_next().as_str(),
        "a stable-lane send after the roll lands on the newest build"
    );
    assert_eq!(
        roll.shifts.stamp("r-new"),
        Some(gn1.clone()),
        "the admission saw the admitting build's generation, which S9 records"
    );

    // A resume stamped `G_N` to `LashSession_g<G_N>` runs on build N, the
    // only deployment serving the lane; its admission ran there once.
    let session_resume = SessionId::from("l9-resume");
    roll.shifts.accept(&session_resume, "r1");
    roll.send(&session_resume, "r-resume", &gn, &session_lane("N"))
        .await;
    let outcome = roll
        .attach(&session_resume, "r-resume", &gn, &session_lane("N"))
        .await;
    assert_eq!(outcome.ran.len(), 1, "the resumed shift ran: {outcome:?}");
    let resumed = roll.invocations_of(&format!(
        "{}/{session_resume}/shift",
        session_lane("N").name()
    ));
    assert_eq!(resumed.len(), 1, "one resume invocation");
    assert_eq!(
        resumed[0].pinned_deployment_id,
        roll.deployment_n.as_str(),
        "only build N serves its own generation lane"
    );
    assert_eq!(roll.shifts.stamp("r-resume"), Some(gn.clone()));

    // The same request id sent to both lanes is admitted once across them:
    // whichever admission runs first consumes the item; the other idles, or
    // co-admits the run whose one `LashTurn` run both calls name.
    let session_both = SessionId::from("l9-both");
    roll.shifts.accept(&session_both, "b1");
    roll.send(&session_both, "r-both", &gn, &stable_session())
        .await;
    roll.send(&session_both, "r-both", &gn, &session_lane("N"))
        .await;
    let stable_outcome = roll
        .attach(&session_both, "r-both", &gn, &stable_session())
        .await;
    let lane_outcome = roll
        .attach(&session_both, "r-both", &gn, &session_lane("N"))
        .await;
    for outcome in [&stable_outcome, &lane_outcome] {
        assert!(
            outcome.ran.len() <= 1,
            "each shift consumed at most the shared item: {outcome:?}"
        );
    }
    assert_eq!(
        roll.shifts.ledger(&session_both).consumed,
        ["b1"],
        "the request's item was consumed once across both lanes"
    );
    assert_eq!(
        roll.shifts.runs_of("b1"),
        1,
        "the run ran once: one LashTurn key names it"
    );
    let turn_key = turn_workflow_key(&session_both, &TurnId::from("b1"));
    assert_eq!(
        roll.invocations_of(&format!("LashTurn/{turn_key}/run"))
            .len(),
        1,
        "one LashTurn run across the two admissions"
    );

    // A crash inside the resume's first admission: N dies after the
    // admission body ran but before its result was journaled; the replay
    // re-runs the body — the journal still records one admission, and the
    // item is consumed once.
    let session_crash = SessionId::from("l9-crash");
    roll.shifts.accept(&session_crash, "c1");
    roll.server.crash_on(
        CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some(format!(
                "lash:{}",
                shift_admission_replay_key(&ShiftRequestId::new("r-crash"), 0)
            )),
        })
        .service(session_lane("N").name().into_owned())
        .handler("shift")
        .key(session_crash.as_str())
        .within_attempts(1),
    );
    let initial = roll
        .send(&session_crash, "r-crash", &gn, &session_lane("N"))
        .await;
    let outcome = roll
        .attach(&session_crash, "r-crash", &gn, &session_lane("N"))
        .await;
    assert_eq!(outcome.ran.len(), 1, "the crashed shift ran: {outcome:?}");
    // The attempt that outlived the crash did not start the leg, so the
    // shift hands off at its first run's boundary (FIG-4556). The
    // continuation is this lane's: a resumed shift stays under the generation
    // its journal family belongs to, and nothing of it reaches the stable
    // name.
    assert!(
        matches!(outcome.stop, ShiftStop::HandedOff { .. }),
        "the replayed shift handed off at its first boundary: {outcome:?}"
    );
    let continuation = roll
        .server
        .journal(initial.as_str())
        .expect("the shift journal")
        .into_iter()
        .filter_map(|entry| entry.one_way_call_command())
        .find(|send| send.handler_name == "shift")
        .and_then(|send| send.idempotency_key)
        .expect("the recorded continuation's request id");
    let rest = roll
        .attach(&session_crash, &continuation, &gn, &session_lane("N"))
        .await;
    assert_eq!(
        (rest.ran.len(), &rest.stop),
        (0, &ShiftStop::Idle),
        "the continuation found nothing left"
    );
    let crashed = roll.invocations_of(&format!(
        "{}/{session_crash}/shift",
        session_lane("N").name()
    ));
    assert_eq!(
        crashed.len(),
        2,
        "the shift, replayed in place, and its continuation on the same lane"
    );
    assert_eq!(
        crashed[0].attempts, 2,
        "the crash forced one replay of the unjournaled admission"
    );
    assert_eq!(crashed[1].attempts, 1, "the continuation ran once");
    assert_eq!(
        crashed[1].pinned_deployment_id,
        roll.deployment_n.as_str(),
        "the continuation ran on the build that serves the lane"
    );
    assert_eq!(
        roll.invocations_of(&format!(
            "{}/{session_crash}/shift",
            stable_session().name()
        )),
        Vec::new(),
        "no leg of a shift resumed on its generation's lane ran under the stable name"
    );
    assert_eq!(
        roll.shifts.admission_runs("r-crash", 0),
        2,
        "the unjournaled admission body re-ran on the replay"
    );
    assert_eq!(
        roll.shifts.ledger(&session_crash).consumed,
        ["c1"],
        "re-running the unjournaled body consumed nothing twice"
    );

    // Red side: requests a lane does not serve are refused typed, before
    // any admission.
    let session_red = SessionId::from("l9-red");
    roll.shifts.accept(&session_red, "never");
    // A `G_{N+1}`-stamped request on `LashSession_g<G_N>`, and a
    // `G_N`-stamped request on `LashSession_g<G_{N+1}>`.
    let red_session_targets = [
        format!("{}/{session_red}/shift", session_lane("N").name()),
        format!("{}/{session_red}/shift", session_lane("N+1").name()),
    ];
    roll.send(&session_red, "r-wrong-new", &gn1, &session_lane("N"))
        .await;
    roll.send(&session_red, "r-wrong-old", &gn, &session_lane("N+1"))
        .await;
    roll.wait_for(|roll| {
        red_session_targets.iter().all(|target| {
            roll.invocations_of(target)
                .iter()
                .any(|view| view.status == "completed")
        })
    })
    .await;
    for target in &red_session_targets {
        let views = roll.invocations_of(target);
        assert_eq!(views.len(), 1, "one misrouted invocation for {target}");
        assert_misrouted(&roll, &views[0], target);
    }

    // A `LashTurn` run on a generation lane whose sender names another
    // generation — or none — is refused the same way.
    for (run, sender) in [("never-new", Some(gn1.clone())), ("never-none", None)] {
        let key = turn_workflow_key(&session_red, &TurnId::from(run));
        let target = format!("{}/{key}/run", turn_lane("N").name());
        roll.ingress
            .send_lash_workflow(
                &turn_lane("N").name(),
                &key,
                "run",
                &RestateRunRequest {
                    sender_generation: sender,
                    admitted: admission_body::admitted(
                        session_red.clone(),
                        TurnId::from(run),
                        ShiftRequestId::new(format!("r-turn-misroute-{run}")),
                        lash_core::engine::AdmissionId::new("misroute"),
                        0,
                        gn.clone(),
                        lash_core::engine::AdmittedWork::Queued {
                            head: lash_core::BatchId::from("scripted-batch"),
                        },
                    ),
                },
            )
            .await
            .expect("send the misrouted turn");
        roll.wait_for(|roll| {
            roll.invocations_of(&target)
                .iter()
                .any(|view| view.status == "completed")
        })
        .await;
        let views = roll.invocations_of(&target);
        assert_eq!(views.len(), 1, "one misrouted turn for {target}");
        assert_misrouted(&roll, &views[0], &target);
    }

    roll.settle().await;

    // Every dispatch a `G_N` lane took landed on build N, and every
    // `G_{N+1}` lane's on N+1: a lane is only ever its own build's.
    for (build, lane) in [("N", session_lane("N")), ("N+1", session_lane("N+1"))] {
        let name = lane.name().into_owned();
        assert!(
            roll.served
                .lock_recover()
                .iter()
                .filter(|(_, dispatch)| dispatch.service == name)
                .all(|(served, _)| served == build),
            "every dispatch of {name} landed on build {build}"
        );
    }

    // The ledger: every accepted item was consumed exactly once, except the
    // session whose only requests were misrouted.
    for (session, items) in [
        (session_held, vec!["h1"]),
        (session_new, vec!["n1", "n2"]),
        (session_resume, vec!["r1"]),
        (session_both, vec!["b1"]),
        (session_crash, vec!["c1"]),
    ] {
        let ledger = roll.shifts.ledger(&session);
        assert_eq!(
            ledger.consumed, items,
            "session `{session}` consumed each item once, none lost, none twice"
        );
        assert!(ledger.open.is_empty());
    }
    let red_ledger = roll.shifts.ledger(&session_red);
    assert_eq!(
        red_ledger.open,
        ["never"],
        "a misroute admitted nothing: the item stays open"
    );
    // Each run run ran once.
    for run in ["h1", "n1", "n2", "r1", "b1", "c1"] {
        assert_eq!(roll.shifts.runs_of(run), 1, "run `{run}` ran once");
    }
    // No session-shift invocation ended in an unexplained failure: the only
    // `Err` outcomes are the typed misroutes asserted above.
    for view in roll.server.invocations() {
        if !(view.target.starts_with("LashSession") || view.target.starts_with("LashTurn")) {
            continue;
        }
        if let Some(Err((_, message))) = roll.server.outcome(&view.id) {
            assert!(
                message.contains("misrouted"),
                "an unexplained shift failure: {view:?}: {message}"
            );
        }
    }
}
