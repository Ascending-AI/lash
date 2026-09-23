//! A turn-input claim bound to the aborted direct turn that held it
//! (FIG-3589, ADR 0069 §7), at the store seam.
//!
//! The runtime binds a direct turn's drive claim when the turn aborts with
//! `Err`. From then on the rows stop lapsing with the claim's lease
//! generation: no claim under any generation takes them, and they are not
//! claimable work, until the aborted turn's redrive re-takes them or a cancel
//! of one of them returns the rest to the queue.

use super::*;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn statuses(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
) -> Vec<(crate::InputId, crate::PendingTurnInputReadStatus)> {
    store
        .list_pending_turn_inputs(session_id)
        .await
        .expect("list pending inputs")
        .into_iter()
        .map(|read| (read.input.input_id, read.status))
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn bound_turn_input_claim_is_excluded_until_its_turn_retakes_it(
    store: Arc<dyn RuntimePersistence>,
) {
    let session_id = SessionId::from("turn-bound-claims");
    let aborted_turn = TurnId::from("aborted-direct-turn");
    let first = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "first"))
        .await
        .expect("enqueue the first input");
    let second = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "second"))
        .await
        .expect("enqueue the second input");

    let aborted_lease =
        claim_session_execution_lease_for_test(&store, &session_id, "aborted-owner").await;
    let drive = store
        .claim_next_turn_inputs(
            &session_id,
            &aborted_lease.fence(),
            &lease_owner("aborted-owner"),
            1,
        )
        .await
        .expect("claim the aborted turn's drive")
        .expect("the first input is claimable");
    assert_eq!(drive.inputs.len(), 1);
    store
        .bind_turn_input_claim(&drive, &aborted_turn)
        .await
        .expect("bind the drive claim to the aborted turn");
    release_session_execution_lease_for_test(&store, &aborted_lease).await;
    let bound = crate::PendingTurnInputReadStatus::TurnBound {
        turn_id: aborted_turn.clone(),
    };
    assert_eq!(
        statuses(&store, &session_id).await,
        vec![
            (first.input_id.clone(), bound.clone()),
            (
                second.input_id.clone(),
                crate::PendingTurnInputReadStatus::Pending
            ),
        ],
        "a released lease does not free a bound claim"
    );

    // A successor generation takes only the unbound row, and another turn's
    // redrive takes nothing.
    let successor =
        claim_session_execution_lease_for_test(&store, &session_id, "successor-owner").await;
    let successor_claim = store
        .claim_next_turn_inputs(
            &session_id,
            &successor.fence(),
            &lease_owner("successor-owner"),
            10,
        )
        .await
        .expect("claim under the successor generation")
        .expect("the unbound input is claimable");
    assert_eq!(
        successor_claim
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![second.input_id.clone()],
        "no generation reclaims a bound row"
    );
    assert!(
        store
            .reclaim_turn_bound_inputs(
                &session_id,
                &successor.fence(),
                &lease_owner("successor-owner"),
                &TurnId::from("some-other-turn"),
            )
            .await
            .expect("another turn's redrive probe")
            .is_none(),
        "only the bound turn's redrive re-takes its rows"
    );
    store
        .abandon_turn_input_claim(&successor_claim)
        .await
        .expect("hand the successor's claim back");

    // The aborted turn's redrive re-takes exactly its bound rows under its own
    // generation, and the binding is gone.
    let redrive = store
        .reclaim_turn_bound_inputs(
            &session_id,
            &successor.fence(),
            &lease_owner("successor-owner"),
            &aborted_turn,
        )
        .await
        .expect("the aborted turn's redrive re-takes its rows")
        .expect("the bound row is re-taken");
    assert_eq!(
        redrive
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>(),
        vec![first.input_id.clone()]
    );
    assert!(
        matches!(
            statuses(&store, &session_id).await[0].1,
            crate::PendingTurnInputReadStatus::Held { .. }
        ),
        "the re-taken row is held by the redrive's live generation"
    );

    // Binding a claim that no longer holds the row is a no-op.
    store
        .bind_turn_input_claim(&drive, &aborted_turn)
        .await
        .expect("bind a superseded claim");
    assert!(
        matches!(
            statuses(&store, &session_id).await[0].1,
            crate::PendingTurnInputReadStatus::Held { .. }
        ),
        "a superseded claim binds nothing"
    );
    release_session_execution_lease_for_test(&store, &successor).await;

    // Cancelling a row of a bound claim returns the claim's other rows to the
    // queue, unbound.
    let third_lease =
        claim_session_execution_lease_for_test(&store, &session_id, "third-owner").await;
    let absorbing = store
        .claim_next_turn_inputs(
            &session_id,
            &third_lease.fence(),
            &lease_owner("third-owner"),
            10,
        )
        .await
        .expect("claim both rows")
        .expect("both rows are claimable");
    assert_eq!(absorbing.inputs.len(), 2);
    store
        .bind_turn_input_claim(&absorbing, &aborted_turn)
        .await
        .expect("bind the absorbing claim");
    release_session_execution_lease_for_test(&store, &third_lease).await;
    assert!(
        store
            .cancel_pending_turn_input(&session_id, &second.input_id)
            .await
            .expect("cancel one bound row")
            .is_cancelled(),
        "a bound row is cancellable by its id"
    );
    assert_eq!(
        statuses(&store, &session_id).await,
        vec![(
            first.input_id.clone(),
            crate::PendingTurnInputReadStatus::Pending
        )],
        "the cancel returns the bound claim's other row to the queue"
    );
}
