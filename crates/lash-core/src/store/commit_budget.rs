use super::{HydratedCheckpointComponent, RuntimeCommit, StoreError};

/// An explicit finite runtime-commit limit or an explicit opt-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitBudgetLimit {
    /// Reject a commit whose measured dimension exceeds this non-zero limit.
    Bounded(std::num::NonZeroUsize),
    /// Apply no limit to this dimension.
    Unbounded,
}

impl CommitBudgetLimit {
    /// Construct a finite, non-zero commit limit.
    ///
    /// # Panics
    ///
    /// Panics when `limit` is zero.
    pub const fn bounded(limit: usize) -> Self {
        match std::num::NonZeroUsize::new(limit) {
            Some(limit) => Self::Bounded(limit),
            None => panic!("commit budget limit must be non-zero"),
        }
    }
}

/// Host-owned limits on one atomic runtime commit.
///
/// Bytes cover the graph delta, hydrated checkpoint, and attachment-manifest
/// ids. Nodes cover the graph delta. Hosts must choose bounded or unbounded
/// behavior for both dimensions; this type deliberately has no `Default`.
/// A 1 MiB byte limit and 512-node limit are the documented recommended
/// starting point; hosts should tune them for their backend latency envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitBudget {
    /// Aggregate logical persisted-payload byte limit.
    pub bytes: CommitBudgetLimit,
    /// Graph nodes written by one commit.
    pub nodes: CommitBudgetLimit,
}

impl CommitBudget {
    /// Construct a budget from independently explicit byte and node limits.
    pub const fn new(bytes: CommitBudgetLimit, nodes: CommitBudgetLimit) -> Self {
        Self { bytes, nodes }
    }

    /// Construct a budget with finite byte and node limits.
    ///
    /// # Panics
    ///
    /// Panics when either limit is zero.
    pub const fn bounded(bytes: usize, nodes: usize) -> Self {
        Self::new(
            CommitBudgetLimit::bounded(bytes),
            CommitBudgetLimit::bounded(nodes),
        )
    }
}

pub(crate) struct RuntimeCommitBudgetMeasurement {
    pub(crate) graph_delta_bytes: usize,
    pub(crate) checkpoint_bytes: usize,
    pub(crate) attachment_manifest_bytes: usize,
    pub(crate) total_bytes: usize,
}

impl RuntimeCommit {
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub const MAX_COMMIT_NODE_COUNT: usize = 512;

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub const MAX_COMMIT_BUDGET_BYTES: usize = 1024 * 1024;

    /// Bound the graph, hydrated checkpoint, and attachment-adoption payloads
    /// before a backend transaction starts.
    ///
    /// This is not a bound on the complete [`RuntimeCommit`]: queue batches,
    /// agent frames, usage deltas, and the durable turn result are currently
    /// outside it.
    pub fn validate_budget(&self) -> Result<(), StoreError> {
        let node_count = self.graph.nodes.len();
        match self.commit_budget.nodes {
            CommitBudgetLimit::Bounded(max_nodes) if node_count > max_nodes.get() => {
                tracing::warn!(
                    target: "lash.runtime_commit.budget",
                    session_id = %self.session_id,
                    dimension = "nodes",
                    actual = node_count,
                    limit = max_nodes.get(),
                    outcome = "rejected",
                    "runtime commit budget decision"
                );
                return Err(StoreError::CommitNodeBudgetExceeded {
                    node_count,
                    max_nodes: max_nodes.get(),
                });
            }
            CommitBudgetLimit::Bounded(max_nodes) => tracing::trace!(
                target: "lash.runtime_commit.budget",
                session_id = %self.session_id,
                dimension = "nodes",
                actual = node_count,
                limit = max_nodes.get(),
                outcome = "admitted",
                "runtime commit budget decision"
            ),
            CommitBudgetLimit::Unbounded => tracing::trace!(
                target: "lash.runtime_commit.budget",
                session_id = %self.session_id,
                dimension = "nodes",
                actual = node_count,
                limit = "unbounded",
                outcome = "admitted",
                "runtime commit budget decision"
            ),
        }

        let CommitBudgetLimit::Bounded(max_bytes) = self.commit_budget.bytes else {
            tracing::trace!(
                target: "lash.runtime_commit.budget",
                session_id = %self.session_id,
                dimension = "bytes",
                measurement = "skipped_unbounded",
                limit = "unbounded",
                outcome = "admitted",
                "runtime commit budget decision"
            );
            return Ok(());
        };
        let measurement = self.measure_budget()?;
        if measurement.total_bytes > max_bytes.get() {
            tracing::warn!(
                target: "lash.runtime_commit.budget",
                session_id = %self.session_id,
                dimension = "bytes",
                graph_delta_bytes = measurement.graph_delta_bytes,
                checkpoint_bytes = measurement.checkpoint_bytes,
                attachment_manifest_bytes = measurement.attachment_manifest_bytes,
                actual = measurement.total_bytes,
                limit = max_bytes.get(),
                outcome = "rejected",
                "runtime commit budget decision"
            );
            return Err(StoreError::CommitByteBudgetExceeded {
                graph_delta_bytes: measurement.graph_delta_bytes,
                checkpoint_bytes: measurement.checkpoint_bytes,
                attachment_manifest_bytes: measurement.attachment_manifest_bytes,
                total_bytes: measurement.total_bytes,
                max_bytes: max_bytes.get(),
            });
        }
        tracing::trace!(
            target: "lash.runtime_commit.budget",
            session_id = %self.session_id,
            dimension = "bytes",
            graph_delta_bytes = measurement.graph_delta_bytes,
            checkpoint_bytes = measurement.checkpoint_bytes,
            attachment_manifest_bytes = measurement.attachment_manifest_bytes,
            actual = measurement.total_bytes,
            limit = max_bytes.get(),
            outcome = "admitted",
            "runtime commit budget decision"
        );
        Ok(())
    }

