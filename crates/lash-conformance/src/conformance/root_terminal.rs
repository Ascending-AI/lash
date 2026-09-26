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
//!   once when the first close fails, and never for a root that did not end.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::engine::{DriveOutcome, DriveStop, RootOutcome, ScopeCloseSink};
use lash_core::store::{
    AdmissionId, ControlIntentId, DriveEpochSeal, RootStartNonce, RootTerminal, RootTerminalCause,
    RootTerminalKind, RootTerminalWrite, TurnCommitId,
};
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use super::drive_admission::{DriveParts, admitted, on_tier};

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

/// L-C1: a root's scope closes only after its terminal evidence is durable,
/// and a close that fails runs again: the recorded close step is retried
/// until the owner acknowledges it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn root_scope_close_runs_after_terminal_evidence_at_least_once(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "root-close", &effect_host, &stores, 8).await;
    let closes = RecordingScopeClose::new(Arc::clone(&parts.store), true);
    parts.host.control.scope_close = closes.clone();
    parts.enqueue("ask", Some("root-close")).await;
    let root = TurnId::from("root-close");
    let mut attempts = 0;
    while closes.closes().len() < 2 && attempts < 3 {
        attempts += 1;
        let request = parts.request(&format!("root-close-drive-{attempts}"));
        let _ = on_tier(&runner, &parts, move |mut runtime, scope| {
            let request = request.clone();
            Box::pin(async move {
                lash_core::drive::drive_session(&mut runtime, &scope, &request)
                    .await
                    .map_err(|abort| format!("{abort:?}"))
            })
        })
        .await;
    }
    let evidence = terminal(&parts, &root)
        .await
        .expect("the root committed its evidence");
    assert_eq!(evidence.kind, RootTerminalKind::Answered);
    let closes = closes.closes();
    assert!(closes.len() >= 2, "the refused close ran again: {closes:?}");
    assert!(
        closes
            .iter()
            .all(|(closed, durable)| *closed == root && *durable),
        "every close named the root after its evidence was durable: {closes:?}"
    );
    assert_eq!(parts.calls(), 1, "the root ran once");
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
    parts.host.control.scope_close =
        Arc::new(crate::RegistryScopeClose::new(Arc::clone(&registry)));
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
