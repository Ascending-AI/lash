//! Root-control laws over each tier's durable session catalog.
#![expect(
    clippy::expect_used,
    reason = "law preconditions and outcomes are assertions"
)]
use super::drive_admission::{DriveParts, on_tier};
use lash_core::engine::*;
use lash_core::store::*;
use lash_sansio::{SessionId, TurnId};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Control {
    fail: AtomicBool,
    permanent: AtomicBool,
    /// The engine resumes the root, then its reply is lost: the admin call
    /// timed out after the server acted.
    lose_resume_reply: AtomicBool,
    cancel_on_resume: Mutex<Option<(Arc<dyn crate::SessionStoreFactory>, RootIntentRequest)>>,
    events: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait::async_trait]
impl SessionControlEngine for Control {
    async fn resume_root(
        &self,
        _: &RootRef,
        _: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let cancellation = self.cancel_on_resume.lock().expect("resume hook").take();
        if let Some((factory, request)) = cancellation {
            factory
                .open_root_intent(&request, 5)
                .await
                .expect("concurrent cancel");
        }
        self.events.lock().expect("events").push("resume");
        if self.lose_resume_reply.swap(false, Ordering::SeqCst) {
            return Err(EngineRefusal::Retryable(
                "the resume timed out after the engine acted".into(),
            ));
        }
        Ok(EngineAck::NothingHeld)
    }
    async fn release_root(
        &self,
        _: &RootRef,
        _: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        if self.permanent.load(Ordering::SeqCst) {
            return Err(EngineRefusal::Permanent {
                code: crate::RuntimeErrorCode::PluginSessionManager,
                message: "permanent engine refusal".into(),
            });
        }
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(EngineRefusal::Retryable("release interrupted".into()));
        }
        self.events.lock().expect("events").push("release");
        Ok(EngineAck::Released)
    }
    async fn reconcile_parks(
        &self,
        _: &dyn ParkRecoveryWriter,
        _: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        Ok(ParkReconcileReport::default())
    }
}
/// The engine's live answer about one stalled execution.
struct Execution {
    stopped: bool,
}
#[async_trait::async_trait]
impl StalledExecution for Execution {
    async fn still_stopped(&self) -> Result<bool, EngineRefusal> {
        Ok(self.stopped)
    }
}
struct Work(Arc<Control>);
impl crate::SessionWorkEngine for Work {
    fn schedule_drive(&self, _: &SessionId, _: DriveRequestId) {
        self.0.events.lock().expect("events").push("schedule");
    }
    fn install_session_driver(
        &self,
        driver: Arc<dyn crate::SessionDriver>,
    ) -> Arc<dyn crate::SessionDriver> {
        driver
    }
    fn control(&self) -> Arc<dyn SessionControlEngine> {
        self.0.clone()
    }
}
struct Close {
    store: Arc<dyn crate::RuntimePersistence>,
    fail: AtomicBool,
    events: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait::async_trait]
impl ScopeCloseSink for Close {
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), crate::StoreError> {
        assert!(
            self.store
                .root_terminal(&terminal.session_id, &terminal.root)
                .await?
                .is_some()
        );
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(crate::StoreError::Backend("close interrupted".into()));
        }
        self.events.lock().expect("events").push("close");
        Ok(())
    }
    async fn close_session_scope(
        &self,
        _: &SessionId,
        _: ControlIntentId,
        _: &[TurnId],
    ) -> Result<(), crate::StoreError> {
        panic!("root verb cannot close session")
    }
}
struct Fixture {
    parts: DriveParts,
    factory: Arc<dyn crate::SessionStoreFactory>,
    root: TurnId,
    input: crate::InputId,
    park: TurnPark,
}
impl Fixture {
    async fn new(
        prefix: &str,
        name: &str,
        host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
    ) -> Self {
        let parts = DriveParts::new(prefix, name, host, stores, 8).await;
        let root = TurnId::from(format!("{name}-root"));
        let input = parts.enqueue("first", Some(root.as_str())).await;
        parts
            .store
            .bind_root_inputs(&parts.session_id, &root, std::slice::from_ref(&input))
            .await
            .expect("bind held input");
        let lease = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
            &parts.store,
            &parts.session_id,
            "parked-execution",
        )
        .await;
        let claim = parts
            .store
            .claim_next_turn_inputs(&parts.session_id, &lease.fence(), &lease.owner, 1)
            .await
            .expect("claim")
            .expect("held input");
        parts
            .store
            .bind_turn_input_claim(&claim, &root, &input)
            .await
            .expect("aborted execution keeps its claim");
        let park = parts
            .store
            .record_turn_park(&TurnParkWrite::refusal(
                parts.session_id.clone(),
                root.clone(),
                ParkReason::ReplayDivergence {
                    message: "old build".into(),
                },
                1,
            ))
            .await
            .expect("park");
        parts
            .store
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("execution stopped while claim stays bound");
        assert!(matches!(
            parts
                .store
                .list_pending_turn_inputs(&parts.session_id)
                .await
                .expect("held inputs")[0]
                .status,
            crate::PendingTurnInputReadStatus::TurnBound { .. }
        ));
        Self {
            parts,
            factory: stores.session_store_factory(),
            root,
            input,
            park,
        }
    }
    async fn verb(&self, verb: RootVerb) -> Result<ControlIntent, RootIntentRefused> {
        self.factory
            .open_root_intent(
                &RootIntentRequest {
                    session_id: self.parts.session_id.clone(),
                    root: self.root.clone(),
                    park: self.park.park_id,
                    verb,
                },
                2,
            )
            .await
    }
    fn control(&self, fail_release: bool, fail_close: bool) -> (Work, Close) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Work(Arc::new(Control {
                fail: AtomicBool::new(fail_release),
                permanent: AtomicBool::new(false),
                lose_resume_reply: AtomicBool::new(false),
                cancel_on_resume: Mutex::new(None),
                events: events.clone(),
            })),
            Close {
                store: self.parts.store.clone(),
                fail: AtomicBool::new(fail_close),
                events,
            },
        )
    }
    async fn apply(
        &self,
        work: &Work,
        close: &Close,
        intent: &ControlIntent,
    ) -> ControlIntentState {
        lash_core::runtime::drive::apply_control_intent(
            self.factory.as_ref(),
            work.0.as_ref(),
            work,
            close,
            intent,
            self.parts.host.clock.as_ref(),
        )
        .await
        .expect("apply intent")
    }
    async fn reconcile(&self, work: &Work, close: &Close) -> ReconcileTick {
        lash_core::runtime::drive::reconcile_once(
            &lash_core::runtime::drive::ReconcileParts {
                sessions: self.factory.as_ref(),
                work,
                scopes: close,
                processes: None,
                clock: self.parts.host.clock.as_ref(),
                duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                relays: &[],
            },
            &ReconcileCursor::default(),
            NonZeroUsize::MIN.saturating_add(63),
        )
        .await
    }
    async fn park(&self) -> Option<TurnPark> {
        self.parts
            .store
            .load_turn_park(&self.parts.session_id)
            .await
            .expect("park read")
    }
    async fn intent_state(&self, id: ControlIntentId) -> ControlIntentState {
        self.factory
            .load_intent(id)
            .await
            .expect("intent read")
            .expect("intent retained")
            .state
    }
}

