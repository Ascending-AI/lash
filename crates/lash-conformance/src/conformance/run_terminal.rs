//! A logical run's terminal evidence through the session shift (FIG-3600
//! S7, FIG-3607 contract 2 and item 7): the laws reach an engine only
//! through the kernel's shift entries, on the controller the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) admits.
//!
//! - L-T3: a committed run answers its terminal by its run, and its
//!   accepted input names the run that took it.
//! - L-R3 / L-T4: admission answers a run that already has evidence from
//!   that evidence and runs nothing.
//! - The successor-sealed refusal (Q-B3): a run whose admission a successor
//!   sealed over commits nothing, and a later shift of it commits once.
//! - L-C1: the run's scope closes after its evidence is durable, at least
//!   once across a crash between the two — inside the close, or at the
//!   report handover before it (FIG-3979) — and never for a parked run.

use crate::ActorContext;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::engine::{RunOutcome, ScopeCloseSink, ShiftAbort, ShiftOutcome, ShiftStop};
use lash_core::store::{
    AdmissionId, ControlIntentId, RunStartNonce, RunTerminal, RunTerminalCause, RunTerminalKind,
    RunTerminalWrite, ShiftEpochSeal, TurnCommitId,
};
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use super::shift_admission::{ShiftParts, admitted, driver_scope, on_tier};

/// A scope owner that records every run close with whether the run's
/// evidence was durable when it was called, and refuses the first close
/// when asked to.
struct RecordingScopeClose {
    store: Arc<dyn crate::RuntimeStore>,
    fail_next: AtomicBool,
    closes: Mutex<Vec<(TurnId, bool)>>,
}

impl RecordingScopeClose {
    fn new(store: Arc<dyn crate::RuntimeStore>, fail_first: bool) -> Arc<Self> {
        Arc::new(Self {
            store,
            fail_next: AtomicBool::new(fail_first),
            closes: Mutex::new(Vec::new()),
        })
    }

    fn closes(&self) -> Vec<(TurnId, bool)> {
        self.closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl ScopeCloseSink for RecordingScopeClose {
    async fn close_run_scope(&self, terminal: &RunTerminal) -> Result<(), crate::StoreError> {
        let durable = self
            .store
            .run_terminal(&terminal.session_id, &terminal.run)
            .await?
            .is_some_and(|stored| stored.same_terminal(terminal));
        self.closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((terminal.run.clone(), durable));
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(crate::StoreError::Backend(
                "the law's scope owner refuses its first close".to_string(),
            ));
        }
        Ok(())
    }

    async fn close_session_scope(
        &self,
        _session: &SessionId,
        _intent: ControlIntentId,
        _runs: &[TurnId],
    ) -> Result<(), crate::StoreError> {
        Ok(())
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own read"
)]
async fn terminal(parts: &ShiftParts, run: &TurnId) -> Option<RunTerminal> {
    parts
        .store
        .run_terminal(&parts.session_id, run)
        .await
        .expect("read the run's terminal evidence")
}

async fn shift(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ShiftParts,
    id: &str,
) -> ShiftOutcome {
    let request = parts.request(id);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            #[expect(
                clippy::expect_used,
                reason = "conformance-law fixture: the shift runs to a stop"
            )]
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await
}

/// L-T3 (the shift half): the run's final commit writes its terminal
/// evidence, addressed by the run, and the accepted input it drove names
/// that run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_run_answers_its_terminal_by_run(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "run-answered", &effect_host, &stores, 8).await;
    let input = parts.enqueue("ask", Some("run-answered")).await;
    let run = TurnId::from("run-answered");
    assert_eq!(terminal(&parts, &run).await, None, "no evidence before");
    assert_eq!(
        parts
            .store
            .run_of_input(&parts.session_id, &input)
            .await
            .expect("read the input's run"),
        None,
        "a pending input has no run"
    );
    let outcome = shift(&runner, &parts, "run-answered-shift").await;
    let committed = match &outcome.ran[..] {
        [RunOutcome::Committed { run: ran, kind, .. }] if *ran == run => *kind,
        _ => panic!("the run commits: {outcome:?}"),
    };
    let evidence = terminal(&parts, &run)
        .await
        .expect("the run's final commit wrote its evidence");
    assert_eq!(evidence.kind(), RunTerminalKind::Answered);
    assert_eq!(committed, evidence.kind());
    // The evidence carries the outcome the run committed, so a follower
    // answers from this row alone (FIG-4345).
    assert_eq!(
        evidence.cause,
        RunTerminalCause::Committed {
            commit: TurnCommitId::new(run.clone(), 0),
            turn: run.clone(),
            outcome: crate::store::RunCommittedOutcome::Finished(
                crate::TurnFinish::AssistantMessage {
                    text: "answer 1".into()
                }
            ),
        }
    );
    assert!(evidence.head_revision.is_some(), "a head commit wrote it");
    assert_eq!(
        parts
            .store
            .run_of_input(&parts.session_id, &input)
            .await
            .expect("read the input's run"),
        Some(run),
        "the accepted input names the run that took it"
    );
}

