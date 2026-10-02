//! Control operations against the same handlers on the double and live server.
mod refusal_classification;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, SessionId,
    SessionShifts, SessionWorkEngine, TurnId,
};
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Shifts {
    session: SessionId,
    run: TurnId,
    next: std::sync::Mutex<Option<TurnId>>,
    input: lash_core::InputId,
    store: lash_core::store::SessionStore,
    restored: AtomicBool,
    admission_fails: AtomicBool,
    /// Admission names the run again after it ended: a store that has not
    /// caught up with the run's release.
    repeat_released: AtomicBool,
    /// A failing admission answers D15's refusal: the session's park names a
    /// redrive that has not settled.
    redrive_unsettled: AtomicBool,
    /// The session holds only a queued command: admission names no run.
    command_only: bool,
    admits: AtomicUsize,
    commits: AtomicUsize,
}
fn fault() -> lash_core::RuntimeError {
    lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::PluginSessionManager,
        "injected execution fault",
    )
}
fn redrive_unsettled() -> lash_core::RuntimeError {
    lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::SessionRedriveUnsettled,
        "the session admits no turn input while its park names an unsettled redrive",
    )
}
/// One admission of a witness `SessionShifts`, recorded as the kernel records its own:
/// the `AdmitShift` step of `ordinal` on the shift's admission scope. `decide`
/// reads live state, so it runs only as the step's first execution; a replay
/// of the shift decodes the verdict that execution journaled and never runs
/// it. A fault of `decide` is the attempt's, never the step's outcome: the
/// engine's retry decides the admission again.
pub(super) async fn recorded_admission<Fut>(
    controller: &lash_core::ScopedEffectController<'_>,
    request: &ShiftRequest,
    admitting_generation: &lash_core::engine::BuildGeneration,
    ordinal: u32,
    decide: impl FnOnce() -> Fut + Send,
) -> Result<AdmitVerdict, ShiftAbort>
where
    Fut: Future<Output = Result<AdmitVerdict, lash_core::RuntimeError>> + Send,
{
    let address = EffectAddress::new(
        controller.execution_scope().clone(),
        shift_admission_replay_key(&request.request, ordinal),
    )
    .map_err(|error| ShiftAbort::Refused(lash_core::RuntimeError::from(error)))?;
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            address,
            RuntimeAttribution::for_session(request.session.clone()),
            format!("shift-admission-{ordinal}"),
        ),
        RuntimeEffectCommand::AdmitShift {
            request: Box::new(AdmitRequest {
                session: request.session.clone(),
                request: request.request.clone(),
                build_generation: admitting_generation.clone(),
            }),
        },
    );
    controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                match decide().await {
                    Ok(verdict) => Ok(RuntimeEffectOutcome::AdmitShift {
                        verdict: Box::new(verdict),
                    }),
                    Err(fault) => Err(lash_core::RuntimeEffectControllerError::from(fault)
                        .retryable_uncommitted_derivation()),
                }
            }),
        )
        .await
        .and_then(RuntimeEffectOutcome::into_admit_shift)
        .map_err(|error| ShiftAbort::Retry(error.into_runtime_error()))
}

