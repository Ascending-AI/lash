//! A logical root's terminal evidence through the session drive (FIG-3600
//! S7, FIG-3607 contract 2 and item 7): the laws reach an engine only
//! through the kernel's drive entries, on the controller the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) admits.
//!
//! - L-T3: a committed root answers its terminal by its root, and its
//!   accepted input names the root that took it.
//! - L-R3 / L-T4: admission answers a root that already has evidence from
//!   that evidence and runs nothing.
//! - The successor-sealed refusal (Q-B3): a root whose admission a successor
//!   sealed over commits nothing, and a later drive of it commits once.
//! - L-C1: the root's scope closes after its evidence is durable, at least
//!   once across a crash between the two, and never for a parked root.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::engine::{DriveOutcome, DriveStop, RootOutcome, ScopeCloseSink};
use lash_core::store::{
    AdmissionId, ControlIntentId, DriveEpochSeal, RootStartNonce, RootTerminal, RootTerminalCause,
    RootTerminalKind, RootTerminalWrite, TurnCommitId,
};
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use super::drive_admission::{DriveParts, admitted, driver_scope, on_tier};

/// A scope owner that records every root close with whether the root's
/// evidence was durable when it was called, and refuses the first close
/// when asked to.
struct RecordingScopeClose {
    store: Arc<dyn crate::RuntimePersistence>,
    fail_next: AtomicBool,
    closes: Mutex<Vec<(TurnId, bool)>>,
}

impl RecordingScopeClose {
    fn new(store: Arc<dyn crate::RuntimePersistence>, fail_first: bool) -> Arc<Self> {
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
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), crate::StoreError> {
        let durable = self
            .store
            .root_terminal(&terminal.session_id, &terminal.root)
            .await?
            .is_some_and(|stored| stored.same_terminal(terminal));
        self.closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((terminal.root.clone(), durable));
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
        _roots: &[TurnId],
    ) -> Result<(), crate::StoreError> {
        Ok(())
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own read"
)]
async fn terminal(parts: &DriveParts, root: &TurnId) -> Option<RootTerminal> {
    parts
        .store
        .root_terminal(&parts.session_id, root)
        .await
        .expect("read the root's terminal evidence")
}

async fn drive(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
    id: &str,
) -> DriveOutcome {
    let request = parts.request(id);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            #[expect(
                clippy::expect_used,
                reason = "conformance-law fixture: the drive runs to a stop"
            )]
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await
}

/// L-T3 (the drive half): the root's final commit writes its terminal
/// evidence, addressed by the root, and the accepted input it drove names
/// that root.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_root_answers_its_terminal_by_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "root-answered", &effect_host, &stores, 8).await;
    let input = parts.enqueue("ask", Some("root-answered")).await;
    let root = TurnId::from("root-answered");
    assert_eq!(terminal(&parts, &root).await, None, "no evidence before");
    assert_eq!(
        parts
            .store
            .root_of_input(&parts.session_id, &input)
            .await
            .expect("read the input's root"),
        None,
        "a pending input has no root"
    );
    let outcome = drive(&runner, &parts, "root-answered-drive").await;
    assert!(
        matches!(&outcome.ran[..], [RootOutcome::Committed { root: ran, .. }] if *ran == root),
        "{outcome:?}"
    );
    let evidence = terminal(&parts, &root)
        .await
        .expect("the root's final commit wrote its evidence");
    assert_eq!(evidence.kind, RootTerminalKind::Answered);
    assert_eq!(
        evidence.cause,
        RootTerminalCause::Committed {
            commit: TurnCommitId::new(root.clone(), 0),
            turn: root.clone(),
            stop: None,
        }
    );
    assert!(evidence.head_revision.is_some(), "a head commit wrote it");
    assert_eq!(
        parts
            .store
            .root_of_input(&parts.session_id, &input)
            .await
            .expect("read the input's root"),
        Some(root),
        "the accepted input names the root that took it"
    );
}