    pub(crate) fn measure_budget(&self) -> Result<RuntimeCommitBudgetMeasurement, StoreError> {
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
        let checkpoint_root = self.checkpoint.manifest()?;
        let checkpoint_root_bytes = rmp_serde::to_vec_named(&checkpoint_root)
            .map(|bytes| bytes.len())
            .map_err(|err| StoreError::RecordEncodingFailed {
                record_kind: "checkpoint root budget measurement".to_string(),
                message: err.to_string(),
            })?;
        let changed_component_bytes = self
            .checkpoint
            .components
            .values()
            .filter_map(HydratedCheckpointComponent::body)
            .fold(0usize, |total, body| total.saturating_add(body.len()));
        let checkpoint_bytes = checkpoint_root_bytes.saturating_add(changed_component_bytes);
        let attachment_manifest_bytes = self
            .committed_attachment_ids
            .iter()
            .fold(0usize, |total, id| total.saturating_add(id.as_str().len()));
        let total_bytes = graph_delta_bytes
            .saturating_add(checkpoint_bytes)
            .saturating_add(attachment_manifest_bytes);
        Ok(RuntimeCommitBudgetMeasurement {
            graph_delta_bytes,
            checkpoint_bytes,
            attachment_manifest_bytes,
            total_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_node_count_over_limit() {
        let state = crate::RuntimeSessionState {
            session_id: "budget-nodes".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
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
        let budget = CommitBudget::bounded(1024 * 1024, 2);
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit.graph = crate::GraphAppend {
            nodes: (0..=2)
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
            }) if node_count == 3 && max_nodes == 2
        ));
    }

    #[test]
    fn keyed_budget_counts_root_and_changed_bodies_but_excludes_unchanged_refs() {
        let state = crate::RuntimeSessionState {
            session_id: "budget-bytes".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let budget = CommitBudget::bounded(128, 512);
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
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
        let changed_body = vec![0; 129];
        commit.checkpoint.components.insert(
            "arbitrary/changed".to_string(),
            crate::HydratedCheckpointComponent::changed(changed_body.clone()),
        );
        commit.checkpoint.components.insert(
            "arbitrary/unchanged".to_string(),
            crate::HydratedCheckpointComponent::Unchanged {
                descriptor: crate::CheckpointComponentDescriptor {
                    blob_ref: crate::BlobRef("existing-content-ref".to_string()),
                    encoding_version: crate::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
                },
            },
        );
        commit.committed_attachment_ids = vec![crate::AttachmentId::new("budget-attachment")];

        let expected_graph_bytes = serde_json::to_vec(&node).expect("encode graph node").len();
        let expected_root_bytes = rmp_serde::to_vec_named(
            &commit
                .checkpoint
                .manifest()
                .expect("project checkpoint root"),
        )
        .expect("encode checkpoint root")
        .len();
        let expected_checkpoint_bytes = expected_root_bytes + changed_body.len();
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
                && max_bytes == 128
        ));
    }

    #[test]
    fn serialized_commit_budget_requires_both_limits() {
        let missing_bytes = serde_json::json!({ "nodes": "unbounded" });
        let error = serde_json::from_value::<CommitBudget>(missing_bytes)
            .expect_err("byte budget must be explicit");
        assert!(error.to_string().contains("bytes"));

        let missing_nodes = serde_json::json!({ "bytes": "unbounded" });
        let error = serde_json::from_value::<CommitBudget>(missing_nodes)
            .expect_err("node budget must be explicit");
        assert!(error.to_string().contains("nodes"));
    }

    #[test]
    fn commit_budget_uses_host_friendly_json_shapes() {
        let budget = CommitBudget::new(
            CommitBudgetLimit::bounded(1_048_576),
            CommitBudgetLimit::Unbounded,
        );
        let encoded = serde_json::to_value(budget).expect("serialize commit budget");
        assert_eq!(
            encoded["bytes"],
            serde_json::json!({ "bounded": 1_048_576 })
        );
        assert_eq!(encoded["nodes"], serde_json::json!("unbounded"));
    }
}