/// L-R3 and L-T4: admission answers a head input whose run already has
/// terminal evidence from that evidence. Nothing is sealed and nothing runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_id_naming_a_terminal_run_is_answered_not_rerun(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "run-adopted", &effect_host, &stores, 8).await;
    let run = TurnId::from("run-adopted");
    // An earlier epoch committed the run: its evidence stands.
    let state = parts.initial_state();
    let operation = crate::OperationId::turn(parts.session_id.clone(), run.clone(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive commit node ids");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state, graph, operation,
    )
    .expect("build the earlier epoch's commit");
    commit.run_terminal = Some(Box::new(RunTerminalWrite {
        run: run.clone(),
        commit: TurnCommitId::new(run.clone(), 0),
        turn: run.clone(),
        outcome: crate::store::RunCommittedOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }));
    // A final commit carries its run's admitted cancellation snapshot
    // (FIG-4848): the earlier epoch admitted the run, over an input of its
    // own, before it committed.
    parts.enqueue("the earlier epoch's input", None).await;
    let commit =
        crate::conformance::admission_support::prepare_final_commit(&parts.store, commit).await;
    parts
        .store
        .commit_runtime_state(commit)
        .await
        .expect("the earlier epoch commits the run");
    assert!(
        terminal(&parts, &run).await.is_some(),
        "the run is terminal"
    );
    let earlier_epoch = parts.epoch().await.epoch;

    parts
        .enqueue("names the ended run", Some("run-adopted"))
        .await;
    let outcome = shift(&runner, &parts, "run-adopted-shift").await;
    assert!(outcome.ran.is_empty(), "{outcome:?}");
    assert_eq!(
        outcome.stop,
        ShiftStop::RunTerminal {
            run,
            kind: RunTerminalKind::Answered,
            commit: Some(TurnCommitId::new(TurnId::from("run-adopted"), 0)),
        }
    );
    assert_eq!(
        parts.epoch().await.epoch,
        earlier_epoch,
        "nothing was sealed"
    );
    assert_eq!(parts.calls(), 0, "nothing ran");
}

/// The successor-sealed commit refusal (ADR 0105 §2, Q-B3): a successor
/// seals the session while the admitted run executes, so the run's commit
/// carries a stale fence. It commits nothing, writes no evidence and closes
/// no scope; a later shift of the same run commits it once.
///
/// The refusal is the run's first commit, which no replay repeats, and it is
/// permanent: the fence can never commit again. The run ends `Refused` with
/// the typed, non-retryable `StoreCommitSuperseded` in the attempt that met
/// it, after one model call, and never asks its engine for a retry
/// (FIG-4512).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_whose_admission_a_successor_sealed_commits_nothing(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-superseded", &effect_host, &stores, 8).await;
    let sealed_over = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let store = Arc::clone(&parts.store);
            let session_id = parts.session_id.clone();
            let sealed_over = Arc::clone(&sealed_over);
            let calls = Arc::clone(&calls);
            move |_request| {
                let store = Arc::clone(&store);
                let session_id = session_id.clone();
                let sealed_over = Arc::clone(&sealed_over);
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if !sealed_over.swap(true, Ordering::SeqCst) {
                        let seal = store
                            .seal_shift_epoch(
                                &session_id,
                                &AdmissionId::new("successor#0"),
                                1,
                                &RunStartNonce::new("successor"),
                                None,
                            )
                            .await
                            .expect("the successor seals");
                        assert!(matches!(seal, ShiftEpochSeal::Sealed(_)), "{seal:?}");
                    }
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: "answer".to_string(),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.models = crate::testing::standard_test_llm_profiles(model.into_handle());
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), false);
    parts.host.control.scope_close = closes.clone();
    let input = parts.enqueue("ask", Some("run-superseded")).await;
    let run = TurnId::from("run-superseded");
    let request = parts.request("run-superseded-shift");
    let refused = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let admitted = admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0)
                    .await
                    .expect("admit the run"),
            );
            lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                .await
                .map(|_| ())
        })
    })
    .await;
    assert!(sealed_over.load(Ordering::SeqCst), "the successor sealed");
    match refused {
        Err(ShiftAbort::Refused(refusal)) => {
            assert_eq!(
                refusal.code,
                crate::RuntimeErrorCode::StoreCommitSuperseded,
                "{refusal:?}"
            );
            assert!(!refusal.is_retryable(), "{refusal:?}");
            assert!(refusal.message.contains("shift fence"), "{refusal:?}");
        }
        other => panic!("the stale commit ends the run refused, with no retry: {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the refused run called the model once"
    );
    assert_eq!(parts.epoch().await.epoch, 2, "the successor's seal stands");
    assert_eq!(terminal(&parts, &run).await, None, "no evidence");
    assert!(parts.applications().await.is_empty(), "nothing committed");
    assert!(closes.closes().is_empty(), "no scope closed");

    let outcome = shift(&runner, &parts, "run-superseded-after").await;
    assert!(
        outcome
            .ran
            .iter()
            .any(|ran| matches!(ran, RunOutcome::Committed { run: ran, .. } if *ran == run)),
        "{outcome:?}"
    );
    assert!(
        terminal(&parts, &run).await.is_some(),
        "the later shift commits"
    );
    assert_eq!(parts.applications().await, vec![(input, run.clone())]);
    assert_eq!(closes.closes(), vec![(run, true)]);
}