/// L-R3 and L-T4: admission answers a head input whose root already has
/// terminal evidence from that evidence. Nothing is sealed and nothing runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_id_naming_a_terminal_root_is_answered_not_rerun(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "root-adopted", &effect_host, &stores, 8).await;
    let root = TurnId::from("root-adopted");
    // An earlier epoch committed the root: its evidence stands.
    let state = parts.initial_state();
    let operation = crate::OperationId::turn(parts.session_id.as_str(), root.as_str(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive commit node ids");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        graph,
        &[],
        operation,
    )
    .expect("build the earlier epoch's commit");
    commit.root_terminal = Some(Box::new(RootTerminalWrite {
        root: root.clone(),
        commit: TurnCommitId::new(root.clone(), 0),
        turn: root.clone(),
        stop: None,
    }));
    parts
        .store
        .commit_runtime_state(commit)
        .await
        .expect("the earlier epoch commits the root");
    assert!(
        terminal(&parts, &root).await.is_some(),
        "the root is terminal"
    );

    parts
        .enqueue("names the ended root", Some("root-adopted"))
        .await;
    let outcome = drive(&runner, &parts, "root-adopted-drive").await;
    assert!(outcome.ran.is_empty(), "{outcome:?}");
    assert_eq!(
        outcome.stop,
        DriveStop::RootTerminal {
            root,
            kind: RootTerminalKind::Answered,
            commit: Some(TurnCommitId::new(TurnId::from("root-adopted"), 0)),
        }
    );
    assert_eq!(parts.epoch().await.epoch, 0, "nothing was sealed");
    assert_eq!(parts.calls(), 0, "nothing ran");
}

/// The successor-sealed commit refusal (ADR 0105 §2, Q-B3): a successor
/// seals the session while the admitted root runs, so the root's commit
/// carries a stale fence. It commits nothing, writes no evidence and closes
/// no scope; a later drive of the same root commits it once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_whose_admission_a_successor_sealed_commits_nothing(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "root-superseded", &effect_host, &stores, 8).await;
    let sealed_over = Arc::new(AtomicBool::new(false));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let store = Arc::clone(&parts.store);
            let session_id = parts.session_id.clone();
            let sealed_over = Arc::clone(&sealed_over);
            move |_request| {
                let store = Arc::clone(&store);
                let session_id = session_id.clone();
                let sealed_over = Arc::clone(&sealed_over);
                async move {
                    if !sealed_over.swap(true, Ordering::SeqCst) {
                        let seal = store
                            .seal_drive_epoch(
                                &session_id,
                                &AdmissionId::new("successor#0"),
                                1,
                                &RootStartNonce::new("successor"),
                            )
                            .await
                            .expect("the successor seals");
                        assert!(matches!(seal, DriveEpochSeal::Sealed(_)), "{seal:?}");
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
    parts.host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), false);
    parts.host.control.scope_close = closes.clone();
    let input = parts.enqueue("ask", Some("root-superseded")).await;
    let root = TurnId::from("root-superseded");
    let request = parts.request("root-superseded-drive");
    let refused = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let admitted = admitted(
                lash_core::drive::admit_drive(&mut runtime, &scope, &request, 0)
                    .await
                    .expect("admit the root"),
            );
            lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted)
                .await
                .map(|_| ())
                .map_err(|abort| format!("{abort:?}"))
        })
    })
    .await;
    assert!(sealed_over.load(Ordering::SeqCst), "the successor sealed");
    let refusal = refused.expect_err("the stale commit is refused");
    assert!(
        refusal.contains("StaleDriveFence") || refusal.contains("drive fence"),
        "{refusal}"
    );
    assert_eq!(parts.epoch().await.epoch, 2, "the successor's seal stands");
    assert_eq!(terminal(&parts, &root).await, None, "no evidence");
    assert!(parts.applications().await.is_empty(), "nothing committed");
    assert!(closes.closes().is_empty(), "no scope closed");

    let outcome = drive(&runner, &parts, "root-superseded-after").await;
    assert!(
        outcome
            .ran
            .iter()
            .any(|ran| matches!(ran, RootOutcome::Committed { root: ran, .. } if *ran == root)),
        "{outcome:?}"
    );
    assert!(
        terminal(&parts, &root).await.is_some(),
        "the later drive commits"
    );
    assert_eq!(parts.applications().await, vec![(input, root.clone())]);
    assert_eq!(closes.closes(), vec![(root, true)]);
}

