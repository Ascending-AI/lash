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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Control {
    fail: AtomicBool,
    permanent: AtomicBool,
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
}

async fn drive(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> DriveOutcome {
    let request = f.parts.request(name);
    on_tier(runner, &f.parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("drive")
        })
    })
    .await
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
            },
            &ReconcileCursor::default(),
            NonZeroUsize::MIN.saturating_add(63),
            "recover",
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
    _: Arc<dyn crate::ConformanceTurnRunner>,
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

pub async fn root_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "scope-recovery", &host, &stores).await;
    let (work, close) = f.control(false, false);
    let parts = lash_core::runtime::drive::ReconcileParts {
        sessions: f.factory.as_ref(),
        work: &work,
        scopes: &close,
        processes: None,
        clock: f.parts.host.clock.as_ref(),
    };
    let first = lash_core::runtime::drive::reconcile_once(
        &parts,
        &ReconcileCursor::default(),
        NonZeroUsize::MIN,
        "parked",
    )
    .await;
    assert_eq!(first.closed_scopes, 0);
    assert!(!work.0.events.lock().expect("events").contains(&"close"));
    let state = f.parts.initial_state();
    let operation = crate::OperationId::turn(f.parts.session_id.as_str(), f.root.as_str(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("nodes");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        graph,
        &[],
        operation,
    )
    .expect("commit");
    commit.root_terminal = Some(Box::new(RootTerminalWrite {
        root: f.root.clone(),
        commit: TurnCommitId::new(f.root.clone(), 0),
        turn: f.root.clone(),
        stop: None,
    }));
    f.parts
        .store
        .commit_runtime_state(commit)
        .await
        .expect("commit before crash");
    assert!(
        f.factory
            .list_open_control_intents(None, NonZeroUsize::MIN)
            .await
            .expect("intents")
            .is_empty()
    );
    for tick in ["restart", "again"] {
        let pass = lash_core::runtime::drive::reconcile_once(
            &parts,
            &ReconcileCursor::default(),
            NonZeroUsize::MIN,
            tick,
        )
        .await;
        assert_eq!(pass.closed_scopes, 1);
        assert!(pass.failures.is_empty(), "{:?}", pass.failures);
    }
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
        .record_engine_park(&target, reason.clone(), EnginePark::new("held-invocation"))
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
            .record_engine_park(&target, reason, EnginePark::new("held-invocation"))
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
