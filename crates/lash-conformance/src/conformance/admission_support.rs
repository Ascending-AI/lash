//! Shared fixtures for laws that admit rows to a run and settle them
//! (FIG-3927): a row is open until a run's admission binds it, and only
//! that run's fenced commit (or its terminal) answers it.

use super::*;
use lash_core::store::{
    AdmittedHead, CheckpointAdmission, IngressRowId, IngressSettlement, RunAdmission,
    RunTerminalWrite, ShiftFence, TurnCommitId,
};
pub(crate) use lash_core::testing::store_fixtures::{
    admit_at_checkpoint_for_test, admit_run_for_test, admit_run_request_for_test,
    settling_commit_for_test,
};

/// Admit `run` headed by `head` under `fence`; the admission must reach
/// its head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the admission is established by the setup"
)]
pub(crate) async fn admitted_run(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &ShiftFence,
    run: &str,
    head: AdmittedHead,
) -> RunAdmission {
    admit_run_for_test(store, fence, &crate::TurnId::fixture(run), head)
        .await
        .expect("admit the run")
        .expect("the run's admission reaches its head")
}

/// Start `run` running under `fence`: enqueue a next-turn head input filed
/// under the run's own id and admit the run with it. Input may address a
/// turn only while that turn runs or once it has ended (ADR 0101 §5.1), so a
/// law that addresses `run`'s turns starts it first, before it enqueues the
/// rows it composes, and the run takes only its own head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the head and its admission are established by the setup"
)]
pub(crate) async fn active_run(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &ShiftFence,
    run: &crate::TurnId,
) -> RunAdmission {
    let head = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                fence.session(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text(format!("{run} head")),
            )
            .with_source_key(run.as_str()),
        )
        .await
        .expect("enqueue the running run's head");
    admitted_run(
        store,
        fence,
        run.as_str(),
        AdmittedHead::Input(head.input_id),
    )
    .await
}

/// [`active_run`] headed by a process wake instead of an input, for a law
/// whose assertions read the session's pending inputs: the run's own head
/// is then no pending input.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the head and its admission are established by the setup"
)]
pub(crate) async fn active_run_on_wake(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &crate::SessionId,
    run: &crate::TurnId,
) -> RunAdmission {
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        store,
        session_id,
        run.as_str(),
    )
    .await;
    let head = store
        .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
            session_id,
            &format!("{run}-starter"),
            1,
            "start the run",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the running run's head");
    admitted_run(
        store,
        &fence,
        run.as_str(),
        AdmittedHead::Batch(head.batch_id),
    )
    .await
}

/// Admit `run` headed by `head` under `fence`, composing its prefix with
/// `policy`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the admission is established by the setup"
)]
pub(crate) async fn admitted_run_with_policy(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &ShiftFence,
    run: &str,
    head: AdmittedHead,
    policy: crate::TurnLaneAdmissionPolicy,
) -> RunAdmission {
    let mut request = admit_run_request_for_test(fence, &crate::TurnId::fixture(run), head);
    request.policy = policy;
    store
        .admit_run(&request)
        .await
        .expect("admit the run")
        .expect("the run's admission reaches its head")
}

/// The settlement completing every row `admission` bound to `run`.
pub(crate) fn completing_admission(run: &str, admission: &RunAdmission) -> IngressSettlement {
    let mut settlement = IngressSettlement::new(crate::TurnId::fixture(run));
    if let Some(inputs) = &admission.inputs {
        settlement.completed_inputs.push(inputs.completion());
    }
    if let Some(queued) = &admission.queued {
        settlement.completed_batches.push(queued.completion());
    }
    settlement
}

/// `settlement` also completing every row a checkpoint `admission` bound.
pub(crate) fn completing_checkpoint(
    mut settlement: IngressSettlement,
    admission: &CheckpointAdmission,
) -> IngressSettlement {
    if let Some(inputs) = &admission.inputs {
        settlement.completed_inputs.push(inputs.completion());
    }
    if let Some(queued) = &admission.queued {
        settlement.completed_batches.push(queued.completion());
    }
    settlement
}

/// A settlement of `run` that hands `rows` back open at their positions.
pub(crate) fn releasing(
    run: &str,
    rows: impl IntoIterator<Item = IngressRowId>,
) -> IngressSettlement {
    let mut settlement = IngressSettlement::new(crate::TurnId::fixture(run));
    settlement.released.extend(rows);
    settlement
}

