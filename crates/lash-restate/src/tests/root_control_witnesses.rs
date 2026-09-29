//! Control operations against the same handlers on the double and live server.
use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::{SessionDriver, SessionId, SessionWorkEngine, TurnId};
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
#[async_trait::async_trait]
impl SessionDriver for Driver {
    async fn admit(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        self.admits.fetch_add(1, Ordering::SeqCst);
        if self.admission_fails.load(Ordering::SeqCst) {
            return Err(DriveAbort::Retry(
                if self.redrive_unsettled.load(Ordering::SeqCst) {
                    redrive_unsettled()
                } else {
                    fault()
                },
            ));
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
                        &state,
                        graph,
                        &[],
                        operation,
                    )
                    .expect("commit");
                commit.root_terminal = Some(Box::new(RootTerminalWrite {
                    root: root.clone(),
                    commit: TurnCommitId::new(root.clone(), 0),
                    turn: root.clone(),
                    stop: None,
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
                head: lash_core::SessionCreationHead::CommittedByCreator,
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
                    after: None,
                    limit: NonZeroUsize::new(16).expect("page"),
                },
            )
            .await
            .expect("park pass")
    }
    /// Attach to `request`'s drive, firing the double's timers while it
    /// runs; `None` when it did not end within the bounded witness.
    async fn attach_within(&self, request: DriveRequestId) -> Option<DriveOutcome> {
        let attach = self.work.attach_drive(&self.driver.session, request);
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
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_root(target, engine).await?;
        Err(EngineRefusal::Retryable(
            "lost reply after the engine resumed".into(),
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
        .work
        .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
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
        .work
        .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
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
    assert!(
        lost.is_open(),
        "the lost reply keeps the redrive open: {lost:?}"
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
    ) -> Result<EngineAck, EngineRefusal> {
        self.inner.resume_root(target, engine).await
    }
    async fn release_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let answer = self.inner.release_root(target, engine).await?;
        if self.interrupt.swap(false, Ordering::SeqCst) {
            return Err(EngineRefusal::Retryable(
                "lost acknowledgement after engine release".into(),
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
                    // A lost release stays on the intent.
                    assert!(matches!(
                        state,
                        ControlIntentState::Failed {
                            retryable: true,
                            ..
                        }
                    ));
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
                .work
                .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
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
    let outcome = f
        .work
        .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
        .await
        .expect("drive");
    assert!(
        matches!(outcome.ran.as_slice(), [RootOutcome::Released { root }, RootOutcome::Committed { root: committed, .. }] if *root == f.driver.root && *committed == next)
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
        .work
        .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
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
        BuildGeneration::for_test("recovery-interval"),
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
