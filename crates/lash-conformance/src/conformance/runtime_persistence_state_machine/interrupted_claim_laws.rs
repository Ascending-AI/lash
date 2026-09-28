use super::*;
use lash_core::testing::conformance_support::ToolStateConformanceAccess;

pub(super) async fn stale_settlement_cannot_damage_successor(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    store
        .enqueue_queued_work(queued_draft(0, 0, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second = store
        .enqueue_queued_work(queued_draft(1, 1, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let stale_owner = owner(0);
    let stale_lease = store
        .seal_claim_epoch_for_test(
            &SessionId::from(SESSION_ID),
            &stale_owner,
            "law-stale-settlement-cannot-damage-successor-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("stale-owner lease busy"))?;
    let stale_claim = store
        .claim_ready_queued_work(
            &SessionId::from(SESSION_ID),
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(4),
        )
        .await
        .map(crate::QueuedWorkClaimOutcome::claim)
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("coalesced work absent"))?;
    store
        .supersede_claim_epoch_for_test(&stale_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let successor_owner = owner(1);
    let successor_lease = store
        .seal_claim_epoch_for_test(
            &SessionId::from(SESSION_ID),
            &successor_owner,
            "law-stale-settlement-cannot-damage-successor-executor-2",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;
    let successor_claim = store
        .claim_ready_queued_work(
            &SessionId::from(SESSION_ID),
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map(crate::QueuedWorkClaimOutcome::claim)
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("successor did not reclaim full composition"))?;

    let mut stale_completion = stale_claim.completion();
    stale_completion.batch_ids = vec![second.batch_id.clone()];
    let mut state = RuntimeSessionState {
        session_id: SessionId::from(SESSION_ID.to_string()),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(
        ToolState::default().with_generation_for_conformance(51),
    ));
    let before_stale_completion = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let stale_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(stale_completion),
        )
        .await;
    prop_assert!(
        matches!(
            stale_result,
            Err(StoreError::QueuedWorkClaimSuperseded { .. })
        ),
        "stale subset settlement was not superseded: {stale_result:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before_stale_completion,
        "stale subset settlement after full-composition reclaim",
    )
    .await
    .map_err(TestCaseError::fail)?;
    let remaining = store
        .list_queued_work(&SessionId::from(SESSION_ID))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(remaining.len(), 2);
    prop_assert!(
        store
            .list_pending_queued_work(&SessionId::from(SESSION_ID))
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "stale subset settlement disturbed successor full-composition claim ownership"
    );
    let third_owner = owner(2);
    prop_assert!(
        store
            .seal_claim_epoch_for_test(
                &SessionId::from(SESSION_ID),
                &third_owner,
                "law-stale-settlement-cannot-damage-successor-executor-3",
                60_000,
            )
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .acquired()
            .is_some(),
        "stale completion must not prevent a successor drive seal"
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        store
            .list_queued_work(&SessionId::from(SESSION_ID))
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "successor could not settle its preserved claim"
    );
    Ok(())
}
