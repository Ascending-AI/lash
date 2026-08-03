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
