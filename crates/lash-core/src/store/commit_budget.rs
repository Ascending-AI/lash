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
/// Bytes cover the complete logical persisted payload carried by a
/// [`RuntimeCommit`]: session configuration, graph delta, hydrated checkpoint,
/// attachment-manifest ids, queued-work batches, the selected Agent Frame,
/// usage deltas, and the durable turn result. Nodes bound all rows the commit
/// writes: graph nodes plus attachment-intent adoption rows. Hosts must choose
/// bounded or unbounded behavior for both dimensions; this type deliberately
/// has no `Default`. The reference curve in ADR 0058 recommends a 1 MiB
/// logical-byte limit because its p95 physical commit interval stays below the
/// named 60 ms target on both reference backends; 512 rows remains the separate
/// starting-point node bound. Hosts should remeasure and tune both limits for
/// their own backend envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitBudget {
    /// Aggregate logical persisted-payload byte limit.
    pub bytes: CommitBudgetLimit,
    /// Rows the commit writes as recorded by this attempt: graph nodes plus
    /// attachment-intent adoption rows. A same-turn-id replay may stamp
    /// prior-attempt rows beyond the count.
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
    pub(crate) graph_rows: usize,
    pub(crate) adopted_intent_rows: usize,
    pub(crate) total_rows: usize,
    pub(crate) session_config_bytes: usize,
    pub(crate) graph_delta_bytes: usize,
    pub(crate) checkpoint_bytes: usize,
    pub(crate) attachment_manifest_bytes: usize,
    pub(crate) queue_batch_bytes: usize,
    pub(crate) agent_frame_bytes: usize,
    pub(crate) usage_delta_bytes: usize,
    pub(crate) turn_result_bytes: usize,
    pub(crate) total_bytes: usize,
}

impl RuntimeCommit {
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub const MAX_COMMIT_NODE_COUNT: usize = 512;

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub const MAX_COMMIT_BUDGET_BYTES: usize = 1024 * 1024;

