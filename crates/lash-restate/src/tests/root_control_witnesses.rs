//! Control operations against the same handlers on the double and live server.
#![expect(
    clippy::expect_used,
    reason = "witness preconditions and outcomes are assertions"
)]
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
    store: Arc<dyn lash_core::RuntimePersistence>,
    restored: AtomicBool,
    admission_fails: AtomicBool,
    commits: AtomicUsize,
}
fn fault() -> lash_core::RuntimeError {
    lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::PluginSessionManager,
        "injected execution fault",
    )
}
#[async_trait::async_trait]
impl SessionDriver for Driver {
    async fn drive(&self, _: DriveRequest) -> Result<DriveOutcome, DriveAbort> {
        panic!("split handlers")
    }
    async fn admit(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        if self.admission_fails.load(Ordering::SeqCst) {
            return Err(DriveAbort::Retry(fault()));
        }
        let root = if self
            .store
            .root_terminal(&self.session, &self.root)
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
                .root_terminal(&self.session, &next)
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
            AdmittedWork::Input {
                head: self.input.clone(),
            },
        )))
    }
    async fn run_root(
        &self,
        _: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> Result<RootOutcome, DriveAbort> {
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
        let mut commit = lash_core::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
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
}
struct Fixture {
    harness: LiveConformanceHarness,
    driver: Arc<Driver>,
    work: crate::RestateSessionWork,
    factory: Arc<dyn lash_core::SessionStoreFactory>,
}
impl Fixture {
    async fn new(server: HarnessServer, admission: bool) -> Self {
        let harness = LiveConformanceHarness::start_on(server).await;
        let session = SessionId::from(format!("control-witness-{}", harness.run_nonce()));
        let root = TurnId::from("root");
        let factory = harness.law_stores().session_store_factory();
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                policy: lash_core::testing::mock_session_policy(),
            })
            .await
            .expect("store");
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
            .bind_root_inputs(&session, &root, std::slice::from_ref(&input))
            .await
            .expect("bind");
        let driver = Arc::new(Driver {
            session,
            root,
            next: std::sync::Mutex::new(None),
            input,
            store,
            restored: AtomicBool::new(false),
            admission_fails: AtomicBool::new(admission),
            commits: AtomicUsize::new(0),
        });
        let work = harness.session_work();
        work.install_session_driver(driver.clone());
        work.send_drive(&driver.session, DriveRequestId::new("initial"))
            .await
            .expect("send");
        Self {
            harness,
            driver,
            work,
            factory,
        }
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
            if (admission && !report.resumed_drives.is_empty())
                || (!admission && !report.parked.is_empty())
            {
                return report;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("engine did not pause within the bounded witness");
    }
    async fn finish(&self) {
        self.harness.finish().await;
    }
}

async fn pause_resume(server: HarnessServer) {
    let f = Fixture::new(server, false).await;
    let report = f.reconcile_until(false).await;
    assert_eq!(report.parked.len(), 1);
    let park = f
        .driver
        .store
        .load_turn_park(&f.driver.session)
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
        f.driver
            .store
            .load_turn_park(&f.driver.session)
            .await
            .expect("park"),
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
    lash_core::drive::apply_control_intent(
        f.factory.as_ref(),
        f.work.control().as_ref(),
        &f.work,
        &NoScopeClose,
        &intent,
        &clock,
    )
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
            .load_turn_park(&f.driver.session)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_admission_is_resumed_without_a_root_park() {
    let f = Fixture::new(HarnessServer::in_process(), true).await;
    let report = f.reconcile_until(true).await;
    assert_eq!(report.resumed_drives, [f.driver.session.clone()]);
    assert!(
        f.driver
            .store
            .load_turn_park(&f.driver.session)
            .await
            .expect("park")
            .is_none()
    );
    f.driver.admission_fails.store(false, Ordering::SeqCst);
    f.driver.restored.store(true, Ordering::SeqCst);
    f.work
        .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
        .await
        .expect("drive");
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
    factory: Arc<dyn lash_core::SessionStoreFactory>,
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
                .load_turn_park(&f.driver.session)
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
            let clock = lash_core::facade_support::SystemClock;
            let scopes = InterruptedClose {
                interrupt: AtomicBool::new(gap == 2),
                calls: AtomicUsize::new(0),
                factory: f.factory.clone(),
            };
            if gap != 0 {
                let engine = InterruptedRelease {
                    inner: f.work.control(),
                    interrupt: AtomicBool::new(gap == 1),
                };
                let state = lash_core::drive::apply_control_intent(
                    f.factory.as_ref(),
                    &engine,
                    &f.work,
                    &scopes,
                    &intent,
                    &clock,
                )
                .await
                .expect("interrupted apply");
                assert!(matches!(
                    state,
                    ControlIntentState::Failed {
                        retryable: true,
                        ..
                    }
                ));
            }
            // Reconstruct recovery from the durable ledger after dropping the
            // interrupted caller. The engine's retained invocation is shared.
            let report = lash_core::drive::reconcile_once(
                &lash_core::drive::ReconcileParts {
                    sessions: f.factory.as_ref(),
                    work: &f.work,
                    scopes: &scopes,
                    processes: None,
                    clock: &clock,
                },
                &ReconcileCursor::default(),
                NonZeroUsize::MIN.saturating_add(15),
                "after-crash",
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
            assert!(
                f.driver
                    .store
                    .load_turn_park(&f.driver.session)
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
        .load_turn_park(&f.driver.session)
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
    lash_core::drive::apply_control_intent(
        f.factory.as_ref(),
        f.work.control().as_ref(),
        &f.work,
        &NoScopeClose,
        &intent,
        &lash_core::facade_support::SystemClock,
    )
    .await
    .expect("release");
    let outcome = f
        .work
        .attach_drive(&f.driver.session, DriveRequestId::new("initial"))
        .await
        .expect("drive");
    assert!(
        matches!(outcome.ran.as_slice(), [RootOutcome::Released { root }, RootOutcome::Committed { root: committed, .. }] if root == &f.driver.root && committed == &next)
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
