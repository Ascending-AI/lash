use super::{
    RuntimeCommit, RuntimeCommitResult, SessionCommitStore, StoreError, graph_realization_digest,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RealizedAgentFrame {
    pub frame_id: crate::AgentFrameId,
    pub created_at: String,
}

/// Commit through the production realization boundary.
///
/// Every runtime path that can adopt a commit result calls this function. The
/// graph digest compares the current proposal with the receipt recorded by the
/// first successful attempt, while the frame-id check prevents a partial or
/// defaulted facade result from erasing live frame metadata.
pub async fn commit_runtime_state_verified(
    store: &(dyn SessionCommitStore + '_),
    commit: RuntimeCommit,
) -> Result<RuntimeCommitResult, StoreError> {
    let proposed = graph_realization_digest(&commit.graph);
    let expected_frame_ids = commit
        .agent_frames
        .iter()
        .map(|frame| frame.frame_id.clone())
        .collect::<Vec<_>>();
    let result = store.commit_runtime_state(commit).await?;
    if proposed != result.realization_digest {
        return Err(StoreError::CommitRealizationMismatch {
            proposed,
            stored: result.realization_digest.clone(),
        });
    }
    let stored_frame_ids = result
        .realized_agent_frames
        .iter()
        .map(|frame| frame.frame_id.clone())
        .collect::<Vec<_>>();
    if expected_frame_ids != stored_frame_ids {
        return Err(StoreError::CommitFrameRealizationMismatch {
            expected: expected_frame_ids,
            stored: stored_frame_ids,
        });
    }
    Ok(result)
}
