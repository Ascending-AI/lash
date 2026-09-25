//! The pending follow-on on the session head (ADR 0101 §3, FIG-3542), as
//! store laws: a frame switch's commit writes it, only its own terminal commit
//! clears it, every other claim and turn commit meets it, and its frame is the
//! head's current frame on every head write.

use super::*;
use pretty_assertions::assert_eq;

const SESSION: &str = "follow-on";
const SWITCHING_TURN: &str = "switching-turn";
const FOLLOW_ON_TURN: &str = "switching-turn:agent-frame:1";

fn session() -> SessionId {
    SessionId::from(SESSION)
}

/// A turn's terminal commit over `state`: the one commit that may write or
/// clear a pending follow-on.
fn terminal_commit(
    state: &RuntimeSessionState,
    turn_id: &str,
    pending_follow_on: Option<crate::store::PendingFollowOn>,
) -> RuntimeCommit {
    commit_as(
        state,
        crate::OperationId::turn(SESSION, turn_id, "final"),
        pending_follow_on,
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a derivable graph is established by the setup"
)]
fn commit_as(
    state: &RuntimeSessionState,
    operation: crate::OperationId,
    pending_follow_on: Option<crate::store::PendingFollowOn>,
) -> RuntimeCommit {
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&state.session_id, &operation)
        .expect("derive commit node ids");
    let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        state,
        graph,
        &[],
        operation,
    )
    .expect("build the commit");
    commit.pending_follow_on = pending_follow_on;
    commit
}

fn follow_on(frame_id: crate::FrameNodeId) -> crate::store::PendingFollowOn {
    crate::store::PendingFollowOn {
        follow_on_turn_id: TurnId::from(FOLLOW_ON_TURN),
        frame_id,
        task: "run in the switched frame".to_string(),
        options: None,
        chain_depth: 1,
        attempts: 0,
    }
}