/// A commit of `root`'s physical turn `turn` over `state` that ends the root,
/// as the runtime writes it: the root's terminal evidence in the head
/// transaction.
fn root_final_commit(
    state: &crate::RuntimeSessionState,
    root: &TurnId,
    turn: &TurnId,
    ordinal: u32,
) -> crate::RuntimeCommit {
    let operation = crate::OperationId::turn(state.session_id.as_str(), turn.as_str(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("nodes");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )
    .expect("commit");
    commit.root_terminal = Some(Box::new(RootTerminalWrite {
        root: root.clone(),
        commit: TurnCommitId::new(root.clone(), ordinal),
        turn: turn.clone(),
        stop: None,
    }));
    commit
}

async fn drive(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> DriveOutcome {
    drive_result(f, runner, name).await.expect("the drive runs")
}

/// One admission of request `name`, answered as it was answered: a verdict,
/// or the abort the drive returns when the step refuses the attempt.
async fn admit_verdict(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> Result<AdmitVerdict, DriveAbort> {
    let request = f.parts.request(name);
    on_tier(runner, &f.parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(
            async move { lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0).await },
        )
    })
    .await
}

/// One drive of request `name` to a stop, answered with the abort it ended
/// on when it refused one.
async fn drive_result(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> Result<DriveOutcome, DriveAbort> {
    let request = f.parts.request(name);
    on_tier(runner, &f.parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(
            async move { lash_core::drive::drive_session(&mut runtime, &scope, &request).await },
        )
    })
    .await
}

/// A racing drive, in flight: in process it answers its refusal at once;
/// on Restate the attempt dies inside its recorded admission step and the
/// server keeps retrying the open invocation until the law settles what it
/// waits on.
fn spawn_drive(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> tokio::task::JoinHandle<Result<DriveOutcome, DriveAbort>> {
    let parts = f.parts.clone();
    let runner = Arc::clone(runner);
    let request = parts.request(name);
    tokio::spawn(async move {
        on_tier(&runner, &parts, move |mut runtime, scope| {
            let request = request.clone();
            Box::pin(async move {
                lash_core::drive::drive_session(&mut runtime, &scope, &request).await
            })
        })
        .await
    })
}

/// Counts the law session's parked-root probes: admission reads the park
/// before it decides, so every probe is one admission evaluation. The
/// second probe is held until the law settles — on Restate a refusal's
/// retry re-decides admission inside its recorded step, and holding that
/// re-decision at the park read keeps it suspended rather than burning the
/// invocation's attempt budget, so the settle cannot lose the race.
struct GateProbe {
    inner: Arc<dyn crate::RuntimePersistence>,
    probed: AtomicUsize,
    probed_wake: tokio::sync::Notify,
    settled: tokio::sync::watch::Receiver<bool>,
}

impl GateProbe {
    /// Wait until at least `reads` park reads were probed.
    async fn await_reads(&self, reads: usize) {
        loop {
            let notified = self.probed_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.probed.load(Ordering::SeqCst) >= reads {
                return;
            }
            notified.await;
        }
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for GateProbe {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_turn_park(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<TurnPark>, crate::StoreError> {
        let probe = self.probed.fetch_add(1, Ordering::SeqCst) + 1;
        self.probed_wake.notify_waiters();
        if probe == 2 {
            // The racing drive's second evaluation is its retry re-deciding
            // admission under the still-unsettled intent: hold it until the
            // law's settle landed, so what it then reads is the settled one.
            let mut settled = self.settled.clone();
            let _ = settled.wait_for(|done| *done).await;
        }
        self.inner.load_turn_park(session_id).await
    }
}

pub async fn a_terminal_root_never_reparks(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "terminal-no-park", &host, &stores).await;
    f.verb(RootVerb::Cancel).await.expect("cancel");
    assert!(matches!(
        f.parts
            .store
            .record_turn_park(&TurnParkWrite::refusal(
                f.parts.session_id.clone(),
                f.root.clone(),
                f.park.reason.clone(),
                3
            ))
            .await,
        Err(crate::StoreError::RootAlreadyTerminal { .. })
    ));
    assert!(
        f.parts
            .store
            .load_turn_park(&f.parts.session_id)
            .await
            .expect("park read")
            .is_none()
    );
}

pub async fn cancel_of_a_parked_root_writes_cancelled_settles_its_input_and_drains_the_next(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "cancel-parked", &host, &stores).await;
    let next = f.parts.enqueue("next", Some("next-root")).await;
    let epoch = f.parts.epoch().await.epoch;
    let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
    assert_eq!(f.parts.epoch().await.epoch, epoch + 1);
    assert!(f.parts.epoch().await.control_pending);
    assert!(matches!(
        f.parts
            .store
            .seal_drive_epoch(
                &f.parts.session_id,
                &AdmissionId::new("premature"),
                epoch + 1,
                &RootStartNonce::new("premature")
            )
            .await
            .expect("fenced seal"),
        DriveEpochSeal::Superseded { .. }
    ));
    let terminal = f
        .factory
        .root_terminal(&f.parts.session_id, &f.root)
        .await
        .expect("terminal")
        .expect("cancelled");
    assert_eq!(terminal.kind, RootTerminalKind::Cancelled);
    assert!(
        matches!(terminal.cause, RootTerminalCause::OperatorCancelled { intent: id } if id == intent.id)
    );
    let pending = f
        .parts
        .store
        .list_pending_turn_inputs(&f.parts.session_id)
        .await
        .expect("pending");
    assert!(pending.iter().all(|row| row.input.input_id != f.input));
    assert!(pending.iter().any(|row| row.input.input_id == next));
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert!(!f.parts.epoch().await.control_pending);
    assert_eq!(
        *work.0.events.lock().expect("events"),
        ["release", "close", "schedule"]
    );
    let outcome = drive(&f, &runner, "after-cancel").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![(next, TurnId::from("next-root"))]
    );
    assert_eq!(f.parts.calls(), 1);
}

pub async fn fork_releases_the_old_owner_before_the_new_root_drives_in_original_order_on_a_fresh_journal(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "fork-parked", &host, &stores).await;
    let second = f.parts.enqueue("second", Some("second")).await;
    f.parts
        .store
        .bind_root_inputs(&f.parts.session_id, &f.root, std::slice::from_ref(&second))
        .await
        .expect("second bound");
    let before = f
        .parts
        .store
        .list_pending_turn_inputs(&f.parts.session_id)
        .await
        .expect("pending");
    let intent = f.verb(RootVerb::Fork).await.expect("fork");
    let ControlIntentKind::Fork {
        new_root: Some(ref new_root),
        ..
    } = intent.kind
    else {
        panic!("new input root");
    };
    assert_eq!(
        *new_root,
        TurnId::from(format!("{}~fork{}", f.root, intent.id))
    );
    for input in [&f.input, &second] {
        assert_eq!(
            f.parts
                .store
                .root_binding(&f.parts.session_id, input)
                .await
                .expect("binding"),
            Some(new_root.clone())
        );
    }
    assert!(
        f.factory
            .root_terminal(&f.parts.session_id, new_root)
            .await
            .expect("new root")
            .is_none()
    );
    let after = f
        .parts
        .store
        .list_pending_turn_inputs(&f.parts.session_id)
        .await
        .expect("pending");
    assert_eq!(
        before
            .iter()
            .map(|r| (&r.input.input_id, r.input.enqueue_seq))
            .collect::<Vec<_>>(),
        after
            .iter()
            .map(|r| (&r.input.input_id, r.input.enqueue_seq))
            .collect::<Vec<_>>()
    );
    let (work, close) = f.control(true, false);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Failed {
            retryable: true,
            ..
        }
    ));
    assert!(f.parts.epoch().await.control_pending);
    assert!(work.0.events.lock().expect("events").is_empty());
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        *work.0.events.lock().expect("events"),
        ["release", "close", "schedule"]
    );
    let outcome = drive(&f, &runner, "after-fork").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), new_root.clone()),
            (second, new_root.clone())
        ]
    );
    assert_eq!(f.parts.calls(), 1);
}

