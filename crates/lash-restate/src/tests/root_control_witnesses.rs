//! Control operations against the same handlers on the double and live server.
use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, SessionDriver,
    SessionId, SessionWorkEngine, TurnId,
};
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Driver {
    session: SessionId,
    root: TurnId,
    next: std::sync::Mutex<Option<TurnId>>,
    input: lash_core::InputId,
    store: lash_core::store::SessionStore,
    restored: AtomicBool,
    admission_fails: AtomicBool,
    /// Admission names the root again after it ended: a store that has not
    /// caught up with the root's release.
    repeat_released: AtomicBool,
    /// A failing admission answers D15's refusal: the session's park names a
    /// redrive that has not settled.
    redrive_unsettled: AtomicBool,
    /// The session holds only a queued command: admission names no root.
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
/// One admission of a witness driver, recorded as the kernel records its own:
/// the `AdmitDrive` step of `ordinal` on the drive's admission scope. `decide`
/// reads live state, so it runs only as the step's first execution; a replay
/// of the drive decodes the verdict that execution journaled and never runs
/// it. A fault of `decide` is the attempt's, never the step's outcome: the
/// engine's retry decides the admission again.
async fn recorded_admission<Fut>(
    controller: &lash_core::ScopedEffectController<'_>,
    request: &DriveRequest,
    ordinal: u32,
    decide: impl FnOnce() -> Fut + Send,
) -> Result<AdmitVerdict, DriveAbort>
where
    Fut: Future<Output = Result<AdmitVerdict, lash_core::RuntimeError>> + Send,
{
    let address = EffectAddress::new(
        controller.execution_scope().clone(),
        drive_admission_replay_key(&request.request, ordinal),
    )
    .map_err(|error| DriveAbort::Refused(lash_core::RuntimeError::from(error)))?;
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            address,
            RuntimeAttribution::for_session(request.session.clone()),
            format!("drive-admission-{ordinal}"),
        ),
        RuntimeEffectCommand::AdmitDrive {
            request: Box::new(AdmitRequest {
                session: request.session.clone(),
                request: request.request.clone(),
                build_generation: request.build_generation.clone(),
            }),
        },
    );
    controller
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                match decide().await {
                    Ok(verdict) => Ok(RuntimeEffectOutcome::AdmitDrive {
                        verdict: Box::new(verdict),
                    }),
                    Err(fault) => Err(lash_core::RuntimeEffectControllerError::from(fault)
                        .retryable_uncommitted_derivation()),
                }
            }),
        )
        .await
        .and_then(RuntimeEffectOutcome::into_admit_drive)
        .map_err(|error| DriveAbort::Retry(error.into_runtime_error()))
}

