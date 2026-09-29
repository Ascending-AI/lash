//! Root-control laws over each tier's durable session catalog.
#![expect(
    clippy::expect_used,
    reason = "law preconditions and outcomes are assertions"
)]
use super::drive_admission::{DriveParts, on_tier};
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::testing::RuntimePersistenceTestDriveExt as _;
use lash_sansio::{SessionId, TurnId};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Control {
    fail: AtomicBool,
    /// Releases that fail retryably before one succeeds.
    fail_releases: AtomicUsize,
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
        if self.fail.swap(false, Ordering::SeqCst)
            || self
                .fail_releases
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
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
struct Work(Arc<Control>, AtomicUsize);
#[async_trait::async_trait]
impl crate::SessionWorkEngine for Work {
    fn schedule_drive(&self, _: &SessionId, _: DriveRequestId) {
        self.0.events.lock().expect("events").push("schedule");
    }
    /// Refuses as many asks as the fixture armed, then accepts each.
    async fn request_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<(), EngineRefusal> {
        if self
            .1
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            self.0.events.lock().expect("events").push("drive-refused");
            return Err(EngineRefusal::Retryable(
                "the drive ask was not accepted".into(),
            ));
        }
        self.schedule_drive(session, request);
        Ok(())
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
#[derive(Clone)]
struct Close {
    store: Arc<dyn crate::RuntimePersistence>,
    fail: Arc<AtomicBool>,
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
/// A clock `offset_ms` ahead of `inner`: a reconcile tick run after an
/// obligation's backoff has elapsed.
#[derive(Debug)]
pub(super) struct ShiftedClock {
    pub(super) inner: Arc<dyn crate::Clock>,
    pub(super) offset_ms: std::sync::atomic::AtomicU64,
}
impl ShiftedClock {
    pub(super) fn new(inner: Arc<dyn crate::Clock>, offset_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            inner,
            offset_ms: std::sync::atomic::AtomicU64::new(offset_ms),
        })
    }
}
#[async_trait::async_trait]
impl crate::Clock for ShiftedClock {
    fn now(&self) -> std::time::Instant {
        self.inner.now()
    }
    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.inner.timestamp_datetime()
            + chrono::Duration::milliseconds(
                i64::try_from(self.offset_ms.load(Ordering::SeqCst)).expect("offset"),
            )
    }
    async fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration).await;
    }
    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.inner.sleep_until(deadline).await;
    }
}
/// A reconcile tick's clock: an hour on, past every backoff a law's
/// failed attempts left.
const LATER_MS: u64 = 3_600_000;
struct Fixture {
    parts: DriveParts,
    factory: Arc<dyn crate::SessionStoreFactory>,
    stores: Arc<dyn crate::StoreSet>,
    /// The store set's `ControlIntent` obligation ledger.
    intents: Arc<dyn ObligationLedger>,
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
        let lease = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
            &parts.store,
            &parts.session_id,
            "parked-execution",
        )
        .await;
        // The aborted execution's root claim: it binds the held input to the
        // root and records the drive, on the head the runtime starts from,
        // that a redrive of the root replays (FIG-3840).
        let state = parts.initial_state();
        // The runtime opens its initial frame before it admits anything.
        let leaf = parts
            .runtime()
            .await
            .export_state()
            .session_graph
            .leaf_node_id
            .clone();
        let admission = parts
            .store
            .admit_root(&lash_core::store::AdmitRootRequest {
                fence: lease.clone(),
                root: root.clone(),
                head: lash_core::store::AdmittedHead::Input(input.clone()),
                max_inputs: 1,
                policy: lash_core::testing::queued_work_admission_policy(1),
                base: lash_core::store::SessionHeadRef {
                    generation: 0,
                    revision: state.head_revision,
                    leaf,
                    checkpoint: state.checkpoint_ref.clone(),
                },
                turn_index: state.turn_index as u64 + 1,
                generation: None,
                admitted_generation: lash_core::engine::BuildGeneration::for_test("root-control"),
            })
            .await
            .expect("admit the root")
            .expect("the root's admission reaches its head");
        assert_eq!(admission.input_ids(), vec![input.clone()]);
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
            .supersede_drive_epoch_for_test(&lease)
            .await
            .expect("execution stopped while the park holds the root");
        assert!(matches!(
            parts
                .store
                .list_pending_turn_inputs(&parts.session_id)
                .await
                .expect("held inputs")[0]
                .status,
            crate::PendingTurnInputReadStatus::Admitted { root: ref holder } if *holder == root
        ));
        Self {
            parts,
            factory: stores.session_store_factory(),
            stores: stores.clone(),
            intents: stores.obligation_ledger(ObligationKind::ControlIntent),
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
    fn control(&self, fail_release: bool, fail_close: bool) -> (Arc<Work>, Arc<Close>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Work(
                Arc::new(Control {
                    fail: AtomicBool::new(fail_release),
                    fail_releases: AtomicUsize::new(0),
                    permanent: AtomicBool::new(false),
                    lose_resume_reply: AtomicBool::new(false),
                    cancel_on_resume: Mutex::new(None),
                    events: events.clone(),
                }),
                AtomicUsize::new(0),
            )),
            Arc::new(Close {
                store: self.parts.store.clone(),
                fail: Arc::new(AtomicBool::new(fail_close)),
                events,
            }),
        )
    }

    /// The `ScopeClose` kind's relay over the law's stores (ADR 0109 §3):
    /// reads terminal evidence from the catalog, closes through `close`.
    /// The law runs on a zero base backoff: a missed first attempt is due
    /// again inside the same tick, the way the un-timed laws always ran.
    fn scope_close_relay(&self, close: &Close) -> lash_core::runtime::drive::ScopeCloseRelay {
        lash_core::runtime::drive::ScopeCloseRelay::new(
            self.stores.obligation_ledger(ObligationKind::ScopeClose),
            self.factory.clone(),
            Arc::new(close.clone()),
        )
        .with_policy(lash_core::runtime::drive::relay::RelayPolicy {
            base_backoff_ms: 0,
            ..lash_core::runtime::drive::relay::RelayPolicy::default()
        })
    }
    /// The `ControlIntent` relay over this fixture's ledger, engine and
    /// scope owner, on `clock`.
    fn relay(
        &self,
        work: &Arc<Work>,
        close: &Arc<Close>,
        clock: Arc<dyn crate::Clock>,
    ) -> lash_core::runtime::drive::ControlIntentRelay {
        lash_core::runtime::drive::ControlIntentRelay::new(
            Arc::clone(&self.intents),
            Arc::clone(&self.factory),
            Arc::clone(work) as Arc<dyn crate::SessionWorkEngine>,
            Arc::clone(close) as Arc<dyn ScopeCloseSink>,
            Arc::new(self.scope_close_relay(close)),
            clock,
        )
    }
    /// The verb's own immediate delivery of `intent`'s obligation.
    async fn apply(
        &self,
        work: &Arc<Work>,
        close: &Arc<Close>,
        intent: &ControlIntent,
    ) -> ControlIntentState {
        self.relay(work, close, Arc::clone(&self.parts.host.clock))
            .deliver_intent(intent)
            .await
            .expect("deliver intent")
    }
    /// One reconcile tick an hour on, with the `ControlIntent` relay
    /// claiming its due obligations.
    async fn reconcile(&self, work: &Arc<Work>, close: &Arc<Close>) -> ReconcileTick {
        let later: Arc<dyn crate::Clock> =
            ShiftedClock::new(Arc::clone(&self.parts.host.clock), LATER_MS);
        let relays: Vec<Arc<dyn lash_core::runtime::drive::relay::ObligationRelay>> = vec![
            Arc::new(self.relay(work, close, Arc::clone(&later))),
            Arc::new(self.scope_close_relay(close)),
        ];
        lash_core::runtime::drive::reconcile_once(
            &lash_core::runtime::drive::ReconcileParts {
                sessions: self.factory.as_ref(),
                work: work.as_ref(),
                scopes: close.as_ref(),
                processes: None,
                clock: later.as_ref(),
                duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                relays: &relays,
            },
            &ReconcileCursor::default(),
            NonZeroUsize::MIN.saturating_add(63),
        )
        .await
    }
    /// The relay's pass over the `ControlIntent` due index in `tick`.
    fn intent_pass(tick: &ReconcileTick) -> RelayPass {
        tick.obligations
            .iter()
            .find(|(kind, _)| *kind == ObligationKind::ControlIntent)
            .map(|(_, pass)| *pass)
            .expect("the tick claimed control-intent obligations")
    }
    /// Where `intent`'s obligation stands.
    async fn obligation(&self, intent: &ControlIntent) -> Option<ObligationState> {
        self.intents
            .state(intent.obligation.as_ref().expect("armed by its verb"))
            .await
            .expect("obligation read")
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

/// The root the fixture's input is bound to, if any.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn input_holder(f: &Fixture) -> Option<TurnId> {
    f.parts
        .store
        .list_pending_turn_inputs(&f.parts.session_id)
        .await
        .expect("list inputs")
        .into_iter()
        .find(|read| read.input.input_id == f.input)
        .and_then(|read| match read.status {
            crate::PendingTurnInputReadStatus::Admitted { root } => Some(root),
            _ => None,
        })
}