    /// Bound the complete logical persisted payload carried by this commit
    /// before a backend transaction starts.
    pub fn validate_budget(&self) -> Result<(), StoreError> {
        let graph_rows = self.graph.nodes.len();
        let adopted_intent_rows = usize::try_from(self.adopted_intent_rows).unwrap_or(usize::MAX);
        let row_count = graph_rows.saturating_add(adopted_intent_rows);
        match self.commit_budget.nodes {
            CommitBudgetLimit::Bounded(max_nodes) if row_count > max_nodes.get() => {
                tracing::warn!(
                    target: "lash.runtime_commit.budget",
                    session_id = %self.session_id,
                    dimension = "nodes",
                    graph_rows,
                    adopted_intent_rows,
                    actual = row_count,
                    limit = max_nodes.get(),
                    outcome = "rejected",
                    "runtime commit budget decision"
                );
                return Err(StoreError::CommitNodeBudgetExceeded {
                    node_count: row_count,
                    max_nodes: max_nodes.get(),
                });
            }
            CommitBudgetLimit::Bounded(max_nodes) => tracing::trace!(
                target: "lash.runtime_commit.budget",
                session_id = %self.session_id,
                dimension = "nodes",
                graph_rows,
                adopted_intent_rows,
                actual = row_count,
                limit = max_nodes.get(),
                outcome = "admitted",
                "runtime commit budget decision"
            ),
            CommitBudgetLimit::Unbounded => tracing::trace!(
                target: "lash.runtime_commit.budget",
                session_id = %self.session_id,
                dimension = "nodes",
                graph_rows,
                adopted_intent_rows,
                actual = row_count,
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
                graph_rows = measurement.graph_rows,
                adopted_intent_rows = measurement.adopted_intent_rows,
                total_rows = measurement.total_rows,
                session_config_bytes = measurement.session_config_bytes,
                graph_delta_bytes = measurement.graph_delta_bytes,
                checkpoint_bytes = measurement.checkpoint_bytes,
                attachment_manifest_bytes = measurement.attachment_manifest_bytes,
                queue_batch_bytes = measurement.queue_batch_bytes,
                agent_frame_bytes = measurement.agent_frame_bytes,
                usage_delta_bytes = measurement.usage_delta_bytes,
                turn_result_bytes = measurement.turn_result_bytes,
                actual = measurement.total_bytes,
                limit = max_bytes.get(),
                outcome = "rejected",
                "runtime commit budget decision"
            );
            return Err(StoreError::CommitByteBudgetExceeded {
                session_config_bytes: measurement.session_config_bytes,
                graph_delta_bytes: measurement.graph_delta_bytes,
                checkpoint_bytes: measurement.checkpoint_bytes,
                attachment_manifest_bytes: measurement.attachment_manifest_bytes,
                queue_batch_bytes: measurement.queue_batch_bytes,
                agent_frame_bytes: measurement.agent_frame_bytes,
                usage_delta_bytes: measurement.usage_delta_bytes,
                turn_result_bytes: measurement.turn_result_bytes,
                total_bytes: measurement.total_bytes,
                max_bytes: max_bytes.get(),
            });
        }
        tracing::trace!(
            target: "lash.runtime_commit.budget",
            session_id = %self.session_id,
            dimension = "bytes",
            graph_rows = measurement.graph_rows,
            adopted_intent_rows = measurement.adopted_intent_rows,
            total_rows = measurement.total_rows,
            session_config_bytes = measurement.session_config_bytes,
            graph_delta_bytes = measurement.graph_delta_bytes,
            checkpoint_bytes = measurement.checkpoint_bytes,
            attachment_manifest_bytes = measurement.attachment_manifest_bytes,
            queue_batch_bytes = measurement.queue_batch_bytes,
            agent_frame_bytes = measurement.agent_frame_bytes,
            usage_delta_bytes = measurement.usage_delta_bytes,
            turn_result_bytes = measurement.turn_result_bytes,
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
        let session_config_bytes = measure_json(serde_json::to_vec(&self.config))?;
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
        let queue_batch_bytes = self.enqueued_queue_batches.iter().try_fold(
            0usize,
            |total, batch| -> Result<usize, StoreError> {
                Ok(total.saturating_add(measure_json(serde_json::to_vec(batch))?))
            },
        )?;
        let agent_frame_bytes = self
            .current_frame_node_id
            .as_ref()
            .map(|frame_node_id| measure_json(serde_json::to_vec(frame_node_id)))
            .transpose()?
            .unwrap_or_default();
        let usage_delta_bytes = self.usage_deltas.iter().try_fold(
            0usize,
            |total, delta| -> Result<usize, StoreError> {
                Ok(total.saturating_add(measure_json(serde_json::to_vec(delta))?))
            },
        )?;
        let turn_result_bytes = measure_json(serde_json::to_vec(&self.turn_commit))?;
        let total_bytes = session_config_bytes
            .saturating_add(graph_delta_bytes)
            .saturating_add(checkpoint_bytes)
            .saturating_add(attachment_manifest_bytes)
            .saturating_add(queue_batch_bytes)
            .saturating_add(agent_frame_bytes)
            .saturating_add(usage_delta_bytes)
            .saturating_add(turn_result_bytes);
        let graph_rows = self.graph.nodes.len();
        let adopted_intent_rows = usize::try_from(self.adopted_intent_rows).unwrap_or(usize::MAX);
        let total_rows = graph_rows.saturating_add(adopted_intent_rows);
        Ok(RuntimeCommitBudgetMeasurement {
            graph_rows,
            adopted_intent_rows,
            total_rows,
            session_config_bytes,
            graph_delta_bytes,
            checkpoint_bytes,
            attachment_manifest_bytes,
            queue_batch_bytes,
            agent_frame_bytes,
            usage_delta_bytes,
            turn_result_bytes,
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
    fn adopted_intent_rows_count_against_the_node_budget() {
        let state = crate::RuntimeSessionState {
            session_id: "budget-adoption-rows".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let budget = CommitBudget::new(CommitBudgetLimit::Unbounded, CommitBudgetLimit::bounded(2));
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit.adopted_intent_rows = 3;

        let error = commit
            .validate_budget()
            .expect_err("adoption rows must consume the configured row budget");
        assert!(matches!(
            &error,
            StoreError::CommitNodeBudgetExceeded {
                node_count: 3,
                max_nodes: 2,
            }
        ));
        assert!(error.to_string().contains("configured 2-row node budget"));
        assert!(
            error
                .to_string()
                .contains("including attachment-intent adoption")
        );
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
        commit.committed_attachment_ids =
            vec![crate::AttachmentId::parse("budget-attachment").expect("valid attachment id")];

        let expected_graph_bytes = serde_json::to_vec(&node).expect("encode graph node").len();
        let expected_session_config_bytes = serde_json::to_vec(&commit.config)
            .expect("encode session config")
            .len();
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
                session_config_bytes,
                graph_delta_bytes,
                checkpoint_bytes,
                attachment_manifest_bytes,
                queue_batch_bytes,
                agent_frame_bytes,
                usage_delta_bytes,
                turn_result_bytes,
                total_bytes,
                max_bytes,
            }) if session_config_bytes == expected_session_config_bytes
                && graph_delta_bytes == expected_graph_bytes
                && checkpoint_bytes == expected_checkpoint_bytes
                && attachment_manifest_bytes == expected_attachment_bytes
                && queue_batch_bytes == 0
                && agent_frame_bytes == 0
                && usage_delta_bytes == 0
                && turn_result_bytes > 0
                && total_bytes
                    == expected_session_config_bytes
                        + expected_graph_bytes
                        + expected_checkpoint_bytes
                        + expected_attachment_bytes
                        + turn_result_bytes
                && max_bytes == 128
        ));
    }

    #[test]
    fn large_head_prompt_is_included_in_commit_byte_budget() {
        let prompt = crate::PromptLayer::new().with_contribution(
            crate::PromptContribution::guidance("large", "x".repeat(4_096)),
        );
        let mut policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);
        policy.prompt = prompt;
        let state = crate::RuntimeSessionState {
            session_id: "budget-head-prompt".to_string(),
            policy,
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.commit_budget = CommitBudget::bounded(512, 512);

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                session_config_bytes,
                graph_delta_bytes: 0,
                max_bytes: 512,
                ..
            }) if session_config_bytes > 4_096
        ));
    }

    #[test]
    fn queue_batch_bytes_can_exceed_the_commit_budget_alone() {
        const BYTE_LIMIT: usize = 2_048;
        let state = crate::RuntimeSessionState {
            session_id: "budget-queue-batch".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let budget = CommitBudget::new(
            CommitBudgetLimit::bounded(BYTE_LIMIT),
            CommitBudgetLimit::Unbounded,
        );
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit
            .validate_budget()
            .expect("the commit without a queue batch must fit");

        commit.enqueued_queue_batches = vec![crate::QueuedWorkBatchDraft::new(
            state.session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            vec![crate::QueuedWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id(&state.session_id, "oversized-queue-batch"),
                "q".repeat(BYTE_LIMIT * 2),
                None,
            )],
        )];

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                max_bytes: BYTE_LIMIT,
                ..
            })
        ));
    }

    #[test]
    fn agent_frame_bytes_can_exceed_the_commit_budget_alone() {
        const BYTE_LIMIT: usize = 2_048;
        let state = crate::RuntimeSessionState {
            session_id: "budget-agent-frame".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let budget = CommitBudget::new(
            CommitBudgetLimit::bounded(BYTE_LIMIT),
            CommitBudgetLimit::Unbounded,
        );
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit
            .validate_budget()
            .expect("the commit without an agent frame must fit");

        commit.current_frame_node_id = Some(crate::FrameNodeId::from_raw_for_testing(
            "f".repeat(BYTE_LIMIT * 2),
        ));

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                max_bytes: BYTE_LIMIT,
                ..
            })
        ));
    }

    #[test]
    fn usage_delta_bytes_can_exceed_the_commit_budget_alone() {
        const BYTE_LIMIT: usize = 2_048;
        let state = crate::RuntimeSessionState {
            session_id: "budget-usage-delta".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let budget = CommitBudget::new(
            CommitBudgetLimit::bounded(BYTE_LIMIT),
            CommitBudgetLimit::Unbounded,
        );
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit
            .validate_budget()
            .expect("the commit without a usage delta must fit");

        let usage = crate::TokenLedgerEntry {
            source: "u".repeat(BYTE_LIMIT * 2),
            model: "budget-model".to_string(),
            usage: crate::TokenUsage::default(),
        };
        commit.usage_deltas =
            crate::store::RuntimeUsageDelta::for_operation(&commit.turn_commit.operation, &[usage])
                .expect("identify the oversized usage delta");

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                max_bytes: BYTE_LIMIT,
                ..
            })
        ));
    }

    #[test]
    fn turn_result_bytes_can_exceed_the_commit_budget_alone() {
        const BYTE_LIMIT: usize = 2_048;
        let state = crate::RuntimeSessionState {
            session_id: "budget-turn-result".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let budget = CommitBudget::new(
            CommitBudgetLimit::bounded(BYTE_LIMIT),
            CommitBudgetLimit::Unbounded,
        );
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit
            .validate_budget()
            .expect("the commit with its ordinary turn result must fit");

        commit.turn_commit = crate::RuntimeTurnCommitStamp::new(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("t".repeat(BYTE_LIMIT * 2)),
            "commit",
        ));

        assert!(matches!(
            commit.validate_budget(),
            Err(StoreError::CommitByteBudgetExceeded {
                max_bytes: BYTE_LIMIT,
                ..
            })
        ));
    }

    #[test]
    fn commit_with_every_payload_family_present_fits_its_byte_budget() {
        const BYTE_LIMIT: usize = 64 * 1024;
        let mut state = crate::RuntimeSessionState {
            session_id: "budget-all-families".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        let usage = crate::TokenLedgerEntry {
            source: "all-families".to_string(),
            model: "budget-model".to_string(),
            usage: crate::TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                ..crate::TokenUsage::default()
            },
        };
        let budget = CommitBudget::new(
            CommitBudgetLimit::bounded(BYTE_LIMIT),
            CommitBudgetLimit::Unbounded,
        );
        let mut commit =
            RuntimeCommit::persisted_state_for_test_with_budget(&state, &[usage], budget);
        commit.committed_attachment_ids = vec![
            crate::AttachmentId::parse("all-families-attachment").expect("valid attachment id"),
        ];
        commit.enqueued_queue_batches = vec![crate::QueuedWorkBatchDraft::new(
            state.session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            vec![crate::QueuedWorkPayload::agent_frame_task(
                crate::session_graph::frame_node_id(&state.session_id, "all-families-follow-up"),
                "follow-up",
                None,
            )],
        )];

        commit
            .validate_budget()
            .expect("a commit with every payload family inside the limit must fit");
        let measurement = commit
            .measure_budget()
            .expect("measure all payload families");
        assert!(measurement.session_config_bytes > 0);
        assert!(measurement.graph_delta_bytes > 0);
        assert!(measurement.checkpoint_bytes > 0);
        assert!(measurement.attachment_manifest_bytes > 0);
        assert!(measurement.queue_batch_bytes > 0);
        assert!(measurement.agent_frame_bytes > 0);
        assert!(measurement.usage_delta_bytes > 0);
        assert!(measurement.turn_result_bytes > 0);
        assert!(measurement.total_bytes < BYTE_LIMIT);
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