/// Commit the switch and return the head it left, owing `FOLLOW_ON_TURN`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_switch(
    store: &Arc<dyn RuntimePersistence>,
) -> (RuntimeSessionState, crate::store::PendingFollowOn) {
    let mut state = RuntimeSessionState {
        session_id: session(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let owed = follow_on(
        state
            .current_frame_node_id
            .clone()
            .expect("the initial frame is current"),
    );
    store
        .commit_runtime_state(terminal_commit(&state, SWITCHING_TURN, Some(owed.clone())))
        .await
        .expect("the switch commit writes its follow-on");
    (loaded_conformance_state(store).await, owed)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_follow_on_is_written_by_its_switch_and_cleared_by_its_terminal(
    store: Arc<dyn RuntimePersistence>,
) {
    let (state, owed) = commit_switch(&store).await;
    assert_eq!(state.pending_follow_on.as_deref(), Some(&owed));
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("load head meta")
            .expect("head")
            .pending_follow_on
            .as_ref(),
        Some(&owed),
        "the fact is its own head column, read with the head"
    );

    // Its own terminal commit clears it, whatever the outcome.
    let receipt = store
        .commit_runtime_state(terminal_commit(&state, FOLLOW_ON_TURN, None))
        .await
        .expect("the follow-on's terminal commit clears its fact");
    assert_eq!(receipt.pending_follow_on, None);
    assert_eq!(
        loaded_conformance_state(&store).await.pending_follow_on,
        None
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_follow_on_blocks_every_claim_but_its_own(store: Arc<dyn RuntimePersistence>) {
    let (_, owed) = commit_switch(&store).await;
    store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session(), "host input"))
        .await
        .expect("enqueue host input");
    let wake = store
        .enqueue_queued_work(queued_draft(
            &session(),
            "wake behind the follow-on",
            DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("enqueue a wake");
    store
        .enqueue_queued_work(checkpoint_claims::queued_session_command_draft(
            &session(),
            "command behind the follow-on",
        ))
        .await
        .expect("enqueue a session command");
    let lease = claim_session_execution_lease_for_test(&store, &session(), "follow-on-owner").await;
    let owner = lease_owner("follow-on-owner");

    assert!(
        store
            .claim_next_turn_inputs(&session(), &lease.fence(), &owner, 8)
            .await
            .expect("idle input claim")
            .is_none(),
        "an idle input claim is blocked, not an error"
    );
    assert!(matches!(
        store
            .claim_ready_queued_work(
                &session(),
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                crate::testing::queued_work_claim_policy(8),
            )
            .await
            .expect("idle queued claim"),
        crate::QueuedWorkClaimOutcome::Refused(crate::QueuedWorkClaimRefusal::FollowOnPending)
    ));
    assert!(
        store
            .claim_leading_ready_session_command(&session(), &lease.fence(), &owner)
            .await
            .expect("command claim")
            .is_none(),
        "a session command waits behind the follow-on"
    );
    let other_turn = store
        .claim_checkpoint_work(
            &session(),
            &lease.fence(),
            &owner,
            &TurnId::from("another-turn"),
            crate::CheckpointKind::AfterWork,
            8,
            crate::testing::queued_work_claim_policy(8),
        )
        .await
        .expect("another turn's checkpoint claim");
    assert!(other_turn.0.is_none() && other_turn.1.is_none());

    // The follow-on's own checkpoint claims.
    let own = store
        .claim_checkpoint_work(
            &session(),
            &lease.fence(),
            &owner,
            &owed.follow_on_turn_id,
            crate::CheckpointKind::AfterWork,
            8,
            crate::testing::queued_work_claim_policy(8),
        )
        .await
        .expect("the follow-on's checkpoint claim");
    assert_eq!(
        own.1
            .expect("the follow-on claims the wake at its checkpoint")
            .batches[0]
            .batch_id,
        wake.batch_id
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_follow_on_refuses_every_other_commit_that_would_drop_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let (state, owed) = commit_switch(&store).await;

    // Another turn's terminal commit is refused, whatever it carries.
    for carried in [None, Some(owed.clone())] {
        let error = store
            .commit_runtime_state(terminal_commit(&state, "another-turn", carried))
            .await
            .expect_err("another turn's commit is refused while a follow-on is owed");
        assert!(matches!(
            error,
            StoreError::FollowOnPending { ref follow_on_turn_id, attempts: 0, .. }
                if *follow_on_turn_id == owed.follow_on_turn_id
        ));
    }
    // A head write that is no turn's terminal must carry the fact unchanged.
    let operation = crate::OperationId::new(
        crate::ExecutionScope::runtime_operation("follow-on-side-write"),
        "record-config",
    );
    let error = store
        .commit_runtime_state(commit_as(&state, operation.clone(), None))
        .await
        .expect_err("a side write may not drop the fact");
    assert!(matches!(error, StoreError::FollowOnPending { .. }));
    store
        .commit_runtime_state(commit_as(&state, operation, Some(owed.clone())))
        .await
        .expect("a side write carrying the fact unchanged commits");
    assert_eq!(
        loaded_conformance_state(&store).await.pending_follow_on,
        Some(Box::new(owed)),
        "the refused writes changed nothing"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_follow_on_frame_is_current_on_every_head_write(
    store: Arc<dyn RuntimePersistence>,
) {
    let mut state = RuntimeSessionState {
        session_id: session(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    let elsewhere = follow_on(crate::session_graph::frame_node_id(&session(), "elsewhere"));
    let error = store
        .commit_runtime_state(terminal_commit(&state, SWITCHING_TURN, Some(elsewhere)))
        .await
        .expect_err("a follow-on off the committed frame is unrepresentable");
    assert!(matches!(error, StoreError::FollowOnFrameNotCurrent { .. }));
    assert!(
        store
            .load_session()
            .await
            .expect("load after refused switch")
            .is_none(),
        "the refused switch wrote nothing"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pending_follow_on_recovery_raise_is_fenced_and_never_resets(
    store: Arc<dyn RuntimePersistence>,
) {
    let (state, owed) = commit_switch(&store).await;
    let revision = state.head_revision;
    let lease = claim_session_execution_lease_for_test(&store, &session(), "recovering").await;
    for expected in 1..=2 {
        let raised = store
            .raise_pending_follow_on_attempts(&lease.authority(), &owed.follow_on_turn_id)
            .await
            .expect("a recovering drive raises the count");
        assert_eq!(raised.attempts, expected);
        let head = store
            .load_session_head_meta()
            .await
            .expect("load head meta")
            .expect("head");
        assert_eq!(head.pending_follow_on, Some(raised));
        assert_eq!(head.head_revision, revision, "the raise moves no revision");
    }
    assert!(matches!(
        store
            .raise_pending_follow_on_attempts(&lease.authority(), &TurnId::from("not-owed"))
            .await,
        Err(StoreError::FollowOnNotPending { .. })
    ));
    store
        .release_session_execution_lease(&lease.authority())
        .await
        .expect("release the recovering lane");
    assert!(
        store
            .raise_pending_follow_on_attempts(&lease.authority(), &owed.follow_on_turn_id)
            .await
            .is_err(),
        "a raise outside the live lane is refused"
    );

    // The raised count is part of the fact: a side write must carry it.
    let mut stale = loaded_conformance_state(&store).await;
    stale.pending_follow_on = Some(Box::new(owed.clone()));
    assert!(matches!(
        store
            .commit_runtime_state(commit_as(
                &stale,
                crate::OperationId::new(
                    crate::ExecutionScope::runtime_operation("follow-on-stale-write"),
                    "record-config",
                ),
                Some(owed.clone()),
            ))
            .await,
        Err(StoreError::FollowOnPending { attempts: 2, .. })
    ));
    // Its terminal clears the fact, and with it the count.
    store
        .commit_runtime_state(terminal_commit(&stale, FOLLOW_ON_TURN, None))
        .await
        .expect("the follow-on's terminal clears its fact");
    let successor = claim_session_execution_lease_for_test(&store, &session(), "after").await;
    assert!(matches!(
        store
            .raise_pending_follow_on_attempts(&successor.authority(), &owed.follow_on_turn_id)
            .await,
        Err(StoreError::FollowOnNotPending { .. })
    ));
}