/// FIG-3927 N2, the verb paths: a parked root's cancel, its fork, the end
/// of its lost run and its session's close each end the root, and after each
/// no row is still bound to it. A fork hands the rows to its new root.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn no_row_stays_bound_after_a_roots_verb_close_or_lost_end(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let cancelled = Fixture::new(prefix, "unbound-cancel", &host, &stores).await;
    assert_eq!(
        input_holder(&cancelled).await,
        Some(cancelled.root.clone()),
        "the parked root holds its input"
    );
    cancelled.verb(RootVerb::Cancel).await.expect("cancel");
    assert_eq!(input_holder(&cancelled).await, None, "cancel");

    let forked = Fixture::new(prefix, "unbound-fork", &host, &stores).await;
    let intent = forked.verb(RootVerb::Fork).await.expect("fork");
    let holder = input_holder(&forked).await;
    assert!(
        holder.is_none() || holder == Some(forked_root(&forked.root, intent.id)),
        "fork: the input is open or bound to the new root, got {holder:?}"
    );

    let lost = Fixture::new(prefix, "unbound-lost", &host, &stores).await;
    lost.factory
        .end_lost_root(
            &RootRef {
                session: lost.parts.session_id.clone(),
                root: lost.root.clone(),
            },
            stores.clock().timestamp_ms(),
        )
        .await
        .expect("end the lost root")
        .expect("the root had no terminal");
    assert_eq!(input_holder(&lost).await, None, "lost-run end");

    let closed = Fixture::new(prefix, "unbound-close", &host, &stores).await;
    closed
        .factory
        .begin_session_close(&closed.parts.session_id, stores.clock().timestamp_ms())
        .await
        .expect("close")
        .expect("the session exists");
    assert_eq!(input_holder(&closed).await, None, "session close");
}