/// The terminal write of `run`'s first physical turn completing it.
pub(crate) fn run_completes(run: &str) -> RunTerminalWrite {
    let run = crate::TurnId::fixture(run);
    RunTerminalWrite {
        commit: TurnCommitId::new(run.clone(), 0),
        turn: lash_core::store::PhysicalTurn::derive_turn_id(&run, 0),
        run,
        outcome: crate::store::RunCommittedOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }
}

/// `commit` settling `settlement` under `fence` and ending its run: the
/// shape of a run's final commit.
pub(crate) fn final_commit(
    commit: crate::RuntimeCommit,
    fence: &ShiftFence,
    settlement: IngressSettlement,
) -> crate::RuntimeCommit {
    let terminal = run_completes(settlement.run.as_str());
    let mut commit = settling_commit_for_test(commit, fence, settlement);
    commit.run_terminal = Some(Box::new(terminal));
    commit
}

/// `commit` applying the command rows `completion` names under `fence`: the
/// command lane's bindless settlement (design §2.7).
pub(crate) fn applying_commands(
    mut commit: crate::RuntimeCommit,
    fence: &ShiftFence,
    completion: crate::QueuedWorkCompletion,
) -> crate::RuntimeCommit {
    commit.shift_fence = Some(Box::new(fence.clone()));
    commit.applied_commands = Some(completion);
    commit
}

/// The row a pending input names.
pub(crate) fn input_row(input: &crate::PendingTurnInput) -> IngressRowId {
    IngressRowId::Input(input.input_id.clone())
}

/// The row a queued batch names.
pub(crate) fn batch_row(batch: &crate::QueuedWorkBatch) -> IngressRowId {
    IngressRowId::Batch(batch.batch_id.clone())
}

/// A bare commit over the store's current head for `session_id`: the base a
/// settling or run-ending commit is built on.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own head read"
)]
pub(crate) async fn head_commit(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &crate::SessionId,
) -> crate::RuntimeCommit {
    let revision = store
        .load_session_head_meta(session_id)
        .await
        .expect("load the head")
        .map_or(0, |meta| meta.head_revision);
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        head_revision: revision,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    crate::RuntimeCommit::persisted_state_for_test(&state)
}

/// Land `run`'s final commit settling `settlement` under `fence`, over the
/// current head.
pub(crate) async fn try_end_run(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &ShiftFence,
    settlement: IngressSettlement,
) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
    let commit = head_commit(store, fence.session()).await;
    store
        .commit_runtime_state(final_commit(commit, fence, settlement))
        .await
}

/// [`try_end_run`], which must land.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the final commit is established by the setup"
)]
pub(crate) async fn end_run(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &ShiftFence,
    settlement: IngressSettlement,
) -> crate::store::RuntimeCommitReceipt {
    try_end_run(store, fence, settlement)
        .await
        .expect("the run's final commit lands")
}

/// Admit `run` headed by `head` and end it completing everything it took;
/// returns the admission.
pub(crate) async fn execute_run_to_end(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &ShiftFence,
    run: &str,
    head: AdmittedHead,
) -> RunAdmission {
    let admission = admitted_run(store, fence, run, head).await;
    end_run(store, fence, completing_admission(run, &admission)).await;
    admission
}