pub async fn verbs_are_park_id_cas(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "park-cas", &host, &stores).await;
    let request = RootIntentRequest {
        session_id: f.parts.session_id.clone(),
        root: f.root.clone(),
        park: ParkId::from_feed_sequence(f.park.park_id.feed_sequence() + 1),
        verb: RootVerb::Cancel,
    };
    assert!(
        matches!(f.factory.open_root_intent(&request, 2).await, Err(RootIntentRefused::ParkSuperseded { current }) if current == f.park.park_id)
    );
    assert!(
        f.factory
            .list_control_intents(None, NonZeroUsize::MIN)
            .await
            .expect("ledger")
            .is_empty()
    );
    let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
    assert!(
        matches!(f.verb(RootVerb::Cancel).await, Err(RootIntentRefused::IntentOpen { intent: id }) if id == intent.id)
    );
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    assert!(matches!(
        f.verb(RootVerb::Cancel).await,
        Err(RootIntentRefused::NotParked)
    ));
}

pub async fn redrive_under_the_same_build_reparks_the_same_park_with_attempts_plus_one(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "redrive-repark", &host, &stores).await;
    let epoch = f.parts.epoch().await.epoch;
    let intent = f.verb(RootVerb::Redrive).await.expect("redrive");
    assert_eq!(f.parts.epoch().await.epoch, epoch);
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    let again = f
        .parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.root.clone(),
            f.park.reason.clone(),
            3,
        ))
        .await
        .expect("repark");
    assert_eq!(again.park_id, f.park.park_id);
    assert_eq!(again.attempts, f.park.attempts + 1);
    assert_eq!(again.resume_intent, None);
    f.verb(RootVerb::Cancel)
        .await
        .expect("cancel reparked root");
}

