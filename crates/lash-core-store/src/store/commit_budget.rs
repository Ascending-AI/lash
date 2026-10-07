use super::{HydratedCheckpointComponent, RuntimeCommit, StoreError};

/// An explicit finite runtime-commit limit or an explicit opt-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitBudgetLimit {
    /// Reject a commit whose measured dimension exceeds this non-zero limit.
    Bounded(std::num::NonZeroUsize),
    Unbounded,
}

impl CommitBudgetLimit {
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
/// and the durable turn result with its session-command outcomes, except a
/// failed settlement's bounded refusal receipt, so a command over the budget
/// settles failed whenever the head's bare commit fits. Nodes bound all rows the commit
/// writes: graph nodes plus attachment-intent adoption rows. Hosts must choose
/// bounded or unbounded behavior for both dimensions; this type deliberately
/// has no `Default`. ADR 0058 documents 1 MiB and 512 recorded rows as
/// starting points. Hosts measure their own byte and row curves, including
/// the joint configured point, and tune both limits for their backend envelope.
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

/// The longest message a failed settlement's refusal receipt carries
/// uncharged. The longest budget refusal, every count at `usize::MAX`, fits.
const MAX_UNCHARGED_REFUSAL_MESSAGE_BYTES: usize = 512;

/// Whether `outcome` is a failed settlement's bounded refusal receipt, which
/// the byte budget does not charge (FIG-4471).
///
/// A failed settlement commits the bare head: whatever its command put in
/// resident state gave way to the durable head, and the receipt is all it
/// adds. Charging that receipt would put a failed settlement over the size of
/// the head's bare commit, which is what creation admits (FIG-4393), so a
/// command over a budget its head fits could never settle and would stay
/// open. Every other outcome, and a refusal whose message exceeds the bound,
/// is charged.
fn is_uncharged_refusal_receipt(outcome: &crate::SessionCommandOutcome) -> bool {
    matches!(
        outcome,
        crate::SessionCommandOutcome::Failed { message, .. }
            if message.len() <= MAX_UNCHARGED_REFUSAL_MESSAGE_BYTES
    )
}

/// Logical payload accounting for one runtime commit, the measurement
/// [`RuntimeCommit::validate_budget`] checks against its limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeCommitBudgetMeasurement {
    /// Graph-node rows written by the commit.
    pub graph_rows: usize,
    /// Attachment-manifest rows stamped as adopted by the commit.
    pub adopted_intent_rows: usize,
    /// Saturating sum of graph and attachment-adoption rows.
    pub total_rows: usize,
    /// Persisted JSON encoding of the session configuration, including prompt.
    pub session_config_bytes: usize,
    pub graph_delta_bytes: usize,
    /// Named-MessagePack size of the hydrated checkpoint.
    pub checkpoint_bytes: usize,
    /// Raw UTF-8 byte length of the committed attachment ids.
    pub attachment_referrer_bytes: usize,
    /// Persisted JSON encoding of the durable turn result stamp.
    pub turn_result_bytes: usize,
    /// Saturating sum of the budgeted components.
    pub total_bytes: usize,
}

impl RuntimeCommit {
    #[cfg(any(test, feature = "testing"))]
    pub const MAX_COMMIT_NODE_COUNT: usize = 512;

    #[cfg(any(test, feature = "testing"))]
    pub const MAX_COMMIT_BUDGET_BYTES: usize = 1024 * 1024;

    /// Bound the complete logical persisted payload carried by this commit
    /// before a backend transaction starts.
    pub fn validate_budget(&self) -> Result<(), StoreError> {
        self.validate_node_budget()?;
        let CommitBudgetLimit::Bounded(max_bytes) = self.commit_budget.bytes else {
            self.trace_unbounded_byte_budget();
            return Ok(());
        };
        let measurement = self.measure_budget()?;
        self.validate_measured_byte_budget(&measurement, max_bytes.get())
    }