/// Prepare a store law's final envelope through real run admission and the
/// durable cancellation-closure slot. The law still supplies the head payload
/// and calls the store directly, so its transaction and replay assertions
/// exercise the backend under test.
#[expect(clippy::expect_used, reason = "store-law admission and closure setup")]
pub(crate) fn prepare_final_commit(
    store: &Arc<dyn crate::RuntimeStore>,
    mut commit: crate::RuntimeCommit,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::RuntimeCommit> + Send + '_>> {
    Box::pin(async move {
        let session = commit.session_id.clone();
        let turn = commit
            .turn_commit
            .operation
            .turn_id()
            .expect("final turn")
            .clone();
        let held = store
            .unfinished_run(&session)
            .await
            .expect("read admitted run");
        let run = commit
            .park_run
            .clone()
            .or_else(|| commit.run_terminal.as_ref().map(|end| end.run.clone()))
            .or_else(|| held.as_ref().map(|held| held.run.clone()))
            .unwrap_or_else(|| turn.clone());
        let fence = match commit.shift_fence.as_deref() {
            Some(fence) => fence.clone(),
            None => match held.as_ref() {
                Some(_) => crate::store::current_shift_fence(store.as_ref(), &session)
                    .await
                    .expect("read the admitted fence")
                    .expect("admitted fence"),
                None => {
                    lash_core::testing::store_fixtures::seal_shift_fence_for_test(
                        store,
                        &session,
                        run.as_str(),
                    )
                    .await
                }
            },
        };
        let head = match held {
            Some(held) => held.head,
            None => {
                let pending = store
                    .list_pending_turn_inputs(&session)
                    .await
                    .expect("read pending inputs");
                let input = match pending.into_iter().find(|read| {
                    matches!(read.input.ingress(), crate::TurnInputIngress::NextTurn)
                        && matches!(read.status, crate::PendingTurnInputReadStatus::Open)
                }) {
                    Some(read) => read.input.input_id,
                    None => {
                        store
                            .enqueue_pending_turn_input(
                                crate::PendingTurnInputDraft::new(
                                    &session,
                                    crate::TurnInputIngress::NextTurn,
                                    crate::TurnInput::text("store law's admitted turn"),
                                )
                                .with_source_key(run.as_str()),
                            )
                            .await
                            .expect("accept the store law's input")
                            .input_id
                    }
                };
                crate::store::AdmittedHead::Input(input)
            }
        };
        let mut request = admit_run_request_for_test(&fence, &run, head);
        let scope = crate::ExecutionScope::turn(&session, &turn);
        let binding = commit.interrupted_turn.as_ref().map_or_else(
            || "lash-conformance-final-turn".to_owned(),
            |closure| closure.settlement.authorization().binding_id().to_owned(),
        );
        let admitted_scope = commit.interrupted_turn.as_ref().map_or_else(
            || scope.clone(),
            |closure| closure.settlement.authorization().admitted_scope().clone(),
        );
        request.turn_cancellation = Some(crate::store::TurnCancellationBinding {
            binding_id: binding.clone(),
            admitted_scope: admitted_scope.clone(),
        });
        let admission = store
            .admit_run(&request)
            .await
            .expect("admit the final turn")
            .expect("the final turn reaches its head");
        commit.shift_fence = Some(Box::new(fence.clone()));
        commit.park_run = Some(run.clone());
        if commit.ingress.is_none() {
            commit.ingress = Some(if turn == run {
                completing_admission(run.as_str(), &admission)
            } else {
                // The switch completed its input rows; its follow-on retains the
                // same admission without settling those rows a second time.
                IngressSettlement::new(run.clone())
            });
        }
        if commit.pending_follow_on.is_none() && commit.run_terminal.is_none() {
            let mut end = run_completes(run.as_str());
            end.commit =
                TurnCommitId::of_physical_turn(&run, &turn).unwrap_or_else(|| end.commit.clone());
            end.turn = turn.clone();
            commit.run_terminal = Some(Box::new(end));
        }
        if commit.interrupted_turn.is_none() {
            let address = crate::TurnAddress::new(&session, &turn);
            let observed = store
                .turn_cancel_request_intent(&address)
                .await
                .expect("read cancel intent");
            assert_eq!(
                observed,
                crate::TurnCancelIntentSnapshot::Absent,
                "completion fixture carries no cancellation request"
            );
            let key = |wait, suffix: &str| crate::AwaitEventKey {
                scope: scope.clone(),
                wait,
                key_id: format!("{turn}:{suffix}"),
                signature: format!("conformance-final:{suffix}"),
            };
            let authorization = crate::TurnCancelClosureAuthorization::new(
                address,
                binding,
                admitted_scope,
                key(crate::AwaitEventWaitIdentity::TurnCancelGate, "cancel"),
                key(
                    crate::AwaitEventWaitIdentity::TurnCancelEscalation,
                    "escalation",
                ),
                key(crate::AwaitEventWaitIdentity::TurnTerminal, "terminal"),
                crate::TurnCancelClosureProposal::CompletionSealed,
                observed.clone(),
                &fence,
            )
            .expect("construct final closure");
            store
                .authorize_turn_cancel_closure(&fence, &authorization)
                .await
                .expect("retain the final closure");
            commit.interrupted_turn = Some(crate::store::InterruptedTurnClosure {
                settlement: crate::TurnCancelClosureSettlement::settled_for_test(
                    authorization,
                    None,
                    None,
                ),
                observed_intent: observed,
                admitted_intent: admission.cancel_intent.clone(),
            });
        }
        commit
    })
}