/// A process registry's scope owner that crashes the execution on its first
/// root close: it fires `crash` and never answers, so the close writes
/// nothing, the way a process that dies between a root's terminal commit and
/// its scope close leaves it. Every later close reaches the registry. It
/// records each root it was asked to close, with whether the root's evidence
/// was durable by then.
struct CrashingRegistryScopeClose {
    registry: crate::RegistryScopeClose,
    store: Arc<dyn crate::RuntimePersistence>,
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
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), crate::StoreError> {
        let durable = self
            .store
            .root_terminal(&terminal.session_id, &terminal.root)
            .await?
            .is_some_and(|stored| stored.same_terminal(terminal));
        self.closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((terminal.root.clone(), durable));
        if self.armed.swap(false, Ordering::SeqCst) {
            self.crash.fire();
            std::future::pending::<()>().await;
        }
        self.registry.close_root_scope(terminal).await
    }

    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), crate::StoreError> {
        self.registry
            .close_session_scope(session, intent, roots)
            .await
    }
}

/// One drive of the law's session for `request`, as the tier runs it.
fn drive_once<'a>(
    mut runtime: crate::LashRuntime,
    scope: crate::ScopedEffectController<'a>,
    request: lash_core::engine::DriveRequest,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        lash_core::drive::drive_session(&mut runtime, &scope, &request)
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
) -> lash_core::runtime::drive::ScopeCloseRelay {
    lash_core::runtime::drive::ScopeCloseRelay::new(
        scope_close_ledger(stores),
        stores.session_store_factory(),
        scopes,
    )
}

/// Wire the host the law's drive runs on so its close step delivers through
/// the obligation the terminal write armed.
fn wire_scope_close_obligations(parts: &mut DriveParts, stores: &Arc<dyn crate::StoreSet>) {
    parts.host.control.scope_close_obligations = Some(scope_close_ledger(stores));
}

/// The state of `root`'s armed scope-close obligation, if the row carries one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the ledger answers its own read"
)]
async fn scope_close_state(
    stores: &Arc<dyn crate::StoreSet>,
    session_id: &SessionId,
    root: &TurnId,
) -> Option<lash_core::store::ObligationState> {
    scope_close_ledger(stores)
        .state(&lash_core::store::scope_close_obligation_id(
            session_id, root,
        ))
        .await
        .expect("read the root's scope-close obligation")
}

/// One reconcile tick over the law's stores (ADR 0108 §5, ADR 0109 §3): the
/// scope-close relay's due pass is the engine-neutral owner of an armed but
/// undelivered close — no terminal-root scan remains. Returns the tick's
/// report; the obligations arm reports no failure.
async fn reconcile_tick(
    stores: &Arc<dyn crate::StoreSet>,
    relay: &lash_core::runtime::drive::ScopeCloseRelay,
    clock: &dyn lash_core::Clock,
    tick: &str,
) -> lash_core::engine::ReconcileTick {
    let factory = stores.session_store_factory();
    let work = crate::NoSessionWork::new();
    let scopes = crate::engine::NoScopeClose;
    let relays: Vec<Arc<dyn lash_core::runtime::drive::relay::ObligationRelay>> =
        vec![Arc::new(relay.clone())];
    let report = lash_core::runtime::drive::reconcile_once(
        &lash_core::runtime::drive::ReconcileParts {
            sessions: factory.as_ref(),
            work: &work,
            scopes: &scopes,
            processes: None,
            clock,
            duties: lash_core::runtime::recovery_lease::RecoveryDuties::ALL,
            relays: &relays,
        },
        &lash_core::engine::ReconcileCursor::default(),
        std::num::NonZeroUsize::new(64).unwrap_or(std::num::NonZeroUsize::MIN),
        tick,
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

/// One pass of the process worker over the law's stores: it delivers the
/// cancel each close row owes the processes living `Until` the closed scope
/// (ADR 0108 §5).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the sweep over the law's stores builds and runs"
)]
async fn recovery_sweep(
    stores: &Arc<dyn crate::StoreSet>,
    effect_host: &Arc<dyn crate::EffectHost>,
) {
    let registry = stores.process_registry();
    let watched = crate::facade_support::watch_process_registry(Arc::clone(&registry));
    let host = crate::LawBackend::over_stores(Arc::clone(stores), Arc::clone(effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(crate::facade_support::PluginHost::new(
                crate::testing::test_standard_protocol_factories(),
            )),
            host,
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(crate::NoSessionWork::new()),
            crate::testing::runtime_lease_owner(),
        ),
    )
    .expect("build the recovery sweep worker");
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("the recovery sweep runs");
}

