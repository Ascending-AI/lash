//! Exact closure receipts, successor fences and authorization consumption.

use super::*;
use pretty_assertions::assert_eq;

/// ADR 0105 §9: a stored commit's exact replay under a superseded fence
/// answers its receipt without writing, including a retained copy of its own
/// settled cancellation closure. `snapshot` serializes every durable table.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_stale_fence_receipt_replay_leaves_the_store_byte_identical<F, Fut>(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
    snapshot: F,
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    let request = session_store_request(
        &SessionId::from("stale-fence-receipt-no-writes"),
        "stale-fence-receipt-model",
        crate::SessionRelation::Root,
    );
    let store = factory.admit_view(&request).await.expect("create store");
    let address = crate::TurnAddress::new(&request.session_id, TurnId::from("settled-turn"));
    let first = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("receipt-first", "receipt-first:1"),
            "receipt-first:executor",
            60_000,
        )
        .await
        .expect("seal the original shift")
        .acquired()
        .expect("the original shift is free");
    let authorization = authorize_closure(
        store.store(),
        &first,
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state)
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &address.turn_id,
            "final",
        ))
        .expect("stamp the final commit");
    commit.shift_fence = Some(Box::new(first.clone()));
    commit.interrupted_turn = Some(crate::store::InterruptedTurnClosure {
        settlement: settled_closure(&authorization, None),
        observed_intent: crate::TurnCancelIntentSnapshot::Absent,
        admitted_intent: None,
    });
    let commit = crate::conformance::prepare_final_commit(store.store(), commit).await;
    let receipt = store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit and settle the original closure");
    assert!(!receipt.receipt_replayed);
    assert!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read the settled closure slot")
            .is_empty()
    );

    // A duplicate authorization can arrive after its commit consumed the
    // slot. Keep that exact artifact present so a replay's cleanup is an
    // observable write rather than a DELETE that happens to match no rows.
    assert_eq!(
        store
            .authorize_turn_cancel_closure(&first, &authorization)
            .await
            .expect("retain the duplicate settled authorization"),
        crate::TurnCancelClosureAuthorizationOutcome::Authorized
    );
    store
        .store()
        .supersede_shift_epoch_for_test(&first)
        .await
        .expect("supersede the original shift");
    let successor = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("receipt-successor", "receipt-successor:1"),
            "receipt-successor:executor",
            60_000,
        )
        .await
        .expect("seal a successor shift")
        .acquired()
        .expect("the successor shift is free");
    assert!(successor.epoch() > first.epoch());
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read the retained closure before replay"),
        vec![authorization.clone()]
    );
    let before = snapshot().await;
    assert!(!before.is_empty(), "the snapshot must cover durable tables");
    let replay = store
        .commit_runtime_state(commit)
        .await
        .expect("the stale fence's exact replay answers its receipt");
    assert!(replay.receipt_replayed);
    assert_eq!(replay.head_revision, receipt.head_revision);
    assert_eq!(replay.checkpoint_ref, receipt.checkpoint_ref);
    let after = snapshot().await;
    assert_eq!(after.len(), before.len(), "replay changed the table set");
    for ((table, before), (after_table, after)) in before.into_iter().zip(after) {
        assert_eq!(after_table, table, "replay changed the table set");
        assert_eq!(after, before, "stale-fence receipt replay wrote to {table}");
    }
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read the retained closure after replay"),
        vec![authorization]
    );
}

/// Replaying an already committed receipt must not consume a newer pending
/// authorization that happens to reuse the same turn address.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(in crate::conformance) async fn turn_cancel_exact_replay_preserves_different_pending_authorization(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let request = session_store_request(
        &SessionId::from("turn-cancel-exact-replay-preserves-new-authorization"),
        "turn-cancel-exact-replay-model",
        crate::SessionRelation::Root,
    );
    let store = factory.admit_view(&request).await.expect("create store");
    let address = crate::TurnAddress::new(
        &request.session_id,
        TurnId::from("turn-cancel-exact-replay:turn"),
    );
    let first = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque("exact-replay-first", "exact-replay-first:1"),
            "exact-replay-first:executor",
            60_000,
        )
        .await
        .expect("claim first exact-replay lane")
        .acquired()
        .expect("first exact-replay lane is free");
    let first_authorization = authorize_closure(
        store.store(),
        &first,
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    let mut state = crate::RuntimeSessionState {
        session_id: request.session_id.clone(),
        ..crate::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state)
        .with_operation(crate::OperationId::turn(
            &request.session_id,
            &address.turn_id,
            "final",
        ))
        .expect("stamp exact replay operation");
    commit.interrupted_turn = Some(crate::store::InterruptedTurnClosure {
        settlement: settled_closure(&first_authorization, None),
        observed_intent: crate::TurnCancelIntentSnapshot::Absent,
        admitted_intent: None,
    });
    commit.shift_fence = Some(Box::new(first.clone()));
    let commit = crate::conformance::prepare_final_commit(store.store(), commit).await;
    store
        .commit_runtime_state(commit.clone())
        .await
        .expect("commit first authorization");

    let successor = store
        .store()
        .seal_shift_epoch_for_test(
            &request.session_id,
            &crate::LeaseOwnerIdentity::opaque(
                "exact-replay-successor",
                "exact-replay-successor:1",
            ),
            "exact-replay-successor:executor",
            60_000,
        )
        .await
        .expect("claim successor exact-replay lane")
        .acquired()
        .expect("successor exact-replay lane is free");
    let successor_authorization = authorize_closure(
        store.store(),
        &successor,
        &address,
        crate::TurnCancelIntentSnapshot::Absent,
        crate::TurnCancelClosureProposal::CompletionSealed,
    )
    .await;
    assert_ne!(successor_authorization, first_authorization);
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read successor authorization before replay"),
        vec![successor_authorization.clone()]
    );

    store
        .commit_runtime_state(commit)
        .await
        .expect("adopt exact committed receipt");
    assert_eq!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read successor authorization after replay"),
        vec![successor_authorization],
        "exact receipt replay must retain a different pending closure authorization"
    );
    assert_eq!(
        store
            .load_session_head_meta()
            .await
            .expect("read head after exact replay")
            .expect("committed head exists")
            .head_revision,
        1,
        "exact replay must not publish another head"
    );
}