impl Driver {
    /// What the session admits now, read from its store.
    async fn decide_admission(
        &self,
        request: &DriveRequest,
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
        let root = if !self.repeat_released.load(Ordering::SeqCst)
            && self
                .store
                .root_terminal(&self.root)
                .await
                .expect("terminal")
                .is_some()
        {
            let next = self.next.lock().expect("next root").clone();
            let Some(next) = next else {
                return Ok(AdmitVerdict::Idle);
            };
            if self
                .store
                .root_terminal(&next)
                .await
                .expect("next terminal")
                .is_some()
            {
                return Ok(AdmitVerdict::Idle);
            }
            next
        } else {
            self.root.clone()
        };
        Ok(AdmitVerdict::Admit(admission_body::admitted(
            self.session.clone(),
            root,
            request.request.clone(),
            AdmissionId::new(format!("{}#{ordinal}", request.request.as_str())),
            0,
            request.build_generation.clone(),
            AdmittedWork::Input {
                head: self.input.clone(),
            },
        )))
    }
}
#[async_trait::async_trait]
impl SessionDriver for Driver {
    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, DriveAbort> {
        recorded_admission(&controller, request, ordinal, || {
            self.decide_admission(request, ordinal)
        })
        .await
    }
    async fn run_root(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> lash_core::engine::RootRunEnd {
        lash_core::engine::RootRunEnd::owing_nothing(
            async {
                let root = admitted.root().clone();
                if !self.restored.load(Ordering::SeqCst) {
                    return Err(DriveAbort::Retry(fault()));
                }
                let mut state =
                    lash_core::RuntimeSessionState::new(lash_core::testing::mock_session_policy());
                state.session_id = self.session.clone();
                state.policy.session_id = Some(self.session.clone());
                let operation =
                    lash_core::OperationId::turn(self.session.as_str(), root.as_str(), "witness");
                let mut graph = state.pending_graph_commit();
                graph
                    .derive_node_ids(&self.session, &operation)
                    .expect("nodes");
                let mut commit =
                    lash_core::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
                        &state, graph, operation,
                    )
                    .expect("commit");
                commit.root_terminal = Some(Box::new(RootTerminalWrite {
                    root: root.clone(),
                    commit: TurnCommitId::new(root.clone(), 0),
                    turn: root.clone(),
                    outcome: lash_core::store::RootCommittedOutcome::Finished(
                        lash_core::facade_support::TurnFinish::AssistantMessage {
                            text: String::new(),
                        },
                    ),
                }));
                self.store
                    .commit_runtime_state(commit)
                    .await
                    .expect("commit root");
                self.commits.fetch_add(1, Ordering::SeqCst);
                Ok(RootOutcome::Committed {
                    root: root.clone(),
                    outcome: lash_core::facade_support::TurnOutcome::Finished(
                        lash_core::facade_support::TurnFinish::AssistantMessage {
                            text: "restored".into(),
                        },
                    ),
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
/// How `request`'s drive of `session` ended, read through every invocation
/// it handed off to: each leg's roots in order, and the last leg's stop. A
/// drive whose attempt replayed hands off at its next root boundary
/// (FIG-4506, FIG-4523), so one leg's answer is not the drive's.
#[expect(
    clippy::result_large_err,
    reason = "matches the ingress client's error API"
)]
async fn attach_whole_drive(
    work: &crate::RestateSessionWork,
    session: &SessionId,
    request: DriveRequestId,
) -> Result<DriveOutcome, crate::SendDriveError> {
    let mut leg = DriveRequest {
        session: session.clone(),
        request,
        // A continuation's request id names its session and request alone.
        build_generation: lash_core::engine::BuildGeneration::for_test("unread"),
    };
    let mut ran = Vec::new();
    loop {
        let outcome = work.attach_drive(&leg.session, leg.request.clone()).await?;
        ran.extend(outcome.ran);
        if !matches!(
            outcome.stop,
            DriveStop::HandedOff { .. } | DriveStop::Draining { .. }
        ) {
            return Ok(DriveOutcome {
                ran,
                stop: outcome.stop,
            });
        }
        leg.request = lash_core::engine::drive_continuation_request(&leg);
    }
}
struct Fixture {
    harness: LiveConformanceHarness,
    driver: Arc<Driver>,
    /// The engine's installation of `driver`, kept for the fixture's life.
    _installation: Arc<dyn SessionDriver>,
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
    fn schedule_drive(&self, session: &SessionId, request: DriveRequestId) {
        self.work.schedule_drive(session, request);
    }
    async fn request_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<(), EngineRefusal> {
        self.work.request_drive(session, request).await
    }
    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        self.work.install_session_driver(driver)
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
    /// names no root.
    async fn with_command(server: HarnessServer) -> Self {
        Self::build(server, true, true).await
    }
    async fn build(server: HarnessServer, admission: bool, command_only: bool) -> Self {
        let harness = LiveConformanceHarness::start_on(server).await;
        let session = SessionId::from(format!("control-witness-{}", harness.run_nonce()));
        let root = TurnId::from("root");
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
                .bind_root_inputs(&root, std::slice::from_ref(&input))
                .await
                .expect("bind");
            (input, None)
        };
        let driver = Arc::new(Driver {
            session,
            root,
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
        let installation = work.install_session_driver(driver.clone());
        work.send_drive(&driver.session, DriveRequestId::new("initial"))
            .await
            .expect("send");
        let intents = harness
            .law_stores()
            .obligation_ledger(ObligationKind::ControlIntent);
        Self {
            harness,
            driver,
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
    ) -> lash_core::drive::ControlIntentRelay {
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
    ) -> lash_core::drive::ControlIntentRelay {
        let work: Arc<dyn SessionWorkEngine> = match control {
            Some(control) => Arc::new(WithControl {
                work: self.work.clone(),
                control,
            }),
            None => Arc::new(self.work.clone()),
        };
        let scope_close = Arc::new(lash_core::drive::ScopeCloseRelay::new(
            self.harness
                .law_stores()
                .obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&self.factory),
            Arc::clone(&scopes),
        ));
        lash_core::drive::ControlIntentRelay::new(
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
        let writer = lash_core::drive::StoreParkRecovery::new(self.factory.as_ref(), &clock);
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
    /// Run the engine until the session's drive is paused.
    async fn await_paused_drive(&self) {
        let admin = self.harness.admin_client();
        for _ in 0..2000 {
            if let Some(server) = self.harness.server_double() {
                server.settle().await;
                server.fire_next_timer();
            }
            if !admin
                .paused_session_drives(
                    &crate::services::DEFAULT_NAMESPACE,
                    self.driver.session.as_str(),
                )
                .await
                .expect("paused drives")
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the session's drive did not pause within the bounded witness");
    }
    /// The session's paused drives now.
    async fn paused_drives(&self) -> usize {
        self.harness
            .admin_client()
            .paused_session_drives(
                &crate::services::DEFAULT_NAMESPACE,
                self.driver.session.as_str(),
            )
            .await
            .expect("paused drives")
            .len()
    }
    /// One park-reconcile pass over the whole listing.
    async fn park_pass(&self) -> ParkReconcileReport {
        let clock = lash_core::facade_support::SystemClock;
        let writer = lash_core::drive::StoreParkRecovery::new(self.factory.as_ref(), &clock);
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
    /// [`attach_whole_drive`] on the fixture's session.
    #[expect(
        clippy::result_large_err,
        reason = "matches the ingress client's error API"
    )]
    async fn attach_whole_drive(
        &self,
        request: DriveRequestId,
    ) -> Result<DriveOutcome, crate::SendDriveError> {
        attach_whole_drive(&self.work, &self.driver.session, request).await
    }
    /// Attach to `request`'s drive, firing the double's timers while it
    /// runs; `None` when it did not end within the bounded witness.
    async fn attach_within(&self, request: DriveRequestId) -> Option<DriveOutcome> {
        let attach = self.attach_whole_drive(request);
        tokio::pin!(attach);
        for _ in 0..1000 {
            if let Some(server) = self.harness.server_double() {
                server.settle().await;
                server.fire_next_timer();
            }
            tokio::select! {
                outcome = &mut attach => return Some(outcome.expect("the drive answers")),
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
    async fn resume_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_root(target, engine, children).await?;
        Err(EngineRefusal::retryable(
            lash_core::RuntimeErrorCode::EngineControlRequest,
            "lost reply after the engine resumed",
        ))
    }
    async fn release_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.release_root(target, engine).await
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
    async fn resume_root(
        &self,
        _: &RootRef,
        _: Option<&EnginePark>,
        _: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::Resumed)
    }
    async fn release_root(
        &self,
        _: &RootRef,
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
        .driver
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
            .root_terminal(&f.driver.session, &f.driver.root)
            .await
            .expect("terminal")
            .is_none()
    );
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::drive::StoreParkRecovery::new(f.factory.as_ref(), &clock);
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
        f.driver.store.load_turn_park().await.expect("park"),
        Some(park.clone())
    );
    f.driver.restored.store(true, Ordering::SeqCst);
    let intent = f
        .factory
        .open_root_intent(
            &RootIntentRequest {
                session_id: f.driver.session.clone(),
                root: f.driver.root.clone(),
                park: park.park_id,
                verb: RootVerb::Redrive,
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
        .attach_whole_drive(DriveRequestId::new("initial"))
        .await
        .expect("resumed drive");
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.driver.commits.load(Ordering::SeqCst), 1);
    assert!(
        f.driver
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

/// ADR 0109 §3: a drive the engine paused in its admission is never resumed
/// by recovery. The pass parks the session's next root, a second pass leaves
/// the drive paused, and only the park's redrive resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_admission_is_parked_and_only_its_redrive_resumes_it() {
    let f = Fixture::new(HarnessServer::in_process(), true).await;
    let report = f.reconcile_until(true).await;
    assert_eq!(
        report.parked,
        vec![ParkTarget::Drive {
            session: f.driver.session.clone()
        }]
    );
    let park = f
        .driver
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("the paused drive parked its session's next root");
    assert_eq!(park.turn_id, f.driver.root);
    assert!(matches!(
        park.reason,
        ParkReason::EngineRetryExhausted { .. }
    ));
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::drive::StoreParkRecovery::new(f.factory.as_ref(), &clock);
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
        f.driver.store.load_turn_park().await.expect("park"),
        Some(park.clone()),
        "a second pass writes nothing"
    );
    assert_eq!(
        f.harness
            .admin_client()
            .paused_session_drives(
                &crate::services::DEFAULT_NAMESPACE,
                f.driver.session.as_str()
            )
            .await
            .expect("paused drives")
            .len(),
        1,
        "recovery never resumes the paused drive"
    );
    f.driver.admission_fails.store(false, Ordering::SeqCst);
    f.driver.restored.store(true, Ordering::SeqCst);
    let intent = f
        .factory
        .open_root_intent(
            &RootIntentRequest {
                session_id: f.driver.session.clone(),
                root: f.driver.root.clone(),
                park: park.park_id,
                verb: RootVerb::Redrive,
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
        .attach_whole_drive(DriveRequestId::new("initial"))
        .await
        .expect("the redrive resumed the drive");
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.driver.commits.load(Ordering::SeqCst), 1);
    assert!(
        f.driver
            .store
            .load_turn_park()
            .await
            .expect("park")
            .is_none(),
        "the root's commit cleared the park"
    );
    f.finish().await;
}

/// FIG-3879 (D15): a drive the engine paused only because its session's park
/// named a redrive that had not settled — every attempt refused its admission
/// retryably, and the redrive's own engine half ran before the pause — is
/// resumed by the recovery pass once that redrive settles. It is never parked
/// again for a second operator redrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drive_paused_only_by_an_unsettled_redrive_is_resumed_once_it_settles() {
    let f = Fixture::new(HarnessServer::in_process(), true).await;
    f.reconcile_until(true).await;
    let park = f
        .driver
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("the paused drive parked its session's next root");
    // The operator redrives. The engine half resumes the drive, but its reply
    // is lost: the intent stays open, and the resumed drive's admission meets
    // the unsettled redrive on every attempt until the engine pauses it again.
    f.driver.redrive_unsettled.store(true, Ordering::SeqCst);
    let intent = f
        .factory
        .open_root_intent(
            &RootIntentRequest {
                session_id: f.driver.session.clone(),
                root: f.driver.root.clone(),
                park: park.park_id,
                verb: RootVerb::Redrive,
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
    f.await_paused_drive().await;
    let open = f.park_pass().await;
    assert!(open.parked.is_empty(), "{open:?}");
    assert_eq!(f.paused_drives().await, 1, "an open redrive owns the drive");
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
    f.driver.redrive_unsettled.store(false, Ordering::SeqCst);
    f.driver.admission_fails.store(false, Ordering::SeqCst);
    f.driver.restored.store(true, Ordering::SeqCst);
    let pass = f.park_pass().await;
    assert!(
        pass.parked.is_empty(),
        "the settled redrive's drive is not parked again: {pass:?}"
    );
    assert_eq!(pass.resumed_drives, vec![f.driver.session.clone()]);
    let outcome = f
        .attach_within(DriveRequestId::new("initial"))
        .await
        .expect("the pass resumed the drive the settled redrive left paused");
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.driver.commits.load(Ordering::SeqCst), 1);
    assert!(
        f.driver
            .store
            .load_turn_park()
            .await
            .expect("park")
            .is_none(),
        "the root's commit cleared the park; no second redrive was needed"
    );
    f.finish().await;
}

/// FIG-3879: a paused drive whose session's next work names no root — here
/// only a queued command — has no park an operator verb could resume it
/// through. The recovery pass releases it, and the command lane drives the
/// session again: the command's ingress obligation asks for a fresh drive,
/// which the released session admits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_drive_with_no_root_to_park_is_released_to_its_command_lane() {
    let f = Fixture::with_command(HarnessServer::in_process()).await;
    f.await_paused_drive().await;
    let pass = f.park_pass().await;
    assert!(pass.parked.is_empty(), "nothing names a root: {pass:?}");
    assert_eq!(pass.released_drives, vec![f.driver.session.clone()]);
    f.driver.admission_fails.store(false, Ordering::SeqCst);
    let admitted = f.driver.admits.load(Ordering::SeqCst);
    // The command's ingress obligation is still owed: its relay asks for the
    // session's drive under its first attempt.
    let batch = f.batch.clone().expect("the queued command");
    let relay = lash_core::drive::IngressRelay::new(
        f.harness
            .law_stores()
            .obligation_ledger(ObligationKind::Ingress),
        Arc::new(f.work.clone()),
        Arc::new(lash_core::facade_support::SystemClock),
    );
    let asked = lash_core::drive::relay::relay_due(
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
        .attach_within(lash_core::drive::ingress_drive_request(
            batch.as_str(),
            lash_core::drive::FIRST_INGRESS_ATTEMPT,
        ))
        .await
        .expect("the command's drive runs: no paused drive holds the session");
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert!(
        f.driver.admits.load(Ordering::SeqCst) > admitted,
        "the command's drive admitted the session"
    );
    assert_eq!(f.paused_drives().await, 0, "no drive is left paused");
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
    async fn resume_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_root(target, engine, children).await
    }
    async fn release_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let answer = self.inner.release_root(target, engine).await?;
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
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), lash_core::StoreError> {
        assert!(
            self.factory
                .root_terminal(&terminal.session_id, &terminal.root)
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
        roots: &[TurnId],
    ) -> Result<(), lash_core::StoreError> {
        for root in roots {
            let terminal = self
                .factory
                .root_terminal(session, root)
                .await?
                .expect("terminal precedes close");
            self.close_root_scope(&terminal).await?;
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
                .driver
                .store
                .load_turn_park()
                .await
                .expect("park")
                .expect("parked");
            let intent = if action == 2 {
                f.factory
                    .begin_session_close(&f.driver.session, 10)
                    .await
                    .expect("close")
                    .expect("intent")
            } else {
                f.factory
                    .open_root_intent(
                        &RootIntentRequest {
                            session_id: f.driver.session.clone(),
                            root: f.driver.root.clone(),
                            park: park.park_id,
                            verb: if action == 0 {
                                RootVerb::Cancel
                            } else {
                                RootVerb::Fork
                            },
                        },
                        10,
                    )
                    .await
                    .expect("verb")
            };
            // The build is restored: were the parked execution resumed
            // rather than released, it would commit.
            f.driver.restored.store(true, Ordering::SeqCst);
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
            let relays: Vec<Arc<dyn lash_core::drive::relay::ObligationRelay>> = vec![
                Arc::new(f.relay_on(None, scopes.clone(), Arc::new(LaterClock(3_600_000)))),
                Arc::new(lash_core::drive::ScopeCloseRelay::new(
                    f.harness
                        .law_stores()
                        .obligation_ledger(ObligationKind::ScopeClose),
                    Arc::clone(&f.factory),
                    scopes.clone(),
                )),
            ];
            let report = lash_core::drive::reconcile_once(
                &lash_core::drive::ReconcileParts {
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
                .attach_whole_drive(DriveRequestId::new("initial"))
                .await
                .expect("released drive");
            assert!(matches!(
                outcome.ran.as_slice(),
                [RootOutcome::Released { .. }]
            ));
            assert_eq!(outcome.stop, DriveStop::Idle);
            assert_eq!(f.driver.commits.load(Ordering::SeqCst), 0);
            // The release killed the parked execution: nothing is left for a
            // stale resume to wake.
            assert!(matches!(
                f.work
                    .control()
                    .resume_root(
                        &RootRef {
                            session: f.driver.session.clone(),
                            root: f.driver.root.clone(),
                        },
                        park.engine.as_ref(),
                        &[],
                    )
                    .await
                    .expect("status read"),
                EngineAck::NothingHeld
            ));
            assert_eq!(f.driver.commits.load(Ordering::SeqCst), 0);
            assert!(
                f.driver
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
        .driver
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("held");
    let intent = f
        .factory
        .open_root_intent(
            &RootIntentRequest {
                session_id: f.driver.session.clone(),
                root: f.driver.root.clone(),
                park: park.park_id,
                verb: RootVerb::Cancel,
            },
            8,
        )
        .await
        .expect("cancel");
    let next = TurnId::from("next");
    *f.driver.next.lock().expect("next") = Some(next.clone());
    f.driver.restored.store(true, Ordering::SeqCst);
    f.relay(None, Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("release");
    // The drive that consumes the release resumed a suspended attempt, so it
    // may hand off at the released root's boundary (FIG-4523). The release's own ask to drive
    // queues behind this invocation, so after a handoff either drive admits
    // the next root: this one's continuation, or that ask's drive ahead of
    // it, which leaves the continuation nothing to admit.
    let outcome = f
        .attach_whole_drive(DriveRequestId::new("initial"))
        .await
        .expect("drive");
    assert!(
        match outcome.ran.as_slice() {
            [RootOutcome::Released { root }] => *root == f.driver.root,
            [
                RootOutcome::Released { root },
                RootOutcome::Committed {
                    root: committed, ..
                },
            ] => *root == f.driver.root && *committed == next,
            _ => false,
        },
        "{outcome:?}"
    );
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.driver.commits.load(Ordering::SeqCst), 1);
    f.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_root_is_consumed_and_the_next_root_runs() {
    released_then_next(HarnessServer::in_process()).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_released_root_is_consumed_and_the_next_root_runs() {
    released_then_next(HarnessServer::Live).await;
}

async fn released_then_repeated(server: HarnessServer) {
    let f = Fixture::new(server, false).await;
    f.reconcile_until(false).await;
    let park = f
        .driver
        .store
        .load_turn_park()
        .await
        .expect("park")
        .expect("held");
    let intent = f
        .factory
        .open_root_intent(
            &RootIntentRequest {
                session_id: f.driver.session.clone(),
                root: f.driver.root.clone(),
                park: park.park_id,
                verb: RootVerb::Cancel,
            },
            8,
        )
        .await
        .expect("cancel");
    f.driver.repeat_released.store(true, Ordering::SeqCst);
    f.driver.restored.store(true, Ordering::SeqCst);
    f.relay(None, Arc::new(NoScopeClose))
        .deliver_intent(&intent)
        .await
        .expect("release");
    let outcome = f
        .attach_whole_drive(DriveRequestId::new("initial"))
        .await
        .expect("drive");
    assert!(
        matches!(outcome.ran.as_slice(), [RootOutcome::Released { root }] if *root == f.driver.root),
        "{:?}",
        outcome.ran
    );
    assert_eq!(
        outcome.stop,
        DriveStop::RootAborted {
            root: f.driver.root.clone()
        },
        "a released root admitted again stops the drive instead of spinning"
    );
    assert_eq!(f.driver.commits.load(Ordering::SeqCst), 0);
    f.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_root_admitted_again_stops_the_drive_root_aborted() {
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
    assert_eq!(paused.len(), 1, "the root's execution pauses");
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
    assert_eq!(paused.len(), 1, "the root's execution pauses");
    // The handle names the paused run of the fixture's session; the verb
    // names a root of another session.
    let handle = EnginePark::new(paused[0].id.clone());
    let other = RootRef {
        session: SessionId::from(format!("{}-other", f.driver.session)),
        root: f.driver.root.clone(),
    };
    let control = f.work.control();
    for refusal in [
        control
            .resume_root(&other, Some(&handle), &[])
            .await
            .expect_err("a resume under another session's handle"),
        control
            .release_root(&other, Some(&handle))
            .await
            .expect_err("a release under another session's handle"),
    ] {
        assert_eq!(
            (refusal.disposition, &refusal.code),
            (
                RefusalDisposition::Permanent,
                &lash_core::RuntimeErrorCode::EngineHandleMismatch
            ),
            "{refusal:?}"
        );
        assert!(
            matches!(
                lash_core::drive::relay::DeliveryFailure::from(refusal),
                lash_core::drive::relay::DeliveryFailure::Refused(cause)
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
        after.iter().map(|run| &run.id).collect::<Vec<_>>(),
        vec![&paused[0].id]
    );
    // The refusals took nothing from the handle: under its own session it
    // still resumes the run, which commits the root and ends the drive.
    f.driver.restored.store(true, Ordering::SeqCst);
    let own = RootRef {
        session: f.driver.session.clone(),
        root: f.driver.root.clone(),
    };
    assert_eq!(
        control
            .resume_root(&own, Some(&handle), &[])
            .await
            .expect("a resume under the run's own session"),
        EngineAck::Resumed
    );
    let outcome = f
        .attach_within(DriveRequestId::new("initial"))
        .await
        .expect("the resumed drive ends");
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.driver.commits.load(Ordering::SeqCst), 1);
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
struct TickingDriver {
    ticks: AtomicUsize,
}
#[async_trait::async_trait]
impl SessionDriver for TickingDriver {
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
        _: &DriveRequest,
        _: u32,
        _: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, DriveAbort> {
        panic!("the recovery interval never admits")
    }
    async fn run_root(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        _: Admitted,
    ) -> lash_core::engine::RootRunEnd {
        lash_core::engine::RootRunEnd::owing_nothing(
            async { panic!("the recovery interval never runs a root") }.await,
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

/// A deployment's session work over an unreachable server: the recovery
/// interval only calls the installed driver.
fn detached_session_work() -> crate::RestateSessionWork {
    crate::RestateSessionWork::new(
        crate::RestateIngressClient::new(crate::RestateConnection::new("http://127.0.0.1:9")),
        crate::RestateSessionDriverSlot::new(),
        lash_core::engine::EngineGeneration::fixed(BuildGeneration::for_test("recovery-interval")),
        crate::RestateNamespace::default(),
        Arc::new(NoEngineControl),
    )
}

async fn first_tick(driver: &TickingDriver) {
    for _ in 0..500 {
        if driver.ticks.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the recovery interval never ticked");
}

/// M1: a deployment runs one recovery interval however often its driver is
/// installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reinstalled_driver_runs_one_recovery_interval() {
    let work = detached_session_work();
    let driver = Arc::new(TickingDriver::default());
    let installed = work.install_session_driver(driver.clone());
    let reinstalled = work.install_session_driver(driver.clone());
    first_tick(&driver).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        driver.ticks.load(Ordering::SeqCst),
        1,
        "a re-install of the live driver starts no second interval"
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

/// The live server's control engine behind a slow control RPC: each root
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
    async fn resume_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_root(target, handle, children).await
    }
    async fn release_root(
        &self,
        target: &RootRef,
        handle: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        self.releases
            .lock()
            .expect("releases")
            .push(std::time::Instant::now());
        tokio::time::sleep(self.delay).await;
        self.inner.release_root(target, handle).await
    }
}

/// A scope owner that records when each root scope closed.
#[derive(Default)]
struct RecordedCloses {
    roots: std::sync::Mutex<Vec<std::time::Instant>>,
}
#[async_trait::async_trait]
impl ScopeCloseSink for RecordedCloses {
    async fn close_root_scope(&self, _: &RootTerminal) -> Result<(), lash_core::StoreError> {
        self.roots
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
struct CadenceDriver {
    factory: Arc<dyn lash_core::DeploymentStore>,
    work: WithControl,
    scopes: Arc<RecordedCloses>,
    relays: Vec<Arc<dyn lash_core::drive::relay::ObligationRelay>>,
    lanes: lash_core::drive::RelayLanes,
    ticks: std::sync::Mutex<Vec<(std::time::Instant, ReconcileTick)>>,
}
#[async_trait::async_trait]
impl SessionDriver for CadenceDriver {
    fn owns_reconciliation(&self) -> bool {
        true
    }
    async fn reconcile(
        &self,
        cursor: &ReconcileCursor,
        page: NonZeroUsize,
    ) -> Result<ReconcileCursor, lash_core::StoreError> {
        let ticked_at = std::time::Instant::now();
        let tick = lash_core::drive::reconcile_once(
            &lash_core::drive::ReconcileParts {
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
        _: &DriveRequest,
        _: u32,
        _: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, DriveAbort> {
        panic!("the recovery interval never admits")
    }
    async fn run_root(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        _: Admitted,
    ) -> lash_core::engine::RootRunEnd {
        lash_core::engine::RootRunEnd::owing_nothing(
            async { panic!("the recovery interval never runs a root") }.await,
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

/// ADR 0109 §1.8 on a live server: the deployment's recovery interval keeps
/// its cadence while a control RPC is slow. A closed session's release — the
/// intent's control RPC — is held for a minute. The production interval
/// fires every `T` regardless; each tick's parks run against the server
/// within its lane wait; the closed root's scope close, a later kind, is
/// delivered in the first tick; the intent's kind is busy while its attempt
/// runs; and the attempt is cut at the host's budget and retried by the
/// first tick its lane is free for.
async fn recovery_tick_keeps_cadence_with_slow_control_rpc(server: HarnessServer) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let stores = harness.law_stores();
    let factory = stores.session_store_factory();
    let session = SessionId::from(format!("cadence-{}", harness.run_nonce()));
    let root = TurnId::from("open-root");
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
        .bind_root_inputs(&session, &root, &[])
        .await
        .expect("an open root");
    let now_ms = || lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock);
    let intent = factory
        .begin_session_close(&session, now_ms())
        .await
        .expect("close")
        .expect("the session exists");
    // The host's attempt budget; the release takes five times as long.
    let budget = std::time::Duration::from_secs(12);
    let tick = lash_core::drive::RECOVERY_TICK;
    let slack = std::time::Duration::from_millis(1_500);
    let pass = lash_core::engine::RecoveryPassBudget {
        attempt: budget,
        ..lash_core::engine::RecoveryPassBudget::default()
    };
    let policy = lash_core::drive::relay::RelayPolicy {
        attempt_budget_ms: pass.attempt_ms(),
        ..lash_core::drive::relay::RelayPolicy::default()
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
        lash_core::drive::ScopeCloseRelay::new(
            stores.obligation_ledger(ObligationKind::ScopeClose),
            Arc::clone(&factory),
            Arc::clone(&scopes) as Arc<dyn ScopeCloseSink>,
        )
        .with_policy(policy),
    );
    let intent_relay = Arc::new(
        lash_core::drive::ControlIntentRelay::new(
            stores.obligation_ledger(ObligationKind::ControlIntent),
            Arc::clone(&factory),
            Arc::new(WithControl {
                work: session_work.clone(),
                control: Arc::clone(&control) as Arc<dyn SessionControlEngine>,
            }),
            Arc::clone(&scopes) as Arc<dyn ScopeCloseSink>,
            Arc::clone(&scope_close) as Arc<dyn lash_core::drive::relay::ObligationRelay>,
            Arc::clone(&clock),
        )
        .with_policy(policy),
    );
    let driver = Arc::new(CadenceDriver {
        factory: Arc::clone(&factory),
        work,
        scopes: Arc::clone(&scopes),
        relays: vec![intent_relay, scope_close],
        lanes: lash_conformance::deployment_tick_lanes(Arc::clone(&clock), pass),
        ticks: std::sync::Mutex::new(Vec::new()),
    });
    let started = std::time::Instant::now();
    let installation = session_work.install_session_driver(driver.clone());
    tokio::time::sleep(budget + 2 * tick + slack).await;
    drop(installation);

    let ticks: Vec<(std::time::Instant, ReconcileTick)> = driver
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
    // A later kind: the closed root's scope close was delivered in the first
    // tick.
    let closes = scopes.roots.lock().expect("closes").clone();
    let [closed] = closes.as_slice() else {
        panic!("one root scope close: {} of them", closes.len());
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

/// FIG-4281: a root that admits its input, records its admission and starts
/// an effect, then blocks inside it. Every execution of the root is counted:
/// recovery must never start another one under a fresh journal.
struct StartedRootDriver {
    session: SessionId,
    root: TurnId,
    input: lash_core::InputId,
    store: lash_core::store::SessionStore,
    admits: AtomicUsize,
    executions: AtomicUsize,
    effects: AtomicUsize,
}
impl StartedRootDriver {
    /// What the session admits now, read from its store.
    async fn decide_admission(
        &self,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, lash_core::RuntimeError> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        if self
            .store
            .root_terminal(&self.root)
            .await
            .expect("terminal")
            .is_some()
        {
            return Ok(AdmitVerdict::Idle);
        }
        Ok(AdmitVerdict::Admit(admission_body::admitted(
            self.session.clone(),
            self.root.clone(),
            request.request.clone(),
            AdmissionId::new(format!("{}#{ordinal}", request.request.as_str())),
            0,
            request.build_generation.clone(),
            AdmittedWork::Input {
                head: self.input.clone(),
            },
        )))
    }
}
#[async_trait::async_trait]
impl SessionDriver for StartedRootDriver {
    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, DriveAbort> {
        recorded_admission(&controller, request, ordinal, || {
            self.decide_admission(request, ordinal)
        })
        .await
    }
    async fn run_root(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> lash_core::engine::RootRunEnd {
        lash_core::engine::RootRunEnd::owing_nothing(
            async {
                self.executions.fetch_add(1, Ordering::SeqCst);
                let fence = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
                    self.store.store(),
                    &self.session,
                    "started-root",
                )
                .await;
                lash_core::testing::store_fixtures::admit_root_for_test(
                    self.store.store(),
                    &fence,
                    admitted.root(),
                    AdmittedHead::Input(self.input.clone()),
                )
                .await
                .expect("admit the root")
                .expect("the root's admission reaches its head");
                self.effects.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<RootOutcome, DriveAbort>>().await
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

/// Let the double run what it can for a moment. A started root blocks inside
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

/// A root admits its input and starts its effect; an operator kills its
/// `LashTurn` run, the drive consumes the release and stops `RootAborted`,
/// and the run's record is purged before any recovery pass. The recovery
/// pass alone ends the root `SubstrateLost` and settles its input: no new
/// submission, no new drive, no second execution. With `admin_outage`, a
/// pass whose admin read fails first ends nothing.
async fn missing_started_root(server: HarnessServer, admin_outage: bool) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let session = SessionId::from(format!("lost-root-{}", harness.run_nonce()));
    let root = TurnId::from("lost-root");
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
            .with_source_key(root.as_str()),
        )
        .await
        .expect("enqueue")
        .input_id;
    let driver = Arc::new(StartedRootDriver {
        session: session.clone(),
        root: root.clone(),
        input: input.clone(),
        store,
        admits: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        effects: AtomicUsize::new(0),
    });
    let work = harness.session_work();
    let _installation = work.install_session_driver(driver.clone());
    work.send_drive(&session, DriveRequestId::new("initial"))
        .await
        .expect("send");
    for _ in 0..2000 {
        if driver.effects.load(Ordering::SeqCst) > 0 {
            break;
        }
        if let Some(server) = harness.server_double() {
            settle_briefly(&server).await;
            server.fire_next_timer();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        driver.effects.load(Ordering::SeqCst),
        1,
        "the root started its one effect"
    );

    let turn = crate::RestateNamespace::default()
        .stable(crate::LashService::TurnDriver)
        .name()
        .into_owned();
    let key = crate::session_driver::turn_workflow_key(&session, &root);
    let killed = harness.harness_admin().kill_workflow_run(&turn, &key).await;
    let attach = attach_whole_drive(&work, &session, DriveRequestId::new("initial"));
    tokio::pin!(attach);
    let mut outcome = None;
    for _ in 0..2000 {
        if let Some(server) = harness.server_double() {
            settle_briefly(&server).await;
            server.fire_next_timer();
        }
        tokio::select! {
            ended = &mut attach => {
                outcome = Some(ended.expect("the drive answers"));
                break;
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
    let outcome = outcome.expect("the drive ends within the bounded witness");
    assert!(
        matches!(outcome.ran.as_slice(), [RootOutcome::Released { root: released }] if *released == root),
        "{:?}",
        outcome.ran
    );
    assert_eq!(outcome.stop, DriveStop::RootAborted { root: root.clone() });
    harness.harness_admin().purge_invocation(&killed).await;
    assert!(
        harness
            .admin_client()
            .root_runs(
                &crate::RestateNamespace::default(),
                std::slice::from_ref(&key)
            )
            .await
            .expect("root runs")
            .is_empty(),
        "the engine holds no run of the root on any lane"
    );
    let executions = driver.executions.load(Ordering::SeqCst);
    let admits = driver.admits.load(Ordering::SeqCst);
    let target = RootRef {
        session: session.clone(),
        root: root.clone(),
    };

    if admin_outage {
        let outage = crate::RestateAdminClient::new(crate::RestateConnection::with_transport(
            "http://admin.invalid",
            Arc::new(AdminOutage),
        ));
        let read = crate::session_control::end_lost_root_runs(
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
            "an unreadable engine is an error of the pass, never a lost root"
        );
        assert!(
            factory
                .root_terminal(&session, &root)
                .await
                .expect("terminal read")
                .is_none(),
            "a failed admin read ends no root"
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
            "the root still holds its input"
        );
    }

    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::drive::StoreParkRecovery::new(factory.as_ref(), &clock);
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
            .is_ok_and(|report| report.ended_roots.contains(&target));
        passes.push(pass);
        if ended {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        matches!(passes.last(), Some(Ok(report)) if report.ended_roots.contains(&target)),
        "a recovery pass ended the lost root: {passes:?}"
    );
    let terminal = factory
        .root_terminal(&session, &root)
        .await
        .expect("terminal read")
        .expect("the root has its terminal");
    assert_eq!(
        terminal.cause,
        RootTerminalCause::SubstrateLost { cancelled_by: None }
    );
    assert!(
        factory
            .list_pending_turn_inputs(&session)
            .await
            .expect("pending")
            .is_empty(),
        "the root's input is settled with it"
    );
    if let Some(server) = harness.server_double() {
        settle_briefly(&server).await;
    }
    assert_eq!(
        driver.executions.load(Ordering::SeqCst),
        executions,
        "recovery never executed the root again"
    );
    assert_eq!(driver.effects.load(Ordering::SeqCst), 1, "one effect, once");
    assert_eq!(
        driver.admits.load(Ordering::SeqCst),
        admits,
        "recovery asked for no drive"
    );
    harness.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_started_root_is_settled_without_new_ingress() {
    missing_started_root(HarnessServer::in_process(), false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_missing_started_root_is_settled_without_new_ingress() {
    missing_started_root(HarnessServer::Live, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_root_run_read_ends_no_root() {
    missing_started_root(HarnessServer::in_process(), true).await;
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

/// Which root of a `SessionTurn` process's session a process-root law admits.
#[derive(Clone, Copy, Debug)]
enum ProcessRoot {
    /// The process's own child root, named by the process (FIG-4378).
    Child,
    /// A root the process's drive admits ahead of its own row, named by its
    /// input's source key (FIG-4403).
    AdmittedAhead,
}

/// A root a `SessionTurn` process drives runs inline in the process's own
/// run, so the engine never holds a `LashTurn` run of its key on any lane:
/// the process's child root (FIG-4378), and every root its drive admits
/// ahead of it in a reused session (FIG-4403), which its name does not tie
/// to the process. The root's admission records the process's run as its
/// executor. While the process is live, the recovery pass leaves the started
/// root and its admitted input to it. Once the process is terminal nothing
/// runs the root, and the pass ends it `SubstrateLost` with its input.
async fn process_root(server: HarnessServer, which: ProcessRoot) {
    let harness = LiveConformanceHarness::start_on(server).await;
    let stores = harness.law_stores();
    let registry = stores.process_registry();
    let factory = stores.session_store_factory();
    let session = SessionId::from(format!("process-child-root-{}", harness.run_nonce()));
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::SessionTurn {
                    definition_key: "process-child-root:v1".to_string(),
                    create_request: Box::new(
                        lash_core::SessionCreateRequest::child_session(
                            "process-child-root-parent",
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
    let target = RootRef {
        session: session.clone(),
        root: match which {
            ProcessRoot::Child => lash_core::runtime::process_session_turn_id(&process_id),
            ProcessRoot::AdmittedAhead => {
                lash_core::TurnId::from(format!("ahead-{}", harness.run_nonce()))
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
            .with_source_key(target.root.as_str()),
        )
        .await
        .expect("enqueue")
        .input_id;
    let fence = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
        store.store(),
        &session,
        "process-child-root",
    )
    .await;
    let mut admission = lash_core::testing::store_fixtures::admit_root_request_for_test(
        &fence,
        &target.root,
        AdmittedHead::Input(input.clone()),
    );
    admission.executor = lash_core::store::RootExecutor::Inline {
        scope: lash_core::ExecutionScope::process(process_id.clone()),
    };
    store
        .store()
        .admit_root(&admission)
        .await
        .expect("admit the process's root")
        .expect("the root's admission reaches its head");
    let key = crate::session_driver::turn_workflow_key(&session, &target.root);
    assert!(
        harness
            .admin_client()
            .root_runs(
                &crate::RestateNamespace::default(),
                std::slice::from_ref(&key)
            )
            .await
            .expect("root runs")
            .is_empty(),
        "the engine holds no LashTurn run of the process's root on any lane"
    );

    let work = harness.session_work();
    let clock = lash_core::facade_support::SystemClock;
    let writer = lash_core::drive::StoreParkRecovery::new(factory.as_ref(), &clock);
    let live = read_recovery_pass(&work, &writer).await;
    assert!(
        live.iter().all(|pass| pass
            .as_ref()
            .is_ok_and(|report| !report.ended_roots.contains(&target))),
        "no pass ends the root a live process runs: {live:?}"
    );
    assert!(
        factory
            .root_terminal(&session, &target.root)
            .await
            .expect("terminal read")
            .is_none(),
        "the live process's root stays open"
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
                    root: target.root.clone(),
                }
        ),
        "the live process's root still holds its input"
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
            .is_ok_and(|report| report.ended_roots.contains(&target))),
        "a pass ends the root once its process is terminal: {ended:?}"
    );
    let terminal = factory
        .root_terminal(&session, &target.root)
        .await
        .expect("terminal read")
        .expect("the root has its terminal");
    assert_eq!(
        terminal.cause,
        RootTerminalCause::SubstrateLost { cancelled_by: None }
    );
    assert!(
        factory
            .list_pending_turn_inputs(&session)
            .await
            .expect("pending")
            .is_empty(),
        "the root's input is settled with it"
    );
    harness.finish().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_process_keeps_its_child_root_and_a_terminal_one_releases_it() {
    process_root(HarnessServer::in_process(), ProcessRoot::Child).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_live_process_keeps_its_child_root_and_a_terminal_one_releases_it() {
    process_root(HarnessServer::Live, ProcessRoot::Child).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_process_keeps_a_root_admitted_ahead_of_its_own_and_a_terminal_one_releases_it() {
    process_root(HarnessServer::in_process(), ProcessRoot::AdmittedAhead).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server"]
async fn live_a_live_process_keeps_a_root_admitted_ahead_of_its_own_and_a_terminal_one_releases_it()
{
    process_root(HarnessServer::Live, ProcessRoot::AdmittedAhead).await;
}