/// FIG-4018: a root whose run met a typed refusal no retry changes ends in
/// the store with that refusal, once. Its input is answered and no longer
/// bound to it, the session has no unfinished root, so its next input is
/// admitted under a new root, and a second end, or the lost-run end after
/// it, writes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_refused_root_ends_once_and_its_next_input_admits_a_new_root(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "refused-end", &host, &stores, 8).await;
    let root = TurnId::from("refused-end-root");
    let input = parts.enqueue("first", Some(root.as_str())).await;
    let fence = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
        &parts.store,
        &parts.session_id,
        "refused-execution",
    )
    .await;
    let state = parts.initial_state();
    let leaf = parts
        .runtime()
        .await
        .export_state()
        .session_graph
        .leaf_node_id
        .clone();
    let base = lash_core::store::SessionHeadRef {
        generation: 0,
        revision: state.head_revision,
        leaf,
        checkpoint: state.checkpoint_ref.clone(),
    };
    let admit = |root: &TurnId, head: &crate::InputId| lash_core::store::AdmitRootRequest {
        fence: fence.clone(),
        root: root.clone(),
        head: lash_core::store::AdmittedHead::Input(head.clone()),
        max_inputs: 1,
        policy: lash_core::testing::queued_work_claim_policy(1),
        base: base.clone(),
        turn_index: state.turn_index as u64 + 1,
        generation: None,
        admitted_generation: lash_core::engine::BuildGeneration::for_test("refused-end"),
    };
    parts
        .store
        .admit_root(&admit(&root, &input))
        .await
        .expect("admit the root")
        .expect("the root's admission reaches its head");
    let next = parts.enqueue("next", Some("refused-end-next")).await;

    let refusal = lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::StoreCommitSuperseded,
        "the head moved under the root's commit",
    );
    let at_ms = stores.clock().timestamp_ms();
    let terminal = parts
        .store
        .end_refused_root(&parts.session_id, &root, &refusal, at_ms)
        .await
        .expect("end the refused root")
        .expect("the root had no terminal");
    assert_eq!(terminal.kind, RootTerminalKind::Failed);
    assert_eq!(
        terminal.cause,
        RootTerminalCause::Refused {
            code: refusal.code.clone(),
            message: refusal.message.clone(),
        }
    );
    assert_eq!(
        parts
            .store
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("terminal read"),
        Some(terminal.clone()),
        "the end is the root's terminal evidence"
    );
    let pending = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("pending");
    assert!(
        pending.iter().all(|row| row.input.input_id != input),
        "the refused root's input is answered: {pending:?}"
    );
    assert!(
        parts
            .store
            .unfinished_root(&parts.session_id)
            .await
            .expect("unfinished read")
            .is_none(),
        "the refused root no longer holds its session"
    );

    assert!(
        parts
            .store
            .end_refused_root(&parts.session_id, &root, &refusal, at_ms + 1)
            .await
            .expect("a second end")
            .is_none(),
        "a second end writes nothing"
    );
    let factory = stores.session_store_factory();
    assert!(
        factory
            .end_lost_root(
                &RootRef {
                    session: parts.session_id.clone(),
                    root: root.clone(),
                },
                at_ms + 2,
            )
            .await
            .expect("the lost-run end")
            .is_none(),
        "the lost-run end writes nothing over the refusal"
    );
    assert_eq!(
        parts
            .store
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("terminal read"),
        Some(terminal),
        "the root keeps its one terminal"
    );

    let next_root = TurnId::from("refused-end-next-root");
    let admission = parts
        .store
        .admit_root(&admit(&next_root, &next))
        .await
        .expect("admit the next root")
        .expect("the next input heads a new root");
    assert_eq!(admission.input_ids(), vec![next]);
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
    // Gap 0: the verb's own delivery never ran. Gap 1: it ran and the
    // engine's release failed. Either way the obligation stays due, and the
    // relay's due pass completes it.
    for gap in 0..2 {
        let f = Fixture::new(prefix, &format!("intent-gap-{gap}"), &host, &stores).await;
        let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
        let (work, close) = f.control(gap == 1, false);
        if gap > 0 {
            assert!(matches!(
                f.apply(&work, &close, &intent).await,
                ControlIntentState::Failed {
                    retryable: true,
                    ..
                }
            ));
        }
        assert_eq!(f.obligation(&intent).await, Some(ObligationState::Due));
        let report = f.reconcile(&work, &close).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(Fixture::intent_pass(&report).delivered, 1);
        assert_eq!(
            f.obligation(&intent).await,
            Some(ObligationState::Delivered)
        );
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
    // Its obligation stalled `refused`, listed for an operator; nothing
    // retries it until one re-arms it.
    assert_eq!(f.obligation(&intent).await, Some(ObligationState::Stalled));
    let stalled = f
        .intents
        .list_stalled(None, NonZeroUsize::MIN.saturating_add(63))
        .await
        .expect("stalled list");
    assert!(stalled.iter().any(|stalled| {
        Some(&stalled.id) == intent.obligation.as_ref()
            && stalled.reason == StallReason::Refused
            && stalled.key
                == Ok(ObligationKey::ControlIntent {
                    intent_id: intent.id,
                })
    }));
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
    // The relay does not retry it, and the scope owner closes the ended
    // root's scope.
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(Fixture::intent_pass(&report).claimed, 0);
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

pub async fn a_diverged_root_parks_once_holds_its_admitted_rows_blocks_admission_and_completes_after_restore(
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
        crate::PendingTurnInputReadStatus::Admitted { root: ref holder } if *holder == f.root
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
    let turn = PhysicalTurn::derive_turn_id(&f.root, 1);
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
/// accepted input's drive is asked for through its ingress obligation —
/// here while its session is parked, where the drive it asks for stops at
/// the park. The engine accepting the ask does not deliver the obligation
/// (D19): its claim holds while the drive runs, so neither the producer nor
/// the tick asks again. The verb that resolves the park drives the session
/// on through its own follow-on drive.
pub async fn a_parked_session_is_asked_to_drive_only_through_its_ingress_obligation(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "parked-drive-ask", &host, &stores).await;
    let behind = f.parts.enqueue("behind", Some("behind-root")).await;
    let (work, close) = f.control(false, false);
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
        ledger.state(&id).await.expect("requested state"),
        Some(ObligationState::Claimed),
        "an accepted ask is not a delivery: the drive's admission settles it"
    );
    relay.deliver_admitted(behind.as_str()).await;
    let cancel = f.verb(RootVerb::Cancel).await.expect("cancel");
    // The engine half ran and was acknowledged under the verb's claim; the
    // drive ask after it was lost with the process, so the claim lapses
    // unsettled.
    let claim = f
        .intents
        .claim(cancel.obligation.as_ref().expect("armed"), 3, 0)
        .await
        .expect("claim")
        .expect("due");
    assert!(matches!(
        f.factory
            .acknowledge_intent(cancel.id, &claim.token, 3)
            .await
            .expect("acknowledge"),
        IntentSettle::Held(ControlIntent {
            state: ControlIntentState::Acknowledged { .. },
            ..
        })
    ));
    let resolved = f.reconcile(&work, &close).await;
    assert!(resolved.failures.is_empty(), "{:?}", resolved.failures);
    // The follow-on drive is the intent obligation's to deliver: the relay
    // retakes the lapsed claim and asks for it. Nothing asks for the input
    // again: its own ask is still claimed, and the tick scans no session.
    assert_eq!(Fixture::intent_pass(&resolved).delivered, 1);
    assert_eq!(
        scheduled(&work),
        2,
        "the resolved park's follow-on drive is the only other ask"
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
            matches!(admitted.work(), AdmittedWork::Commands { .. }),
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
    // The intent's obligation relay settles the lost intent within a tick.
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(Fixture::intent_pass(&report).delivered, 1);
    assert!(
        matches!(
            f.intent_state(intent.id).await,
            ControlIntentState::Acknowledged { .. }
        ),
        "the relay settled the lost acknowledgement"
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

/// A clock that stands still until a law moves it.
#[derive(Debug)]
struct ManualClock(std::sync::atomic::AtomicU64);
#[async_trait::async_trait]
impl crate::Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.0.load(Ordering::SeqCst)),
        )
    }
    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }
    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