pub async fn cancel_or_fork_of_a_redriving_root_is_refused(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "redrive-refusal", &host, &stores).await;
    let intent = f.verb(RootVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    for verb in [RootVerb::Cancel, RootVerb::Fork] {
        assert!(
            matches!(f.verb(verb).await, Err(RootIntentRefused::Redriving { intent: id }) if id == intent.id)
        );
    }
}

pub async fn an_intent_survives_a_crash_at_every_gap_and_reconcile_completes_it(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for gap in 0..3 {
        let f = Fixture::new(prefix, &format!("intent-gap-{gap}"), &host, &stores).await;
        let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
        let (work, close) = f.control(gap == 1, gap == 2);
        if gap > 0 {
            assert!(matches!(
                f.apply(&work, &close, &intent).await,
                ControlIntentState::Failed {
                    retryable: true,
                    ..
                }
            ));
        }
        let report = lash_core::runtime::drive::reconcile_once(
            &lash_core::runtime::drive::ReconcileParts {
                sessions: f.factory.as_ref(),
                work: &work,
                scopes: &close,
                processes: None,
                clock: f.parts.host.clock.as_ref(),
                duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                relays: &[],
            },
            &ReconcileCursor::default(),
            NonZeroUsize::MIN.saturating_add(63),
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
        assert!(work.0.events.lock().expect("events").contains(&"close"));
    }
}

pub async fn engine_refusals_are_retained_and_listed(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "retained-refusal", &host, &stores).await;
    let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
    let (work, close) = f.control(false, false);
    work.0.permanent.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Failed {
            retryable: false,
            ..
        }
    ));
    assert!(
        f.factory
            .list_open_control_intents(None, NonZeroUsize::MIN)
            .await
            .expect("retry list")
            .is_empty()
    );
    let retained = f
        .factory
        .list_control_intents(None, NonZeroUsize::MIN)
        .await
        .expect("operator list");
    assert!(
        matches!(&retained[0].state, ControlIntentState::Failed { last_error, retryable: false } if last_error.contains("permanent engine refusal"))
    );
    // A verb the engine refused for good is surfaced, never a wedge: its
    // store half ended the root and raised the epoch, so the unreleased
    // execution is fenced, and the session drives the next send.
    assert!(!f.parts.epoch().await.control_pending);
    // Reconciliation does not retry it, and closes the ended root's scope.
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(report.intents.is_empty());
    assert!(work.0.events.lock().expect("events").contains(&"close"));
    let next = f
        .parts
        .enqueue("after-refusal", Some("after-refusal"))
        .await;
    let outcome = drive(&f, &runner, "after-refusal").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.parts.calls(), 1, "the send behind the refused verb runs");
    assert_eq!(
        f.parts.applications().await,
        vec![(next, TurnId::from("after-refusal"))]
    );
}

pub async fn sends_behind_a_parked_root_commit_but_are_not_admitted(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "behind-park", &host, &stores).await;
    let next = f.parts.enqueue("behind", Some("behind-root")).await;
    let before = f.parts.epoch().await;
    let outcome = drive(&f, &runner, "blocked").await;
    assert!(matches!(outcome.stop, DriveStop::Parked(_)));
    assert!(outcome.ran.is_empty());
    assert_eq!(f.parts.calls(), 0);
    assert_eq!(f.parts.epoch().await, before);
    assert!(
        f.parts
            .store
            .list_pending_turn_inputs(&f.parts.session_id)
            .await
            .expect("rows")
            .iter()
            .any(|row| row.input.input_id == next)
    );
}

pub async fn redrive_under_a_restored_build_completes_once_and_clears_the_park(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "restored-redrive", &host, &stores).await;
    let intent = f.verb(RootVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    let outcome = drive(&f, &runner, "restored").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(f.parts.calls(), 1);
    assert_eq!(
        f.parts.applications().await,
        vec![(f.input.clone(), f.root.clone())]
    );
    assert!(
        f.parts
            .store
            .load_turn_park(&f.parts.session_id)
            .await
            .expect("park")
            .is_none()
    );
    assert!(
        f.factory
            .root_terminal(&f.parts.session_id, &f.root)
            .await
            .expect("terminal")
            .is_some()
    );
}

