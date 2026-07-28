use super::{RuntimeCommit, RuntimeCommitResult, SessionCommitStore, StoreError};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RealizedNodeTimestamp {
    pub node_id: String,
    pub timestamp: String,
}

/// Commit through the production realization boundary.
pub async fn commit_runtime_state_verified(
    store: &(dyn SessionCommitStore + '_),
    commit: RuntimeCommit,
) -> Result<RuntimeCommitResult, StoreError> {
    commit.validate_budget()?;
    store.commit_runtime_state(commit).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct NonValidatingFacadeStore {
        commit_attempts: AtomicUsize,
    }

    crate::impl_noop_attachment_manifest!(NonValidatingFacadeStore);

    #[async_trait::async_trait]
    impl SessionCommitStore for NonValidatingFacadeStore {
        async fn load_session(
            &self,
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
            let realized_node_timestamps = commit
                .graph
                .appended_nodes()
                .map(|node| RealizedNodeTimestamp {
                    node_id: node.node_id.clone(),
                    timestamp: node.timestamp.clone(),
                })
                .collect();
            let manifest = super::super::SessionCheckpoint::new(
                commit.checkpoint.turn_state,
                commit.checkpoint.tool_state_ref,
                commit.checkpoint.plugin_snapshot_ref,
                commit.checkpoint.plugin_snapshot_revision,
                commit.checkpoint.execution_state_ref,
            );
            Ok(RuntimeCommitResult {
                head_revision: commit.expected_head_revision + 1,
                checkpoint_ref: "empty-frame-facade".to_string().into(),
                manifest,
                realized_node_timestamps,
                enqueued_queue_batches: Vec::new(),
                turn_input_applications: Vec::new(),
            })
        }

        async fn ensure_session_bound(
            &self,
            _session_id: &str,
            _policy: &crate::SessionPolicy,
        ) -> Result<(), StoreError> {
            Ok(())
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
    async fn verified_commit_rejects_node_budget_before_calling_a_non_validating_store() {
        let store = NonValidatingFacadeStore::default();
        let state = crate::RuntimeSessionState {
            session_id: "boundary-budget".to_string(),
            ..crate::RuntimeSessionState::default()
        };
        let node = crate::SessionNodeRecord {
            node_id: "node".to_string(),
            parent_node_id: None,
            timestamp: "2026-07-27T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("budget", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.graph = super::super::GraphAppend {
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