/// A process registry's scope owner that crashes the execution on its first
/// run close: it fires `crash` and never answers, so the close writes
/// nothing, the way a process that dies between a run's terminal commit and
/// its scope close leaves it. Every later close reaches the registry. It
/// records each run it was asked to close, with whether the run's evidence
/// was durable by then.
struct CrashingRegistryScopeClose {
    registry: crate::RegistryScopeClose,
    store: Arc<dyn crate::RuntimeStore>,
    crash: crate::ConformanceCrash,
    armed: AtomicBool,
    closes: Mutex<Vec<(TurnId, bool)>>,
}

impl CrashingRegistryScopeClose {
    fn closes(&self) -> Vec<(TurnId, bool)> {
        self.closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl ScopeCloseSink for CrashingRegistryScopeClose {
    async fn close_run_scope(&self, terminal: &RunTerminal) -> Result<(), crate::StoreError> {
        let durable = self
            .store
            .run_terminal(&terminal.session_id, &terminal.run)
            .await?
            .is_some_and(|stored| stored.same_terminal(terminal));
        self.closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((terminal.run.clone(), durable));
        if self.armed.swap(false, Ordering::SeqCst) {
            self.crash.fire();
            std::future::pending::<()>().await;
        }
        self.registry.close_run_scope(terminal).await
    }

    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        runs: &[TurnId],
    ) -> Result<(), crate::StoreError> {
        self.registry
            .close_session_scope(session, intent, runs)
            .await
    }
}

/// One shift of the law's session for `request`, as the tier runs it.
fn work_once<'a>(
    mut runtime: crate::LashRuntime,
    scope: crate::ActorContext,
    request: lash_core::engine::ShiftRequest,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        lash_core::shift::work_session(&mut runtime, &scope, &request)
            .await
            .map(|_| ())
            .map_err(|abort| format!("{abort:?}"))
    })
}

/// A clock that reads `inner`'s wall time `offset_ms` ahead: a due pass run
/// under it sees a claim whose delivery crashed — held until its TTL — as
/// lapsed, the way the next tick on a real clock sees it (ADR 0109 §1.4).
#[derive(Debug)]
struct OffsetClock {
    inner: Arc<dyn lash_core::Clock>,
    offset_ms: u64,
}

#[async_trait::async_trait]
impl lash_core::Clock for OffsetClock {
    fn now(&self) -> std::time::Instant {
        self.inner.now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        self.inner.timestamp_datetime() + chrono::Duration::milliseconds(self.offset_ms as i64)
    }

    async fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration).await
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        self.inner.sleep_until(deadline).await
    }
}

/// The `ScopeClose` kind's ledger over the law's stores (ADR 0109 §3).
fn scope_close_ledger(
    stores: &Arc<dyn crate::StoreSet>,
) -> Arc<dyn lash_core::store::ObligationLedger> {
    stores.obligation_ledger(lash_core::store::ObligationKind::ScopeClose)
}

/// The kind's relay over the law's stores: reads terminal evidence from the
/// catalog, closes through `scopes`.
fn scope_close_relay(
    stores: &Arc<dyn crate::StoreSet>,
    scopes: Arc<dyn ScopeCloseSink>,
) -> lash_core::runtime::shift::ScopeCloseRelay {
    lash_core::runtime::shift::ScopeCloseRelay::new(
        scope_close_ledger(stores),
        stores.session_store_factory(),
        scopes,
    )
}

/// The state of `run`'s armed scope-close obligation, if the row carries one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the ledger answers its own read"
)]
async fn scope_close_state(
    stores: &Arc<dyn crate::StoreSet>,
    session_id: &SessionId,
    run: &TurnId,
) -> Option<lash_core::store::ObligationState> {
    scope_close_ledger(stores)
        .state(
            &lash_core::store::ObligationKey::ScopeClose {
                session_id: session_id.clone(),
                run: run.clone(),
            }
            .id(),
        )
        .await
        .expect("read the run's scope-close obligation")
}