pub async fn a_stale_redrive_is_fenced_by_a_later_cancel(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let racing = Fixture::new(prefix, "redrive-ack-race", &host, &stores).await;
    let pending = racing.verb(RootVerb::Redrive).await.expect("redrive");
    let (work, close) = racing.control(false, false);
    *work.0.cancel_on_resume.lock().expect("resume hook") = Some((
        racing.factory.clone(),
        RootIntentRequest {
            session_id: racing.parts.session_id.clone(),
            root: racing.root.clone(),
            park: racing.park.park_id,
            verb: RootVerb::Cancel,
        },
    ));
    assert!(matches!(
        racing.apply(&work, &close, &pending).await,
        ControlIntentState::Superseded { .. }
    ));
    let f = Fixture::new(prefix, "stale-redrive", &host, &stores).await;
    // The fence the parked root's execution was sealed under.
    let DriveEpochSeal::Sealed(fence) = f
        .parts
        .store
        .seal_drive_epoch(
            &f.parts.session_id,
            &AdmissionId::new("parked-root#0"),
            f.parts.epoch().await.epoch,
            &RootStartNonce::new("parked-execution"),
        )
        .await
        .expect("seal")
    else {
        panic!("the parked root's admission seals");
    };
    let redrive = f.verb(RootVerb::Redrive).await.expect("redrive");
    let cancel = f
        .verb(RootVerb::Cancel)
        .await
        .expect("cancel wins before resume acknowledgement");
    let (work, close) = f.control(false, false);
    assert!(
        matches!(f.apply(&work, &close, &redrive).await, ControlIntentState::Superseded { by } if by == cancel.id)
    );
    assert!(work.0.events.lock().expect("events").is_empty());
    assert!(matches!(
        f.parts
            .store
            .record_turn_park(&TurnParkWrite::refusal(
                f.parts.session_id.clone(),
                f.root.clone(),
                f.park.reason.clone(),
                4
            ))
            .await,
        Err(crate::StoreError::RootAlreadyTerminal { .. })
    ));
    // Nor can the stale execution commit: its fence is the one the cancel
    // raised the epoch past.
    let before = f
        .parts
        .store
        .load_session_head_meta()
        .await
        .expect("head")
        .map(|head| head.head_revision);
    let mut zombie = root_final_commit(&f.parts.initial_state(), &f.root, &f.root, 0);
    zombie.drive_fence = Some(Box::new(fence));
    assert!(matches!(
        f.parts.store.commit_runtime_state(zombie).await,
        Err(crate::StoreError::StaleDriveFence { .. })
    ));
    assert_eq!(
        f.parts
            .store
            .load_session_head_meta()
            .await
            .expect("head")
            .map(|head| head.head_revision),
        before,
        "no commit lands after the cancel"
    );
    assert_eq!(
        f.factory
            .root_terminal(&f.parts.session_id, &f.root)
            .await
            .expect("terminal")
            .expect("cancelled")
            .kind,
        RootTerminalKind::Cancelled
    );
}

pub async fn cancel_fork_and_close_raise_the_drive_epoch_and_redrive_does_not(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for verb in [RootVerb::Redrive, RootVerb::Cancel, RootVerb::Fork] {
        let f = Fixture::new(prefix, &format!("epoch-{verb:?}"), &host, &stores).await;
        let before = f.parts.epoch().await.epoch;
        f.verb(verb).await.expect("verb");
        assert_eq!(
            f.parts.epoch().await.epoch,
            before + u64::from(verb != RootVerb::Redrive)
        );
    }
    let f = Fixture::new(prefix, "epoch-close", &host, &stores).await;
    let before = f.parts.epoch().await.epoch;
    f.factory
        .begin_session_close(&f.parts.session_id, 3)
        .await
        .expect("close");
    assert_eq!(f.parts.epoch().await.epoch, before + 1);
}

pub async fn an_exhausted_root_parks_engine_retry_exhausted_via_reconcile_idempotently_with_no_evidence(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "engine-exhaustion", &host, &stores, 8).await;
    let root = TurnId::from("exhausted");
    parts.enqueue("input", Some(root.as_str())).await;
    let factory = stores.session_store_factory();
    let writer =
        lash_core::drive::StoreParkRecovery::new(factory.as_ref(), parts.host.clock.as_ref());
    let target = ParkTarget::Root {
        session: parts.session_id.clone(),
        root: root.clone(),
    };
    let reason = ParkReason::EngineRetryExhausted {
        attempts: 8,
        last_failure_code: Some("500".into()),
        message: "engine retries exhausted".into(),
    };
    let first = writer
        .record_engine_park(
            &target,
            reason.clone(),
            EnginePark::new("held-invocation"),
            &Execution { stopped: true },
        )
        .await
        .expect("recover");
    let EngineParkRecorded::Parked(id) = first else {
        panic!("new park");
    };
    let before = parts
        .store
        .load_turn_park(&parts.session_id)
        .await
        .expect("park")
        .expect("held");
    assert_eq!(before.reason, reason);
    assert_eq!(before.engine, Some(EnginePark::new("held-invocation")));
    assert_eq!(
        writer
            .record_engine_park(
                &target,
                reason,
                EnginePark::new("held-invocation"),
                &Execution { stopped: true },
            )
            .await
            .expect("repeat"),
        EngineParkRecorded::AttachedToExisting(id)
    );
    assert_eq!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("park"),
        Some(before)
    );
    assert!(
        factory
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("terminal")
            .is_none()
    );
}

pub async fn a_parked_roots_fence_stays_current_until_a_verb(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "park-fence", &host, &stores).await;
    let before = f.parts.epoch().await;
    f.parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.root.clone(),
            f.park.reason.clone(),
            4,
        ))
        .await
        .expect("repark");
    assert_eq!(f.parts.epoch().await, before);
    f.verb(RootVerb::Cancel).await.expect("cancel");
    assert!(f.parts.epoch().await.epoch > before.epoch);
}

pub async fn a_diverged_root_parks_once_holds_claims_blocks_admission_and_completes_after_restore(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "diverged-restore", &host, &stores).await;
    let held = f
        .parts
        .store
        .list_pending_turn_inputs(&f.parts.session_id)
        .await
        .expect("held");
    assert!(matches!(
        held[0].status,
        crate::PendingTurnInputReadStatus::TurnBound { .. }
    ));
    let again = f
        .parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.root.clone(),
            f.park.reason.clone(),
            4,
        ))
        .await
        .expect("same refusal");
    assert_eq!(again.park_id, f.park.park_id);
    assert_eq!(
        f.parts
            .store
            .list_pending_turn_inputs(&f.parts.session_id)
            .await
            .expect("held")
            .len(),
        held.len()
    );
    let blocked = drive(&f, &runner, "before-restore").await;
    assert!(matches!(blocked.stop, DriveStop::Parked(_)));
    assert_eq!(f.parts.calls(), 0);
    let intent = f
        .verb(RootVerb::Redrive)
        .await
        .expect("restored build redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    let resumed = drive(&f, &runner, "after-restore").await;
    assert_eq!(resumed.stop, DriveStop::Idle);
    assert_eq!(f.parts.calls(), 1);
    assert_eq!(
        f.parts.applications().await,
        vec![(f.input.clone(), f.root.clone())]
    );
    assert!(
        f.parts
            .store
            .load_turn_park(&f.parts.session_id)
            .await
            .expect("park")
            .is_none()
    );
}