    pub(super) fn validate_budget_and_record_size(
        &self,
        metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
        permit: Option<&lash_trace::EmissionPermit>,
    ) -> Result<(), StoreError> {
        let node_result = self.validate_node_budget();
        let CommitBudgetLimit::Bounded(max_bytes) = self.commit_budget.bytes else {
            if node_result.is_ok() {
                self.trace_unbounded_byte_budget();
            }
            return node_result;
        };
        let measurement = match self.measure_budget() {
            Ok(measurement) => measurement,
            Err(measurement_error) => {
                return match node_result {
                    Ok(()) => Err(measurement_error),
                    Err(node_error) => Err(node_error),
                };
            }
        };
        let outcome = if node_result.is_err() || measurement.total_bytes > max_bytes.get() {
            "rejected"
        } else {
            "admitted"
        };
        crate::operational_metrics::record_runtime_commit_budgeted_size(
            metrics,
            permit,
            measurement.total_bytes,
            outcome,
        );
        node_result?;
        self.validate_measured_byte_budget(&measurement, max_bytes.get())
    }

    fn validate_node_budget(&self) -> Result<(), StoreError> {
        let graph_rows = self.graph.nodes().len();
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

        Ok(())
    }

    fn trace_unbounded_byte_budget(&self) {
        tracing::trace!(
            target: "lash.runtime_commit.budget",
            session_id = %self.session_id,
            dimension = "bytes",
            measurement = "skipped_unbounded",
            limit = "unbounded",
            outcome = "admitted",
            "runtime commit budget decision"
        );
    }

