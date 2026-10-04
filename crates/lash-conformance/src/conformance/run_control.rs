//! Run-control laws over each tier's durable session catalog.
#![expect(
    clippy::expect_used,
    reason = "law preconditions and outcomes are assertions"
)]
use super::shift_admission::{ShiftParts, on_tier};
use lash_core::engine::*;
use lash_core::store::*;
use lash_core::testing::RuntimeStoreTestShiftExt as _;
use lash_core::testing::{Gate, Script};
use lash_sansio::{SessionId, TurnId};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod intent_ledger_fault;
mod interleavings;
mod lost_run;
mod ownership;
pub use intent_ledger_fault::*;
pub use interleavings::*;
pub use lost_run::*;
pub use ownership::*;

struct Control {
    fail: AtomicBool,
    /// Releases that fail retryably before one succeeds.
    fail_releases: AtomicUsize,
    permanent: AtomicBool,
    /// The engine resumes the run, then its reply is lost: the admin call
    /// timed out after the server acted.
    lose_resume_reply: AtomicBool,
    cancel_on_resume: Mutex<Option<(Arc<dyn crate::DeploymentStore>, RunIntentRequest)>>,
    /// Sessions whose every release waits at a gate the law opens.
    release_gates: Mutex<Vec<(SessionId, Arc<Gate>)>>,
    events: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait::async_trait]
impl SessionControlEngine for Control {
    async fn resume_run(
        &self,
        _: &RunRef,
        _: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let cancellation = self.cancel_on_resume.lock().expect("resume hook").take();
        if let Some((factory, request)) = cancellation {
            factory
                .open_run_intent(&request, 5)
                .await
                .expect("concurrent cancel");
        }
        self.events.lock().expect("events").push("resume");
        if self.lose_resume_reply.swap(false, Ordering::SeqCst) {
            return Err(EngineRefusal::retryable(
                crate::RuntimeErrorCode::EngineControlRequest,
                "the resume timed out after the engine acted",
            ));
        }
        Ok(EngineAck::NothingHeld)
    }
    async fn release_run(
        &self,
        run: &RunRef,
        _: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        let gate = self
            .release_gates
            .lock()
            .expect("release gates")
            .iter()
            .find(|(session, _)| *session == run.session)
            .map(|(_, gate)| Arc::clone(gate));
        if let Some(gate) = gate {
            gate.pass().await;
        }
        if self.permanent.load(Ordering::SeqCst) {
            return Err(EngineRefusal::permanent(
                crate::RuntimeErrorCode::PluginSessionManager,
                "permanent engine refusal",
            ));
        }
        if self.fail.swap(false, Ordering::SeqCst)
            || self
                .fail_releases
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
            return Err(EngineRefusal::retryable(
                crate::RuntimeErrorCode::EngineControlRequest,
                "release interrupted",
            ));
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
    fn schedule_shift(&self, _: &SessionId, _: ShiftRequestId) {
        self.0.events.lock().expect("events").push("schedule");
    }
    /// Refuses as many asks as the fixture armed, then accepts each.
    async fn request_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<(), EngineRefusal> {
        if self
            .1
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            self.0.events.lock().expect("events").push("shift-refused");
            return Err(EngineRefusal::retryable(
                crate::RuntimeErrorCode::EngineControlRequest,
                "the shift ask was not accepted",
            ));
        }
        self.schedule_shift(session, request);
        Ok(())
    }
    fn install_session_shifts(
        &self,
        shifts: Arc<dyn crate::SessionShifts>,
    ) -> Arc<dyn crate::SessionShifts> {
        shifts
    }
    fn control(&self) -> Arc<dyn SessionControlEngine> {
        self.0.clone()
    }
}
#[derive(Clone)]
struct Close {
    store: Arc<dyn crate::RuntimeStore>,
    fail: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait::async_trait]