/// B1: a root that parks on a later physical turn — a frame switch's
/// follow-on turn — is still one root. The commit that ends it clears its
/// park whichever physical turn committed, so the root is never parked and
/// terminal at once: the verbs answer `NotParked` and nothing is left for a
/// drain to count.
pub async fn a_root_parked_on_a_later_physical_turn_is_cleared_by_its_commit(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "later-physical-park", &host, &stores).await;
    let redrive = f.verb(RootVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &redrive).await;
    let turn = QueuedRunPosition::derive_turn_id(&f.root, 1);
    f.parts
        .store
        .commit_runtime_state(root_final_commit(
            &f.parts.initial_state(),
            &f.root,
            &turn,
            1,
        ))
        .await
        .expect("the root's final commit on its second physical turn");
    assert!(
        f.factory
            .root_terminal(&f.parts.session_id, &f.root)
            .await
            .expect("terminal")
            .is_some()
    );
    assert_eq!(
        f.park().await,
        None,
        "the root's commit clears its park whichever physical turn committed"
    );
    assert!(matches!(
        f.verb(RootVerb::Cancel).await,
        Err(RootIntentRefused::NotParked)
    ));
}

/// H2: a redrive whose resume reached the engine but whose acknowledgement
/// was lost is settled by the root running past it. When the root parks
/// again, and when an operator then cancels it, reconciliation never resumes
/// the root a second time: a store-terminal root is never resumed.
pub async fn a_redrive_the_root_ran_past_is_never_applied_again(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "lapsed-redrive", &host, &stores).await;
    let redrive = f.verb(RootVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    work.0.lose_resume_reply.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Failed {
            retryable: true,
            ..
        }
    ));
    // The resumed root runs, refuses again under the same build, re-parks.
    let again = f
        .parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.root.clone(),
            f.park.reason.clone(),
            3,
        ))
        .await
        .expect("repark");
    assert_eq!(again.resume_intent, None);
    let cancel = f
        .verb(RootVerb::Cancel)
        .await
        .expect("cancel the re-parked root");
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let resumes = work
        .0
        .events
        .lock()
        .expect("events")
        .iter()
        .filter(|event| **event == "resume")
        .count();
    assert_eq!(
        resumes, 1,
        "a redrive the root ran past never resumes it again"
    );
    assert!(!f.intent_state(redrive.id).await.is_open());
    assert!(matches!(
        f.intent_state(cancel.id).await,
        ControlIntentState::Acknowledged { .. }
    ));
}

/// M3: a paused-execution listing read before a redrive resumed the root is
/// stale. Recording the engine's park from it must not re-park the running
/// root: the park keeps naming the redrive, so a cancel stays refused while
/// the root runs.
pub async fn a_stale_paused_listing_never_reparks_a_resumed_root(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "stale-listing", &host, &stores).await;
    let redrive = f.verb(RootVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    let writer =
        lash_core::drive::StoreParkRecovery::new(f.factory.as_ref(), f.parts.host.clock.as_ref());
    let target = ParkTarget::Root {
        session: f.parts.session_id.clone(),
        root: f.root.clone(),
    };
    let exhausted = ParkReason::EngineRetryExhausted {
        attempts: 8,
        last_failure_code: None,
        message: "listed before the resume".into(),
    };
    assert_eq!(
        writer
            .record_engine_park(
                &target,
                exhausted.clone(),
                EnginePark::new("listed-before-resume"),
                // The redrive resumed it after the listing: it runs.
                &Execution { stopped: false },
            )
            .await
            .expect("record from the stale listing"),
        EngineParkRecorded::Redriven
    );
    let park = f.park().await.expect("still parked");
    assert_eq!(
        park.resume_intent,
        Some(redrive.id),
        "a stale listing never re-parks the running root"
    );
    assert_eq!(park.attempts, f.park.attempts);
    assert!(matches!(
        f.verb(RootVerb::Cancel).await,
        Err(RootIntentRefused::Redriving { .. })
    ));
    // An execution the engine finds still stopped after the redrive
    // resumed it stopped again: that re-parks the root, clearing the
    // redrive, so the operator can act on it again.
    let EngineParkRecorded::AttachedToExisting(id) = writer
        .record_engine_park(
            &target,
            exhausted,
            EnginePark::new("stopped-again"),
            &Execution { stopped: true },
        )
        .await
        .expect("record the stopped-again execution")
    else {
        panic!("the same park is re-parked");
    };
    assert_eq!(id, f.park.park_id);
    let again = f.park().await.expect("re-parked");
    assert_eq!(again.resume_intent, None);
    assert_eq!(again.attempts, f.park.attempts + 1);
    f.verb(RootVerb::Cancel)
        .await
        .expect("the re-parked root can be cancelled");
}

