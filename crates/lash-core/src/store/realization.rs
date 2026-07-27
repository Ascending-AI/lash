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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct NonValidatingFacadeStore {
        commit_attempts: AtomicUsize,
        drop_frame_realization: bool,
    }

    crate::impl_noop_attachment_manifest!(NonValidatingFacadeStore);

    #[async_trait::async_trait]
    impl SessionCommitStore for NonValidatingFacadeStore {
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
            self.commit_attempts.fetch_add(1, Ordering::SeqCst);
            let realization_digest = graph_realization_digest(&commit.graph);
            let realized_agent_frames = if self.drop_frame_realization {
                Vec::new()
            } else {
                commit
                    .agent_frames
                    .iter()
                    .map(|frame| RealizedAgentFrame {
                        frame_id: frame.frame_id.clone(),
                        created_at: frame.created_at.clone(),
                    })
                    .collect()
            };
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
                realized_agent_frames,
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
        let store = NonValidatingFacadeStore {
            drop_frame_realization: true,
            ..NonValidatingFacadeStore::default()
        };
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

        let err = commit_runtime_state_verified(&store, commit)
            .await
            .expect_err("an empty facade frame echo must be rejected");

        assert!(matches!(
            err,
            StoreError::CommitFrameRealizationMismatch {
                expected: actual_expected,
                stored,
            } if actual_expected == expected && stored.is_empty()
        ));
        assert_eq!(store.commit_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn verified_commit_rejects_node_budget_before_calling_a_non_validating_store() {
        let store = NonValidatingFacadeStore::default();
        let state = crate::RuntimeSessionState {
            session_id: "boundary-budget".to_string(),
            ..crate::RuntimeSessionState::default()
        };
        let node = crate::SessionNodeRecord {
            node_id: "node".to_string(),
            parent_node_id: None,
            caused_by: None,
            agent_frame_id: None,
            timestamp: "2026-07-27T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("budget", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
        commit.graph = super::super::GraphCommitDelta::Append {
            nodes: (0..=RuntimeCommit::MAX_COMMIT_NODE_COUNT)
                .map(|index| crate::SessionNodeRecord {
                    node_id: format!("node-{index}"),
                    ..node.clone()
                })
                .collect(),
            leaf_node_id: None,
        };

        let err = commit_runtime_state_verified(&store, commit)
            .await
            .expect_err("the shared boundary must reject the oversized commit");

        assert!(matches!(
            err,
            StoreError::CommitNodeBudgetExceeded {
                node_count,
                max_nodes,
            } if node_count == RuntimeCommit::MAX_COMMIT_NODE_COUNT + 1
                && max_nodes == RuntimeCommit::MAX_COMMIT_NODE_COUNT
        ));
        assert_eq!(
            store.commit_attempts.load(Ordering::SeqCst),
            0,
            "the non-validating store must not be called"
        );
    }
}