impl ScopeCloseSink for Close {
    async fn close_run_scope(&self, terminal: &RunTerminal) -> Result<(), crate::StoreError> {
        assert!(
            self.store
                .run_terminal(&terminal.session_id, &terminal.run)
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
        panic!("run verb cannot close session")
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
    parts: ShiftParts,
    factory: Arc<dyn crate::DeploymentStore>,
    stores: Arc<dyn crate::StoreSet>,
    /// The store set's `ControlIntent` obligation ledger.
    intents: Arc<dyn ObligationLedger>,
    run: TurnId,
    input: crate::InputId,
    park: TurnPark,
}
/// A session whose run holds its admitted input under a live shift fence,
/// before anything parked it.
struct AdmittedRun {
    parts: ShiftParts,
    run: TurnId,
    input: crate::InputId,
    lease: ShiftFence,
}
impl AdmittedRun {
    async fn new(
        prefix: &str,
        name: &str,
        host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
    ) -> Self {
        let parts = ShiftParts::new(prefix, name, host, stores, 8).await;
        let run = TurnId::fixture(format!("{name}-run"));
        let input = parts.enqueue("first", Some(run.as_str())).await;
        let lease = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
            &parts.store,
            &parts.session_id,
            "parked-execution",
        )
        .await;
        // The aborted execution's run claim: it binds the held input to the
        // run and records the shift, on the head the runtime starts from,
        // that a redrive of the run replays (FIG-3840).
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
            .admit_run(&lash_core::store::AdmitRunRequest {
                unsealed_epoch: None,
                fence: lease.clone(),
                run: run.clone(),
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
                admitted_generation: lash_core::engine::BuildGeneration::for_test("run-control"),
                executor: lash_core::store::RunExecutor::run(&lash_core::store::AdmissionId::new(
                    "fixture#0",
                )),
                plugins: Default::default(),
                turn_cancellation: None,
                trace_scopes: std::sync::Arc::new(lash_core::UntracedScopes),
            })
            .await
            .expect("admit the run")
            .expect("the run's admission reaches its head");
        assert_eq!(admission.input_ids(), vec![input.clone()]);
        Self {
            parts,
            run,
            input,
            lease,
        }
    }
}
impl Fixture {
    async fn new(
        prefix: &str,
        name: &str,
        host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
    ) -> Self {
        let admitted = AdmittedRun::new(prefix, name, host, stores).await;
        let park = admitted
            .parts
            .store
            .record_turn_park(&TurnParkWrite::refusal(
                admitted.parts.session_id.clone(),
                admitted.run.clone(),
                ParkReason::ReplayDivergence {
                    message: "old build".into(),
                },
                1,
            ))
            .await
            .map(lash_core::store::StoreTransition::into_record)
            .expect("park");
        Self::parked(admitted, stores, park).await
    }
    /// The fixture over `admitted`, whose run `park` holds: its execution
    /// stops here.
    async fn parked(
        admitted: AdmittedRun,
        stores: &Arc<dyn crate::StoreSet>,
        park: TurnPark,
    ) -> Self {
        let AdmittedRun {
            parts,
            run,
            input,
            lease,
        } = admitted;
        parts
            .store
            .supersede_shift_epoch_for_test(&lease)
            .await
            .expect("execution stopped while the park holds the run");
        assert!(matches!(
            parts
                .store
                .list_pending_turn_inputs(&parts.session_id)
                .await
                .expect("held inputs")[0]
                .status,
            crate::PendingTurnInputReadStatus::Admitted { run: ref holder } if *holder == run
        ));
        Self {
            parts,
            factory: stores.session_store_factory(),
            stores: stores.clone(),
            intents: stores.obligation_ledger(ObligationKind::ControlIntent),
            run,
            input,
            park,
        }
    }
    async fn verb(&self, verb: RunVerb) -> Result<ControlIntent, RunIntentRefused> {
        self.verb_at(verb, 2).await
    }
    /// `verb` recorded at `now_ms`, which is also when its obligation is due.
    async fn verb_at(&self, verb: RunVerb, now_ms: u64) -> Result<ControlIntent, RunIntentRefused> {
        self.factory
            .open_run_intent(
                &RunIntentRequest {
                    session_id: self.parts.session_id.clone(),
                    run: self.run.clone(),
                    park: self.park.park_id,
                    verb,
                },
                now_ms,
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
                    release_gates: Mutex::new(Vec::new()),
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
    fn scope_close_relay(&self, close: &Close) -> lash_core::runtime::shift::ScopeCloseRelay {
        lash_core::runtime::shift::ScopeCloseRelay::new(
            self.stores.obligation_ledger(ObligationKind::ScopeClose),
            self.factory.clone(),
            Arc::new(close.clone()),
        )
        .with_policy(lash_core::runtime::shift::relay::RelayPolicy {
            base_backoff_ms: 0,
            ..lash_core::runtime::shift::relay::RelayPolicy::default()
        })
    }
    /// The `ControlIntent` relay over this fixture's ledger, engine and
    /// scope owner, on `clock`.
    fn relay(
        &self,
        work: &Arc<Work>,
        close: &Arc<Close>,
        clock: Arc<dyn crate::Clock>,
    ) -> lash_core::runtime::shift::ControlIntentRelay {
        lash_core::runtime::shift::ControlIntentRelay::new(
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
        let relays: Vec<Arc<dyn lash_core::runtime::shift::relay::ObligationRelay>> = vec![
            Arc::new(self.relay(work, close, Arc::clone(&later))),
            Arc::new(self.scope_close_relay(close)),
        ];
        lash_core::runtime::shift::reconcile_once(
            &lash_core::runtime::shift::ReconcileParts {
                metrics: &Default::default(),
                sessions: self.factory.as_ref(),
                work: work.as_ref(),
                scopes: close.as_ref(),
                processes: None,
                clock: later.as_ref(),
                duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
                relays: &relays,
                lanes: &super::helpers::law_tick_lanes(Arc::clone(&later)),
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
            .state(intent.obligation_id().expect("armed by its verb"))
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
    /// Whether queued batch `batch_id` is still open on the session's
    /// queue: not yet applied, and not withdrawn.
    async fn command_is_open(&self, batch_id: &crate::BatchId) -> bool {
        self.parts
            .store
            .list_queued_work(&self.parts.session_id)
            .await
            .expect("queue read")
            .iter()
            .any(|batch| batch.batch_id == *batch_id)
    }
    async fn intent(&self, id: ControlIntentId) -> ControlIntent {
        self.factory
            .load_intent(id)
            .await
            .expect("intent read")
            .expect("intent retained")
    }
    async fn intent_state(&self, id: ControlIntentId) -> ControlIntentState {
        self.intent(id).await.state
    }
    /// Whether `id`'s engine half is still owed.
    async fn owed(&self, id: ControlIntentId) -> bool {
        self.intent(id).await.engine_half_owed()
    }
}

/// A commit of `run`'s physical turn `turn` over `state` that ends the run,
/// as the runtime writes it: the run's terminal evidence in the head
/// transaction.
pub(super) fn run_final_commit(
    state: &crate::RuntimeSessionState,
    run: &TurnId,
    turn: &TurnId,
    ordinal: u32,
) -> crate::RuntimeCommit {
    let operation = crate::OperationId::turn(state.session_id.clone(), turn.clone(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("nodes");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state, graph, operation,
    )
    .expect("commit");
    commit.run_terminal = Some(Box::new(RunTerminalWrite {
        run: run.clone(),
        commit: TurnCommitId::new(run.clone(), ordinal),
        turn: turn.clone(),
        outcome: crate::store::RunCommittedOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }));
    commit
}

async fn shift(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> ShiftOutcome {
    shift_result(f, runner, name).await.expect("the shift runs")
}

/// One shift of request `name` to a stop, answered with the abort it ended
/// on when it refused one.
async fn shift_result(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> Result<ShiftOutcome, ShiftAbort> {
    let request = f.parts.request(name);
    on_tier(runner, &f.parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(
            async move { lash_core::shift::work_session(&mut runtime, &scope, &request).await },
        )
    })
    .await
}

/// A racing shift, in flight: in process it answers its refusal at once;
/// on Restate the attempt dies inside its recorded admission step and the
/// server keeps retrying the open invocation until the law settles what it
/// waits on.
fn spawn_shift(
    f: &Fixture,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    name: &str,
) -> tokio::task::JoinHandle<Result<ShiftOutcome, ShiftAbort>> {
    let parts = f.parts.clone();
    let runner = Arc::clone(runner);
    let request = parts.request(name);
    tokio::spawn(async move {
        on_tier(&runner, &parts, move |mut runtime, scope| {
            let request = request.clone();
            Box::pin(
                async move { lash_core::shift::work_session(&mut runtime, &scope, &request).await },
            )
        })
        .await
    })
}

/// Puts the law session's store under `script` and holds its second
/// parked-run probe at the returned gate. Admission reads the park before it
/// decides, so every probe is one admission evaluation. On Restate a
/// refusal's retry re-decides admission inside its recorded step, and holding
/// that re-decision at the park read keeps it suspended rather than burning
/// the invocation's attempt budget, so the law's settle cannot lose the race.
fn hold_the_second_park_probe(f: &mut Fixture, script: &Script) -> Arc<Gate> {
    let held = script.on(StoreOp::load_turn_park).nth(2).before().pause();
    f.parts.store = script.wrap("racing", Arc::clone(&f.parts.store));
    held
}

pub async fn a_terminal_run_never_reparks(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "terminal-no-park", &host, &stores).await;
    f.verb(RunVerb::Cancel).await.expect("cancel");
    assert!(matches!(
        f.parts
            .store
            .record_turn_park(&TurnParkWrite::refusal(
                f.parts.session_id.clone(),
                f.run.clone(),
                f.park.reason.clone(),
                3
            ))
            .await
            .map(lash_core::store::StoreTransition::into_record),
        Err(crate::StoreError::RunAlreadyTerminal { .. })
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

/// The run the fixture's input is bound to, if any.
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
            crate::PendingTurnInputReadStatus::Admitted { run } => Some(run),
            _ => None,
        })
}

/// FIG-3927 N2, the verb paths: a parked run's cancel, its fork, the end
/// of its lost execution and its session's close each end the run, and after each
/// no row is still bound to it. A fork hands the rows to its new run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn no_row_stays_bound_after_a_runs_verb_close_or_lost_end(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let cancelled = Fixture::new(prefix, "unbound-cancel", &host, &stores).await;
    assert_eq!(
        input_holder(&cancelled).await,
        Some(cancelled.run.clone()),
        "the parked run holds its input"
    );
    cancelled.verb(RunVerb::Cancel).await.expect("cancel");
    assert_eq!(input_holder(&cancelled).await, None, "cancel");

    let forked = Fixture::new(prefix, "unbound-fork", &host, &stores).await;
    let intent = forked.verb(RunVerb::Fork).await.expect("fork");
    let holder = input_holder(&forked).await;
    assert!(
        holder.is_none() || holder == Some(forked_run(&forked.run, intent.id)),
        "fork: the input is open or bound to the new run, got {holder:?}"
    );

    let lost = Fixture::new(prefix, "unbound-lost", &host, &stores).await;
    lost.factory
        .end_lost_run(
            &RunRef {
                session: lost.parts.session_id.clone(),
                run: lost.run.clone(),
            },
            RunLoss::FailedRun,
            stores.clock().timestamp_ms(),
        )
        .await
        .expect("end the lost run")
        .expect("the run had no terminal");
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

/// FIG-4018: a run whose execution met a typed refusal no retry changes ends in
/// the store with that refusal, once. Its input is answered and no longer
/// bound to it, the session has no unfinished run, so its next input is
/// admitted under a new run, and a second end, or the lost-run end after
/// it, writes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_refused_run_ends_once_and_its_next_input_admits_a_new_run(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "refused-end", &host, &stores, 8).await;
    let run = TurnId::from("refused-end-run");
    let input = parts.enqueue("first", Some(run.as_str())).await;
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
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
    let admit = |run: &TurnId, head: &crate::InputId| lash_core::store::AdmitRunRequest {
        unsealed_epoch: None,
        fence: fence.clone(),
        run: run.clone(),
        head: lash_core::store::AdmittedHead::Input(head.clone()),
        max_inputs: 1,
        policy: lash_core::testing::queued_work_admission_policy(1),
        base: base.clone(),
        turn_index: state.turn_index as u64 + 1,
        admitted_generation: lash_core::engine::BuildGeneration::for_test("refused-end"),
        executor: lash_core::store::RunExecutor::run(&lash_core::store::AdmissionId::new(
            "fixture#0",
        )),
        plugins: Default::default(),
        turn_cancellation: None,
        trace_scopes: std::sync::Arc::new(lash_core::UntracedScopes),
    };
    parts
        .store
        .admit_run(&admit(&run, &input))
        .await
        .expect("admit the run")
        .expect("the run's admission reaches its head");
    let next = parts.enqueue("next", Some("refused-end-next")).await;

    let refusal = lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::StoreCommitSuperseded,
        "the head moved under the run's commit",
    );
    let at_ms = stores.clock().timestamp_ms();
    let crate::store::RunEndOutcome::Ended(terminal) = parts
        .store
        .end_refused_run(&fence, &run, &refusal, at_ms)
        .await
        .expect("end the refused run")
    else {
        panic!("the run had no terminal");
    };
    assert_eq!(terminal.kind(), RunTerminalKind::Failed);
    assert_eq!(
        terminal.cause,
        RunTerminalCause::Refused {
            code: refusal.code.clone(),
            message: refusal.message.clone(),
            refusal_cause: None,
        }
    );
    assert_eq!(
        parts
            .store
            .run_terminal(&parts.session_id, &run)
            .await
            .expect("terminal read"),
        Some(terminal.clone()),
        "the end is the run's terminal evidence"
    );
    let pending = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("pending");
    assert!(
        pending.iter().all(|row| row.input.input_id != input),
        "the refused run's input is answered: {pending:?}"
    );
    assert!(
        parts
            .store
            .unfinished_run(&parts.session_id)
            .await
            .expect("unfinished read")
            .is_none(),
        "the refused run no longer holds its session"
    );

    assert_eq!(
        parts
            .store
            .end_refused_run(&fence, &run, &refusal, at_ms + 1)
            .await
            .expect("a second end"),
        crate::store::RunEndOutcome::AlreadyEnded(terminal.clone()),
        "a second end writes nothing"
    );
    let factory = stores.session_store_factory();
    assert!(
        factory
            .end_lost_run(
                &RunRef {
                    session: parts.session_id.clone(),
                    run: run.clone(),
                },
                RunLoss::NoRun,
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
            .run_terminal(&parts.session_id, &run)
            .await
            .expect("terminal read"),
        Some(terminal),
        "the run keeps its one terminal"
    );

    let next_run = TurnId::from("refused-end-next-run");
    let admission = parts
        .store
        .admit_run(&admit(&next_run, &next))
        .await
        .expect("admit the next run")
        .expect("the next input heads a new run");
    assert_eq!(admission.input_ids(), vec![next]);

    // A refusal's structured cause is stored with it: a run refused because
    // its session was deleted answers its inputs with the session-retirement
    // refusal, not with the bare code.
    let retirement = lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::SessionDeleted,
        "the session was deleted under the run",
    )
    .with_cause(lash_core::RuntimeErrorCause::SessionDeleted {
        session_id: parts.session_id.clone(),
    });
    assert!(
        matches!(
            parts
                .store
                .end_refused_run(&fence, &next_run, &retirement, at_ms + 3)
                .await
                .expect("end the retired run"),
            crate::store::RunEndOutcome::Ended(_)
        ),
        "the next run had no terminal"
    );
    let stored = parts
        .store
        .run_terminal(&parts.session_id, &next_run)
        .await
        .expect("terminal read")
        .expect("the retired run's terminal");
    assert_eq!(
        stored.cause,
        RunTerminalCause::Refused {
            code: retirement.code.clone(),
            message: retirement.message.clone(),
            refusal_cause: retirement.cause.clone(),
        },
        "the stored refusal keeps its session-retirement cause"
    );
}

pub async fn cancel_of_a_parked_run_writes_cancelled_settles_its_input_and_drains_the_next(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "cancel-parked", &host, &stores).await;
    let next = f.parts.enqueue("next", Some("next-run")).await;
    let epoch = f.parts.epoch().await.epoch;
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
    assert_eq!(f.parts.epoch().await.epoch, epoch + 1);
    assert!(f.parts.epoch().await.control_pending);
    assert!(matches!(
        f.parts
            .store
            .seal_shift_epoch(
                &f.parts.session_id,
                &AdmissionId::new("premature"),
                epoch + 1,
                &RunStartNonce::new("premature"),
                None
            )
            .await
            .expect("fenced seal"),
        ShiftEpochSeal::Superseded { .. }
    ));
    let terminal = f
        .factory
        .run_terminal(&f.parts.session_id, &f.run)
        .await
        .expect("terminal")
        .expect("cancelled");
    assert_eq!(terminal.kind(), RunTerminalKind::Cancelled);
    assert!(
        matches!(terminal.cause, RunTerminalCause::OperatorCancelled { intent: id } if id == intent.id)
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
    let outcome = shift(&f, &runner, "after-cancel").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![(next, TurnId::from("next-run"))]
    );
    assert_eq!(f.parts.calls(), 1);
}

pub async fn fork_releases_the_old_owner_before_the_new_run_executes_in_original_order_on_a_fresh_journal(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut f = Fixture::new(prefix, "fork-parked", &host, &stores).await;
    // The parked run composed both inputs, which only a composing drain
    // does (FIG-4457).
    f.parts.compose_inputs();
    let second = f.parts.enqueue("second", Some("second")).await;
    f.parts
        .store
        .bind_run_inputs(&f.parts.session_id, &f.run, std::slice::from_ref(&second))
        .await
        .expect("second bound");
    let before = f
        .parts
        .store
        .list_pending_turn_inputs(&f.parts.session_id)
        .await
        .expect("pending");
    let intent = f.verb(RunVerb::Fork).await.expect("fork");
    let ControlIntentKind::Fork {
        new_run: Some(ref new_run),
        ..
    } = intent.kind
    else {
        panic!("new input run");
    };
    assert_eq!(
        *new_run,
        TurnId::fixture(format!("{}~fork{}", f.run, intent.id))
    );
    for input in [&f.input, &second] {
        assert_eq!(
            f.parts
                .store
                .run_binding(&f.parts.session_id, input)
                .await
                .expect("binding"),
            Some(new_run.clone())
        );
    }
    assert!(
        f.factory
            .run_terminal(&f.parts.session_id, new_run)
            .await
            .expect("new run")
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
        ControlIntentState::Pending
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
    let outcome = shift(&f, &runner, "after-fork").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), new_run.clone()),
            (second, new_run.clone())
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
    let request = RunIntentRequest {
        session_id: f.parts.session_id.clone(),
        run: f.run.clone(),
        park: ParkId::from_feed_sequence(f.park.park_id.feed_sequence() + 1),
        verb: RunVerb::Cancel,
    };
    assert!(
        matches!(f.factory.open_run_intent(&request, 2).await, Err(RunIntentRefused::ParkSuperseded { current }) if current == f.park.park_id)
    );
    assert!(
        f.factory
            .list_control_intents(None, NonZeroUsize::MIN)
            .await
            .expect("ledger")
            .is_empty()
    );
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
    assert!(
        matches!(f.verb(RunVerb::Cancel).await, Err(RunIntentRefused::IntentOpen { intent: id }) if id == intent.id)
    );
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    assert!(matches!(
        f.verb(RunVerb::Cancel).await,
        Err(RunIntentRefused::NotParked)
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
    let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
    assert_eq!(f.parts.epoch().await.epoch, epoch);
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    let again = f
        .parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.run.clone(),
            f.park.reason.clone(),
            3,
        ))
        .await
        .map(lash_core::store::StoreTransition::into_record)
        .expect("repark");
    assert_eq!(again.park_id, f.park.park_id);
    assert_eq!(again.attempts, f.park.attempts + 1);
    assert_eq!(again.resume_intent, None);
    f.verb(RunVerb::Cancel).await.expect("cancel reparked run");
}

pub async fn cancel_or_fork_of_a_redriving_run_is_refused(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "redrive-refusal", &host, &stores).await;
    let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    for verb in [RunVerb::Cancel, RunVerb::Fork] {
        assert!(
            matches!(f.verb(verb).await, Err(RunIntentRefused::Redriving { intent: id }) if id == intent.id)
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
        let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
        let (work, close) = f.control(gap == 1, false);
        if gap > 0 {
            assert!(matches!(
                f.apply(&work, &close, &intent).await,
                ControlIntentState::Pending
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
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
    let (work, close) = f.control(false, false);
    work.0.permanent.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Refused { cause }
            if cause.code == crate::RuntimeErrorCode::PluginSessionManager
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
        Some(&stalled.id) == intent.obligation_id()
            && stalled.reason == StallReason::Refused
            && stalled.last_error.as_ref().is_some_and(|error| {
                error.code == crate::RuntimeErrorCode::PluginSessionManager
                    && error.message == "permanent engine refusal"
            })
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
        matches!(&retained[0].state, ControlIntentState::Refused { cause } if cause.code == crate::RuntimeErrorCode::PluginSessionManager && cause.message == "permanent engine refusal")
    );
    // A verb the engine refused for good is surfaced, never a wedge: its
    // store half ended the run and raised the epoch, so the unreleased
    // execution is fenced, and the session shifts the next send.
    assert!(!f.parts.epoch().await.control_pending);
    // The relay does not retry it, and the scope owner closes the ended
    // run's scope.
    let report = f.reconcile(&work, &close).await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(Fixture::intent_pass(&report).claimed, 0);
    assert!(work.0.events.lock().expect("events").contains(&"close"));
    let next = f
        .parts
        .enqueue("after-refusal", Some("after-refusal"))
        .await;
    let outcome = shift(&f, &runner, "after-refusal").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.parts.calls(), 1, "the send behind the refused verb runs");
    assert_eq!(
        f.parts.applications().await,
        vec![(next, TurnId::from("after-refusal"))]
    );
}

pub async fn sends_behind_a_parked_run_commit_but_are_not_admitted(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "behind-park", &host, &stores).await;
    let next = f.parts.enqueue("behind", Some("behind-run")).await;
    let before = f.parts.epoch().await;
    let outcome = shift(&f, &runner, "blocked").await;
    assert!(matches!(outcome.stop, ShiftStop::Parked(_)));
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
    let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    let outcome = shift(&f, &runner, "restored").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(f.parts.calls(), 1);
    assert_eq!(
        f.parts.applications().await,
        vec![(f.input.clone(), f.run.clone())]
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
            .run_terminal(&f.parts.session_id, &f.run)
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
    let pending = racing.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = racing.control(false, false);
    *work.0.cancel_on_resume.lock().expect("resume hook") = Some((
        racing.factory.clone(),
        RunIntentRequest {
            session_id: racing.parts.session_id.clone(),
            run: racing.run.clone(),
            park: racing.park.park_id,
            verb: RunVerb::Cancel,
        },
    ));
    assert!(matches!(
        racing.apply(&work, &close, &pending).await,
        ControlIntentState::Superseded { .. }
    ));
    let f = Fixture::new(prefix, "stale-redrive", &host, &stores).await;
    // The fence the parked run's execution was sealed under.
    let ShiftEpochSeal::Sealed(fence) = f
        .parts
        .store
        .seal_shift_epoch(
            &f.parts.session_id,
            &AdmissionId::new("parked-run#0"),
            f.parts.epoch().await.epoch,
            &RunStartNonce::new("parked-execution"),
            None,
        )
        .await
        .expect("seal")
    else {
        panic!("the parked run's admission seals");
    };
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    let cancel = f
        .verb(RunVerb::Cancel)
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
                f.run.clone(),
                f.park.reason.clone(),
                4
            ))
            .await
            .map(lash_core::store::StoreTransition::into_record),
        Err(crate::StoreError::RunAlreadyTerminal { .. })
    ));
    // Nor can the stale execution commit: its fence is the one the cancel
    // raised the epoch past.
    let before = f
        .parts
        .store
        .load_session_head_meta(&f.parts.session_id)
        .await
        .expect("head")
        .map(|head| head.head_revision);
    let mut zombie = run_final_commit(&f.parts.initial_state(), &f.run, &f.run, 0);
    zombie.shift_fence = Some(Box::new(fence));
    assert!(matches!(
        f.parts.store.commit_runtime_state(zombie).await,
        Err(crate::StoreError::StaleShiftFence { .. })
    ));
    assert_eq!(
        f.parts
            .store
            .load_session_head_meta(&f.parts.session_id)
            .await
            .expect("head")
            .map(|head| head.head_revision),
        before,
        "no commit lands after the cancel"
    );
    assert_eq!(
        f.factory
            .run_terminal(&f.parts.session_id, &f.run)
            .await
            .expect("terminal")
            .expect("cancelled")
            .kind(),
        RunTerminalKind::Cancelled
    );
}

pub async fn cancel_fork_and_close_raise_the_shift_epoch_and_redrive_does_not(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for verb in [RunVerb::Redrive, RunVerb::Cancel, RunVerb::Fork] {
        let f = Fixture::new(prefix, &format!("epoch-{verb:?}"), &host, &stores).await;
        let before = f.parts.epoch().await.epoch;
        f.verb(verb).await.expect("verb");
        assert_eq!(
            f.parts.epoch().await.epoch,
            before + u64::from(verb != RunVerb::Redrive)
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

pub async fn an_exhausted_run_parks_engine_retry_exhausted_via_reconcile_idempotently_with_no_evidence(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "engine-exhaustion", &host, &stores, 8).await;
    let run = TurnId::from("exhausted");
    parts.enqueue("input", Some(run.as_str())).await;
    let factory = stores.session_store_factory();
    let writer =
        lash_core::shift::StoreParkRecovery::new(factory.as_ref(), parts.host.clock.as_ref());
    let target = ParkTarget::Run {
        session: parts.session_id.clone(),
        run: run.clone(),
    };
    let reason = ParkReason::engine_retry_exhausted(
        8,
        Some("500".into()),
        "engine retries exhausted".into(),
    );
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
            .run_terminal(&parts.session_id, &run)
            .await
            .expect("terminal")
            .is_none()
    );
}

pub async fn a_parked_runs_fence_stays_current_until_a_verb(
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
            f.run.clone(),
            f.park.reason.clone(),
            4,
        ))
        .await
        .map(lash_core::store::StoreTransition::into_record)
        .expect("repark");
    assert_eq!(f.parts.epoch().await, before);
    f.verb(RunVerb::Cancel).await.expect("cancel");
    assert!(f.parts.epoch().await.epoch > before.epoch);
}

pub async fn a_diverged_run_parks_once_holds_its_admitted_rows_blocks_admission_and_completes_after_restore(
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
        crate::PendingTurnInputReadStatus::Admitted { run: ref holder } if *holder == f.run
    ));
    let again = f
        .parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.run.clone(),
            f.park.reason.clone(),
            4,
        ))
        .await
        .map(lash_core::store::StoreTransition::into_record)
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
    let blocked = shift(&f, &runner, "before-restore").await;
    assert!(matches!(blocked.stop, ShiftStop::Parked(_)));
    assert_eq!(f.parts.calls(), 0);
    let intent = f
        .verb(RunVerb::Redrive)
        .await
        .expect("restored build redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &intent).await;
    let resumed = shift(&f, &runner, "after-restore").await;
    assert_eq!(resumed.stop, ShiftStop::Idle);
    assert_eq!(f.parts.calls(), 1);
    assert_eq!(
        f.parts.applications().await,
        vec![(f.input.clone(), f.run.clone())]
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

/// B1: a run that parks on a later physical turn — a frame switch's
/// follow-on turn — is still one run. The commit that ends it clears its
/// park whichever physical turn committed, so the run is never parked and
/// terminal at once: the verbs answer `NotParked` and nothing is left for a
/// drain to count.
pub async fn a_run_parked_on_a_later_physical_turn_is_cleared_by_its_commit(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "later-physical-park", &host, &stores).await;
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    f.apply(&work, &close, &redrive).await;
    let turn = PhysicalTurn::derive_turn_id(&f.run, 1);
    f.parts
        .store
        .commit_runtime_state(run_final_commit(&f.parts.initial_state(), &f.run, &turn, 1))
        .await
        .expect("the run's final commit on its second physical turn");
    assert!(
        f.factory
            .run_terminal(&f.parts.session_id, &f.run)
            .await
            .expect("terminal")
            .is_some()
    );
    assert_eq!(
        f.park().await,
        None,
        "the run's commit clears its park whichever physical turn committed"
    );
    assert!(matches!(
        f.verb(RunVerb::Cancel).await,
        Err(RunIntentRefused::NotParked)
    ));
}

/// H2: a redrive whose resume reached the engine but whose acknowledgement
/// was lost is settled by the run running past it. When the run parks
/// again, and when an operator then cancels it, reconciliation never resumes
/// the run a second time: a store-terminal run is never resumed.
pub async fn a_redrive_the_run_ran_past_is_never_applied_again(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "lapsed-redrive", &host, &stores).await;
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    work.0.lose_resume_reply.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Pending
    ));
    // The resumed run executes, refuses again under the same build, re-parks.
    let again = f
        .parts
        .store
        .record_turn_park(&TurnParkWrite::refusal(
            f.parts.session_id.clone(),
            f.run.clone(),
            f.park.reason.clone(),
            3,
        ))
        .await
        .map(lash_core::store::StoreTransition::into_record)
        .expect("repark");
    assert_eq!(again.resume_intent, None);
    let cancel = f
        .verb(RunVerb::Cancel)
        .await
        .expect("cancel the re-parked run");
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
        "a redrive the run ran past never resumes it again"
    );
    assert!(!f.owed(redrive.id).await);
    assert!(matches!(
        f.intent_state(cancel.id).await,
        ControlIntentState::Acknowledged { .. }
    ));
}