impl Shifts {
    /// What the session admits now, read from its store.
    async fn decide_admission(
        &self,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
    ) -> Result<AdmitVerdict, lash_core::RuntimeError> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        if self.admission_fails.load(Ordering::SeqCst) {
            return Err(if self.redrive_unsettled.load(Ordering::SeqCst) {
                redrive_unsettled()
            } else {
                fault()
            });
        }
        if self.command_only {
            return Ok(AdmitVerdict::Idle);
        }
        let run = if !self.repeat_released.load(Ordering::SeqCst)
            && self
                .store
                .run_terminal(&self.run)
                .await
                .expect("terminal")
                .is_some()
        {
            let next = self.next.lock().expect("next run").clone();
            let Some(next) = next else {
                return Ok(AdmitVerdict::Idle);
            };
            if self
                .store
                .run_terminal(&next)
                .await
                .expect("next terminal")
                .is_some()
            {
                return Ok(AdmitVerdict::Idle);
            }
            next
        } else {
            self.run.clone()
        };
        Ok(AdmitVerdict::Admit(admission_body::admitted(
            self.session.clone(),
            run,
            request.request.clone(),
            AdmissionId::new(format!("{}#{ordinal}", request.request.as_str())),
            0,
            admitting_generation.clone(),
            AdmittedWork::Input {
                head: self.input.clone(),
            },
        )))
    }
}
#[async_trait::async_trait]
impl SessionShifts for Shifts {
    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        recorded_admission(&controller, request, admitting_generation, ordinal, || {
            self.decide_admission(request, admitting_generation, ordinal)
        })
        .await
    }
    async fn execute_run(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async {
                let run = admitted.run().clone();
                if !self.restored.load(Ordering::SeqCst) {
                    return Err(ShiftAbort::Retry(fault()));
                }
                let mut state =
                    lash_core::RuntimeSessionState::new(lash_core::testing::mock_session_policy());
                state.session_id = self.session.clone();
                let operation =
                    lash_core::OperationId::turn(self.session.clone(), run.clone(), "witness");
                let mut graph = state.pending_graph_commit();
                graph
                    .derive_node_ids(&self.session, &operation)
                    .expect("nodes");
                let mut commit =
                    lash_core::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
                        &state, graph, operation,
                    )
                    .expect("commit");
                commit.run_terminal = Some(Box::new(RunTerminalWrite {
                    run: run.clone(),
                    commit: TurnCommitId::new(run.clone(), 0),
                    turn: run.clone(),
                    outcome: lash_core::store::RunCommittedOutcome::Finished(
                        lash_core::facade_support::TurnFinish::AssistantMessage {
                            text: String::new(),
                        },
                    ),
                }));
                self.store
                    .commit_runtime_state(commit)
                    .await
                    .expect("commit run");
                self.commits.fetch_add(1, Ordering::SeqCst);
                Ok(RunOutcome::Committed {
                    run: run.clone(),
                    kind: lash_core::store::RunTerminalKind::Answered,
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
/// How `request`'s shift of `session` ended, read through every invocation
/// it handed off to: each leg's runs in order, and the last leg's stop. A
/// shift whose attempt replayed hands off at its next run boundary
/// (FIG-4506, FIG-4523), so one leg's answer is not the shift's.
#[expect(
    clippy::result_large_err,
    reason = "matches the ingress client's error API"
)]
pub(super) async fn attach_whole_shift(
    work: &crate::RestateSessionWork,
    session: &SessionId,
    request: ShiftRequestId,
) -> Result<ShiftOutcome, crate::SendShiftError> {
    let mut leg = ShiftRequest {
        session: session.clone(),
        request,
        // A continuation's request id names its session and request alone.
        intended_lane: None,
    };
    let mut ran = Vec::new();
    loop {
        let outcome = work.attach_shift(&leg.session, leg.request.clone()).await?;
        ran.extend(outcome.ran);
        if !matches!(
            outcome.stop,
            ShiftStop::HandedOff { .. } | ShiftStop::Draining { .. }
        ) {
            return Ok(ShiftOutcome {
                ran,
                stop: outcome.stop,
            });
        }
        leg.request = lash_core::engine::shift_continuation_request(&leg);
    }
}
struct Fixture {
    harness: LiveConformanceHarness,
    shifts: Arc<Shifts>,
    /// The engine's installation of `shifts`, kept for the fixture's life.
    _installation: Arc<dyn SessionShifts>,
    work: crate::RestateSessionWork,
    factory: Arc<dyn lash_core::DeploymentStore>,
    /// The law stores' `ControlIntent` obligation ledger.
    intents: Arc<dyn ObligationLedger>,
    /// The queued command of a [`Fixture::with_command`] session.
    batch: Option<lash_core::BatchId>,
}
/// The session work of `work` with its control engine replaced: an engine
/// half that fails where the fixture injects it.
struct WithControl {
    work: crate::RestateSessionWork,
    control: Arc<dyn SessionControlEngine>,
}
#[async_trait::async_trait]
impl SessionWorkEngine for WithControl {
    fn schedule_shift(&self, session: &SessionId, request: ShiftRequestId) {
        self.work.schedule_shift(session, request);
    }
    async fn request_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<(), EngineRefusal> {
        self.work.request_shift(session, request).await
    }
    fn install_session_shifts(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        self.work.install_session_shifts(shifts)
    }
    fn control(&self) -> Arc<dyn SessionControlEngine> {
        Arc::clone(&self.control)
    }
}
impl Fixture {
    async fn new(server: HarnessServer, admission: bool) -> Self {
        Self::build(server, admission, false).await
    }
    /// A session whose only work is a queued command: its next admission
    /// names no run.
    async fn with_command(server: HarnessServer) -> Self {
        Self::build(server, true, true).await
    }
    async fn build(server: HarnessServer, admission: bool, command_only: bool) -> Self {
        let harness = LiveConformanceHarness::start_on(server).await;
        Self::from_harness(harness, admission, command_only).await
    }
    async fn from_harness(
        harness: LiveConformanceHarness,
        admission: bool,
        command_only: bool,
    ) -> Self {
        let session = SessionId::fixture(format!("control-witness-{}", harness.run_nonce()));
        let run = TurnId::from("run");
        let factory = harness.law_stores().session_store_factory();
        let store = lash_core::runtime::admit_session_view(
            &factory,
            &lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                config: lash_core::testing::mock_session_policy().into(),
                head: lash_core::SessionCreationHead::Config,
                owning_process_id: None,
            },
        )
        .await
        .expect("store");
        let (input, batch) = if command_only {
            let batch = store
                .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
                    session.clone(),
                    lash_core::DeliveryPolicy::AfterCurrentTurnCommit,
                    lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                        reason: "the session's only work".into(),
                    },
                ))
                .await
                .expect("enqueue the command")
                .batch_id;
            (lash_core::InputId::from("no-input"), Some(batch))
        } else {
            let input = store
                .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                    session.clone(),
                    lash_core::TurnInputIngress::next_turn(),
                    lash_core::TurnInput::text("input"),
                ))
                .await
                .expect("enqueue")
                .input_id;
            store
                .bind_run_inputs(&run, std::slice::from_ref(&input))
                .await
                .expect("bind");
            (input, None)
        };
        let shifts = Arc::new(Shifts {
            session,
            run,
            next: std::sync::Mutex::new(None),
            input,
            store,
            restored: AtomicBool::new(false),
            admission_fails: AtomicBool::new(admission),
            repeat_released: AtomicBool::new(false),
            redrive_unsettled: AtomicBool::new(false),
            command_only,
            admits: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
        });
        let work = harness.session_work();
        let installation = work.install_session_shifts(shifts.clone());
        work.send_shift(&shifts.session, ShiftRequestId::new("initial"))
            .await
            .expect("send");
        let intents = harness
            .law_stores()
            .obligation_ledger(ObligationKind::ControlIntent);
        Self {
            harness,
            shifts,
            _installation: installation,
            work,
            factory,
            intents,
            batch,
        }
    }
    /// The `ControlIntent` relay over the fixture's ledger and engine —
    /// its control engine replaced by `control` when given — closing
    /// scopes through `scopes`.
    fn relay(
        &self,
        control: Option<Arc<dyn SessionControlEngine>>,
        scopes: Arc<dyn ScopeCloseSink>,
    ) -> lash_core::shift::ControlIntentRelay {
        self.relay_on(
            control,
            scopes,
            Arc::new(lash_core::facade_support::SystemClock),
        )
    }
    fn relay_on(
        &self,
        control: Option<Arc<dyn SessionControlEngine>>,
        scopes: Arc<dyn ScopeCloseSink>,
        clock: Arc<dyn lash_core::Clock>,
    ) -> lash_core::shift::ControlIntentRelay {
        let work: Arc<dyn SessionWorkEngine> = match control {
            Some(control) => Arc::new(WithControl {
                work: self.work.clone(),
                control,
            }),
            None => Arc::new(self.work.clone()),
        };
        let scope_close = Arc::new(lash_core::shift::ScopeCloseRelay::new(
            self.harness
                .law_stores()
                .obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&self.factory),
            Arc::clone(&scopes),
        ));
        lash_core::shift::ControlIntentRelay::new(
            Arc::clone(&self.intents),
            Arc::clone(&self.factory),
            work,
            scopes,
            scope_close,
            clock,
        )
    }
    async fn reconcile_until(&self, admission: bool) -> ParkReconcileReport {
        let clock = lash_core::facade_support::SystemClock;
        let writer = lash_core::shift::StoreParkRecovery::new(self.factory.as_ref(), &clock);
        for _ in 0..2000 {
            if let Some(server) = self.harness.server_double() {
                server.settle().await;
                server.fire_next_timer();
            }
            let report = self
                .work
                .control()
                .reconcile_parks(
                    &writer,
                    EnginePage {
                        budget: std::time::Duration::from_secs(1),
                        after: None,
                        limit: NonZeroUsize::MIN,
                    },
                )
                .await
                .expect("reconcile");
            let _ = admission;
            if !report.parked.is_empty() {
                return report;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("engine did not pause within the bounded witness");
    }
    /// Run the engine until the session's shift is paused.
    async fn await_paused_shift(&self) {
        let admin = self.harness.admin_client();
        for _ in 0..2000 {
            if let Some(server) = self.harness.server_double() {
                server.settle().await;
                server.fire_next_timer();
            }
            if !admin
                .paused_session_shifts(
                    &crate::services::DEFAULT_NAMESPACE,
                    self.shifts.session.as_str(),
                )
                .await
                .expect("paused shifts")
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the session's shift did not pause within the bounded witness");
    }
    /// The session's paused shifts now.
    async fn paused_shifts(&self) -> usize {
        self.harness
            .admin_client()
            .paused_session_shifts(
                &crate::services::DEFAULT_NAMESPACE,
                self.shifts.session.as_str(),
            )
            .await
            .expect("paused shifts")
            .len()
    }
    /// One park-reconcile pass over the whole listing.
    async fn park_pass(&self) -> ParkReconcileReport {
        let clock = lash_core::facade_support::SystemClock;
        let writer = lash_core::shift::StoreParkRecovery::new(self.factory.as_ref(), &clock);
        self.work
            .control()
            .reconcile_parks(
                &writer,
                EnginePage {
                    budget: std::time::Duration::from_secs(1),
                    after: None,
                    limit: NonZeroUsize::new(16).expect("page"),
                },
            )
            .await
            .expect("park pass")
    }
    /// [`attach_whole_shift`] on the fixture's session.
    #[expect(
        clippy::result_large_err,
        reason = "matches the ingress client's error API"
    )]
    pub(super) async fn attach_whole_shift(
        &self,
        request: ShiftRequestId,
    ) -> Result<ShiftOutcome, crate::SendShiftError> {
        attach_whole_shift(&self.work, &self.shifts.session, request).await
    }
    /// Attach to `request`'s shift, firing the double's timers while it
    /// runs; `None` when it did not end within the bounded witness.
    async fn attach_within(&self, request: ShiftRequestId) -> Option<ShiftOutcome> {
        let attach = self.attach_whole_shift(request);
        tokio::pin!(attach);
        for _ in 0..1000 {
            if let Some(server) = self.harness.server_double() {
                server.settle().await;
                server.fire_next_timer();
            }
            tokio::select! {
                outcome = &mut attach => return Some(outcome.expect("the shift answers")),
                () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
        None
    }
    async fn finish(&self) {
        self.harness.finish().await;
    }
}

/// An engine half whose resume ran but whose reply was lost: the redrive
/// intent stays open, retryable.
struct LostResumeReply {
    inner: Arc<dyn SessionControlEngine>,
}
#[async_trait::async_trait]
impl SessionControlEngine for LostResumeReply {
    async fn reconcile_parks(
        &self,
        writer: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        self.inner.reconcile_parks(writer, page).await
    }
    async fn resume_run(
        &self,
        target: &RunRef,
        engine: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_run(target, engine, children).await?;
        Err(EngineRefusal::retryable(
            lash_core::RuntimeErrorCode::EngineControlRequest,
            "lost reply after the engine resumed",
        ))
    }
    async fn release_run(
        &self,
        target: &RunRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.release_run(target, engine).await
    }
}

/// The engine half's reply, redelivered: the engine already resumed what the
/// redrive holds, and answers without touching it again.
struct ReplyOnly;
#[async_trait::async_trait]
impl SessionControlEngine for ReplyOnly {
    async fn reconcile_parks(
        &self,
        _: &dyn ParkRecoveryWriter,
        _: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        Ok(ParkReconcileReport::default())
    }
    async fn resume_run(
        &self,
        _: &RunRef,
        _: Option<&EnginePark>,
        _: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::Resumed)
    }
    async fn release_run(
        &self,
        _: &RunRef,
        _: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }
}

async fn pause_resume(server: HarnessServer) {
    let f = Fixture::new(server, false).await;
    let report = f.reconcile_until(false).await;
    assert_eq!(report.parked.len(), 1);
    let park = f
        .shifts
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("parked");
    assert!(park.engine.is_some());
    assert!(matches!(
        park.reason,
        ParkReason::EngineRetryExhausted { .. }
    ));
    assert!(
        f.factory
            .run_terminal(&f.shifts.session, &f.shifts.run)
            .await
            .expect("terminal")
            .is_none()
    );
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::shift::StoreParkRecovery::new(f.factory.as_ref(), &clock);
    f.work
        .control()
        .reconcile_parks(
            &writer,
            EnginePage {
                budget: std::time::Duration::from_secs(1),
                after: None,
                limit: NonZeroUsize::MIN,
            },
        )
        .await
        .expect("second pass");
    assert_eq!(
        f.shifts.store.load_turn_park().await.expect("park"),
        Some(park.clone())
    );
    f.shifts.restored.store(true, Ordering::SeqCst);
    let intent = f
        .factory
        .open_run_intent(
            &RunIntentRequest {
                session_id: f.shifts.session.clone(),
                run: f.shifts.run.clone(),
                park: park.park_id,
                verb: RunVerb::Redrive,
            },
            5,
        )
        .await
        .expect("redrive");
    f.relay(None, Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("apply");
    let outcome = f
        .attach_whole_shift(ShiftRequestId::new("initial"))
        .await
        .expect("resumed shift");
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 1);
    assert!(
        f.shifts
            .store
            .load_turn_park()
            .await
            .expect("park")
            .is_none()
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_reconcile_resume_preserves_the_journal_and_clears_the_park() {
    pause_resume(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_pause_reconcile_resume_preserves_the_journal_and_clears_the_park() {
    pause_resume(HarnessServer::Live).await;
}

/// ADR 0109 §3: a shift the engine paused in its admission is never resumed
/// by recovery. The pass parks the session's next run, a second pass leaves
/// the shift paused, and only the park's redrive resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_admission_is_parked_and_only_its_redrive_resumes_it() {
    let f = Fixture::new(HarnessServer::in_process(), true).await;
    let report = f.reconcile_until(true).await;
    assert_eq!(
        report.parked,
        vec![ParkTarget::Shift {
            session: f.shifts.session.clone()
        }]
    );
    let park = f
        .shifts
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("the paused shift parked its session's next run");
    assert_eq!(park.turn_id, f.shifts.run);
    assert!(matches!(
        park.reason,
        ParkReason::EngineRetryExhausted { .. }
    ));
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::shift::StoreParkRecovery::new(f.factory.as_ref(), &clock);
    let again = f
        .work
        .control()
        .reconcile_parks(
            &writer,
            EnginePage {
                budget: std::time::Duration::from_secs(1),
                after: None,
                limit: NonZeroUsize::MIN,
            },
        )
        .await
        .expect("second pass");
    assert_eq!((again.attached, again.parked.len()), (1, 0));
    assert_eq!(
        f.shifts.store.load_turn_park().await.expect("park"),
        Some(park.clone()),
        "a second pass writes nothing"
    );
    assert_eq!(
        f.harness
            .admin_client()
            .paused_session_shifts(
                &crate::services::DEFAULT_NAMESPACE,
                f.shifts.session.as_str()
            )
            .await
            .expect("paused shifts")
            .len(),
        1,
        "recovery never resumes the paused shift"
    );
    f.shifts.admission_fails.store(false, Ordering::SeqCst);
    f.shifts.restored.store(true, Ordering::SeqCst);
    let intent = f
        .factory
        .open_run_intent(
            &RunIntentRequest {
                session_id: f.shifts.session.clone(),
                run: f.shifts.run.clone(),
                park: park.park_id,
                verb: RunVerb::Redrive,
            },
            5,
        )
        .await
        .expect("redrive");
    f.relay(None, Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("apply");
    let outcome = f
        .attach_whole_shift(ShiftRequestId::new("initial"))
        .await
        .expect("the redrive resumed the shift");
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 1);
    assert!(
        f.shifts
            .store
            .load_turn_park()
            .await
            .expect("park")
            .is_none(),
        "the run's commit cleared the park"
    );
    f.finish().await;
}

/// FIG-3879 (D15): a shift the engine paused only because its session's park
/// named a redrive that had not settled — every attempt refused its admission
/// retryably, and the redrive's own engine half ran before the pause — is
/// resumed by the recovery pass once that redrive settles. It is never parked
/// again for a second operator redrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_paused_only_by_an_unsettled_redrive_is_resumed_once_it_settles() {
    let f = Fixture::new(HarnessServer::in_process(), true).await;
    f.reconcile_until(true).await;
    let park = f
        .shifts
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("the paused shift parked its session's next run");
    // The operator redrives. The engine half resumes the shift, but its reply
    // is lost: the intent stays open, and the resumed shift's admission meets
    // the unsettled redrive on every attempt until the engine pauses it again.
    f.shifts.redrive_unsettled.store(true, Ordering::SeqCst);
    let intent = f
        .factory
        .open_run_intent(
            &RunIntentRequest {
                session_id: f.shifts.session.clone(),
                run: f.shifts.run.clone(),
                park: park.park_id,
                verb: RunVerb::Redrive,
            },
            5,
        )
        .await
        .expect("redrive");
    let lost = f
        .relay(
            Some(Arc::new(LostResumeReply {
                inner: f.work.control(),
            })),
            Arc::new(NoScopeClose),
        )
        .deliver_intent(&intent)
        .await
        .expect("apply");
    assert_eq!(
        lost,
        ControlIntentState::Pending,
        "the lost reply leaves the redrive pending"
    );
    assert!(
        f.factory
            .load_intent(intent.id)
            .await
            .expect("intent")
            .expect("retained")
            .engine_half_owed(),
        "the lost reply keeps the redrive owed"
    );
    f.await_paused_shift().await;
    let open = f.park_pass().await;
    assert!(open.parked.is_empty(), "{open:?}");
    assert_eq!(f.paused_shifts().await, 1, "an open redrive owns the shift");
    // The redrive's reply lands: it settles without another engine call.
    let settled = f
        .relay(Some(Arc::new(ReplyOnly)), Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("settle");
    assert!(
        matches!(settled, ControlIntentState::Acknowledged { .. }),
        "{settled:?}"
    );
    f.shifts.redrive_unsettled.store(false, Ordering::SeqCst);
    f.shifts.admission_fails.store(false, Ordering::SeqCst);
    f.shifts.restored.store(true, Ordering::SeqCst);
    let pass = f.park_pass().await;
    assert!(
        pass.parked.is_empty(),
        "the settled redrive's shift is not parked again: {pass:?}"
    );
    assert_eq!(pass.resumed_shifts, vec![f.shifts.session.clone()]);
    let outcome = f
        .attach_within(ShiftRequestId::new("initial"))
        .await
        .expect("the pass resumed the shift the settled redrive left paused");
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 1);
    assert!(
        f.shifts
            .store
            .load_turn_park()
            .await
            .expect("park")
            .is_none(),
        "the run's commit cleared the park; no second redrive was needed"
    );
    f.finish().await;
}

/// FIG-3879: a paused shift whose session's next work names no run — here
/// only a queued command — has no park an operator verb could resume it
/// through. The recovery pass releases it, and the command lane executes the
/// session again: the command's ingress obligation asks for a fresh shift,
/// which the released session admits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_shift_with_no_run_to_park_is_released_to_its_command_lane() {
    let f = Fixture::with_command(HarnessServer::in_process()).await;
    f.await_paused_shift().await;
    let pass = f.park_pass().await;
    assert!(pass.parked.is_empty(), "nothing names a run: {pass:?}");
    assert_eq!(pass.released_shifts, vec![f.shifts.session.clone()]);
    f.shifts.admission_fails.store(false, Ordering::SeqCst);
    let admitted = f.shifts.admits.load(Ordering::SeqCst);
    // The command's ingress obligation is still owed: its relay asks for the
    // session's shift under its first attempt.
    let batch = f.batch.clone().expect("the queued command");
    let relay = lash_core::shift::IngressRelay::new(
        f.harness
            .law_stores()
            .obligation_ledger(ObligationKind::Ingress),
        Arc::new(f.work.clone()),
        Arc::new(lash_core::facade_support::SystemClock),
    );
    let asked = lash_core::shift::relay::relay_due(
        &relay,
        &lash_core::facade_support::SystemClock,
        NonZeroUsize::new(16).expect("page"),
    )
    .await
    .expect("relay pass");
    assert_eq!(
        asked.requested, 1,
        "the command's obligation asks: {asked:?}"
    );
    let outcome = f
        .attach_within(lash_core::shift::ingress_shift_request(
            batch.as_str(),
            lash_core::shift::FIRST_INGRESS_ATTEMPT,
        ))
        .await
        .expect("the command's shift runs: no paused shift holds the session");
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert!(
        f.shifts.admits.load(Ordering::SeqCst) > admitted,
        "the command's shift admitted the session"
    );
    assert_eq!(f.paused_shifts().await, 0, "no shift is left paused");
    f.finish().await;
}

struct InterruptedRelease {
    inner: Arc<dyn SessionControlEngine>,
    interrupt: AtomicBool,
}
#[async_trait::async_trait]
impl SessionControlEngine for InterruptedRelease {
    async fn reconcile_parks(
        &self,
        writer: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        self.inner.reconcile_parks(writer, page).await
    }
    async fn resume_run(
        &self,
        target: &RunRef,
        engine: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_run(target, engine, children).await
    }
    async fn release_run(
        &self,
        target: &RunRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let answer = self.inner.release_run(target, engine).await?;
        if self.interrupt.swap(false, Ordering::SeqCst) {
            return Err(EngineRefusal::retryable(
                lash_core::RuntimeErrorCode::EngineControlRequest,
                "lost acknowledgement after engine release",
            ));
        }
        Ok(answer)
    }
}
struct InterruptedClose {
    interrupt: AtomicBool,
    calls: AtomicUsize,
    factory: Arc<dyn lash_core::DeploymentStore>,
}
#[async_trait::async_trait]
impl ScopeCloseSink for InterruptedClose {
    async fn close_run_scope(&self, terminal: &RunTerminal) -> Result<(), lash_core::StoreError> {
        assert!(
            self.factory
                .run_terminal(&terminal.session_id, &terminal.run)
                .await?
                .is_some()
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.interrupt.swap(false, Ordering::SeqCst) {
            return Err(lash_core::StoreError::Backend(
                "lost acknowledgement after scope close".into(),
            ));
        }
        Ok(())
    }
    async fn close_session_scope(
        &self,
        session: &SessionId,
        _: ControlIntentId,
        runs: &[TurnId],
    ) -> Result<(), lash_core::StoreError> {
        for run in runs {
            let terminal = self
                .factory
                .run_terminal(session, run)
                .await?
                .expect("terminal precedes close");
            self.close_run_scope(&terminal).await?;
        }
        Ok(())
    }
}
async fn crash_gaps(server: HarnessServer) {
    for action in 0..3 {
        for gap in 0..3 {
            let f = Fixture::new(server.clone(), false).await;
            f.reconcile_until(false).await;
            let park = f
                .shifts
                .store
                .load_turn_park()
                .await
                .expect("park")
                .expect("parked");
            let intent = if action == 2 {
                f.factory
                    .begin_session_close(&f.shifts.session, 10)
                    .await
                    .expect("close")
                    .expect("intent")
            } else {
                f.factory
                    .open_run_intent(
                        &RunIntentRequest {
                            session_id: f.shifts.session.clone(),
                            run: f.shifts.run.clone(),
                            park: park.park_id,
                            verb: if action == 0 {
                                RunVerb::Cancel
                            } else {
                                RunVerb::Fork
                            },
                        },
                        10,
                    )
                    .await
                    .expect("verb")
            };
            // The build is restored: were the parked execution resumed
            // rather than released, it would commit.
            f.shifts.restored.store(true, Ordering::SeqCst);
            let scopes = Arc::new(InterruptedClose {
                interrupt: AtomicBool::new(gap == 2),
                calls: AtomicUsize::new(0),
                factory: f.factory.clone(),
            });
            if gap != 0 {
                let engine = Arc::new(InterruptedRelease {
                    inner: f.work.control(),
                    interrupt: AtomicBool::new(gap == 1),
                });
                let state = f
                    .relay(Some(engine), scopes.clone())
                    .deliver_intent(&intent)
                    .await
                    .expect("interrupted apply");
                if gap == 1 {
                    // A lost release leaves the intent pending: the failed
                    // attempt is its obligation's alone.
                    assert_eq!(state, ControlIntentState::Pending);
                } else {
                    // A missed scope close is its armed `ScopeClose`
                    // obligation's to retry: it never holds the intent open.
                    assert!(matches!(state, ControlIntentState::Acknowledged { .. }));
                }
            }
            // Reconstruct recovery from the durable ledger after dropping the
            // interrupted caller, an hour on, past the failed attempt's
            // backoff. The engine's retained invocation is shared.
            let later = LaterClock(3_600_000);
            let relays: Vec<Arc<dyn lash_core::shift::relay::ObligationRelay>> = vec![
                Arc::new(f.relay_on(None, scopes.clone(), Arc::new(LaterClock(3_600_000)))),
                Arc::new(lash_core::shift::ScopeCloseRelay::new(
                    f.harness
                        .law_stores()
                        .obligation_ledger(ObligationKind::ScopeClose),
                    Arc::clone(&f.factory),
                    scopes.clone(),
                )),
            ];
            let report = lash_core::shift::reconcile_once(
                &lash_core::shift::ReconcileParts {
                    sessions: f.factory.as_ref(),
                    work: &f.work,
                    scopes: scopes.as_ref(),
                    processes: None,
                    clock: &later,
                    duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                    relays: &relays,
                    lanes: &lash_conformance::law_tick_lanes(Arc::new(LaterClock(3_600_000))),
                },
                &ReconcileCursor::default(),
                NonZeroUsize::MIN.saturating_add(15),
            )
            .await;
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert!(matches!(
                f.factory
                    .load_intent(intent.id)
                    .await
                    .expect("intent")
                    .expect("retained")
                    .state,
                ControlIntentState::Acknowledged { .. }
            ));
            assert!(scopes.calls.load(Ordering::SeqCst) > 0);
            let outcome = f
                .attach_whole_shift(ShiftRequestId::new("initial"))
                .await
                .expect("released shift");
            assert!(matches!(
                outcome.ran.as_slice(),
                [RunOutcome::Released { .. }]
            ));
            assert_eq!(outcome.stop, ShiftStop::Idle);
            assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 0);
            // The release killed the parked execution: nothing is left for a
            // stale resume to wake.
            assert!(matches!(
                f.work
                    .control()
                    .resume_run(
                        &RunRef {
                            session: f.shifts.session.clone(),
                            run: f.shifts.run.clone(),
                        },
                        park.engine.as_ref(),
                        &[],
                    )
                    .await
                    .expect("status read"),
                EngineAck::NothingHeld
            ));
            assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 0);
            assert!(
                f.shifts
                    .store
                    .load_turn_park()
                    .await
                    .expect("park")
                    .is_none()
            );
            f.finish().await;
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_fork_and_close_recover_each_engine_crash_gap() {
    crash_gaps(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_cancel_fork_and_close_recover_each_engine_crash_gap() {
    crash_gaps(HarnessServer::Live).await;
}

async fn released_then_next(server: HarnessServer) {
    let f = Fixture::new(server, false).await;
    f.reconcile_until(false).await;
    let park = f
        .shifts
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("held");
    let intent = f
        .factory
        .open_run_intent(
            &RunIntentRequest {
                session_id: f.shifts.session.clone(),
                run: f.shifts.run.clone(),
                park: park.park_id,
                verb: RunVerb::Cancel,
            },
            8,
        )
        .await
        .expect("cancel");
    let next = TurnId::from("next");
    *f.shifts.next.lock().expect("next") = Some(next.clone());
    f.shifts.restored.store(true, Ordering::SeqCst);
    f.relay(None, Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("release");
    // The shift that consumes the release resumed a suspended attempt, so it
    // may hand off at the released run's boundary (FIG-4523). The release's own ask to work
    // queues behind this invocation, so after a handoff either shift admits
    // the next run: this one's continuation, or that ask's shift ahead of
    // it, which leaves the continuation nothing to admit.
    let outcome = f
        .attach_whole_shift(ShiftRequestId::new("initial"))
        .await
        .expect("shift");
    assert!(
        match outcome.ran.as_slice() {
            [RunOutcome::Released { run }] => *run == f.shifts.run,
            [
                RunOutcome::Released { run },
                RunOutcome::Committed { run: committed, .. },
            ] => *run == f.shifts.run && *committed == next,
            _ => false,
        },
        "{outcome:?}"
    );
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 1);
    f.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_run_is_consumed_and_the_next_run_executes() {
    released_then_next(HarnessServer::in_process()).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_released_run_is_consumed_and_the_next_run_executes() {
    released_then_next(HarnessServer::Live).await;
}

async fn released_then_repeated(server: HarnessServer) {
    let f = Fixture::new(server, false).await;
    f.reconcile_until(false).await;
    let park = f
        .shifts
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("held");
    let intent = f
        .factory
        .open_run_intent(
            &RunIntentRequest {
                session_id: f.shifts.session.clone(),
                run: f.shifts.run.clone(),
                park: park.park_id,
                verb: RunVerb::Cancel,
            },
            8,
        )
        .await
        .expect("cancel");
    f.shifts.repeat_released.store(true, Ordering::SeqCst);
    f.shifts.restored.store(true, Ordering::SeqCst);
    f.relay(None, Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("release");
    let outcome = f
        .attach_whole_shift(ShiftRequestId::new("initial"))
        .await
        .expect("shift");
    assert!(
        matches!(outcome.ran.as_slice(), [RunOutcome::Released { run }] if *run == f.shifts.run),
        "{:?}",
        outcome.ran
    );
    assert_eq!(
        outcome.stop,
        ShiftStop::RunAborted {
            run: f.shifts.run.clone()
        },
        "a released run admitted again stops the shift instead of spinning"
    );
    assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 0);
    f.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_run_admitted_again_stops_the_shift_run_aborted() {
    released_then_repeated(HarnessServer::in_process()).await;
}

/// A park writer whose every write fails: a store that is down for one
/// session while the engine's listing names it.
struct FailingParkWriter;
#[async_trait::async_trait]
impl ParkRecoveryWriter for FailingParkWriter {
    async fn record_engine_park(
        &self,
        _: &ParkTarget,
        _: ParkReason,
        _: EnginePark,
        _: &dyn StalledExecution,
    ) -> Result<EngineParkRecorded, lash_core::StoreError> {
        Err(lash_core::StoreError::Backend(
            "injected park write failure".into(),
        ))
    }
}

/// M2: one paused execution the pass cannot record never fails the page. The
/// failure is reported for that execution and the cursor moves past it, so
/// every later paused execution is still reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_failing_paused_execution_never_fails_the_park_page() {
    let f = Fixture::new(HarnessServer::in_process(), false).await;
    let admin = f.harness.admin_client();
    let server = f.harness.server_double().expect("double");
    let mut paused = Vec::new();
    for _ in 0..2000 {
        server.settle().await;
        server.fire_next_timer();
        paused = admin
            .paused_work_page(&crate::services::DEFAULT_NAMESPACE, None, NonZeroUsize::MIN)
            .await
            .expect("paused listing");
        if !paused.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(paused.len(), 1, "the run's execution pauses");
    let report = f
        .work
        .control()
        .reconcile_parks(
            &FailingParkWriter,
            EnginePage {
                budget: std::time::Duration::from_secs(1),
                after: None,
                limit: NonZeroUsize::MIN,
            },
        )
        .await
        .expect("a failing execution never fails the page");
    assert!(report.parked.is_empty());
    assert!(
        matches!(report.failed.as_slice(), [(execution, error)] if execution.as_str() == paused[0].id && error.contains("injected park write failure")),
        "{:?}",
        report.failed
    );
    assert_eq!(
        report.next,
        Some(EngineCursor::new(paused[0].id.clone())),
        "the cursor moves past the failed execution"
    );
    f.finish().await;
}

/// F12 (FIG-4648): a control verb Restate can never carry out is refused for
/// good under its typed code, so the intent's relay stalls it at once instead
/// of retrying it to the attempt ceiling. A stored engine handle that names
/// a run of another session is such a verb, for a resume and a release
/// alike.
async fn mismatched_handle(server: HarnessServer) {
    let f = Fixture::new(server, false).await;
    let admin = f.harness.admin_client();
    let mut paused = Vec::new();
    for _ in 0..2000 {
        if let Some(double) = f.harness.server_double() {
            double.settle().await;
            double.fire_next_timer();
        }
        paused = admin
            .paused_work_page(&crate::services::DEFAULT_NAMESPACE, None, NonZeroUsize::MIN)
            .await
            .expect("paused listing");
        if !paused.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(paused.len(), 1, "the run's execution pauses");
    // The handle names the paused run of the fixture's session; the verb
    // names a run of another session.
    let handle = EnginePark::new(paused[0].id.clone());
    let other = RunRef {
        session: SessionId::fixture(format!("{}-other", f.shifts.session)),
        run: f.shifts.run.clone(),
    };
    let control = f.work.control();
    for refusal in [
        control
            .resume_run(&other, Some(&handle), &[])
            .await
            .expect_err("a resume under another session's handle"),
        control
            .release_run(&other, Some(&handle))
            .await
            .expect_err("a release under another session's handle"),
    ] {
        assert_eq!(
            (refusal.disposition, &refusal.code),
            (
                RefusalClass::Permanent,
                &lash_core::RuntimeErrorCode::EngineHandleMismatch
            ),
            "{refusal:?}"
        );
        assert!(
            matches!(
                lash_core::shift::relay::DeliveryFailure::from(refusal),
                lash_core::shift::relay::DeliveryFailure::Refused(cause)
                    if cause.code == lash_core::RuntimeErrorCode::EngineHandleMismatch
            ),
            "the relay stalls it refused, under the engine's code"
        );
    }
    // The run the handle names was neither resumed nor killed.
    let after = admin
        .paused_work_page(&crate::services::DEFAULT_NAMESPACE, None, NonZeroUsize::MIN)
        .await
        .expect("paused listing");
    assert_eq!(
        after
            .iter()
            .map(|executed| &executed.id)
            .collect::<Vec<_>>(),
        vec![&paused[0].id]
    );
    // The refusals took nothing from the handle: under its own session it
    // still resumes the execution, which commits the run and ends the shift.
    f.shifts.restored.store(true, Ordering::SeqCst);
    let own = RunRef {
        session: f.shifts.session.clone(),
        run: f.shifts.run.clone(),
    };
    assert_eq!(
        control
            .resume_run(&own, Some(&handle), &[])
            .await
            .expect("a resume under the run's own session"),
        EngineAck::Resumed
    );
    let outcome = f
        .attach_within(ShiftRequestId::new("initial"))
        .await
        .expect("the resumed shift ends");
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.shifts.commits.load(Ordering::SeqCst), 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handle_naming_another_sessions_run_is_refused_for_good_with_its_code() {
    mismatched_handle(HarnessServer::in_process()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_handle_naming_another_sessions_run_is_refused_for_good_with_its_code() {
    mismatched_handle(HarnessServer::Live).await;
}

#[derive(Default)]
struct TickingShifts {
    ticks: AtomicUsize,
}
#[async_trait::async_trait]
impl SessionShifts for TickingShifts {
    fn owns_reconciliation(&self) -> bool {
        true
    }
    async fn reconcile(
        &self,
        cursor: &ReconcileCursor,
        _: NonZeroUsize,
    ) -> Result<ReconcileCursor, lash_core::StoreError> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(cursor.clone())
    }
    async fn admit(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        _: &ShiftRequest,
        _admitting_generation: &lash_core::engine::BuildGeneration,
        _: u32,
        _: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        panic!("the recovery interval never admits")
    }
    async fn execute_run(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        _: Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async { panic!("the recovery interval never executes a run") }.await,
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

/// A deployment's session work over an unreachable server: the recovery
/// interval only calls the installed `SessionShifts`.
fn detached_session_work() -> crate::RestateSessionWork {
    crate::RestateSessionWork::new(
        crate::RestateIngressClient::new(crate::RestateConnection::new("http://127.0.0.1:9")),
        crate::RestateSessionShiftsSlot::new(),
        lash_core::engine::EngineGeneration::fixed(BuildGeneration::for_test("recovery-interval")),
        crate::RestateNamespace::default(),
        Arc::new(NoEngineControl),
    )
}

async fn first_tick(shifts: &TickingShifts) {
    for _ in 0..500 {
        if shifts.ticks.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the recovery interval never ticked");
}

/// M1: a deployment runs one recovery interval however often its `SessionShifts` is
/// installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reinstalled_driver_runs_one_recovery_interval() {
    let work = detached_session_work();
    let shifts = Arc::new(TickingShifts::default());
    let installed = work.install_session_shifts(shifts.clone());
    let reinstalled = work.install_session_shifts(shifts.clone());
    first_tick(&shifts).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        shifts.ticks.load(Ordering::SeqCst),
        1,
        "a re-install of the live `SessionShifts` starts no second interval"
    );
    drop((installed, reinstalled));
}

/// The system clock its `0` milliseconds on: a reconcile tick run after a failed
/// attempt's backoff elapsed.
#[derive(Debug)]
struct LaterClock(u64);
#[async_trait::async_trait]
impl lash_core::Clock for LaterClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::milliseconds(i64::try_from(self.0).expect("offset"))
    }
    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }
    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

/// The live server's control engine behind a slow control RPC: each run
/// release waits `delay` before it reaches the server, as an admin call held
/// for its whole control timeout does. It records when each release and each
/// park reconcile began.
struct SlowControlRpc {
    inner: Arc<dyn SessionControlEngine>,
    delay: std::time::Duration,
    releases: std::sync::Mutex<Vec<std::time::Instant>>,
    parks: std::sync::Mutex<Vec<std::time::Instant>>,
}
#[async_trait::async_trait]
impl SessionControlEngine for SlowControlRpc {
    async fn reconcile_parks(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        self.parks
            .lock()
            .expect("parks")
            .push(std::time::Instant::now());
        self.inner.reconcile_parks(parks, page).await
    }
    async fn resume_run(
        &self,
        target: &RunRef,
        handle: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_run(target, handle, children).await
    }
    async fn release_run(
        &self,
        target: &RunRef,
        handle: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        self.releases
            .lock()
            .expect("releases")
            .push(std::time::Instant::now());
        tokio::time::sleep(self.delay).await;
        self.inner.release_run(target, handle).await
    }
}

/// A scope owner that records when each run scope closed.
#[derive(Default)]
struct RecordedCloses {
    runs: std::sync::Mutex<Vec<std::time::Instant>>,
}
#[async_trait::async_trait]
impl ScopeCloseSink for RecordedCloses {
    async fn close_run_scope(&self, _: &RunTerminal) -> Result<(), lash_core::StoreError> {
        self.runs
            .lock()
            .expect("closes")
            .push(std::time::Instant::now());
        Ok(())
    }
    async fn close_session_scope(
        &self,
        _: &SessionId,
        _: ControlIntentId,
        _: &[TurnId],
    ) -> Result<(), lash_core::StoreError> {
        Ok(())
    }
}

/// A deployment's recovery pass over the law's stores: every tick is the
/// kernel's `reconcile_once` on the deployment's own lanes, recorded.
struct CadenceShifts {
    factory: Arc<dyn lash_core::DeploymentStore>,
    work: WithControl,
    scopes: Arc<RecordedCloses>,
    relays: Vec<Arc<dyn lash_core::shift::relay::ObligationRelay>>,
    lanes: lash_core::shift::RelayLanes,
    ticks: std::sync::Mutex<Vec<(std::time::Instant, ReconcileTick)>>,
}
#[async_trait::async_trait]
impl SessionShifts for CadenceShifts {
    fn owns_reconciliation(&self) -> bool {
        true
    }
    async fn reconcile(
        &self,
        cursor: &ReconcileCursor,
        page: NonZeroUsize,
    ) -> Result<ReconcileCursor, lash_core::StoreError> {
        let ticked_at = std::time::Instant::now();
        let tick = lash_core::shift::reconcile_once(
            &lash_core::shift::ReconcileParts {
                sessions: self.factory.as_ref(),
                work: &self.work,
                scopes: self.scopes.as_ref(),
                processes: None,
                clock: &lash_core::facade_support::SystemClock,
                duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                relays: &self.relays,
                lanes: &self.lanes,
            },
            cursor,
            page,
        )
        .await;
        let next = tick.next.clone();
        self.ticks.lock().expect("ticks").push((ticked_at, tick));
        Ok(next)
    }
    async fn admit(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        _: &ShiftRequest,
        _admitting_generation: &lash_core::engine::BuildGeneration,
        _: u32,
        _: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        panic!("the recovery interval never admits")
    }
    async fn execute_run(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        _: Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async { panic!("the recovery interval never executes a run") }.await,
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

/// ADR 0109 §1.8 on a live server: the deployment's recovery interval keeps
/// its cadence while a control RPC is slow. A closed session's release — the
/// intent's control RPC — is held for a minute. The production interval
/// fires every `T` regardless; each tick's parks run against the server
/// within its lane wait; the closed run's scope close, a later kind, is
/// delivered in the first tick; the intent's kind is busy while its attempt
/// runs; and the attempt is cut at the host's budget and retried by the
/// first tick its lane is free for.
async fn recovery_tick_keeps_cadence_with_slow_control_rpc(server: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let stores = harness.law_stores();
    let factory = stores.session_store_factory();
    let session = SessionId::fixture(format!("cadence-{}", harness.run_nonce()));
    let run = TurnId::from("open-run");
    factory
        .admit_session(&lash_core::SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: vec![],
            config: lash_core::testing::mock_session_policy().into(),
            head: lash_core::SessionCreationHead::Config,
            owning_process_id: None,
        })
        .await
        .expect("session");
    factory
        .bind_run_inputs(&session, &run, &[])
        .await
        .expect("an open run");
    let now_ms = || lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock);
    let intent = factory
        .begin_session_close(&session, now_ms())
        .await
        .expect("close")
        .expect("the session exists");
    // The host's attempt budget; the release takes five times as long.
    let budget = std::time::Duration::from_secs(12);
    let tick = lash_core::shift::RECOVERY_TICK;
    let slack = std::time::Duration::from_millis(1_500);
    let pass = lash_core::engine::RecoveryPassBudget {
        attempt: budget,
        ..lash_core::engine::RecoveryPassBudget::default()
    };
    let policy = lash_core::shift::relay::RelayPolicy {
        attempt_budget_ms: pass.attempt_ms(),
        ..lash_core::shift::relay::RelayPolicy::default()
    };
    let session_work = harness.session_work();
    let control = Arc::new(SlowControlRpc {
        inner: session_work.control(),
        delay: 5 * budget,
        releases: std::sync::Mutex::new(Vec::new()),
        parks: std::sync::Mutex::new(Vec::new()),
    });
    let work = WithControl {
        work: session_work.clone(),
        control: Arc::clone(&control) as Arc<dyn SessionControlEngine>,
    };
    let scopes = Arc::new(RecordedCloses::default());
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::facade_support::SystemClock);
    let scope_close = Arc::new(
        lash_core::shift::ScopeCloseRelay::new(
            stores.obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&factory),
            Arc::clone(&scopes) as Arc<dyn ScopeCloseSink>,
        )
        .with_policy(policy),
    );
    let intent_relay = Arc::new(
        lash_core::shift::ControlIntentRelay::new(
            stores.obligation_ledger(ObligationKind::ControlIntent),
            Arc::clone(&factory),
            Arc::new(WithControl {
                work: session_work.clone(),
                control: Arc::clone(&control) as Arc<dyn SessionControlEngine>,
            }),
            Arc::clone(&scopes) as Arc<dyn ScopeCloseSink>,
            Arc::clone(&scope_close) as Arc<dyn lash_core::shift::relay::ObligationRelay>,
            Arc::clone(&clock),
        )
        .with_policy(policy),
    );
    let shifts = Arc::new(CadenceShifts {
        factory: Arc::clone(&factory),
        work,
        scopes: Arc::clone(&scopes),
        relays: vec![intent_relay, scope_close],
        lanes: lash_conformance::deployment_tick_lanes(Arc::clone(&clock), pass),
        ticks: std::sync::Mutex::new(Vec::new()),
    });
    let started = std::time::Instant::now();
    let installation = session_work.install_session_shifts(shifts.clone());
    tokio::time::sleep(budget + 2 * tick + slack).await;
    drop(installation);

    let ticks: Vec<(std::time::Instant, ReconcileTick)> = shifts
        .ticks
        .lock()
        .expect("ticks")
        .iter()
        .map(|(at, tick)| (*at, tick.clone()))
        .collect();
    let offsets: Vec<std::time::Duration> = ticks.iter().map(|(at, _)| *at - started).collect();
    assert!(
        ticks.len() >= 4,
        "the interval ticked through the slow release: {offsets:?}"
    );
    // Cadence: the interval fires on its grid, whatever the release spends.
    let first = ticks[0].0;
    for (index, (at, _)) in ticks.iter().enumerate() {
        let due = first + tick * u32::try_from(index).expect("tick index");
        assert!(
            *at + std::time::Duration::from_millis(50) >= due && *at <= due + slack,
            "tick {index} fired {:?} after the first, due {:?}: {offsets:?}",
            *at - first,
            due - first
        );
    }
    // Parks: each tick's park reconcile reached the server within its lane
    // wait.
    let parks = control.parks.lock().expect("parks").clone();
    assert!(parks.len() >= ticks.len() - 1, "{} parks", parks.len());
    for ((at, _), park) in ticks.iter().zip(&parks) {
        assert!(
            *park <= *at + pass.tick_wait + slack,
            "the parks of the tick at {:?} ran {:?} after it",
            *at - first,
            *park - *at
        );
    }
    // A later kind: the closed run's scope close was delivered in the first
    // tick.
    let closes = scopes.runs.lock().expect("closes").clone();
    let [closed] = closes.as_slice() else {
        panic!("one run scope close: {} of them", closes.len());
    };
    assert!(
        *closed <= first + slack,
        "closed {:?} after the first tick",
        *closed - first
    );
    // The slow kind: busy, never re-claimed, while its attempt ran.
    for (at, tick) in &ticks[1..] {
        if *at + slack < first + budget {
            assert_eq!(
                tick.obligations_busy,
                vec![ObligationKind::ControlIntent],
                "the tick {:?} after the first found the intent's lane busy",
                *at - first
            );
        }
    }
    // The attempt was cut at the budget and retried by a later tick.
    let releases = control.releases.lock().expect("releases").clone();
    let [cut, retried, ..] = releases.as_slice() else {
        panic!("the cut attempt was retried: {} releases", releases.len());
    };
    assert!(
        *retried >= *cut + budget && *retried <= *cut + budget + tick + slack,
        "the retry began {:?} after the cut attempt",
        *retried - *cut
    );
    assert_eq!(
        factory
            .load_intent(intent.id)
            .await
            .expect("intent")
            .expect("retained")
            .state,
        ControlIntentState::Pending
    );
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_recovery_tick_keeps_cadence_with_slow_control_rpc() {
    recovery_tick_keeps_cadence_with_slow_control_rpc(HarnessServer::Live).await;
}

/// FIG-4281: a run that admits its input, records its admission and starts
/// an effect, then blocks inside it. Every execution of the run is counted:
/// recovery must never start another one under a fresh journal.
struct StartedRunShifts {
    session: SessionId,
    run: TurnId,
    input: lash_core::InputId,
    store: lash_core::store::SessionStore,
    admits: AtomicUsize,
    executions: AtomicUsize,
    effects: AtomicUsize,
}
impl StartedRunShifts {
    /// What the session admits now, read from its store.
    async fn decide_admission(
        &self,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
    ) -> Result<AdmitVerdict, lash_core::RuntimeError> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        if self
            .store
            .run_terminal(&self.run)
            .await
            .expect("terminal")
            .is_some()
        {
            return Ok(AdmitVerdict::Idle);
        }
        Ok(AdmitVerdict::Admit(admission_body::admitted(
            self.session.clone(),
            self.run.clone(),
            request.request.clone(),
            AdmissionId::new(format!("{}#{ordinal}", request.request.as_str())),
            0,
            admitting_generation.clone(),
            AdmittedWork::Input {
                head: self.input.clone(),
            },
        )))
    }
}
#[async_trait::async_trait]
impl SessionShifts for StartedRunShifts {
    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        recorded_admission(&controller, request, admitting_generation, ordinal, || {
            self.decide_admission(request, admitting_generation, ordinal)
        })
        .await
    }
    async fn execute_run(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async {
                self.executions.fetch_add(1, Ordering::SeqCst);
                let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
                    self.store.store(),
                    &self.session,
                    "started-run",
                )
                .await;
                lash_core::testing::store_fixtures::admit_run_for_test(
                    self.store.store(),
                    &fence,
                    admitted.run(),
                    AdmittedHead::Input(self.input.clone()),
                )
                .await
                .expect("admit the run")
                .expect("the run's admission reaches its head");
                self.effects.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<RunOutcome, ShiftAbort>>().await
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

/// Let the double run what it can for a moment. A started run blocks inside
/// its effect on purpose, so the double never settles while it runs.
async fn settle_briefly(server: &lash_restate_test::RestateTestServer) {
    let _ = tokio::time::timeout(std::time::Duration::from_millis(50), server.settle()).await;
}

/// An admin endpoint that answers every request 503: the engine's state is
/// unreadable, which proves nothing about any run.
#[derive(Debug)]
struct AdminOutage;
#[async_trait::async_trait]
impl lash_http_transport::HttpTransport for AdminOutage {
    async fn send(
        &self,
        _request: lash_http_transport::HttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_http_transport::HttpResponse, lash_http_transport::LlmTransportError> {
        Ok(lash_http_transport::HttpResponse {
            status: 503,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: lash_http_transport::HttpResponseBody::buffered("admin unavailable".to_string()),
        })
    }
}

/// A run admits its input and starts its effect; an operator kills its
/// `LashTurn` run, the shift consumes the release and stops `RunAborted`,
/// and the run's record is purged before any recovery pass. The recovery
/// pass alone ends the run `SubstrateLost` and settles its input: no new
/// submission, no new shift, no second execution. With `admin_outage`, a
/// pass whose admin read fails first ends nothing.
async fn missing_started_run(server: HarnessServer, admin_outage: bool) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let session = SessionId::fixture(format!("lost-run-{}", harness.run_nonce()));
    let run = TurnId::from("lost-run");
    let factory = harness.law_stores().session_store_factory();
    let store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: vec![],
            config: lash_core::testing::mock_session_policy().into(),
            head: lash_core::SessionCreationHead::Config,
            owning_process_id: None,
        },
    )
    .await
    .expect("store");
    let input = store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session.clone(),
                lash_core::TurnInputIngress::next_turn(),
                lash_core::TurnInput::text("input"),
            )
            .with_source_key(run.as_str()),
        )
        .await
        .expect("enqueue")
        .input_id;
    let shifts = Arc::new(StartedRunShifts {
        session: session.clone(),
        run: run.clone(),
        input: input.clone(),
        store,
        admits: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
    });
    let work = harness.session_work();
    let _installation = work.install_session_shifts(shifts.clone());
    work.send_shift(&session, ShiftRequestId::new("initial"))
        .await
        .expect("send");
    for _ in 0..2000 {
        if shifts.effects.load(Ordering::SeqCst) > 0 {
            break;
        }
        if let Some(server) = harness.server_double() {
            settle_briefly(&server).await;
            server.fire_next_timer();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        shifts.effects.load(Ordering::SeqCst),
        1,
        "the run started its one effect"
    );

    let turn = crate::RestateNamespace::default()
        .stable(crate::LashService::TurnDriver)
        .name()
        .into_owned();
    let key = crate::session_shifts::turn_workflow_key(&session, &run);
    let killed = harness.harness_admin().kill_workflow_run(&turn, &key).await;
    let attach = attach_whole_shift(&work, &session, ShiftRequestId::new("initial"));
    tokio::pin!(attach);
    let mut outcome = None;
    for _ in 0..2000 {
        if let Some(server) = harness.server_double() {
            settle_briefly(&server).await;
            server.fire_next_timer();
        }
        tokio::select! {
            ended = &mut attach => {
                outcome = Some(ended.expect("the shift answers"));
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
    let outcome = outcome.expect("the shift ends within the bounded witness");
    assert!(
        matches!(outcome.ran.as_slice(), [RunOutcome::Released { run: released }] if *released == run),
        "{:?}",
        outcome.ran
    );
    assert_eq!(outcome.stop, ShiftStop::RunAborted { run: run.clone() });
    harness.harness_admin().purge_invocation(&killed).await;
    assert!(
        harness
            .admin_client()
            .run_executions(
                &crate::RestateNamespace::default(),
                std::slice::from_ref(&key)
            )
            .await
            .expect("run executes")
            .is_empty(),
        "the engine holds no execution of the run on any lane"
    );
    let executions = shifts.executions.load(Ordering::SeqCst);
    let admits = shifts.admits.load(Ordering::SeqCst);
    let target = RunRef {
        session: session.clone(),
        run: run.clone(),
    };

    if admin_outage {
        let outage = crate::RestateAdminClient::new(crate::RestateConnection::with_transport(
            "http://admin.invalid",
            Arc::new(AdminOutage),
        ));
        let read = crate::session_control::end_lost_run_executions(
            &outage,
            &crate::RestateIngressClient::new(harness.connection()),
            &crate::RestateNamespace::default(),
            &factory,
            &harness.law_stores().process_registry(),
            crate::session_control::RecoveryScan {
                limit: NonZeroUsize::new(16).expect("page"),
                after: &mut None,
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            },
        )
        .await;
        assert!(
            read.is_err(),
            "an unreadable engine is an error of the pass, never a lost run"
        );
        assert!(
            factory
                .run_terminal(&session, &run)
                .await
                .expect("terminal read")
                .is_none(),
            "a failed admin read ends no run"
        );
        assert!(
            matches!(
                factory
                    .list_pending_turn_inputs(&session)
                    .await
                    .expect("pending")
                    .as_slice(),
                [row] if row.input.input_id == input
                    && matches!(row.status, lash_core::PendingTurnInputReadStatus::Admitted { .. })
            ),
            "the run still holds its input"
        );
    }

    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::shift::StoreParkRecovery::new(factory.as_ref(), &clock);
    // A pass the engine cannot answer ends nothing and the next pass
    // retries it, as the recovery interval does: a loaded server's admin
    // query can time out.
    let mut passes = Vec::new();
    for _ in 0..20 {
        let pass = work
            .control()
            .reconcile_parks(
                &writer,
                EnginePage {
                    after: None,
                    limit: NonZeroUsize::new(16).expect("page"),
                    budget: std::time::Duration::from_secs(1),
                },
            )
            .await;
        let ended = pass
            .as_ref()
            .is_ok_and(|report| report.ended_runs.contains(&target));
        passes.push(pass);
        if ended {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        matches!(passes.last(), Some(Ok(report)) if report.ended_runs.contains(&target)),
        "a recovery pass ended the lost run: {passes:?}"
    );
    let terminal = factory
        .run_terminal(&session, &run)
        .await
        .expect("terminal read")
        .expect("the run has its terminal");
    assert_eq!(
        terminal.cause,
        RunTerminalCause::SubstrateLost { cancelled_by: None }
    );
    assert!(
        factory
            .list_pending_turn_inputs(&session)
            .await
            .expect("pending")
            .is_empty(),
        "the run's input is settled with it"
    );
    if let Some(server) = harness.server_double() {
        settle_briefly(&server).await;
    }
    assert_eq!(
        shifts.executions.load(Ordering::SeqCst),
        executions,
        "recovery never executed the run again"
    );
    assert_eq!(shifts.effects.load(Ordering::SeqCst), 1, "one effect, once");
    assert_eq!(
        shifts.admits.load(Ordering::SeqCst),
        admits,
        "recovery asked for no shift"
    );
    harness.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_started_run_is_settled_without_new_ingress() {
    missing_started_run(HarnessServer::in_process(), false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_missing_started_run_is_settled_without_new_ingress() {
    missing_started_run(HarnessServer::Live, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_run_execution_read_ends_no_run() {
    missing_started_run(HarnessServer::in_process(), true).await;
}

/// Run recovery passes until one reads the engine without a failure, as the
/// recovery interval retries a pass a loaded server's admin query timed out.
async fn read_recovery_pass(
    work: &crate::RestateSessionWork,
    writer: &dyn ParkRecoveryWriter,
) -> Vec<Result<ParkReconcileReport, EngineRefusal>> {
    let mut passes = Vec::new();
    for _ in 0..20 {
        let pass = work
            .control()
            .reconcile_parks(
                writer,
                EnginePage {
                    after: None,
                    limit: NonZeroUsize::new(16).expect("page"),
                    budget: std::time::Duration::from_secs(5),
                },
            )
            .await;
        let read = pass.as_ref().is_ok_and(|report| report.failed.is_empty());
        passes.push(pass);
        if read {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        matches!(passes.last(), Some(Ok(report)) if report.failed.is_empty()),
        "a recovery pass read the engine: {passes:?}"
    );
    passes
}

/// Which run of a `SessionTurn` process's session a process-run law admits.
#[derive(Clone, Copy, Debug)]
enum ProcessRun {
    /// The process's own child run, named by the process (FIG-4378).
    Child,
    /// A run the process's shift admits ahead of its own row, named by its
    /// input's source key (FIG-4403).
    AdmittedAhead,
}

/// A run a `SessionTurn` process executes runs inline in the process's own
/// run, so the engine never holds a `LashTurn` run of its key on any lane:
/// the process's child run (FIG-4378), and every run its shift admits
/// ahead of it in a reused session (FIG-4403), which its name does not tie
/// to the process. The run's admission records the process's run as its
/// executor. While the process is live, the recovery pass leaves the started
/// run and its admitted input to it. Once the process is terminal nothing
/// executes the run, and the pass ends it `SubstrateLost` with its input.
async fn process_run(server: HarnessServer, which: ProcessRun) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let stores = harness.law_stores();
    let registry = stores.process_registry();
    let factory = stores.session_store_factory();
    let session = SessionId::fixture(format!("process-child-run-{}", harness.run_nonce()));
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::SessionTurn {
                    definition_key: "process-child-run:v1".to_string(),
                    create_request: Box::new(
                        lash_core::SessionCreateRequest::child_session(
                            "process-child-run-parent",
                            lash_core::SessionStartPoint::Empty,
                            lash_core::PluginOptions::default(),
                        )
                        .with_session_id(&session),
                    ),
                    turn_input: Box::new(lash_core::TurnInput::text("child turn")),
                    result: lash_core::SessionTurnOutcome::Turn,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(
                super::persist_session_turn_env_ref(stores.process_env_store().as_ref()).await,
            )),
        )
        .await
        .expect("register the SessionTurn process")
        .id;
    let target = RunRef {
        session: session.clone(),
        run: match which {
            ProcessRun::Child => lash_core::runtime::process_session_turn_id(&process_id),
            ProcessRun::AdmittedAhead => {
                lash_core::TurnId::fixture(format!("ahead-{}", harness.run_nonce()))
            }
        },
    };
    let store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: vec![],
            config: lash_core::testing::mock_session_policy().into(),
            head: lash_core::SessionCreationHead::Config,
            owning_process_id: Some(process_id.clone()),
        },
    )
    .await
    .expect("store");
    let input = store
        .enqueue_pending_turn_input(
            lash_core::PendingTurnInputDraft::new(
                session.clone(),
                lash_core::TurnInputIngress::next_turn(),
                lash_core::TurnInput::text("child turn"),
            )
            .with_source_key(target.run.as_str()),
        )
        .await
        .expect("enqueue")
        .input_id;
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        store.store(),
        &session,
        "process-child-run",
    )
    .await;
    let mut admission = lash_core::testing::store_fixtures::admit_run_request_for_test(
        &fence,
        &target.run,
        AdmittedHead::Input(input.clone()),
    );
    admission.executor = lash_core::store::RunExecutor::Acceptor {
        scope: lash_core::ExecutionScope::process(process_id.clone()),
    };
    store
        .store()
        .admit_run(&admission)
        .await
        .expect("admit the process's run")
        .expect("the run's admission reaches its head");
    let key = crate::session_shifts::turn_workflow_key(&session, &target.run);
    assert!(
        harness
            .admin_client()
            .run_executions(
                &crate::RestateNamespace::default(),
                std::slice::from_ref(&key)
            )
            .await
            .expect("run executes")
            .is_empty(),
        "the engine holds no LashTurn run of the process's run on any lane"
    );

    let work = harness.session_work();
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::shift::StoreParkRecovery::new(factory.as_ref(), &clock);
    let live = read_recovery_pass(&work, &writer).await;
    assert!(
        live.iter().all(|pass| pass
            .as_ref()
            .is_ok_and(|report| !report.ended_runs.contains(&target))),
        "no pass ends the run a live process runs: {live:?}"
    );
    assert!(
        factory
            .run_terminal(&session, &target.run)
            .await
            .expect("terminal read")
            .is_none(),
        "the live process's run stays open"
    );
    assert!(
        matches!(
            factory
                .list_pending_turn_inputs(&session)
                .await
                .expect("pending")
                .as_slice(),
            [row] if row.input.input_id == input
                && row.status == lash_core::PendingTurnInputReadStatus::Admitted {
                    run: target.run.clone(),
                }
        ),
        "the live process's run still holds its input"
    );

    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("child done"),
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.as_str()),
        )
        .await
        .expect("complete the process");
    let ended = read_recovery_pass(&work, &writer).await;
    assert!(
        ended.iter().any(|pass| pass
            .as_ref()
            .is_ok_and(|report| report.ended_runs.contains(&target))),
        "a pass ends the run once its process is terminal: {ended:?}"
    );
    let terminal = factory
        .run_terminal(&session, &target.run)
        .await
        .expect("terminal read")
        .expect("the run has its terminal");
    assert_eq!(
        terminal.cause,
        RunTerminalCause::SubstrateLost { cancelled_by: None }
    );
    assert!(
        factory
            .list_pending_turn_inputs(&session)
            .await
            .expect("pending")
            .is_empty(),
        "the run's input is settled with it"
    );
    harness.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_process_keeps_its_child_run_and_a_terminal_one_releases_it() {
    process_run(HarnessServer::in_process(), ProcessRun::Child).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_live_process_keeps_its_child_run_and_a_terminal_one_releases_it() {
    process_run(HarnessServer::Live, ProcessRun::Child).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_process_keeps_a_run_admitted_ahead_of_its_own_and_a_terminal_one_releases_it() {
    process_run(HarnessServer::in_process(), ProcessRun::AdmittedAhead).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_live_process_keeps_a_run_admitted_ahead_of_its_own_and_a_terminal_one_releases_it()
{
    process_run(HarnessServer::Live, ProcessRun::AdmittedAhead).await;
}
