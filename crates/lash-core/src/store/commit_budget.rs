use super::{RuntimeCommit, StoreError};

impl RuntimeCommit {
    /// Maximum number of graph nodes a single commit may write.
    ///
    /// This and [`MAX_COMMIT_BUDGET_BYTES`] bound work before any backend opens
    /// a transaction. They were selected from the SQL commit-size benchmark at
    /// `lash-postgres-store/tests/commit_size_benchmark.rs`.
    pub const MAX_COMMIT_NODE_COUNT: usize = 512;

    /// Maximum aggregate persisted-payload bytes for the graph delta,
    /// checkpoint, and attachment-manifest ids a single commit may carry.
    ///
    /// Graph nodes use their exact JSON row representation. Checkpoints use a
    /// backend-neutral named-MessagePack representation of the hydrated value.
    /// Durable backends store the manifest and present component bodies as
    /// separately addressed named-MessagePack blobs; SQLite may compress them.
    /// Attachment ids use the exact UTF-8 bytes bound into manifest updates.
    /// Backend envelope, compression, row, and page overhead is deliberately
    /// outside this caller-controlled logical-payload budget.
    pub const MAX_COMMIT_BUDGET_BYTES: usize = 1024 * 1024;

    /// Bound the graph, hydrated checkpoint, and attachment-adoption payloads
    /// before a backend transaction starts.
    ///
    /// This is not a bound on the complete [`RuntimeCommit`]: queue batches,
    /// agent frames, usage deltas, and the durable turn result are currently
    /// outside it.
    pub fn validate_budget(&self) -> Result<(), StoreError> {
        let node_count = self.graph.nodes.len();
        if node_count > Self::MAX_COMMIT_NODE_COUNT {
            return Err(StoreError::CommitNodeBudgetExceeded {
                node_count,
                max_nodes: Self::MAX_COMMIT_NODE_COUNT,
            });
        }

        let measure_json = |result: Result<Vec<u8>, serde_json::Error>| {
            result.map(|bytes| bytes.len()).map_err(|err| {
                StoreError::Backend(format!(
                    "failed to measure runtime commit transaction budget: {err}"
                ))
            })
        };
        let graph_delta_bytes = self.graph.nodes.iter().try_fold(
            0usize,
            |total, node| -> Result<usize, StoreError> {
                Ok(total.saturating_add(measure_json(serde_json::to_vec(node))?))
            },
        )?;
        let checkpoint_bytes = rmp_serde::to_vec_named(&self.checkpoint)
            .map(|bytes| bytes.len())
            .map_err(|err| {
                StoreError::Backend(format!(
                    "failed to measure runtime commit transaction budget: {err}"
                ))
            })?;
        let attachment_manifest_bytes = self
            .committed_attachment_ids
            .iter()
            .fold(0usize, |total, id| total.saturating_add(id.as_str().len()));
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
    fn rejects_node_count_over_limit() {
        let state = crate::RuntimeSessionState {
            session_id: "budget-nodes".to_string(),
            ..Default::default()
        };
        let node = crate::SessionNodeRecord {
            node_id: "node".to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("budget", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.graph = crate::GraphAppend {
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
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        let node = crate::SessionNodeRecord {
            node_id: "budget-node".to_string(),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("budget", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        commit.graph = crate::GraphAppend {
            nodes: vec![node.clone()],
            leaf_node_id: Some(node.node_id.clone()),
        };
        commit.checkpoint.execution_state =
            Some(vec![0; RuntimeCommit::MAX_COMMIT_BUDGET_BYTES + 1]);
        commit.committed_attachment_ids = vec![crate::AttachmentId::new("budget-attachment")];

        let expected_graph_bytes = serde_json::to_vec(&node).expect("encode graph node").len();
        let expected_checkpoint_bytes = rmp_serde::to_vec_named(&commit.checkpoint)
            .expect("encode hydrated checkpoint")
            .len();
        let expected_attachment_bytes = "budget-attachment".len();

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                graph_delta_bytes,
                checkpoint_bytes,
                attachment_manifest_bytes,
                total_bytes,
                max_bytes,
            }) if graph_delta_bytes == expected_graph_bytes
                && checkpoint_bytes == expected_checkpoint_bytes
                && attachment_manifest_bytes == expected_attachment_bytes
                && total_bytes
                    == expected_graph_bytes
                        + expected_checkpoint_bytes
                        + expected_attachment_bytes
                && max_bytes == RuntimeCommit::MAX_COMMIT_BUDGET_BYTES
        ));
    }
}