/// M3: a paused-execution listing read before a redrive resumed the run is
/// stale. Recording the engine's park from it must not re-park the running
/// run: the park keeps naming the redrive, so a cancel stays refused while
/// the run executes.
pub async fn a_stale_paused_listing_never_reparks_a_resumed_run(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "stale-listing", &host, &stores).await;
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    let writer =
        lash_core::shift::StoreParkRecovery::new(f.factory.as_ref(), f.parts.host.clock.as_ref());
    let target = ParkTarget::Run {
        session: f.parts.session_id.clone(),
        run: f.run.clone(),
    };
    let exhausted = ParkReason::engine_retry_exhausted(8, None, "listed before the resume".into());
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
        "a stale listing never re-parks the running run"
    );
    assert_eq!(park.attempts, f.park.attempts);
    assert!(matches!(
        f.verb(RunVerb::Cancel).await,
        Err(RunIntentRefused::Redriving { .. })
    ));
    // An execution the engine finds still stopped after the redrive
    // resumed it stopped again: that re-parks the run, clearing the
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
    f.verb(RunVerb::Cancel)
        .await
        .expect("the re-parked run can be cancelled");
}

/// Reconcile never scans the catalog for unexecuted input (ADR 0109 §3): an
/// accepted input's shift is asked for through its ingress obligation —
/// here while its session is parked, where the shift it asks for stops at
/// the park. The engine accepting the ask does not deliver the obligation
/// (D19): its claim holds while the shift runs, so neither the producer nor
/// the tick asks again. The verb that resolves the park works the session
/// on through its own follow-on shift.
pub async fn a_parked_session_is_asked_to_work_only_through_its_ingress_obligation(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "parked-shift-ask", &host, &stores).await;
    let behind = f.parts.enqueue("behind", Some("behind-run")).await;
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
    let relay = lash_core::runtime::shift::IngressRelay::new(
        Arc::clone(&ledger),
        Arc::clone(&work) as Arc<dyn crate::SessionWorkEngine>,
        Arc::clone(&f.parts.host.clock),
    );
    let id = lash_core::store::ObligationKey::Ingress {
        session_id: f.parts.session_id.clone(),
        item_id: (behind.as_str()).to_string(),
    }
    .id();
    assert_eq!(
        ledger.state(&id).await.expect("armed state"),
        Some(ObligationState::Due),
        "the input's acceptance armed its obligation"
    );
    relay
        .deliver_admitted(&f.parts.session_id, behind.as_str())
        .await;
    assert_eq!(scheduled(&work), 1, "the obligation asks for the shift");
    assert_eq!(
        ledger.state(&id).await.expect("requested state"),
        Some(ObligationState::Claimed),
        "an accepted ask is not a delivery: the shift's admission settles it"
    );
    relay
        .deliver_admitted(&f.parts.session_id, behind.as_str())
        .await;
    let cancel = f.verb(RunVerb::Cancel).await.expect("cancel");
    // The engine half ran and was acknowledged under the verb's claim; the
    // shift ask after it was lost with the process, so the claim lapses
    // unsettled.
    let claim = f
        .intents
        .claim(
            cancel.obligation_id().expect("armed"),
            &crate::store::ClaimToken::mint(),
            3,
            0,
        )
        .await
        .expect("claim")
        .expect("due");
    assert!(matches!(
        f.factory
            .acknowledge_intent(cancel.id, &claim.token, 3)
            .await
            .expect("acknowledge"),
        IntentSettle::Held(settled)
            if matches!(settled.state, ControlIntentState::Acknowledged { .. })
    ));
    let resolved = f.reconcile(&work, &close).await;
    assert!(resolved.failures.is_empty(), "{:?}", resolved.failures);
    // The follow-on shift is the intent obligation's to deliver: the relay
    // retakes the lapsed claim and asks for it. Nothing asks for the input
    // again: its own ask is still claimed, and the tick scans no session.
    assert_eq!(Fixture::intent_pass(&resolved).delivered, 1);
    assert_eq!(
        scheduled(&work),
        2,
        "the resolved park's follow-on shift is the only other ask"
    );
}

