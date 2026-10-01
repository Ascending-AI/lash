//! Shared fixtures for laws that admit rows to a root and settle them
//! (FIG-3927): a row is open until a root's admission binds it, and only
//! that root's fenced commit (or its terminal) answers it.

use super::*;
use lash_core::store::{
    AdmittedHead, CheckpointAdmission, DriveFence, IngressRowId, IngressSettlement, RootAdmission,
    RootTerminalWrite, TurnCommitId,
};
pub(crate) use lash_core::testing::store_fixtures::{
    admit_at_checkpoint_for_test, admit_root_for_test, admit_root_request_for_test,
    settling_commit_for_test,
};

/// Admit `root` headed by `head` under `fence`; the admission must reach
/// its head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the admission is established by the setup"
)]
pub(crate) async fn admitted_root(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &DriveFence,
    root: &str,
    head: AdmittedHead,
) -> RootAdmission {
    admit_root_for_test(store, fence, &crate::TurnId::from(root), head)
        .await
        .expect("admit the root")
        .expect("the root's admission reaches its head")
}

/// Start `root` running under `fence`: enqueue a next-turn head input filed
/// under the root's own id and admit the root with it. Input may address a
/// turn only while that turn runs or once it has ended (ADR 0101 §5.1), so a
/// law that addresses `root`'s turns starts it first, before it enqueues the
/// rows it composes, and the root takes only its own head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the head and its admission are established by the setup"
)]
pub(crate) async fn running_root(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &DriveFence,
    root: &crate::TurnId,
) -> RootAdmission {
    let head = store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                fence.session(),
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text(format!("{root} head")),
            )
            .with_source_key(root.as_str()),
        )
        .await
        .expect("enqueue the running root's head");
    admitted_root(
        store,
        fence,
        root.as_str(),
        AdmittedHead::Input(head.input_id),
    )
    .await
}

/// [`running_root`] headed by a process wake instead of an input, for a law
/// whose assertions read the session's pending inputs: the root's own head
/// is then no pending input.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the head and its admission are established by the setup"
)]
pub(crate) async fn running_root_on_wake(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &crate::SessionId,
    root: &crate::TurnId,
) -> RootAdmission {
    let fence = lash_core::testing::store_fixtures::seal_drive_fence_for_test(
        store,
        session_id,
        root.as_str(),
    )
    .await;
    let head = store
        .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
            session_id,
            &format!("{root}-starter"),
            1,
            "start the root",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue the running root's head");
    admitted_root(
        store,
        &fence,
        root.as_str(),
        AdmittedHead::Batch(head.batch_id),
    )
    .await
}

/// Admit `root` headed by `head` under `fence`, composing its prefix with
/// `policy`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the admission is established by the setup"
)]
pub(crate) async fn admitted_root_with_policy(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &DriveFence,
    root: &str,
    head: AdmittedHead,
    policy: crate::TurnLaneAdmissionPolicy,
) -> RootAdmission {
    let mut request = admit_root_request_for_test(fence, &crate::TurnId::from(root), head);
    request.policy = policy;
    store
        .admit_root(&request)
        .await
        .expect("admit the root")
        .expect("the root's admission reaches its head")
}

/// The settlement completing every row `admission` bound to `root`.
pub(crate) fn completing_admission(root: &str, admission: &RootAdmission) -> IngressSettlement {
    let mut settlement = IngressSettlement::new(crate::TurnId::from(root));
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

/// A settlement of `root` that hands `rows` back open at their positions.
pub(crate) fn releasing(
    root: &str,
    rows: impl IntoIterator<Item = IngressRowId>,
) -> IngressSettlement {
    let mut settlement = IngressSettlement::new(crate::TurnId::from(root));
    settlement.released.extend(rows);
    settlement
}

/// The terminal write of `root`'s first physical turn completing it.
pub(crate) fn root_completes(root: &str) -> RootTerminalWrite {
    let root = crate::TurnId::from(root);
    RootTerminalWrite {
        commit: TurnCommitId::new(root.clone(), 0),
        turn: lash_core::store::PhysicalTurn::derive_turn_id(&root, 0),
        root,
        outcome: crate::store::RootCommittedOutcome::Finished(
            lash_core::facade_support::TurnFinish::AssistantMessage {
                text: String::new(),
            },
        ),
    }
}

/// `commit` settling `settlement` under `fence` and ending its root: the
/// shape of a root's final commit.
pub(crate) fn final_commit(
    commit: crate::RuntimeCommit,
    fence: &DriveFence,
    settlement: IngressSettlement,
) -> crate::RuntimeCommit {
    let terminal = root_completes(settlement.root.as_str());
    let mut commit = settling_commit_for_test(commit, fence, settlement);
    commit.root_terminal = Some(Box::new(terminal));
    commit
}

/// `commit` applying the command rows `completion` names under `fence`: the
/// command lane's bindless settlement (design §2.7).
pub(crate) fn applying_commands(
    mut commit: crate::RuntimeCommit,
    fence: &DriveFence,
    completion: crate::QueuedWorkCompletion,
) -> crate::RuntimeCommit {
    commit.drive_fence = Some(Box::new(fence.clone()));
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
/// settling or root-ending commit is built on.
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

/// Land `root`'s final commit settling `settlement` under `fence`, over the
/// current head.
pub(crate) async fn try_end_root(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &DriveFence,
    settlement: IngressSettlement,
) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
    let commit = head_commit(store, fence.session()).await;
    store
        .commit_runtime_state(final_commit(commit, fence, settlement))
        .await
}

/// [`try_end_root`], which must land.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the final commit is established by the setup"
)]
pub(crate) async fn end_root(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &DriveFence,
    settlement: IngressSettlement,
) -> crate::store::RuntimeCommitReceipt {
    try_end_root(store, fence, settlement)
        .await
        .expect("the root's final commit lands")
}

/// Admit `root` headed by `head` and end it completing everything it took;
/// returns the admission.
pub(crate) async fn drive_root_to_end(
    store: &Arc<dyn crate::RuntimeStore>,
    fence: &DriveFence,
    root: &str,
    head: AdmittedHead,
) -> RootAdmission {
    let admission = admitted_root(store, fence, root, head).await;
    end_root(store, fence, completing_admission(root, &admission)).await;
    admission
}
