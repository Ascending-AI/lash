//! A parked turn's record (FIG-3586, FIG-3600), at the store seam.
//!
//! A turn parks when it aborts on a replay refusal: it keeps every claim it
//! holds, and the store records why, one record per session. The record lives
//! exactly while its turn does — a cancel that withdraws the parked turn's
//! held input clears it, as does any commit of the session.

use super::*;

fn park(
    session_id: &SessionId,
    turn_id: &TurnId,
    reason: crate::store::TurnParkReason,
) -> crate::store::TurnPark {
    crate::store::TurnPark {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        reason,
        parked_at_ms: 1_234,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn turn_park_lives_while_its_turn_holds_work(store: Arc<dyn RuntimePersistence>) {
    let session_id = SessionId::from("turn-parks");
    let parked_turn = TurnId::from("parked-direct-turn");
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the absent park"),
        None
    );

    // The aborted turn holds its drive claim, bound to it, and parks.
    let input = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(&session_id, "parked"))
        .await
        .expect("enqueue the parked turn's input");
    let lease = claim_session_execution_lease_for_test(&store, &session_id, "parking-owner").await;
    let drive = store
        .claim_next_turn_inputs(
            &session_id,
            &lease.fence(),
            &lease_owner("parking-owner"),
            1,
        )
        .await
        .expect("claim the drive")
        .expect("the input is claimable");
    store
        .bind_turn_input_claim(&drive, &parked_turn, &input.input_id)
        .await
        .expect("bind the drive claim to the aborted turn");
    let divergence = park(
        &session_id,
        &parked_turn,
        crate::store::TurnParkReason::ReplayDivergence {
            message: "diverged at issue ordinal 3".to_string(),
        },
    );
    store
        .record_turn_park(&divergence)
        .await
        .expect("record the park");
    release_session_execution_lease_for_test(&store, &lease).await;
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park"),
        Some(divergence),
        "the park reads back as recorded, and a released lease does not clear it"
    );

    // Parking again replaces the record: one per session.
    let cutover = park(
        &session_id,
        &parked_turn,
        crate::store::TurnParkReason::KeyFormatCutover {
            message: "grammar none".to_string(),
        },
    );
    store
        .record_turn_park(&cutover)
        .await
        .expect("re-park the turn");
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the replaced park"),
        Some(cutover.clone())
    );

    // A cancel that withdraws the parked turn's input settles its park.
    let cancelled = store
        .cancel_pending_turn_input(&session_id, &input.input_id)
        .await
        .expect("cancel the receipt's input");
    assert!(
        matches!(
            cancelled,
            crate::PendingTurnInputCancelOutcome::Cancelled(_)
        ),
        "the receipt's input cancels: {cancelled:?}"
    );
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the cleared park"),
        None,
        "a turn that holds no work any more is not parked"
    );

    // Another turn's commit leaves the park; the parked turn's own commit
    // settles it, in its own transaction.
    store
        .record_turn_park(&cutover)
        .await
        .expect("park the turn again");
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let other = RuntimeCommit::persisted_state_with_operation_for_testing(
        &state,
        &[],
        crate::store::OperationId::turn(session_id.clone(), TurnId::from("another-turn"), "final"),
    );
    commit_runtime_state_for_test(&store, other, "other-owner")
        .await
        .expect("commit another turn");
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park after another turn's commit"),
        Some(cutover),
        "another turn's commit does not settle the parked turn"
    );
    let after_other = RuntimeSessionState {
        head_revision: state.head_revision + 1,
        ..state.clone()
    };
    let own = RuntimeCommit::persisted_state_with_operation_for_testing(
        &after_other,
        &[],
        crate::store::OperationId::turn(session_id.clone(), parked_turn.clone(), "final"),
    );
    commit_runtime_state_for_test(&store, own, "settling-owner")
        .await
        .expect("commit the parked turn");
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park after the commit"),
        None,
        "the parked turn's commit settles it"
    );
}