/// D15: while a parked run's park names a redrive intent that is not yet
/// settled, admission of a new turn input answers a typed retryable
/// refusal — never a recorded verdict, never a failed turn. The parked run
/// owns the session head (FIG-4202), so a session command queued beside the
/// send waits too: the command lane never drains ahead of an unfinished
/// run. Once the intent settles the parked run executes once, the command
/// applies at its boundary, and the send lands behind both.
pub async fn a_send_racing_an_unsettled_redrive_is_refused_until_the_redrive_settles(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut f = Fixture::new(prefix, "send-redrive-race", &host, &stores).await;
    let send = f.parts.enqueue("racing send", Some("racing-run")).await;
    let command = f
        .parts
        .store
        .enqueue_queued_work(crate::QueuedWorkBatchDraft::new(
            f.parts.session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::SessionCommand::RefreshToolCatalog {
                reason: "queued while the redrive is unsettled".into(),
            },
        ))
        .await
        .expect("command accepted");
    let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
    assert!(f.owed(intent.id).await);
    // The send's shift meets the unsettled redrive. In process its typed
    // refusal answers at once; on Restate the attempt dies inside its
    // recorded admission step and the open invocation retries until the
    // intent settles. The probe holds that retry's re-decision at the park
    // read, so the law's settle always lands inside the attempt budget.
    let script = Script::new();
    let held = hold_the_second_park_probe(&mut f, &script);
    let mut racing = spawn_shift(&f, &runner, "racing");
    // The racing shift evaluated the parked run.
    script.called(StoreOp::load_turn_park, 1).await;
    assert_eq!(
        f.parts.calls(),
        0,
        "the parked run is not run ahead of its resume"
    );
    assert!(
        f.parts.applications().await.is_empty(),
        "nothing is interleaved with the unsettled redrive"
    );
    // A second probe is a re-decision under the unsettled intent — the
    // refusal's own retry on Restate. Admission that instead ran the held
    // input reaches a second read only behind a committed run, so a
    // re-decision with nothing run is evidence the gate held.
    let redecided = tokio::time::timeout(std::time::Duration::from_millis(400), held.reached(1))
        .await
        .is_ok();
    if redecided {
        assert_eq!(
            f.parts.calls(),
            0,
            "a re-decision ran the run ahead of its redrive"
        );
        assert!(
            f.parts.applications().await.is_empty(),
            "a re-decision interleaved a run with the unsettled redrive"
        );
    }
    assert!(
        f.command_is_open(&command.batch_id).await,
        "the command lane does not drain ahead of the unsettled redrive's run"
    );
    let answered = if racing.is_finished() {
        Some((&mut racing).await.expect("the racing shift ran"))
    } else {
        None
    };
    match &answered {
        Some(Err(ShiftAbort::Retry(refusal))) => {
            assert_eq!(
                refusal.code,
                crate::RuntimeErrorCode::SessionRedriveUnsettled,
                "{refusal:?}"
            );
            assert!(refusal.is_retryable(), "{refusal:?}");
        }
        // A shift that lands while its redrive is unsettled is the bug
        // this law rules out (D15): admitted ahead of the redrive.
        Some(Ok(landed)) => assert!(
            !f.owed(intent.id).await,
            "the send landed while the redrive was unsettled: {landed:?}"
        ),
        Some(Err(abort)) => {
            panic!("the refusal is retryable, never a failed turn: {abort:?}")
        }
        None => {}
    }
    // The engine half answers: the parked execution is resumed under
    // its sealed fence (or, holding nothing, a shift is asked to
    // re-run the run). The intent settles; admission proceeds — on
    // Restate the held retry admits what was refused.
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Acknowledged { .. }
    ));
    held.open_all();
    let raced = match answered {
        Some(raced) => raced,
        None => racing.await.expect("the racing shift ran"),
    };
    match raced {
        Ok(settled) => assert_eq!(settled.stop, ShiftStop::Idle),
        Err(ShiftAbort::Retry(_)) => {
            let settled = shift(&f, &runner, "after-settle").await;
            assert_eq!(settled.stop, ShiftStop::Idle);
        }
        Err(abort) => panic!("the settled shift ended on a refusal: {abort:?}"),
    }
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), f.run.clone()),
            (send, TurnId::from("racing-run"))
        ],
        "the held input executes the run once, then the send lands"
    );
    assert_eq!(
        f.parts.calls(),
        2,
        "the parked run executes once and the send's run once"
    );
    assert!(
        !f.command_is_open(&command.batch_id).await,
        "the command applied at the parked run's boundary"
    );
    assert!(
        f.park().await.is_none(),
        "the run's commit cleared its park"
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
        .enqueue("queued send", Some("queued-send-run"))
        .await;
    let intent = f.verb(RunVerb::Redrive).await.expect("redrive");
    // The engine resumed the run but its reply was lost: the intent stays
    // open, retryable.
    let (work, close) = f.control(false, false);
    work.0.lose_resume_reply.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.apply(&work, &close, &intent).await,
        ControlIntentState::Pending
    ));
    assert!(f.owed(intent.id).await);
    // The queued send's shift meets the unsettled redrive: its refusal is
    // the typed retryable one in process, and an invocation the server
    // keeps retrying on Restate. The probe holds that retry's re-decision
    // at the park read until the law's reconcile lands.
    let script = Script::new();
    let held = hold_the_second_park_probe(&mut f, &script);
    let mut racing = spawn_shift(&f, &runner, "racing");
    // The racing shift evaluated the parked run.
    script.called(StoreOp::load_turn_park, 1).await;
    assert_eq!(f.parts.calls(), 0);
    assert!(f.parts.applications().await.is_empty());
    let redecided = tokio::time::timeout(std::time::Duration::from_millis(400), held.reached(1))
        .await
        .is_ok();
    if redecided {
        assert_eq!(
            f.parts.calls(),
            0,
            "a re-decision ran the run ahead of its redrive"
        );
        assert!(
            f.parts.applications().await.is_empty(),
            "a re-decision interleaved a run with the unsettled redrive"
        );
    }
    let answered = if racing.is_finished() {
        Some((&mut racing).await.expect("the racing shift ran"))
    } else {
        None
    };
    match &answered {
        Some(Err(ShiftAbort::Retry(refusal))) => assert_eq!(
            refusal.code,
            crate::RuntimeErrorCode::SessionRedriveUnsettled,
            "the queued send is refused retryably while the redrive is \
             unsettled: {refusal:?}"
        ),
        Some(Ok(landed)) => assert!(
            !f.owed(intent.id).await,
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
    held.open_all();
    // After the tick the queued send is admitted: the held input executes
    // the run once and the send lands behind it. On Restate the racing
    // invocation's retry admits it; in process the shift answers on the
    // next request.
    let raced = match answered {
        Some(raced) => raced,
        None => racing.await.expect("the racing shift ran"),
    };
    match raced {
        Ok(admitted) => assert_eq!(admitted.stop, ShiftStop::Idle),
        Err(ShiftAbort::Retry(_)) => {
            let admitted = shift(&f, &runner, "after-tick").await;
            assert_eq!(admitted.stop, ShiftStop::Idle);
        }
        Err(abort) => panic!("the settled shift ended on a refusal: {abort:?}"),
    }
    assert_eq!(
        f.parts.applications().await,
        vec![
            (f.input.clone(), f.run.clone()),
            (send, TurnId::from("queued-send-run"))
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
/// execution is released. The run's scope close — its children's cancel —
/// is not the intent's to finish: a child whose cancel keeps failing leaves
/// the intent acknowledged, its obligation delivered and its session
/// admitting, and the run scope's own recovery owner closes the scope.
pub async fn a_failing_child_cancel_never_wedges_its_runs_cancel_or_fork(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for verb in [RunVerb::Cancel, RunVerb::Fork] {
        let f = Fixture::new(prefix, &format!("child-cancel-{verb:?}"), &host, &stores).await;
        let next = f.parts.enqueue("behind", Some("behind-run")).await;
        let intent = f.verb(verb).await.expect("verb");
        // The run's scope close fails: a child's cancel did not go through.
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
        // The ended run's scope is its recovery owner's to close.
        let report = f.reconcile(&work, &close).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(work.0.events.lock().expect("events").contains(&"close"));
        if verb == RunVerb::Cancel {
            let outcome = shift(&f, &runner, "after-child-cancel").await;
            assert_eq!(outcome.stop, ShiftStop::Idle);
            assert_eq!(
                f.parts.applications().await,
                vec![(next, TurnId::from("behind-run"))]
            );
        }
    }
}

/// S8-C (ADR 0109 §1.3): every write that settles an intent's engine half
/// compares its obligation's claim. A delivery whose claim lapsed and was
/// retaken by another relay neither acknowledges nor fails the intent; the
/// live claim does.
///
/// FIG-4186: the relay hands each delivery the claim that owns its attempt,
/// never one it looks up by obligation id. One relay's due page holds A then
/// B; A's release waits while both claims lapse and the same relay's due
/// index retakes them under fresh tokens. The page's stale B then runs
/// before the fresh one: it writes under its own lapsed token, so it settles
/// nothing, and each fresh claim's delivery settles its intent. A row claimed straight from the ledger
/// delivers through the relay too.
pub async fn a_delivery_whose_claim_was_retaken_never_settles_its_intent(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "retaken-claim", &host, &stores).await;
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
    let id = intent
        .obligation_id()
        .cloned()
        .expect("the verb armed its obligation");
    // A relay claims it with a claim that lapses at once...
    let stale = f
        .intents
        .claim(&id, &crate::store::ClaimToken::mint(), 3, 0)
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
            .refuse_intent(
                intent.id,
                &stale.token,
                &crate::store::DeliveryError::new(
                    crate::RuntimeErrorCode::EngineHandleMismatch,
                    "late refusal",
                ),
                5,
            )
            .await
            .expect("stale refusal"),
        IntentSettle::ClaimLost
    );
    assert_eq!(f.intent_state(intent.id).await, ControlIntentState::Pending);
    assert!(f.parts.epoch().await.control_pending);
    assert!(matches!(
        f.factory
            .acknowledge_intent(intent.id, &fresh.token, 6)
            .await
            .expect("live acknowledgement"),
        IntentSettle::Held(settled)
            if settled.state == ControlIntentState::Acknowledged { at_ms: 6 }
    ));
    assert!(!f.parts.epoch().await.control_pending);
    assert_eq!(
        f.intents
            .settle(&id, &fresh.token, ObligationSettlement::Delivered, 6)
            .await
            .expect("the live claim settles"),
        SettleOutcome::Applied
    );

    // One relay, one page: A is due before B.
    let a = Fixture::new(prefix, "retaken-page-a", &host, &stores).await;
    let b = Fixture::new(prefix, "retaken-page-b", &host, &stores).await;
    let intent_a = a.verb_at(RunVerb::Cancel, 10).await.expect("cancel A");
    let intent_b = b.verb_at(RunVerb::Cancel, 11).await.expect("cancel B");
    let (work, close) = a.control(false, false);
    let gate_a = Arc::new(Gate::new("the release of A"));
    let gate_b = Arc::new(Gate::new("the release of B"));
    work.0.release_gates.lock().expect("release gates").extend([
        (a.parts.session_id.clone(), Arc::clone(&gate_a)),
        (b.parts.session_id.clone(), Arc::clone(&gate_b)),
    ]);
    let clock = Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(20)));
    let relay = Arc::new(a.relay(&work, &close, Arc::clone(&clock) as Arc<dyn crate::Clock>));
    let stale = {
        let relay = Arc::clone(&relay);
        let clock = Arc::clone(&clock);
        tokio::spawn(async move {
            lash_core::runtime::shift::relay::relay_due(
                relay.as_ref(),
                clock.as_ref(),
                NonZeroUsize::MIN.saturating_add(63),
            )
            .await
            .expect("a due page over the law's stores")
        })
    };
    gate_a.reached(1).await;
    // Both claims lapse while A's release waits, and the same relay's due
    // index retakes both under fresh tokens.
    let ttl = lash_core::runtime::shift::relay::RelayPolicy::default().claim_ttl_ms;
    clock.0.store(20 + ttl + 1, Ordering::SeqCst);
    let mut fresh = lash_core::runtime::shift::relay::ObligationRelay::ledger(relay.as_ref())
        .claim_due(20 + ttl + 1, ttl, NonZeroUsize::MIN.saturating_add(63))
        .await
        .expect("due claim");
    fresh.sort_by_key(|claimed| claimed.id != *intent_a.obligation_id().expect("armed"));
    let [fresh_a, fresh_b]: [ClaimedObligation; 2] =
        fresh.try_into().expect("both lapsed claims are retaken");
    assert_eq!((fresh_a.attempts, fresh_b.attempts), (2, 2));
    // The stale page runs on: its A, then its B, each under its own claim.
    gate_a.open_one();
    gate_b.open_one();
    assert_eq!(
        stale.await.expect("the stale page"),
        RelayPass {
            claimed: 2,
            claim_lost: 2,
            ..RelayPass::default()
        },
        "each stale delivery wrote under its own lapsed claim"
    );
    assert_eq!(
        b.intent_state(intent_b.id).await,
        ControlIntentState::Pending,
        "the stale B delivery never settles the intent its fresh claim holds"
    );
    assert!(b.parts.epoch().await.control_pending);
    assert_eq!(
        a.intent_state(intent_a.id).await,
        ControlIntentState::Pending
    );
    // Each fresh claim's own delivery settles its intent.
    gate_a.open_one();
    gate_b.open_one();
    for (fixture, intent, fresh) in [(&a, &intent_a, fresh_a), (&b, &intent_b, fresh_b)] {
        assert_eq!(
            lash_core::runtime::shift::relay::deliver_claimed(
                relay.as_ref(),
                fresh,
                clock.as_ref()
            )
            .await
            .expect("the fresh delivery"),
            lash_core::runtime::shift::relay::RelayVerdict::Delivered
        );
        assert!(matches!(
            fixture.intent_state(intent.id).await,
            ControlIntentState::Acknowledged { .. }
        ));
        assert_eq!(
            fixture.obligation(intent).await,
            Some(ObligationState::Delivered)
        );
        assert!(!fixture.parts.epoch().await.control_pending);
    }

    // A row claimed straight from the ledger delivers through the relay: the
    // delivery carries the claim, whoever took it.
    let c = Fixture::new(prefix, "ledger-claimed", &host, &stores).await;
    let intent_c = c.verb_at(RunVerb::Cancel, 12).await.expect("cancel C");
    let claimed = c
        .intents
        .claim(
            intent_c.obligation_id().expect("armed"),
            &crate::store::ClaimToken::mint(),
            lash_core::ClockWallTime::timestamp_ms(clock.as_ref()),
            60_000,
        )
        .await
        .expect("claim")
        .expect("due");
    assert_eq!(
        lash_core::runtime::shift::relay::deliver_claimed(relay.as_ref(), claimed, clock.as_ref())
            .await
            .expect("delivery"),
        lash_core::runtime::shift::relay::RelayVerdict::Delivered
    );
    assert!(matches!(
        c.intent_state(intent_c.id).await,
        ControlIntentState::Acknowledged { .. }
    ));
}