/// One reconcile tick over the law's stores (ADR 0108 §5, ADR 0109 §3): the
/// scope-close relay's due pass is the engine-neutral owner of an armed but
/// undelivered close — no terminal-run scan remains. Returns the tick's
/// report; the obligations arm reports no failure.
async fn reconcile_tick(
    stores: &Arc<dyn crate::StoreSet>,
    relay: &lash_core::runtime::shift::ScopeCloseRelay,
    clock: Arc<dyn lash_core::Clock>,
) -> lash_core::engine::ReconcileTick {
    let factory = stores.session_store_factory();
    let work = crate::NoSessionWork::new();
    let scopes = crate::engine::NoScopeClose;
    let relays: Vec<Arc<dyn lash_core::runtime::shift::relay::ObligationRelay>> =
        vec![Arc::new(relay.clone())];
    let report = lash_core::runtime::shift::reconcile_once(
        &lash_core::runtime::shift::ReconcileParts {
            metrics: &Default::default(),
            sessions: factory.as_ref(),
            work: &work,
            scopes: &scopes,
            clock: clock.as_ref(),
            duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
            relays: &relays,
            lanes: &super::helpers::law_tick_lanes(Arc::clone(&clock)),
        },
        &lash_core::engine::ReconcileCursor::default(),
        std::num::NonZeroUsize::new(64).unwrap_or(std::num::NonZeroUsize::MIN),
    )
    .await;
    let obligation_failures: Vec<_> = report
        .failures
        .iter()
        .filter(|failure| failure.arm == lash_core::engine::ReconcileArm::Obligations)
        .map(|failure| failure.error.clone())
        .collect();
    assert!(
        obligation_failures.is_empty(),
        "the obligations arm delivered every due close: {obligation_failures:?}"
    );
    report
}

/// The `ScopeClose` pass one tick reported (claims, delivered, and the rest).
fn scope_close_pass(report: &lash_core::engine::ReconcileTick) -> lash_core::engine::RelayPass {
    report
        .obligations
        .iter()
        .find(|(kind, _)| *kind == lash_core::store::ObligationKind::ScopeClose)
        .map(|(_, pass)| *pass)
        .unwrap_or_default()
}

/// One recovery pass over the law's stores: the parent-end obligation
/// pass delivers the cancel each close row owes the processes living `Until`
/// the closed scope (ADR 0108 §5) — the same `relay_due` the deployment's
/// reconcile tick runs (ADR 0109).
async fn recovery_pass(stores: &Arc<dyn crate::StoreSet>) {
    let pass = crate::deliver_due_parent_end_obligations(stores).await;
    assert_eq!(
        pass.stalled, 0,
        "no parent-end obligation stalls in a healthy world: {pass:?}"
    );
}

/// A process living `Until` `run`'s turn scope, registered while the run
/// has not ended.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registry admits a start under a live run"
)]
async fn until_run(
    registry: &Arc<dyn crate::ProcessRegistry>,
    session_id: &SessionId,
    run: &TurnId,
) -> crate::ProcessRecord {
    registry
        .register_process(crate::started_until_starter(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                crate::ProcessProvenance::session(crate::SessionScope::new(session_id.clone())),
                lash_core::Lifetime::Detached,
            ),
            lash_core::ScopeId::turn(session_id.clone(), run.clone()),
        ))
        .await
        .expect("a start living until the running run is admitted")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registry answers its own reads"
)]
async fn run_close_row(
    registry: &Arc<dyn crate::ProcessRegistry>,
    session_id: &SessionId,
    run: &TurnId,
) -> Option<crate::ParentEndPlan> {
    registry
        .get_parent_end_plan(&lash_core::ScopeId::turn(session_id.clone(), run.clone()))
        .await
        .expect("read the run's close row")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registry answers its own reads"
)]
async fn owes_cancel(
    registry: &Arc<dyn crate::ProcessRegistry>,
    process: &crate::ProcessId,
) -> bool {
    registry
        .get_process(process)
        .await
        .expect("read the child")
        .expect("the child is recorded")
        .cancel_request
        .is_some()
}