/// A process living `Until` `root`'s turn scope, registered while the root
/// has not ended.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registry admits a start under a live root"
)]
async fn until_root(
    registry: &Arc<dyn crate::ProcessRegistry>,
    session_id: &SessionId,
    root: &TurnId,
) -> crate::ProcessRecord {
    registry
        .register_process(crate::started_until_starter(
            crate::ProcessRegistration::new(
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::Rerunnable,
                crate::ProcessProvenance::session(crate::SessionScope::new(session_id.as_str())),
                lash_core::Lifetime::Detached,
            ),
            lash_core::ScopeId::turn(session_id.clone(), root.clone()),
        ))
        .await
        .expect("a start living until the running root is admitted")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registry answers its own reads"
)]
async fn root_close_row(
    registry: &Arc<dyn crate::ProcessRegistry>,
    session_id: &SessionId,
    root: &TurnId,
) -> Option<crate::ParentEndPlan> {
    registry
        .get_parent_end_plan(&lash_core::ScopeId::turn(session_id.clone(), root.clone()))
        .await
        .expect("read the root's close row")
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

/// L-C1 (FIG-3607 item 7, ADR 0109 §3): a root's scope closes after its
/// terminal evidence is durable, at least once, and never for a root that
/// is parked.
///
/// - **A crash between the commit and the close.** The execution that
///   committed the root's end dies inside the close's first delivery. The
///   evidence stands, the scope is still open, and the obligation the
///   terminal transaction armed on the root's own row is the recovery
///   owner: a claim whose delivery crashed lapses at its TTL, the next due
///   pass retakes it and delivers the close, and a further tick claims
///   nothing — an already-delivered root is not closed again. The process
///   worker then delivers the cancel the close row owes the process living
///   `Until` the root.
/// - **A parked root.** It has no terminal evidence, so neither the drive
///   nor the recovery closes its scope: its `Until` child owes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn root_scope_close_runs_after_terminal_evidence_at_least_once_never_for_parked(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let registry = stores.process_registry();

    // A crash between the root's terminal commit and its scope close.
    let mut parts = DriveParts::new(prefix, "root-close-crash", &effect_host, &stores, 8).await;
    let crash = crate::ConformanceCrash::new();
    let closes = Arc::new(CrashingRegistryScopeClose {
        registry: crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock()),
        store: Arc::clone(&parts.store),
        crash: crash.clone(),
        armed: AtomicBool::new(true),
        closes: Mutex::new(Vec::new()),
    });
    parts.host.control.scope_close = closes.clone();
    wire_scope_close_obligations(&mut parts, &stores);
    let root = TurnId::from("root-close-crash");
    let child = until_root(&registry, &parts.session_id, &root).await;
    parts.enqueue("ask", Some(root.as_str())).await;
    let request = parts.request("root-close-crash-drive");
    let crashing = parts.clone();
    let crashing_request = request.clone();
    runner
        .run_turn_until_crash(
            driver_scope(&parts),
            Arc::new(move |scope| {
                let parts = crashing.clone();
                let request = crashing_request.clone();
                Box::pin(async move {
                    let _ = drive_once(parts.runtime().await, scope, request).await;
                    crate::ConformanceTurnEnd::Settled
                })
            }),
            crash.clone(),
        )
        .await;
    assert!(crash.has_fired(), "the execution died inside the close");
    let evidence = terminal(&parts, &root)
        .await
        .expect("the root's terminal commit landed before the crash");
    assert_eq!(evidence.kind, RootTerminalKind::Answered);
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &root).await,
        Some(lash_core::store::ObligationState::Claimed),
        "the terminal transaction armed the root's scope-close obligation; \
         the crashed delivery's claim still holds it"
    );
    assert!(
        root_close_row(&registry, &parts.session_id, &root)
            .await
            .is_none(),
        "the crash took the close: the root's scope is still open"
    );
    assert!(
        !owes_cancel(&registry, &child.id).await,
        "nothing has closed the child's scope yet"
    );

    // The tier's recovery: the redelivered execution replays the root to its
    // recorded close step — the claimed obligation answers not-due, and the
    // recorded outcome stands. The lapsed claim is the due pass's to retake.
    let _ = on_tier(&runner, &parts, move |runtime, scope| {
        drive_once(runtime, scope, request.clone())
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
    // tick claims nothing — the delivered root is not closed again.
    let relay = scope_close_relay(&stores, closes.clone());
    let lapsed = OffsetClock {
        inner: Arc::clone(&parts.host.clock),
        offset_ms: lash_core::runtime::drive::relay::RelayPolicy::default().claim_ttl_ms + 1,
    };
    let first = reconcile_tick(&stores, &relay, &lapsed, "root-close-recover").await;
    let pass = scope_close_pass(&first);
    assert_eq!(
        (pass.claimed, pass.delivered),
        (1, 1),
        "the due pass delivered the armed close once: {pass:?}"
    );
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &root).await,
        Some(lash_core::store::ObligationState::Delivered),
        "the delivered close settled the obligation"
    );
    let closed_once = closes.closes().len();
    let second = reconcile_tick(&stores, &relay, &lapsed, "root-close-again").await;
    let pass = scope_close_pass(&second);
    assert_eq!(pass.claimed, 0, "the second tick claimed nothing: {pass:?}");
    assert_eq!(
        closes.closes().len(),
        closed_once,
        "a second reconciliation tick does not close an already-delivered root: {:?}",
        closes.closes()
    );

    recovery_sweep(&stores, &effect_host).await;
    assert!(
        root_close_row(&registry, &parts.session_id, &root)
            .await
            .is_some(),
        "recovery closed the root's scope"
    );
    assert!(
        owes_cancel(&registry, &child.id).await,
        "the process living until the ended root is owed its cancel"
    );
    assert!(
        closes
            .closes()
            .iter()
            .all(|(closed, durable)| *closed == root && *durable),
        "every close named the root after its evidence was durable: {:?}",
        closes.closes()
    );
    assert_eq!(parts.calls(), 1, "the root ran once");

    // A parked root.
    let mut parts = DriveParts::new(prefix, "root-close-parked", &effect_host, &stores, 8).await;
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), false);
    parts.host.control.scope_close = closes.clone();
    wire_scope_close_obligations(&mut parts, &stores);
    let parked = TurnId::from("root-close-parked");
    let child = until_root(&registry, &parts.session_id, &parked).await;
    parts.enqueue("ask", Some(parked.as_str())).await;
    parts
        .store
        .record_turn_park(&lash_core::store::TurnParkWrite::refusal(
            parts.session_id.clone(),
            parked.clone(),
            lash_core::store::ParkReason::ReplayDivergence {
                message: "the root's replay diverged".into(),
            },
            1,
        ))
        .await
        .expect("park the root");
    drive(&runner, &parts, "root-close-parked-drive").await;
    assert_eq!(
        scope_close_state(&stores, &parts.session_id, &parked).await,
        None,
        "a parked root armed no scope-close obligation"
    );
    let tick = reconcile_tick(
        &stores,
        &scope_close_relay(&stores, closes.clone()),
        parts.host.clock.as_ref(),
        "root-close-parked",
    )
    .await;
    assert_eq!(
        scope_close_pass(&tick).claimed,
        0,
        "the due pass found no close to deliver"
    );
    recovery_sweep(&stores, &effect_host).await;
    assert_eq!(
        terminal(&parts, &parked).await,
        None,
        "a parked root has no end"
    );
    assert!(
        closes.closes().is_empty(),
        "no close was asked for the parked root"
    );
    assert!(
        root_close_row(&registry, &parts.session_id, &parked)
            .await
            .is_none(),
        "a parked root's scope stays open"
    );
    assert!(
        !owes_cancel(&registry, &child.id).await,
        "the process living until the parked root owes nothing"
    );
    assert_eq!(parts.calls(), 0, "the parked root ran nothing");
}