/// S8-C (ADR 0109 §3): a cancel's or fork's delivery is done once the old
/// execution is released. The root's scope close — its children's cancel —
/// is not the intent's to finish: a child whose cancel keeps failing leaves
/// the intent acknowledged, its obligation delivered and its session
/// admitting, and the root scope's own recovery owner closes the scope.
pub async fn a_failing_child_cancel_never_wedges_its_roots_cancel_or_fork(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for verb in [RootVerb::Cancel, RootVerb::Fork] {
        let f = Fixture::new(prefix, &format!("child-cancel-{verb:?}"), &host, &stores).await;
        let next = f.parts.enqueue("behind", Some("behind-root")).await;
        let intent = f.verb(verb).await.expect("verb");
        // The root's scope close fails: a child's cancel did not go through.
        let (work, close) = f.control(false, true);
        assert!(
            matches!(
                f.apply(&work, &close, &intent).await,
                ControlIntentState::Acknowledged { .. }
            ),
            "{verb:?}: a failed child cancel holds the verb open"
        );
        assert!(
            !f.parts.epoch().await.control_pending,
            "{verb:?}: the session admits again"
        );
        assert_eq!(
            f.obligation(&intent).await,
            Some(ObligationState::Delivered)
        );
        assert_eq!(
            *work.0.events.lock().expect("events"),
            ["release", "schedule"]
        );
        // The ended root's scope is its recovery owner's to close.
        let report = f.reconcile(&work, &close).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(work.0.events.lock().expect("events").contains(&"close"));
        if verb == RootVerb::Cancel {
            let outcome = drive(&f, &runner, "after-child-cancel").await;
            assert_eq!(outcome.stop, DriveStop::Idle);
            assert_eq!(
                f.parts.applications().await,
                vec![(next, TurnId::from("behind-root"))]
            );
        }
    }
}