/// L-C1 (FIG-3607 item 7, ADR 0109 §3): a run's scope closes after its
/// terminal evidence is durable, at least once, and never for a run that
/// is parked.
///
/// - **A crash between the commit and the close.** The execution that
///   committed the run's end dies inside the close's first delivery. The
///   evidence stands, the scope is still open, and the obligation the
///   terminal transaction armed on the run's own row is the recovery
///   owner: a claim whose delivery crashed lapses at its TTL, the next due
///   pass retakes it and delivers the close, and a further tick claims
///   nothing — an already-delivered run is not closed again. The process
///   worker then delivers the cancel the close row owes the process living
///   `Until` the run.
/// - **A parked run.** It has no terminal evidence, so neither the shift
///   nor the recovery closes its scope: its `Until` child owes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn run_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let registry = stores.process_registry();

    // A crash between the run's terminal commit and its scope close.
    let mut parts = ShiftParts::new(prefix, "run-close-crash", &effect_host, &stores, 8).await;
    let crash = crate::ConformanceCrash::new();
    let closes = Arc::new(CrashingRegistryScopeClose {
        registry: crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock()),
        store: Arc::clone(&parts.store),
        crash: crash.clone(),
        armed: AtomicBool::new(true),
        closes: Mutex::new(Vec::new()),
    });
    parts.host.control.scope_close = closes.clone();
    let run = TurnId::from("run-close-crash");
    let child = until_run(&registry, &parts.session_id, &run).await;
    parts.enqueue("ask", Some(run.as_str())).await;
    let request = parts.request("run-close-crash-shift");
    let crashing = parts.clone();
    let crashing_request = request.clone();
    runner
        .run_turn_until_crash(
            driver_scope(&parts),
            Arc::new(move |scope| {
                let parts = crashing.clone();
                let request = crashing_request.clone();
                Box::pin(async move {
                    let _ = work_once(parts.runtime().await, scope, request).await;
                    crate::ConformanceTurnEnd::Settled
                })
            }),
            crash.clone(),
        )
        .await;
    assert!(crash.has_fired(), "the execution died inside the close");
    let evidence = terminal(&parts, &run)
        .await
        .expect("the run's terminal commit landed before the crash");
    assert_eq!(evidence.kind(), RunTerminalKind::Answered);
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &run).await,
        Some(lash_core::store::ObligationState::Claimed),
        "the terminal transaction armed the run's scope-close obligation; \
         the crashed delivery's claim still holds it"
    );
    assert!(
        run_close_row(&registry, &parts.session_id, &run)
            .await
            .is_none(),
        "the crash took the close: the run's scope is still open"
    );
    assert!(
        !owes_cancel(&registry, &child.id).await,
        "nothing has closed the child's scope yet"
    );

    // The tier's recovery: the redelivered execution replays the run to its
    // recorded close step — the claimed obligation answers not-due, and the
    // recorded outcome stands. The lapsed claim is the due pass's to retake.
    let _ = on_tier(&runner, &parts, move |runtime, scope| {
        work_once(runtime, scope, request.clone())
    })
    .await;
    assert_eq!(
        closes.closes().len(),
        1,
        "no second close ran while the crashed claim held: {:?}",
        closes.closes()
    );

    // The due pass once the claim lapses: it claims the armed row, delivers
    // the close to the scope owner, and settles the row delivered. A second
    // tick claims nothing — the delivered run is not closed again.
    let relay = scope_close_relay(&stores, closes.clone());
    let lapsed: Arc<dyn lash_core::Clock> = Arc::new(OffsetClock {
        inner: Arc::clone(&parts.host.clock),
        offset_ms: lash_core::runtime::shift::relay::RelayPolicy::default().claim_ttl_ms + 1,
    });
    let first = reconcile_tick(&stores, &relay, Arc::clone(&lapsed)).await;
    let pass = scope_close_pass(&first);
    assert_eq!(
        (pass.claimed, pass.delivered),
        (1, 1),
        "the due pass delivered the armed close once: {pass:?}"
    );
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &run).await,
        Some(lash_core::store::ObligationState::Delivered),
        "the delivered close settled the obligation"
    );
    let closed_once = closes.closes().len();
    let second = reconcile_tick(&stores, &relay, Arc::clone(&lapsed)).await;
    let pass = scope_close_pass(&second);
    assert_eq!(pass.claimed, 0, "the second tick claimed nothing: {pass:?}");
    assert_eq!(
        closes.closes().len(),
        closed_once,
        "a second reconciliation tick does not close an already-delivered run: {:?}",
        closes.closes()
    );

    recovery_pass(&stores).await;
    assert!(
        run_close_row(&registry, &parts.session_id, &run)
            .await
            .is_some(),
        "recovery closed the run's scope"
    );
    assert!(
        owes_cancel(&registry, &child.id).await,
        "the process living until the ended run is owed its cancel"
    );
    assert!(
        closes
            .closes()
            .iter()
            .all(|(closed, durable)| *closed == run && *durable),
        "every close named the run after its evidence was durable: {:?}",
        closes.closes()
    );
    assert_eq!(parts.calls(), 1, "the run ran once");

    // A parked run.
    let mut parts = ShiftParts::new(prefix, "run-close-parked", &effect_host, &stores, 8).await;
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), false);
    parts.host.control.scope_close = closes.clone();
    let parked = TurnId::from("run-close-parked");
    let child = until_run(&registry, &parts.session_id, &parked).await;
    parts.enqueue("ask", Some(parked.as_str())).await;
    parts
        .store
        .record_turn_park(&lash_core::store::TurnParkWrite::refusal(
            parts.session_id.clone(),
            parked.clone(),
            lash_core::store::ParkReason::ReplayDivergence {
                message: "the run's replay diverged".into(),
            },
            1,
        ))
        .await
        .map(lash_core::store::StoreTransition::into_record)
        .expect("park the run");
    shift(&runner, &parts, "run-close-parked-shift").await;
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &parked).await,
        None,
        "a parked run armed no scope-close obligation"
    );
    let tick = reconcile_tick(
        &stores,
        &scope_close_relay(&stores, closes.clone()),
        Arc::clone(&parts.host.clock),
    )
    .await;
    assert_eq!(
        scope_close_pass(&tick).claimed,
        0,
        "the due pass found no close to deliver"
    );
    recovery_pass(&stores).await;
    assert_eq!(
        terminal(&parts, &parked).await,
        None,
        "a parked run has no end"
    );
    assert!(
        closes.closes().is_empty(),
        "no close was asked for the parked run"
    );
    assert!(
        run_close_row(&registry, &parts.session_id, &parked)
            .await
            .is_none(),
        "a parked run's scope stays open"
    );
    assert!(
        !owes_cancel(&registry, &child.id).await,
        "the process living until the parked run owes nothing"
    );
    assert_eq!(parts.calls(), 0, "the parked run ran nothing");
}

