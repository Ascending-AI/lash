use super::{GraphCommitDelta, RuntimeCommit, StoreError};

impl RuntimeCommit {
    /// Maximum number of graph nodes a single commit may write.
    ///
    /// This and [`MAX_COMMIT_BUDGET_BYTES`] bound work before any backend opens
    /// a transaction. They were selected from the SQL commit-size benchmark at
    /// `lash-postgres-store/tests/commit_size_benchmark.rs`.
    pub const MAX_COMMIT_NODE_COUNT: usize = 512;

    /// Maximum aggregate serialized graph-delta, checkpoint, and attachment
    /// manifest bytes a single commit may carry.
    pub const MAX_COMMIT_BUDGET_BYTES: usize = 1024 * 1024;

    /// Enforce the invariant that runtime commit transactions contain no
    /// unbounded caller-controlled work.
    pub fn validate_budget(&self) -> Result<(), StoreError> {
        let node_count = match &self.graph {
            GraphCommitDelta::Unchanged { .. } => 0,
            GraphCommitDelta::Append { nodes, .. } => nodes.len(),
        };
        if node_count > Self::MAX_COMMIT_NODE_COUNT {
            return Err(StoreError::CommitNodeBudgetExceeded {
                node_count,
                max_nodes: Self::MAX_COMMIT_NODE_COUNT,
            });
        }

        let measure = |result: Result<Vec<u8>, serde_json::Error>| {
            result.map(|bytes| bytes.len()).map_err(|err| {
                StoreError::Backend(format!(
                    "failed to measure runtime commit transaction budget: {err}"
                ))
            })
        };
        let graph_delta_bytes = measure(serde_json::to_vec(&self.graph))?;
        let checkpoint_bytes = measure(serde_json::to_vec(&self.checkpoint))?;
        let attachment_manifest_bytes =
            measure(serde_json::to_vec(&self.committed_attachment_ids))?;
        let total_bytes = graph_delta_bytes
            .saturating_add(checkpoint_bytes)
            .saturating_add(attachment_manifest_bytes);
        if total_bytes > Self::MAX_COMMIT_BUDGET_BYTES {
            return Err(StoreError::CommitByteBudgetExceeded {
                graph_delta_bytes,
                checkpoint_bytes,
                attachment_manifest_bytes,
                total_bytes,
                max_bytes: Self::MAX_COMMIT_BUDGET_BYTES,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_node_count_before_backend_work() {
        let state = crate::RuntimeSessionState {
            session_id: "budget-nodes".to_string(),
            ..Default::default()
        };
        let node = crate::SessionNodeRecord {
            node_id: "node".to_string(),
            parent_node_id: None,
            caused_by: None,
            agent_frame_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("budget", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
        commit.graph = GraphCommitDelta::Append {
            nodes: (0..=RuntimeCommit::MAX_COMMIT_NODE_COUNT)
                .map(|index| crate::SessionNodeRecord {
                    node_id: format!("node-{index}"),
                    ..node.clone()
                })
                .collect(),
            leaf_node_id: None,
        };

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitNodeBudgetExceeded {
                node_count,
                max_nodes
            }) if node_count == RuntimeCommit::MAX_COMMIT_NODE_COUNT + 1
                && max_nodes == RuntimeCommit::MAX_COMMIT_NODE_COUNT
        ));
    }

    #[test]
    fn reports_each_byte_component() {
        let state = crate::RuntimeSessionState {
            session_id: "budget-bytes".to_string(),
            ..Default::default()
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
        commit.checkpoint.execution_state =
            Some(vec![0; RuntimeCommit::MAX_COMMIT_BUDGET_BYTES + 1]);

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                checkpoint_bytes,
                total_bytes,
                max_bytes,
                ..
            }) if checkpoint_bytes > RuntimeCommit::MAX_COMMIT_BUDGET_BYTES
                && total_bytes >= checkpoint_bytes
                && max_bytes == RuntimeCommit::MAX_COMMIT_BUDGET_BYTES
        ));
    }
}