/// Reconcile never scans the catalog for undriven input (ADR 0109 §3): an
/// accepted input's drive is asked for through its ingress obligation, once
/// — here while its session is parked, where the drive it asks for stops at
/// the park and the verb that resolves the park drives the session on.
pub async fn a_parked_session_is_asked_to_drive_only_through_its_ingress_obligation(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "parked-drive-ask", &host, &stores).await;
    let behind = f.parts.enqueue("behind", Some("behind-root")).await;
    let (work, close) = f.control(false, false);
    let work = Arc::new(work);
    let scheduled = |work: &Work| {
        work.0
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| **event == "schedule")
            .count()
    };
    let parked = f.reconcile(&work, &close).await;
    assert!(parked.failures.is_empty(), "{:?}", parked.failures);
    assert_eq!(scheduled(&work), 0, "reconcile scans no session for input");
    let ledger = stores.obligation_ledger(ObligationKind::Ingress);
    let relay = lash_core::runtime::drive::IngressRelay::new(
        Arc::clone(&ledger),
        Arc::clone(&work) as Arc<dyn crate::SessionWorkEngine>,
        Arc::clone(&f.parts.host.clock),
    );
    let id = ingress_obligation::ingress_obligation_id(behind.as_str());
    assert_eq!(
        ledger.state(&id).await.expect("armed state"),
        Some(ObligationState::Due),
        "the input's acceptance armed its obligation"
    );
    relay.deliver_admitted(behind.as_str()).await;
    assert_eq!(scheduled(&work), 1, "the obligation asks for the drive");
    assert_eq!(
        ledger.state(&id).await.expect("delivered state"),
        Some(ObligationState::Delivered)
    );
    relay.deliver_admitted(behind.as_str()).await;
    let resolved = f.reconcile(&work, &close).await;
    assert!(resolved.failures.is_empty(), "{:?}", resolved.failures);
    assert_eq!(
        scheduled(&work),
        1,
        "a delivered obligation is not asked again, by its producer or by reconcile"
    );
}

/// D15: while a parked root's park names a redrive intent that is not yet
/// settled, admission of a new turn input answers a typed retryable
/// refusal — never a recorded verdict, never a failed turn — and the
/// command lane still drains first. Once the intent settles the parked
/// root runs once and the send lands behind it.
pub async fn a_send_racing_an_unsettled_redrive_is_refused_until_the_redrive_settles(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    // The command lane drains first (ADR 0101 §4): a session command
    // pending while the redrive is unsettled is admitted ahead of the
    // send waiting behind it.
    let lane = Fixture::new(prefix, "redrive-command-lane", &host, &stores).await;
    lane.parts
        .enqueue("racing send", Some("lane-send-root"))
        .await;
    lane.parts
        .store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            lane.parts.session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "drain while the redrive is unsettled".into(),
            },
        ))
        .await
        .expect("command accepted");
    lane.verb(RootVerb::Redrive).await.expect("redrive");
    match admit_verdict(&lane, &runner, "command-lane").await {
        Ok(AdmitVerdict::Admit(admitted)) => assert!(
            matches!(admitted.work(), AdmittedWork::Queued),
            "the command lane drains while the redrive is unsettled: {:?}",
            admitted.work()
        ),
        other => panic!("the command lane drains first: {other:?}"),
    }

    let mut f = Fixture::new(prefix, "send-redrive-race", &host, &stores).await;
    let send = f.parts.enqueue("racing send", Some("racing-root")).await;
    let intent = f.verb(RootVerb::Redrive).await.expect("redrive");
    assert!(f.intent_state(intent.id).await.is_open());
    // The send's drive meets the unsettled redrive. In process its typed
    // refusal answers at once; on Restate the attempt dies inside its
    // recorded admission step and the open invocation retries until the
    // intent settles. The probe holds that retry's re-decision at the park
    // read, so the law's settle always lands inside the attempt budget.
    let (settled, settled_rx) = tokio::sync::watch::channel(false);
    let probe = Arc::new(GateProbe {
        inner: Arc::clone(&f.parts.store),
        probed: AtomicUsize::new(0),
        probed_wake: tokio::sync::Notify::new(),
        settled: settled_rx,
    });
    f.parts.store = Arc::clone(&probe) as Arc<dyn crate::RuntimePersistence>;
    let mut racing = spawn_drive(&f, &runner, "racing");
    tokio::time::timeout(std::time::Duration::from_secs(30), probe.await_reads(1))
        .await
        .expect("the racing drive evaluated the parked root");
    assert_eq!(
        f.parts.calls(),
        0,
        "the parked root is not run ahead of its resume"
    );
    assert!(
        f.parts.applications().await.is_empty(),
        "nothing is interleaved with the unsettled redrive"
    );
    // A second probe is a re-decision under the unsettled intent — the
    // refusal's own retry on Restate. Admission that instead ran the held
    // input reaches a second read only behind a committed root, so a
    // re-decision with nothing run is evidence the gate held.
    let redecided =
        tokio::time::timeout(std::time::Duration::from_millis(400), probe.await_reads(2))
            .await
            .is_ok();
    if redecided {
        assert_eq!(
            f.parts.calls(),
            0,
            "a re-decision ran the root ahead of its redrive"
        );
        assert!(
            f.parts.applications().await.is_empty(),
            "a re-decision interleaved a run with the unsettled redrive"
        );
    }
    let answered = if racing.is_finished() {
        Some((&mut racing).await.expect("the racing drive ran"))
    } else {
        None
    };
    match &answered {
        Some(Err(DriveAbort::Retry(refusal))) => {
            assert_eq!(
                refusal.code,
                crate::RuntimeErrorCode::SessionRedriveUnsettled,
                "{refusal:?}"
            );
            assert!(refusal.is_retryable(), "{refusal:?}");
        }
        // A drive that lands while its redrive is unsettled is the bug
        // this law rules out (D15): admitted ahead of the redrive.
        Some(Ok(landed)) => assert!(
            !f.intent_state(intent.id).await.is_open(),
            "the send landed while the redrive was unsettled: {landed:?}"
        ),
        Some(Err(abort)) => {
            panic!("the refusal is retryable, never a failed turn: {abort:?}")
        }
        None => {}
    }
    // The engine half answers: the parked execution is resumed under
    // its sealed fence (or, holding nothing, a drive is asked to
    // re-run the root). The intent settles; admission proceeds — on
    // Restate the held retry admits what was refused.
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    let _ = settled.send(true);
    let raced = match answered {
        Some(raced) => raced,
        None => racing.await.expect("the racing drive ran"),
    };
    match raced {
        Ok(settled) => assert_eq!(settled.stop, DriveStop::Idle),
        Err(DriveAbort::Retry(_)) => {
            let settled = drive(&f, &runner, "after-settle").await;
            assert_eq!(settled.stop, DriveStop::Idle);
        }
        Err(abort) => panic!("the settled drive ended on a refusal: {abort:?}"),
    }
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), f.root.clone()),
            (send, TurnId::from("racing-root"))
        ],
        "the held input runs the root once, then the send lands"
    );
    assert_eq!(
        f.parts.calls(),
        2,
        "the parked root runs once and the send's root once"
    );
    assert!(
        f.park().await.is_none(),
        "the root's commit cleared its park"
    );
}