/// L-C1, the settlement half: a queued root that settles without a head
/// commit ends at its settlement, whose transaction wrote the root's
/// evidence, and its scope closes after that evidence like any other
/// terminal root's.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_queued_root_settled_without_a_commit_closes_after_its_evidence(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "root-settled", &effect_host, &stores, 8).await;
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), false);
    parts.host.control.scope_close = closes.clone();
    wire_scope_close_obligations(&mut parts, &stores);
    // An earlier drain admitted a named run and selected nothing before it
    // stopped: the run is the session's pending root.
    let lease = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
        &parts.store,
        &parts.session_id,
        "root-settled-earlier-drain",
    )
    .await;
    let run = parts
        .store
        .begin_or_resume_queued_run(
            &lease.authority(),
            lash_core::store::BeginQueuedRun {
                session_id: parts.session_id.clone(),
                identity: Some(crate::ExecutionScope::queue_drain(
                    &parts.session_id,
                    "root-settled-run",
                )),
                request: lash_core::store::QueuedRunRequest::Automatic,
                configuration: crate::RuntimeCommit::persisted_state_for_test(
                    &parts.initial_state(),
                    &[],
                )
                .config,
                expected_head_revision: 0,
                initial_turn_index: 1,
                generation: None,
                admitted_generation: lash_core::engine::BuildGeneration::for_test("conformance"),
            },
        )
        .await
        .expect("an earlier drain admits the run");
    parts
        .store
        .release_session_execution_lease(&lease.authority())
        .await
        .expect("the earlier drain stops");
    let root = TurnId::from(run.scope.id());
    assert_eq!(terminal(&parts, &root).await, None, "the run is pending");

    drive(&runner, &parts, "root-settled-drive").await;
    let evidence = terminal(&parts, &root)
        .await
        .expect("the run's settlement wrote its root's evidence");
    assert_eq!(evidence.cause, RootTerminalCause::SettledEmpty);
    assert_eq!(evidence.head_revision, None, "no head commit ended it");
    assert_eq!(closes.closes(), vec![(root, true)]);
    assert_eq!(parts.calls(), 0, "nothing ran");
}