    fn validate_measured_byte_budget(
        &self,
        measurement: &RuntimeCommitBudgetMeasurement,
        max_bytes: usize,
    ) -> Result<(), StoreError> {
        if measurement.total_bytes > max_bytes {
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
                attachment_referrer_bytes = measurement.attachment_referrer_bytes,
                turn_result_bytes = measurement.turn_result_bytes,
                actual = measurement.total_bytes,
                limit = max_bytes,
                outcome = "rejected",
                "runtime commit budget decision"
            );
            return Err(StoreError::CommitByteBudgetExceeded {
                session_config_bytes: measurement.session_config_bytes,
                graph_delta_bytes: measurement.graph_delta_bytes,
                checkpoint_bytes: measurement.checkpoint_bytes,
                attachment_referrer_bytes: measurement.attachment_referrer_bytes,
                turn_result_bytes: measurement.turn_result_bytes,
                total_bytes: measurement.total_bytes,
                max_bytes,
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
            attachment_referrer_bytes = measurement.attachment_referrer_bytes,
            turn_result_bytes = measurement.turn_result_bytes,
            actual = measurement.total_bytes,
            limit = max_bytes,
            outcome = "admitted",
            "runtime commit budget decision"
        );
        Ok(())
    }

    /// Refuse a session config whose created head no commit fits under
    /// `commit_budget` (FIG-4393): the bare commit over the created head, a
    /// session command's settlement, with the session's initial frame.
    ///
    /// Creation writes the config head in the catalog's own transaction,
    /// outside any runtime commit (FIG-4099), so nothing else measures it,
    /// and every later commit carries the head's config and checkpoint
    /// manifest: a head whose bare commit exceeds the budget refuses every
    /// write, a session command's settlement included.
    pub(super) fn validate_created_head_budget(
        session_id: &crate::SessionId,
        config: crate::PersistedSessionConfig,
        commit_budget: CommitBudget,
        fleet_format: super::FleetFormat,
    ) -> Result<(), StoreError> {
        Self::created_head_budget_probe(session_id, config, commit_budget, fleet_format)?
            .validate_budget()
    }

    fn created_head_budget_probe(
        session_id: &crate::SessionId,
        config: crate::PersistedSessionConfig,
        commit_budget: CommitBudget,
        fleet_format: super::FleetFormat,
    ) -> Result<Self, StoreError> {
        let mut state = crate::RuntimeSessionState {
            session_id: session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                config.turn_budget,
                config.max_tool_calls,
            ))
        };
        crate::session_state::adopt_session_config(&mut state, &config);
        // Every node timestamp occupies exactly 30 bytes; use a deterministic
        // instant for this sizing probe without reading the clock.
        state.ensure_agent_frame_initialized_with_timestamp(|| {
            "1970-01-01T00:00:00.000000001Z"
                .parse()
                .expect("canonical sizing timestamp")
        });
        // A session command's settlement commits under its batch's queue
        // drain; every batch id has the derived id's length.
        let operation = super::OperationId::new(
            crate::ExecutionScope::session_operation(
                session_id.clone(),
                super::queued_work::derive_batch_id(session_id, None, 0, None),
            ),
            "session-command",
        );
        let (commit, _) = Self::persisted_state_with_operation_and_budget(
            &mut state,
            operation,
            commit_budget,
            fleet_format,
        )?;
        Ok(commit)
    }

    pub fn measure_budget(&self) -> Result<RuntimeCommitBudgetMeasurement, StoreError> {
        let measure_json = |result: Result<Vec<u8>, serde_json::Error>| {
            result.map(|bytes| bytes.len()).map_err(|err| {
                StoreError::Backend(format!(
                    "failed to measure runtime commit transaction budget: {err}"
                ))
            })
        };
        let session_config_bytes = measure_json(serde_json::to_vec(&self.config))?;
        let graph_delta_bytes = self.graph.nodes().iter().try_fold(
            0usize,
            |total, node| -> Result<usize, StoreError> {
                Ok(total.saturating_add(measure_json(serde_json::to_vec(node))?))
            },
        )?;
        let checkpoint_root = self
            .checkpoint
            .manifest(crate::store::FleetFormat::current())?;
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
        let attachment_referrer_bytes = self
            .committed_attachment_ids
            .iter()
            .fold(0usize, |total, id| total.saturating_add(id.as_str().len()));
        let charged_outcomes = self
            .command_outcomes
            .iter()
            .filter(|(_, outcome)| !is_uncharged_refusal_receipt(outcome))
            .collect::<std::collections::BTreeMap<_, _>>();
        let command_outcome_bytes = if charged_outcomes.is_empty() {
            0
        } else {
            measure_json(serde_json::to_vec(&charged_outcomes))?
        };
        let turn_result_bytes = measure_json(serde_json::to_vec(&self.turn_commit))?
            .saturating_add(command_outcome_bytes);
        let total_bytes = session_config_bytes
            .saturating_add(graph_delta_bytes)
            .saturating_add(checkpoint_bytes)
            .saturating_add(attachment_referrer_bytes)
            .saturating_add(turn_result_bytes);
        let graph_rows = self.graph.nodes().len();
        let adopted_intent_rows = usize::try_from(self.adopted_intent_rows).unwrap_or(usize::MAX);
        let total_rows = graph_rows.saturating_add(adopted_intent_rows);
        Ok(RuntimeCommitBudgetMeasurement {
            graph_rows,
            adopted_intent_rows,
            total_rows,
            session_config_bytes,
            graph_delta_bytes,
            checkpoint_bytes,
            attachment_referrer_bytes,
            turn_result_bytes,
            total_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    #[test]
    fn created_head_budget_probe_has_a_fixed_full_precision_timestamp() {
        let session_id = SessionId::from("creation-budget-timestamp");
        let config = crate::testing::store_fixtures::root_session_request(&session_id).config;
        let probe = RuntimeCommit::created_head_budget_probe(
            &session_id,
            config,
            CommitBudget::new(CommitBudgetLimit::Unbounded, CommitBudgetLimit::Unbounded),
            super::super::FleetFormat::current(),
        )
        .expect("build the creation sizing probe");
        let nodes = probe.graph.nodes();
        assert_eq!(nodes.len(), 1, "the probe includes the initial frame");
        assert_eq!(
            nodes[0].timestamp.to_string().len(),
            crate::session_graph::NodeTimestamp::WIDTH
        );
        assert_eq!(
            nodes[0].timestamp.to_string(),
            "1970-01-01T00:00:00.000000001Z"
        );
        let reserved = probe.measure_budget().expect("measure the sizing probe");
        let mut realized = probe.clone();
        realized.graph.nodes_mut()[0].timestamp = "2026-10-02T00:00:00.123456789Z"
            .parse()
            .expect("canonical realized timestamp");
        let actual = realized
            .measure_budget()
            .expect("measure the realized frame");
        assert_eq!(
            actual.total_bytes, reserved.total_bytes,
            "timestamp width is invariant"
        );
    }

    #[test]
    fn adopted_intent_rows_count_against_the_node_budget() {
        let state = crate::RuntimeSessionState {
            session_id: SessionId::from("budget-adoption-rows"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        let budget = CommitBudget::new(CommitBudgetLimit::Unbounded, CommitBudgetLimit::bounded(2));
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, budget);
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
            session_id: SessionId::from("budget-bytes"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        let budget = CommitBudget::bounded(128, 512);
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, budget);
        let node = crate::SessionNodeRecord {
            node_id: "budget-node".into(),
            parent_node_id: None,
            timestamp: "2026-07-26T00:00:00.000000000Z"
                .parse()
                .expect("canonical node timestamp"),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("budget", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        commit.graph = crate::GraphAppend::Extend {
            nodes: vec![node.clone()],
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
                .manifest(crate::store::FleetFormat::current())
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
                attachment_referrer_bytes,
                turn_result_bytes,
                total_bytes,
                max_bytes,
            }) if session_config_bytes == expected_session_config_bytes
                && graph_delta_bytes == expected_graph_bytes
                && checkpoint_bytes == expected_checkpoint_bytes
                && attachment_referrer_bytes == expected_attachment_bytes
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

    /// The longest refusals a failed settlement records, every count at
    /// `usize::MAX`, stay within the uncharged receipt's bound.
    #[test]
    fn the_longest_budget_refusals_fit_the_uncharged_receipt() {
        let bytes = StoreError::CommitByteBudgetExceeded {
            session_config_bytes: usize::MAX,
            graph_delta_bytes: usize::MAX,
            checkpoint_bytes: usize::MAX,
            attachment_referrer_bytes: usize::MAX,
            turn_result_bytes: usize::MAX,
            total_bytes: usize::MAX,
            max_bytes: usize::MAX,
        };
        let nodes = StoreError::CommitNodeBudgetExceeded {
            node_count: usize::MAX,
            max_nodes: usize::MAX,
        };
        for refusal in [bytes, nodes] {
            let message = crate::runtime_error::runtime_error_from_store_commit(refusal).message;
            assert!(
                message.len() <= MAX_UNCHARGED_REFUSAL_MESSAGE_BYTES,
                "{} bytes: {message}",
                message.len()
            );
        }
    }

    /// A failed settlement measures as its bare commit (FIG-4471): its
    /// bounded refusal receipt is not charged, so a budget the bare commit
    /// fits admits it. Every other outcome, and an unbounded refusal, is.
    #[test]
    fn a_failed_settlement_measures_as_its_bare_commit() {
        let state = crate::RuntimeSessionState {
            session_id: SessionId::from("budget-failed-settlement"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        let unbounded =
            CommitBudget::new(CommitBudgetLimit::Unbounded, CommitBudgetLimit::Unbounded);
        let bare = RuntimeCommit::persisted_state_for_test_with_budget(&state, unbounded);
        let bare_bytes = bare
            .measure_budget()
            .expect("measure the bare commit")
            .total_bytes;
        let settled = |outcome: crate::SessionCommandOutcome| {
            let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(
                &state,
                CommitBudget::new(
                    CommitBudgetLimit::bounded(bare_bytes),
                    CommitBudgetLimit::Unbounded,
                ),
            );
            commit.turn_commit = bare.turn_commit.clone();
            commit
                .command_outcomes
                .insert(crate::BatchId::from("qwb:settled"), outcome);
            commit
        };
        let refusal = |message: String| crate::SessionCommandOutcome::Failed {
            code: crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded,
            message,
        };

        let failed = settled(refusal("r".repeat(MAX_UNCHARGED_REFUSAL_MESSAGE_BYTES)));
        assert_eq!(
            failed
                .measure_budget()
                .expect("measure the failed settlement")
                .total_bytes,
            bare_bytes
        );
        failed
            .validate_budget()
            .expect("a budget the bare commit fits admits its failed settlement");

        for charged in [
            refusal("r".repeat(MAX_UNCHARGED_REFUSAL_MESSAGE_BYTES + 1)),
            crate::SessionCommandOutcome::AppendSessionNodes {
                outcome: crate::session_append::AppendSessionNodesOutcome::Appended {
                    node_ids: Vec::new(),
                    leaf_node_id: None,
                },
            },
        ] {
            assert!(matches!(
                settled(charged).validate_budget(),
                Err(StoreError::CommitByteBudgetExceeded { max_bytes, .. }) if max_bytes == bare_bytes
            ));
        }
    }
}