/// A report sink that crashes the execution at its first run's report
/// handover: it fires `crash` and never returns, so nothing after the
/// handover runs — the run's scope close included — the way a process that
/// dies after its run's `TurnPersisted` and before the close leaves it.
struct CrashingHandover {
    crash: crate::ConformanceCrash,
    armed: AtomicBool,
    handed: Mutex<Vec<TurnId>>,
}

#[async_trait::async_trait]
impl lash_core::shift::RunSettledSink for CrashingHandover {
    async fn settled(&self, _runtime: &crate::LashRuntime, run: lash_core::shift::SettledRun<'_>) {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(run.run.clone());
        if self.armed.swap(false, Ordering::SeqCst) {
            self.crash.fire();
            std::future::pending::<()>().await;
        }
    }
}

/// L-C1 across the report handover (FIG-3979): a run's report is handed
/// over after its final commit and `TurnPersisted`, before its recorded
/// scope close. An execution that dies at the handover has written the
/// run's evidence and armed its `ScopeClose` obligation, and has closed
/// nothing: the tier's redrive, or the obligation's due pass, still closes
/// the run's scope, once, after its evidence is durable, and the process
/// living `Until` the run is owed its cancel.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_crashed_at_its_report_handover_still_closes_its_scope(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let registry = stores.process_registry();
    let mut parts = ShiftParts::new(prefix, "run-handover-crash", &effect_host, &stores, 8).await;
    let crash = crate::ConformanceCrash::new();
    let closes = Arc::new(CrashingRegistryScopeClose {
        registry: crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock()),
        store: Arc::clone(&parts.store),
        crash: crash.clone(),
        armed: AtomicBool::new(false),
        closes: Mutex::new(Vec::new()),
    });
    parts.host.control.scope_close = closes.clone();
    let handover = Arc::new(CrashingHandover {
        crash: crash.clone(),
        armed: AtomicBool::new(true),
        handed: Mutex::new(Vec::new()),
    });
    let run = TurnId::from("run-handover-crash");
    let child = until_run(&registry, &parts.session_id, &run).await;
    parts.enqueue("ask", Some(run.as_str())).await;
    let request = parts.request("run-handover-crash-shift");
    let crashing = parts.clone();
    let crashing_request = request.clone();
    let crashing_handover = Arc::clone(&handover);
    runner
        .run_turn_until_crash(
            driver_scope(&parts),
            Arc::new(move |scope| {
                let parts = crashing.clone();
                let request = crashing_request.clone();
                let handover = Arc::clone(&crashing_handover);
                Box::pin(async move {
                    let mut runtime = parts.runtime().await;
                    let sinks = lash_core::shift::ShiftSinks {
                        settled: handover.as_ref(),
                        ..lash_core::shift::ShiftSinks::default()
                    };
                    let _ =
                        lash_core::shift::work_session_with(&mut runtime, &scope, &request, sinks)
                            .await;
                    crate::ConformanceTurnEnd::Settled
                })
            }),
            crash.clone(),
        )
        .await;
    assert!(
        crash.has_fired(),
        "the execution died at the report handover"
    );
    assert_eq!(
        handover
            .handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
        vec![run.clone()],
        "the run's report was handed over once, before the crash"
    );
    let evidence = terminal(&parts, &run)
        .await
        .expect("the run's terminal commit landed before the handover");
    assert_eq!(evidence.kind(), RunTerminalKind::Answered);
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &run).await,
        Some(lash_core::store::ObligationState::Due),
        "the terminal transaction armed the run's scope-close obligation, \
         and nothing claimed it before the crash"
    );
    assert!(
        closes.closes().is_empty(),
        "the crash came before the close: {:?}",
        closes.closes()
    );
    assert!(
        run_close_row(&registry, &parts.session_id, &run)
            .await
            .is_none(),
        "the run's scope is still open"
    );

    // The tier's recovery: the redelivered execution replays the run to
    // the close step its crashed execution never recorded, and runs it. A
    // due pass after it finds nothing left to close.
    let _ = on_tier(&runner, &parts, move |runtime, scope| {
        work_once(runtime, scope, request.clone())
    })
    .await;
    let relay = scope_close_relay(&stores, closes.clone());
    reconcile_tick(&stores, &relay, Arc::clone(&parts.host.clock)).await;
    assert_eq!(
        closes.closes(),
        vec![(run.clone(), true)],
        "the run's scope closed once, after its evidence was durable"
    );
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &run).await,
        Some(lash_core::store::ObligationState::Delivered),
        "the close settled the run's obligation"
    );
    recovery_pass(&stores).await;
    assert!(
        run_close_row(&registry, &parts.session_id, &run)
            .await
            .is_some(),
        "recovery closed the run's scope"
    );
    assert!(
        owes_cancel(&registry, &child.id).await,
        "the process living until the ended run is owed its cancel"
    );
    assert_eq!(parts.calls(), 1, "the run ran once");
}