/// L-C1 with the process registry as the scope owner (FIG-3607 R9, R11): a
/// root's end closes `Turn(root)` in the registry's scope-close ledger. A
/// process living `Until` the root is owed its cancel from that row, and a
/// start that names the ended root — as its lifetime or its starter — is
/// refused afterwards. Before the root ran, the same starts were admitted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_end_closes_its_turn_scope_in_the_process_registry(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "root-registry-close", &effect_host, &stores, 8).await;
    let registry = stores.process_registry();
    parts.host.control.scope_close = Arc::new(crate::RegistryScopeClose::new(
        Arc::clone(&registry),
        stores.clock(),
    ));
    wire_scope_close_obligations(&mut parts, &stores);
    let root = TurnId::from("root-registry-close");
    let turn = lash_core::ScopeId::turn(parts.session_id.clone(), root.clone());
    let registration = || {
        crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::session(crate::SessionScope::new(parts.session_id.as_str())),
            lash_core::Lifetime::Detached,
        )
    };
    let until_root = registry
        .register_process(crate::started_until_starter(registration(), turn.clone()))
        .await
        .expect("a start living until the running root is admitted");
    registry
        .register_process(crate::started_detached(registration(), turn.clone()))
        .await
        .expect("a detached start the running root made is admitted");

    parts.enqueue("ask", Some(root.as_str())).await;
    let outcome = drive(&runner, &parts, "root-registry-close-drive").await;
    assert!(
        outcome
            .ran
            .iter()
            .any(|ran| matches!(ran, RootOutcome::Committed { root: ran, .. } if *ran == root)),
        "{outcome:?}"
    );
    assert!(terminal(&parts, &root).await.is_some(), "the root ended");

    let closed = registry
        .get_parent_end_plan(&turn)
        .await
        .expect("read the root's close row")
        .expect("the root's end closes its turn scope in the registry");
    assert_eq!(closed.parent, turn);
    assert_eq!(
        registry
            .list_parent_end_children(&turn, None, std::num::NonZeroUsize::MIN)
            .await
            .expect("page the ended root's children")
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![until_root.id],
        "the process living until the root is owed its cancel"
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
            "a {what} start the ended root would make is refused"
        );
    }
}
