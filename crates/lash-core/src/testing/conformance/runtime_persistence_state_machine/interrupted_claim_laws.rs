use super::*;

pub(super) async fn stale_settlement_cannot_damage_successor(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_queued_work(queued_draft(0, 0, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second = store
        .enqueue_queued_work(queued_draft(1, 1, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let stale_owner = owner(0);
    let stale_lease = store
        .try_claim_session_execution_lease(SESSION_ID, &stale_owner, 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("stale-owner lease busy"))?;
    let stale_claim = store
        .claim_ready_queued_work(
            SESSION_ID,
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(4),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("coalesced work absent"))?;
    store
        .release_session_execution_lease(&stale_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let successor_owner = owner(1);
    let successor_lease = store
        .try_claim_session_execution_lease(SESSION_ID, &successor_owner, 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;
    let before_partial_selection = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let partial_selection = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await;
    prop_assert!(
        matches!(
            &partial_selection,
            Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids,
            }) if required_batch_ids == &[first.batch_id.clone(), second.batch_id.clone()]
        ),
        "partial selection did not return the literal interrupted composition: {partial_selection:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before_partial_selection,
        "partial interrupted-composition selected claim",
    )
    .await
    .map_err(TestCaseError::fail)?;
    let successor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &[first.batch_id.clone(), second.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("successor did not reclaim full composition"))?;

    let mut stale_completion = stale_claim.completion();
    stale_completion.batch_ids = vec![second.batch_id.clone()];
    let mut state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(ToolState::default().with_generation(51)));
    let before_stale_completion = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let stale_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(stale_lease.completion())
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
        .list_queued_work(SESSION_ID)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(remaining.len(), 2);
    prop_assert!(
        store
            .list_pending_queued_work(SESSION_ID)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "stale subset settlement disturbed successor full-composition claim ownership"
    );
    let third_owner = owner(2);
    prop_assert!(
        matches!(
            store
                .try_claim_session_execution_lease(SESSION_ID, &third_owner, 60_000)
                .await
                .map_err(|error| TestCaseError::fail(error.to_string()))?,
            SessionExecutionLeaseClaimOutcome::Busy { .. }
        ),
        "stale completion released the successor session lease"
    );
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(successor_lease.completion())
                .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        store
            .list_queued_work(SESSION_ID)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "successor could not settle its preserved claim"
    );
    Ok(())
}