/// S8-C (ADR 0109 §1.3): every write that settles an intent's engine half
/// compares its obligation's claim. A delivery whose claim lapsed and was
/// retaken by another relay neither acknowledges nor fails the intent; the
/// live claim does.
pub async fn a_delivery_whose_claim_was_retaken_never_settles_its_intent(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "retaken-claim", &host, &stores).await;
    let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
    let id = intent
        .obligation
        .clone()
        .expect("the verb armed its obligation");
    // A relay claims it with a claim that lapses at once...
    let stale = f
        .intents
        .claim(&id, 3, 0)
        .await
        .expect("claim")
        .expect("due");
    // ...and another relay retakes it through the due index.
    let fresh = f
        .intents
        .claim_due(4, 60_000, NonZeroUsize::MIN.saturating_add(63))
        .await
        .expect("due claim");
    let [fresh] = fresh
        .into_iter()
        .filter(|claimed| claimed.id == id)
        .collect::<Vec<_>>()
        .try_into()
        .expect("the lapsed claim is retaken");
    assert_ne!(fresh.token, stale.token);
    assert_eq!(fresh.attempts, 2);
    assert_eq!(
        f.factory
            .acknowledge_intent(intent.id, &stale.token, 5)
            .await
            .expect("stale acknowledgement"),
        IntentSettle::ClaimLost
    );
    assert_eq!(
        f.factory
            .record_intent_failure(intent.id, &stale.token, "late refusal", false, 5)
            .await
            .expect("stale failure"),
        IntentSettle::ClaimLost
    );
    assert_eq!(f.intent_state(intent.id).await, ControlIntentState::Pending);
    assert!(f.parts.epoch().await.control_pending);
    assert!(matches!(
        f.factory
            .acknowledge_intent(intent.id, &fresh.token, 6)
            .await
            .expect("live acknowledgement"),
        IntentSettle::Held(ControlIntent {
            state: ControlIntentState::Acknowledged { at_ms: 6 },
            ..
        })
    ));
    assert!(!f.parts.epoch().await.control_pending);
}