/// A redelivered command run replays its recorded journal (FIG-3893, ADR
/// 0101 §4): a run that applied the session's queued command dies before
/// the engine records its end. The redelivered execution replays the same
/// journal and answers the same outcome, and the command applied once. A
/// command run ends like any other run (FIG-4202): its end writes its
/// terminal evidence, `CommandsApplied`, and arms its scope's close, which
/// runs once, after that evidence is durable, however many executions
/// replay it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_command_runs_redrive_replays_its_recorded_outcome(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "queued-close-redrive", &effect_host, &stores, 8).await;
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), false);
    parts.host.control.scope_close = closes.clone();
    // A session command alone: the shift admits a command run, which
    // applies it.
    parts
        .store
        .enqueue_queued_work(
            crate::QueuedWorkBatchDraft::new(
                &parts.session_id,
                crate::DeliveryPolicy::EarliestSafeBoundary,
                crate::SessionCommand::RefreshToolCatalog {
                    reason: "queued close redrive".to_string(),
                },
            )
            .with_source_key("queued-close-redrive-command"),
        )
        .await
        .expect("enqueue the command");
    let request = parts.request("queued-close-redrive-shift");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<ShiftOutcome, String>>();
    let attempt = |crash: bool| -> crate::ConformanceTurnAttempt {
        let parts = parts.clone();
        let request = request.clone();
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let outcome = lash_core::shift::work_session(&mut runtime, &scope, &request)
                    .await
                    .map_err(|abort| format!("{abort:?}"));
                let _ = tx.send(outcome);
                if crash {
                    panic!("the shift's execution dies after its command run ended");
                }
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    let executing = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = driver_scope(&parts);
        let (crashing, redrive) = (attempt(true), attempt(false));
        async move {
            runner
                .run_crashed_then_redriven_turn(scope, crashing, redrive)
                .await;
        }
    });
    let first = rx
        .recv()
        .await
        .expect("the first execution ran")
        .expect("the first execution executes the command run");
    let again = rx
        .recv()
        .await
        .expect("the redelivered execution ran")
        .expect("the redelivered execution executes the same run");
    // A redelivery whose journal diverged from the recorded one never
    // settles: its engine refuses the attempt and retries it forever. Bound
    // the wait so the divergence fails here rather than at the harness's
    // test timeout.
    tokio::time::timeout(std::time::Duration::from_secs(60), executing)
        .await
        .expect("the redelivered execution replays its recorded journal to its end")
        .expect("the tier settles the shift");
    let [RunOutcome::Applied { run }] = first.ran.as_slice() else {
        panic!("one command run ran and applied the command: {first:?}");
    };
    assert!(run.as_str().starts_with("shift-commands:"), "{first:?}");
    assert_eq!(again, first, "the redrive answers the recorded outcome");
    let ended = terminal(&parts, run)
        .await
        .expect("a command run's end writes its terminal evidence");
    assert_eq!(ended.cause, RunTerminalCause::CommandsApplied);
    assert_eq!(ended.kind(), RunTerminalKind::Answered);
    assert_eq!(
        closes.closes(),
        vec![(run.clone(), true)],
        "the command run's scope closed once, after its evidence was durable"
    );
    assert!(
        parts
            .store
            .list_open_queued_work(&parts.session_id)
            .await
            .expect("read the command lane")
            .is_empty(),
        "the command applied"
    );
    assert_eq!(parts.calls(), 0, "nothing ran a model");
}

