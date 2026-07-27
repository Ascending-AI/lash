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
    commit.validate_budget()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyFrameFacadeStore;

    crate::impl_noop_attachment_manifest!(EmptyFrameFacadeStore);

    #[async_trait::async_trait]
    impl SessionCommitStore for EmptyFrameFacadeStore {
        async fn load_session(
            &self,
            _scope: super::super::SessionReadScope,
        ) -> Result<Option<super::super::PersistedSessionRead>, StoreError> {
            Ok(None)
        }

        async fn load_node(
            &self,
            _node_id: &str,
        ) -> Result<Option<crate::SessionNodeRecord>, StoreError> {
            Ok(None)
        }

        async fn commit_runtime_state(
            &self,
            commit: RuntimeCommit,
        ) -> Result<RuntimeCommitResult, StoreError> {
            let realization_digest = graph_realization_digest(&commit.graph);
            let manifest = super::super::SessionCheckpoint::new(
                commit.checkpoint.turn_state,
                commit.checkpoint.tool_state_ref,
                commit.checkpoint.plugin_snapshot_ref,
                commit.checkpoint.plugin_snapshot_revision,
                commit.checkpoint.execution_state_ref,
            );
            Ok(RuntimeCommitResult {
                head_revision: commit.expected_head_revision.unwrap_or_default() + 1,
                checkpoint_ref: "empty-frame-facade".to_string().into(),
                manifest,
                realization_digest,
                realized_agent_frames: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                turn_input_applications: Vec::new(),
            })
        }

        async fn save_session_meta(
            &self,
            _meta: super::super::SessionMeta,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        async fn load_session_meta(&self) -> Result<Option<super::super::SessionMeta>, StoreError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn verified_commit_rejects_facade_that_drops_frame_realization() {
        let mut state = crate::RuntimeSessionState {
            session_id: "frame-guard".to_string(),
            ..crate::RuntimeSessionState::default()
        };
        state.ensure_agent_frame_initialized();
        let expected = state
            .agent_frames
            .iter()
            .map(|frame| frame.frame_id.clone())
            .collect::<Vec<_>>();
        let commit = RuntimeCommit::persisted_state(&state, &[]);

        let err = commit_runtime_state_verified(&EmptyFrameFacadeStore, commit)
            .await
            .expect_err("an empty facade frame echo must be rejected");

        assert!(matches!(
            err,
            StoreError::CommitFrameRealizationMismatch {
                expected: actual_expected,
                stored,
            } if actual_expected == expected && stored.is_empty()
        ));
    }
}