/// S8-C (ADR 0109 §1.4, §3): an intent whose engine half keeps failing is
/// retried after a capped exponential backoff, never before, and at the
/// kind's attempt ceiling its obligation stalls and the intent closes
/// `Failed { retryable: false }` — surfaced, never a wedge: its session
/// drives the next root. Re-arming the stalled obligation reopens the
/// intent, and its next delivery completes it.
pub async fn an_intent_whose_engine_half_keeps_failing_stalls_at_its_ceiling_and_unwedges_its_session(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "intent-ceiling", &host, &stores).await;
    let next = f.parts.enqueue("behind", Some("behind-root")).await;
    let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
    let id = intent.obligation.clone().expect("armed");
    let (work, close) = f.control(false, false);
    work.0.fail_releases.store(usize::MAX, Ordering::SeqCst);
    let clock = Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(
        f.parts.host.clock.timestamp_ms(),
    )));
    let policy = lash_core::runtime::drive::relay::RelayPolicy {
        attempt_ceiling: std::num::NonZeroU32::new(3).expect("ceiling"),
        ..Default::default()
    };
    let relay = f
        .relay(&work, &close, Arc::clone(&clock) as Arc<dyn crate::Clock>)
        .with_policy(policy);
    let page = NonZeroUsize::MIN.saturating_add(63);
    // Attempt 1: the verb's own.
    assert!(matches!(
        relay.deliver_intent(&intent).await.expect("deliver"),
        ControlIntentState::Failed {
            retryable: true,
            ..
        }
    ));
    assert!(f.parts.epoch().await.control_pending);
    for attempt in 2..=3_u32 {
        // Never before its backoff...
        let early = lash_core::runtime::drive::relay::relay_due(&relay, clock.as_ref(), page)
            .await
            .expect("early pass");
        assert_eq!(early.claimed, 0, "attempt {attempt} waits for its backoff");
        clock
            .0
            .fetch_add(policy.backoff_ms(attempt - 1), Ordering::SeqCst);
        // ...and taken once it elapsed.
        let pass = lash_core::runtime::drive::relay::relay_due(&relay, clock.as_ref(), page)
            .await
            .expect("pass");
        assert_eq!(pass.claimed, 1, "attempt {attempt}");
        if attempt < 3 {
            assert_eq!(pass.retried, 1);
            assert!(f.parts.epoch().await.control_pending);
        } else {
            assert_eq!(pass.stalled, 1);
        }
    }
    let stalled = f
        .intents
        .list_stalled(None, page)
        .await
        .expect("stalled")
        .into_iter()
        .find(|stalled| stalled.id == id)
        .expect("the obligation stalled");
    assert_eq!(stalled.reason, StallReason::AttemptsExhausted);
    assert_eq!(stalled.attempts, 3);
    assert!(matches!(
        f.intent_state(intent.id).await,
        ControlIntentState::Failed {
            retryable: false,
            ..
        }
    ));
    // Stalled, not a wedge: the session drives its next root.
    assert!(!f.parts.epoch().await.control_pending);
    assert!(work.0.events.lock().expect("events").contains(&"schedule"));
    let outcome = drive(&f, &runner, "after-ceiling").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![(next, TurnId::from("behind-root"))]
    );
    // Nothing retries a stalled obligation...
    clock.0.fetch_add(policy.max_backoff_ms, Ordering::SeqCst);
    let idle = lash_core::runtime::drive::relay::relay_due(&relay, clock.as_ref(), page)
        .await
        .expect("idle pass");
    assert_eq!(idle.claimed, 0);
    // ...until an operator re-arms it: that reopens the intent, and its
    // next delivery completes it.
    work.0.fail_releases.store(0, Ordering::SeqCst);
    assert!(
        f.intents
            .rearm(&id, clock.0.load(Ordering::SeqCst))
            .await
            .expect("rearm")
    );
    assert_eq!(f.intent_state(intent.id).await, ControlIntentState::Pending);
    let pass = lash_core::runtime::drive::relay::relay_due(&relay, clock.as_ref(), page)
        .await
        .expect("re-armed pass");
    assert_eq!(pass.delivered, 1);
    assert!(matches!(
        f.intent_state(intent.id).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        f.obligation(&intent).await,
        Some(ObligationState::Delivered)
    );
}