/// L-C1 with the process registry as the scope owner (FIG-3607 R9, R11): a
/// run's end closes `Turn(run)` in the registry's scope-close ledger. A
/// process living `Until` the run is owed its cancel from that row, and a
/// start that names the ended run — as its lifetime or its starter — is
/// refused afterwards. Before the run ran, the same starts were admitted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_end_closes_its_turn_scope_in_the_process_registry(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-registry-close", &effect_host, &stores, 8).await;
    let registry = stores.process_registry();
    parts.host.control.scope_close = Arc::new(
        crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock())
            .with_session_store_factory(stores.session_store_factory()),
    );
    let run = TurnId::from("run-registry-close");
    let turn = lash_core::ScopeId::turn(parts.session_id.clone(), run.clone());
    let registration = || {
        lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            crate::ProcessProvenance::session(crate::SessionScope::new(parts.session_id.clone())),
            lash_core::Lifetime::Detached,
        )
    };
    let until_run = registry
        .register_process(crate::started_until_starter(registration(), turn.clone()))
        .await
        .expect("a start living until the running run is admitted");
    registry
        .register_process(crate::started_detached(registration(), turn.clone()))
        .await
        .expect("a detached start the running run made is admitted");

    parts.enqueue("ask", Some(run.as_str())).await;
    let outcome = shift(&runner, &parts, "run-registry-close-shift").await;
    assert!(
        outcome
            .ran
            .iter()
            .any(|ran| matches!(ran, RunOutcome::Committed { run: ran, .. } if *ran == run)),
        "{outcome:?}"
    );
    assert!(terminal(&parts, &run).await.is_some(), "the run ended");

    let closed = registry
        .get_parent_end_plan(&turn)
        .await
        .expect("read the run's close row")
        .expect("the run's end closes its turn scope in the registry");
    assert_eq!(closed.parent, turn);
    assert_eq!(
        registry
            .list_parent_end_children(&turn, None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page the ended run's children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![until_run.id],
        "the process living until the run is owed its cancel"
    );
    for (what, refused) in [
        (
            "until",
            crate::started_until_starter(registration(), turn.clone()),
        ),
        (
            "detached",
            crate::started_detached(registration(), turn.clone()),
        ),
    ] {
        assert!(
            matches!(
                registry.register_process(refused).await,
                Err(crate::PluginError::ParentEnded { .. })
            ),
            "a {what} start the ended run would make is refused"
        );
    }
}

/// A run's admission takes a second input whose source key names a turn
/// scope that will never become a run. Ending the admitted run closes that
/// scope and delivers its ParentEnd obligation while the session stays open.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the stores answer their own writes"
)]
pub async fn a_joined_inputs_turn_scope_closes_with_its_admitting_run(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "joined-scope-close", &effect_host, &stores, 8).await;
    parts.compose_inputs();
    let registry = stores.process_registry();
    parts.host.control.scope_close = Arc::new(
        crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock())
            .with_session_store_factory(stores.session_store_factory()),
    );
    let run = TurnId::from("joined-scope-run");
    let joined = TurnId::from("joined-scope-input");
    let child = until_run(&registry, &parts.session_id, &joined).await;
    parts.enqueue("head", Some(run.as_str())).await;
    let joined_input = parts.enqueue("joined", Some(joined.as_str())).await;

    let outcome = shift(&runner, &parts, "joined-scope-shift").await;
    assert!(
        matches!(&outcome.ran[..], [RunOutcome::Committed { run: ran, .. }] if *ran == run),
        "{outcome:?}"
    );
    assert_eq!(
        parts
            .store
            .run_binding(&parts.session_id, &joined_input)
            .await
            .expect("read the admission binding"),
        Some(run.clone()),
        "the ending run took the joined input"
    );
    assert!(terminal(&parts, &run).await.is_some(), "the run ended");
    assert!(
        registry
            .get_parent_end_plan(&lash_core::ScopeId::session(parts.session_id.clone()))
            .await
            .expect("read the session scope")
            .is_none(),
        "the session remains open"
    );
    let joined_scope = lash_core::ScopeId::turn(parts.session_id.clone(), joined.clone());
    assert!(
        registry
            .get_parent_end_plan(&joined_scope)
            .await
            .expect("read the joined scope")
            .is_some(),
        "the run's close records the joined scope's end"
    );
    recovery_pass(&stores).await;
    assert!(
        owes_cancel(&registry, &child.id).await,
        "the joined scope's child is reaped before session close"
    );
}