/// D15: a redrive acknowledgement lost after the engine resumed never
/// wedges the session — reconcile's redrive arm settles the intent within
/// a tick — after which the queued send is admitted.
pub async fn a_lost_redrive_ack_is_settled_by_reconcile_and_the_queued_send_is_admitted(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut f = Fixture::new(prefix, "lost-redrive-ack", &host, &stores).await;
    let send = f
        .parts
        .enqueue("queued send", Some("queued-send-root"))
        .await;
    let intent = f.verb(RootVerb::Redrive).await.expect("redrive");
    // The engine resumed the root but its reply was lost: the intent stays
    // open, retryable.
    let (work, close) = f.control(false, false);
    work.0.lose_resume_reply.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Failed {
            retryable: true,
            ..
        }
    ));
    assert!(f.intent_state(intent.id).await.is_open());
    // The queued send's drive meets the unsettled redrive: its refusal is
    // the typed retryable one in process, and an invocation the server
    // keeps retrying on Restate. The probe holds that retry's re-decision
    // at the park read until the law's reconcile lands.
    let (settled, settled_rx) = tokio::sync::watch::channel(false);
    let probe = Arc::new(GateProbe {
        inner: Arc::clone(&f.parts.store),
        probed: AtomicUsize::new(0),
        probed_wake: tokio::sync::Notify::new(),
        settled: settled_rx,
    });
    f.parts.store = Arc::clone(&probe) as Arc<dyn crate::RuntimePersistence>;
    let mut racing = spawn_drive(&f, &runner, "racing");
    tokio::time::timeout(std::time::Duration::from_secs(30), probe.await_reads(1))
        .await
        .expect("the racing drive evaluated the parked root");
    assert_eq!(f.parts.calls(), 0);
    assert!(f.parts.applications().await.is_empty());
    let redecided =
        tokio::time::timeout(std::time::Duration::from_millis(400), probe.await_reads(2))
            .await
            .is_ok();
    if redecided {
        assert_eq!(
            f.parts.calls(),
            0,
            "a re-decision ran the root ahead of its redrive"
        );
        assert!(
            f.parts.applications().await.is_empty(),
            "a re-decision interleaved a run with the unsettled redrive"
        );
    }
    let answered = if racing.is_finished() {
        Some((&mut racing).await.expect("the racing drive ran"))
    } else {
        None
    };
    match &answered {
        Some(Err(DriveAbort::Retry(refusal))) => assert_eq!(
            refusal.code,
            crate::RuntimeErrorCode::SessionRedriveUnsettled,
            "the queued send is refused retryably while the redrive is \
             unsettled: {refusal:?}"
        ),
        Some(Ok(landed)) => assert!(
            !f.intent_state(intent.id).await.is_open(),
            "the queued send landed while the redrive was unsettled: {landed:?}"
        ),
        Some(Err(abort)) => {
            panic!("the refusal is retryable, never a failed turn: {abort:?}")
        }
        None => {}
    }
    // Reconcile's redrive/park arm settles the lost intent within a tick.
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(
        report.intents.iter().any(|(id, state)| {
            *id == intent.id && matches!(state, ControlIntentState::Acknowledged { .. })
        }),
        "reconcile settled the lost acknowledgement: {:?}",
        report.intents
    );
    let _ = settled.send(true);
    // After the tick the queued send is admitted: the held input drives
    // the root once and the send lands behind it. On Restate the racing
    // invocation's retry admits it; in process the drive answers on the
    // next request.
    let raced = match answered {
        Some(raced) => raced,
        None => racing.await.expect("the racing drive ran"),
    };
    match raced {
        Ok(admitted) => assert_eq!(admitted.stop, DriveStop::Idle),
        Err(DriveAbort::Retry(_)) => {
            let admitted = drive(&f, &runner, "after-tick").await;
            assert_eq!(admitted.stop, DriveStop::Idle);
        }
        Err(abort) => panic!("the settled drive ended on a refusal: {abort:?}"),
    }
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), f.root.clone()),
            (send, TurnId::from("queued-send-root"))
        ]
    );
    assert_eq!(f.parts.calls(), 2);
}
