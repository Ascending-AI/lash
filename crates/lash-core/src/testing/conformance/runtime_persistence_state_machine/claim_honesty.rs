//! Executable documentation for the current pre-reclaim settlement shape.
//! The paired reclaim-mediated LAW remains in the parent state-machine module.

use super::*;

/// NON-LAW: demonstrates current backend behavior before successor re-claim;
/// acceptance is deliberately not a conformance promise (ADR 0029).
pub(super) async fn non_law_pre_reclaim_commit_symmetry<F, Fut>(
    make: &F,
    seed: u64,
) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Arc<dyn RuntimePersistence>>,
{
    // FIG-460 / ADR 0045 make the lease advisory and CAS authoritative. ADR 0029
    // makes supersession reclaim-mediated, so both pre-reclaim commit shapes
    // share authorization on current backends.
    async fn run(store: Arc<dyn RuntimePersistence>, carrying_claim: bool) -> Result<u64, String> {
        let batch = store
            .enqueue_queued_work(queued_draft(0, 0, false))
            .await
            .map_err(|error| error.to_string())?;
        let stale_owner = owner(0);
        let stale_lease = store
            .try_claim_session_execution_lease(SESSION_ID, &stale_owner, 60_000)
            .await
            .map_err(|error| error.to_string())?
            .acquired()
            .ok_or_else(|| "stale-owner lease busy".to_string())?;
        let claim = store
            .claim_ready_queued_work_by_batch_ids(
                SESSION_ID,
                &stale_lease.fence(),
                &stale_owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&batch.batch_id),
            )
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "queued work absent".to_string())?;
        store
            .release_session_execution_lease(&stale_lease.completion())
            .await
            .map_err(|error| error.to_string())?;
        let successor_owner = owner(1);
        let _successor_lease = store
            .try_claim_session_execution_lease(SESSION_ID, &successor_owner, 60_000)
            .await
            .map_err(|error| error.to_string())?
            .acquired()
            .ok_or_else(|| "successor lease busy".to_string())?;
        let state = RuntimeSessionState {
            session_id: SESSION_ID.to_string(),
            tool_state_snapshot: Some(ToolState::default().with_generation(61)),
            ..RuntimeSessionState::default()
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .releasing_session_execution_lease(stale_lease.completion());
        if carrying_claim {
            commit = commit.completing_queue_claim(claim.completion());
        }
        let result = store
            .commit_runtime_state(commit.clone())
            .await
            .map_err(|error| error.to_string())?;
        let durable = store
            .load_session()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "accepted pre-reclaim commit was not durable".to_string())?;
        if durable.head_revision != result.head_revision {
            return Err("accepted pre-reclaim commit did not publish its head".to_string());
        }
        let remaining_after_first = store
            .list_queued_work(SESSION_ID)
            .await
            .map_err(|error| error.to_string())?;
        if remaining_after_first.is_empty() != carrying_claim {
            return Err("claim settlement did not match the commit shape".to_string());
        }
        let replay = store
            .commit_runtime_state(commit)
            .await
            .map_err(|error| error.to_string())?;
        if replay.head_revision != result.head_revision {
            return Err("identical pre-reclaim replay advanced the head twice".to_string());
        }
        let remaining_after_replay = store
            .list_queued_work(SESSION_ID)
            .await
            .map_err(|error| error.to_string())?;
        if remaining_after_replay
            .iter()
            .map(|batch| &batch.batch_id)
            .ne(remaining_after_first.iter().map(|batch| &batch.batch_id))
        {
            return Err("identical pre-reclaim replay changed batch consumption".to_string());
        }
        Ok(result.head_revision)
    }

    let claim_free = run(make(seed).await, false)
        .await
        .map_err(TestCaseError::fail)?;
    let claim_carrying = run(make(seed + 1).await, true)
        .await
        .map_err(TestCaseError::fail)?;
    prop_assert_eq!(claim_free, 1);
    prop_assert_eq!(claim_carrying, 1);
    Ok(())
}

/// LAW: successor head advancement may change rejection precedence, never the
/// requirement that a superseded predecessor fails without durable mutation.
pub(super) async fn law_reclaimed_predecessor_rejection_survives_successor_head_advance(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let batch = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let predecessor_owner = owner(0);
    let predecessor_lease = store
        .try_claim_session_execution_lease(SESSION_ID, &predecessor_owner, 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("predecessor lease busy"))?;
    let predecessor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &predecessor_lease.fence(),
            &predecessor_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("predecessor queued work absent"))?;
    store
        .release_session_execution_lease(&predecessor_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;

    let successor_owner = owner(1);
    let successor_lease = store
        .try_claim_session_execution_lease(SESSION_ID, &successor_owner, 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;
    let successor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("successor did not reclaim queued work"))?;
    let successor_state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(32)),
        ..RuntimeSessionState::default()
    };
    let successor_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&successor_state, &[])
                .completing_queue_claim(successor_claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(successor_result.head_revision, 1);

    let before_predecessor = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let predecessor_state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        tool_state_snapshot: Some(ToolState::default().with_generation(33)),
        ..RuntimeSessionState::default()
    };
    let predecessor_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&predecessor_state, &[])
                .completing_queue_claim(predecessor_claim.completion()),
        )
        .await;
    prop_assert!(
        matches!(
            predecessor_result,
            Err(StoreError::QueuedWorkClaimSuperseded { .. })
                | Err(StoreError::HeadRevisionConflict { .. })
        ),
        "superseded predecessor with a stale head was not rejected: {predecessor_result:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before_predecessor,
        "reclaimed predecessor after successor head advance",
    )
    .await
    .map_err(TestCaseError::fail)?;
    Ok(())
}