/// S8-C (ADR 0109 §1.4, §3): an intent whose engine half keeps failing is
/// retried after a capped exponential backoff, never before, and at the
/// kind's attempt ceiling its obligation stalls under the engine's code. The
/// intent stays pending and owes nothing more — surfaced, never a wedge: its
/// session shifts the next run. Re-arming the stalled obligation makes the
/// intent owed again, and its next delivery completes it.
pub async fn an_intent_whose_engine_half_keeps_failing_stalls_at_its_ceiling_and_unwedges_its_session(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "intent-ceiling", &host, &stores).await;
    let next = f.parts.enqueue("behind", Some("behind-run")).await;
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
    let id = intent.obligation_id().cloned().expect("armed");
    let (work, close) = f.control(false, false);
    work.0.fail_releases.store(usize::MAX, Ordering::SeqCst);
    let clock = Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(
        f.parts.host.clock.timestamp_ms(),
    )));
    let policy = lash_core::runtime::shift::relay::RelayPolicy {
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
        ControlIntentState::Pending
    ));
    assert!(f.parts.epoch().await.control_pending);
    for attempt in 2..=3_u32 {
        // Never before its backoff...
        let early = lash_core::runtime::shift::relay::relay_due(&relay, clock.as_ref(), page)
            .await
            .expect("early pass");
        assert_eq!(early.claimed, 0, "attempt {attempt} waits for its backoff");
        clock
            .0
            .fetch_add(policy.backoff_ms(attempt - 1), Ordering::SeqCst);
        // ...and taken once it elapsed.
        let pass = lash_core::runtime::shift::relay::relay_due(&relay, clock.as_ref(), page)
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
    assert_eq!(
        stalled.last_error.map(|error| error.code),
        Some(crate::RuntimeErrorCode::EngineControlRequest),
        "the stall row keeps the engine's code"
    );
    // The attempts running out is the obligation's fact alone: the intent
    // stays pending and owes nothing while its obligation is stalled.
    assert_eq!(f.intent_state(intent.id).await, ControlIntentState::Pending);
    assert!(!f.owed(intent.id).await);
    // Stalled, not a wedge: the session shifts its next run.
    assert!(!f.parts.epoch().await.control_pending);
    assert!(work.0.events.lock().expect("events").contains(&"schedule"));
    let outcome = shift(&f, &runner, "after-ceiling").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(
        f.parts.applications().await,
        vec![(next, TurnId::from("behind-run"))]
    );
    // Nothing retries a stalled obligation...
    clock.0.fetch_add(policy.max_backoff_ms, Ordering::SeqCst);
    let idle = lash_core::runtime::shift::relay::relay_due(&relay, clock.as_ref(), page)
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
    assert!(f.owed(intent.id).await, "a re-armed intent is owed again");
    let pass = lash_core::runtime::shift::relay::relay_due(&relay, clock.as_ref(), page)
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

/// S8-C (ADR 0109 §3): a cancel's follow-on shift is part of its delivery,
/// not a fire-and-forget ask. An ask the engine did not accept leaves the
/// intent acknowledged and its obligation due; the relay's next pass asks
/// again and only then delivers it.
pub async fn a_refused_follow_on_shift_keeps_the_intents_obligation_due(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "refused-follow-on", &host, &stores).await;
    let intent = f.verb(RunVerb::Cancel).await.expect("cancel");
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
        ["release", "close", "shift-refused"]
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
        ["release", "close", "shift-refused", "schedule"],
        "the redelivery asks for the shift again"
    );
    assert_eq!(
        events.iter().filter(|event| **event == "release").count(),
        1,
        "the redelivery releases nothing twice"
    );
}
