//! A parked turn's record (FIG-3586, FIG-3600, FIG-3587, FIG-3659), at the
//! store seam.
//!
//! A turn parks when it aborts on a replay refusal: it keeps every claim it
//! holds, and the store records why, one record per session. The record lives
//! exactly while its turn does — a cancel that withdraws the parked turn's
//! held input clears it, as does any commit of the session. The first park
//! stamps `since_ms` and the durable `park_id`; a re-park of the same turn
//! keeps both and counts the refusal in `attempts`.

use super::*;

fn park(
    session_id: &SessionId,
    turn_id: &TurnId,
    reason: crate::store::ParkReason,
) -> crate::store::TurnParkWrite {
    crate::store::TurnParkWrite {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        reason,
        at_ms: 1_234,
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
    let divergence = store
        .record_turn_park(&park(
            &session_id,
            &parked_turn,
            crate::store::ParkReason::ReplayDivergence {
                message: "diverged at issue ordinal 3".to_string(),
            },
        ))
        .await
        .expect("record the park");
    release_session_execution_lease_for_test(&store, &lease).await;
    assert_eq!(
        divergence.since_ms, 1_234,
        "the first park stamps its refusal time"
    );
    assert_eq!(
        divergence.last_refused_ms, 1_234,
        "the first refusal is the most recent one"
    );
    assert_eq!(divergence.attempts, 1, "the first park counts one refusal");
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park"),
        Some(divergence.clone()),
        "the park reads back as recorded, and a released lease does not clear it"
    );

    // Parking the same turn again keeps the park's identity and first-park
    // time, counts the refusal, and updates the reason.
    let cutover = crate::store::ParkReason::RetiredGeneration {
        generation: Some(crate::ExecutableGeneration::new("blake3:old")),
        message: "grammar none".to_string(),
    };
    let reparked = store
        .record_turn_park(&crate::store::TurnParkWrite {
            at_ms: 4_567,
            ..park(&session_id, &parked_turn, cutover.clone())
        })
        .await
        .expect("re-park the turn");
    assert_eq!(
        reparked.park_id, divergence.park_id,
        "a same-turn re-park keeps the park identity"
    );
    assert_eq!(
        reparked.since_ms, divergence.since_ms,
        "a same-turn re-park keeps the first park's time"
    );
    assert_eq!(reparked.attempts, 2, "the re-park counts the refusal");
    assert_eq!(
        reparked.last_refused_ms, 4_567,
        "the re-park stamps the latest refusal"
    );
    assert_eq!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the replaced park"),
        Some(reparked.clone())
    );

    // Every reason round-trips through the persisted record, the FIG-3587
    // binding drift and effect replay divergence with their fields.
    for reason in [
        crate::store::ParkReason::BindingDrift {
            message: "code cell binding `tools.probe` (tool `tool:probe`) is missing".to_string(),
        },
        crate::store::ParkReason::EffectReplayDivergence {
            effect_kind: "llm_call".to_string(),
            message: "divergent_paths=[command.request.generation]".to_string(),
        },
        cutover.clone(),
    ] {
        let parked = store
            .record_turn_park(&park(&session_id, &parked_turn, reason))
            .await
            .expect("park the turn under the reason");
        assert_eq!(
            store
                .load_turn_park(&session_id)
                .await
                .expect("read the reason back"),
            Some(parked),
            "the persisted park reads back with its reason"
        );
    }

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
    let cutover = store
        .record_turn_park(&park(
            &session_id,
            &parked_turn,
            crate::store::ParkReason::EffectReplayDivergence {
                effect_kind: "llm_call".to_string(),
                message: "the model call's envelope drifted".to_string(),
            },
        ))
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