/// S8-C (ADR 0109 §3): a cancel's follow-on drive is part of its delivery,
/// not a fire-and-forget ask. An ask the engine did not accept leaves the
/// intent acknowledged and its obligation due; the relay's next pass asks
/// again and only then delivers it.
pub async fn a_refused_follow_on_drive_keeps_the_intents_obligation_due(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "refused-follow-on", &host, &stores).await;
    let intent = f.verb(RootVerb::Cancel).await.expect("cancel");
    let (work, close) = f.control(false, false);
    work.1.store(1, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert!(!f.parts.epoch().await.control_pending);
    assert_eq!(f.obligation(&intent).await, Some(ObligationState::Due));
    assert_eq!(
        *work.0.events.lock().expect("events"),
        ["release", "close", "drive-refused"]
    );
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(Fixture::intent_pass(&report).delivered, 1);
    assert_eq!(
        f.obligation(&intent).await,
        Some(ObligationState::Delivered)
    );
    let events = work.0.events.lock().expect("events").clone();
    assert_eq!(
        events[..4],
        ["release", "close", "drive-refused", "schedule"],
        "the redelivery asks for the drive again"
    );
    assert_eq!(
        events.iter().filter(|event| **event == "release").count(),
        1,
        "the redelivery releases nothing twice"
    );
}
