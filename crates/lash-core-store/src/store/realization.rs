use super::{RuntimeCommit, RuntimeCommitReceipt, SessionCommitStore, StoreError};

/// Commit through the production realization boundary.
///
/// # Panics
/// Panics in every build profile if a newly committed receipt fails to advance
/// the expected revision. Receipt replay performs no new commit and is exempt.
pub async fn commit_runtime_state_verified(
    store: &(dyn SessionCommitStore + '_),
    commit: RuntimeCommit,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let meta = store
        .load_session_meta_for_commit(&commit.session_id)
        .await?
        .ok_or_else(|| StoreError::SessionBindingNotMaterialized {
            session_id: commit.session_id.clone(),
        })?;
    if meta.session_id != commit.session_id {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "SessionMeta",
            message: format!(
                "metadata read for session `{}` names session `{}`",
                commit.session_id, meta.session_id
            ),
        });
    }
    commit.validate_budget_and_record_size()?;
    let expected_revision = commit.expected_head_revision;
    let receipt = store.commit_runtime_state(commit).await?;
    assert!(
        receipt.receipt_replayed || receipt.head_revision > expected_revision,
        "committed head revision must advance"
    );
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;
    use crate::session_graph::RealizedNodeTimestamp;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FacadeTestStore {
        commit_attempts: AtomicUsize,
        materialized_session: Option<SessionId>,
        planner_validates_budget: bool,
        advances_revision: bool,
        replayed: bool,
    }

    #[async_trait::async_trait]
    impl SessionCommitStore for FacadeTestStore {
        async fn read_session_state_version(
            &self,
            _session_id: &SessionId,
        ) -> Result<u32, StoreError> {
            Ok(super::super::OLDEST_SUPPORTED_SESSION_STATE_VERSION)
        }

        async fn load_session_head_meta(
            &self,
            _session_id: &SessionId,
        ) -> Result<Option<super::super::SessionHeadMeta>, StoreError> {
            Ok(None)
        }

        async fn retain_admission_base(
            &self,
            _fence: &super::super::DriveFence,
            _base: &super::super::SessionHeadRef,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        async fn committed_turn_exists(
            &self,
            _session_id: &SessionId,
            _turn_id: &crate::TurnId,
        ) -> Result<bool, StoreError> {
            Ok(false)
        }

        async fn drain_end_exists(
            &self,
            _session_id: &SessionId,
            _drain_id: &str,
        ) -> Result<bool, StoreError> {
            Ok(false)
        }

        async fn commit_runtime_state(
            &self,
            commit: RuntimeCommit,
        ) -> Result<RuntimeCommitReceipt, StoreError> {
            self.commit_attempts.fetch_add(1, Ordering::SeqCst);
            if self.planner_validates_budget {
                super::super::RuntimeCommitPlanner::prepare(
                    commit.clone(),
                    super::super::FleetFormat::current(),
                )?;
            }
            let realized_node_timestamps = commit
                .graph
                .appended_nodes()
                .map(|node| RealizedNodeTimestamp {
                    node_id: node.node_id.clone(),
                    timestamp: node.timestamp.clone(),
                })
                .collect();
            let manifest = commit
                .checkpoint
                .manifest(super::super::FleetFormat::current())?;
            Ok(RuntimeCommitReceipt {
                schema_version: crate::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
                head_revision: commit.expected_head_revision + u64::from(self.advances_revision),
                checkpoint_ref: "empty-frame-facade".to_string().into(),
                manifest,
                committed_leaf_node_id: commit
                    .graph
                    .leaf_node_id()
                    .or(commit.graph_base_leaf_node_id.as_ref())
                    .cloned(),
                realized_node_timestamps,
                committed_usage_delta_identities: commit
                    .usage_deltas
                    .iter()
                    .map(|delta| delta.identity.clone())
                    .collect(),
                failure_evidence: commit.failure_evidence.clone(),
                outcome: commit.outcome.clone(),
                pending_follow_on: None,
                turn_input_applications: Vec::new(),
                turn_cancel_input_outcome: crate::TurnCancelInputOutcome::default(),
                receipt_replayed: self.replayed,
            })
        }

        async fn raise_pending_follow_on_attempts(
            &self,
            fence: &super::super::DriveFence,
            follow_on_turn_id: &crate::TurnId,
        ) -> Result<super::super::PendingFollowOn, StoreError> {
            Err(StoreError::FollowOnNotPending {
                session_id: fence.session().clone(),
                follow_on_turn_id: follow_on_turn_id.clone(),
            })
        }

        async fn save_session_meta(
            &self,
            _meta: super::super::SessionMeta,
        ) -> Result<(), StoreError> {
            Ok(())
        }

        async fn load_session_meta(
            &self,
            _session_id: &SessionId,
        ) -> Result<Option<super::super::SessionMeta>, StoreError> {
            Ok(self
                .materialized_session
                .as_ref()
                .map(|session_id| super::super::SessionMeta {
                    owning_process_id: None,
                    session_id: session_id.clone(),
                    relation: crate::SessionRelation::Root,
                    pending_observer_intents: Vec::new(),
                }))
        }

        async fn load_session_meta_for_commit(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<super::super::SessionMeta>, StoreError> {
            self.load_session_meta(session_id).await
        }

        async fn record_turn_park(
            &self,
            _park: &super::super::TurnParkWrite,
        ) -> Result<super::super::TurnPark, StoreError> {
            Err(StoreError::UnsupportedStoreOperation {
                operation: "record_turn_park",
            })
        }

        async fn load_turn_park(
            &self,
            _session_id: &SessionId,
        ) -> Result<Option<super::super::TurnPark>, StoreError> {
            Ok(None)
        }
    }

    #[cfg(feature = "otel-trace")]
    #[tokio::test]
    async fn verified_commit_records_one_budget_histogram_observation_across_planner_validation() {
        let metrics = crate::operational_metrics::TestMetrics::install();
        let state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        let store = FacadeTestStore {
            materialized_session: Some(state.session_id.clone()),
            planner_validates_budget: true,
            advances_revision: true,
            ..Default::default()
        };

        commit_runtime_state_verified(&store, RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("the bounded commit should be admitted");

        assert_eq!(store.commit_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            metrics.histogram_count("lash.runtime_commit.budgeted_size"),
            1,
            "the facade owns the only histogram observation even when the planner revalidates"
        );
    }

    #[cfg(feature = "otel-trace")]
    #[tokio::test]
    async fn verified_commit_records_node_budget_rejection_after_binding() {
        let metrics = crate::operational_metrics::TestMetrics::install();
        let state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        let store = FacadeTestStore {
            materialized_session: Some(state.session_id.clone()),
            ..Default::default()
        };
        let budget = super::super::CommitBudget::bounded(1024 * 1024, 1);
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget);
        commit.adopted_intent_rows = 2;

        let error = commit_runtime_state_verified(&store, commit)
            .await
            .expect_err("the node budget must reject the commit");

        assert!(matches!(
            error,
            StoreError::CommitNodeBudgetExceeded {
                node_count: 2,
                max_nodes: 1,
            }
        ));
        assert_eq!(store.commit_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            metrics.histogram_count("lash.runtime_commit.budgeted_size"),
            1,
            "a bounded node rejection is a rejected budget observation"
        );
    }

    #[cfg(feature = "otel-trace")]
    #[tokio::test]
    async fn verified_commit_does_not_record_budget_before_binding_fences() {
        let metrics = crate::operational_metrics::TestMetrics::install();
        let state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);

        let missing_error =
            commit_runtime_state_verified(&FacadeTestStore::default(), commit.clone())
                .await
                .expect_err("missing binding metadata must fence the commit");
        assert!(matches!(
            missing_error,
            StoreError::SessionBindingNotMaterialized { .. }
        ));

        let mismatch_error = commit_runtime_state_verified(
            &FacadeTestStore {
                materialized_session: Some(SessionId::from("different-session")),
                ..Default::default()
            },
            commit,
        )
        .await
        .expect_err("metadata naming another session must fence the commit");
        assert!(matches!(
            mismatch_error,
            StoreError::StoredDataCorrupt {
                record_kind: "SessionMeta",
                ..
            }
        ));
        assert_eq!(
            metrics.histogram_count("lash.runtime_commit.budgeted_size"),
            0,
            "binding failures are not commit-budget decisions"
        );
    }

    #[cfg(feature = "otel-trace")]
    #[tokio::test]
    async fn verified_commit_skips_histogram_for_unbounded_byte_budget() {
        let metrics = crate::operational_metrics::TestMetrics::install();
        let state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        let store = FacadeTestStore {
            materialized_session: Some(state.session_id.clone()),
            planner_validates_budget: true,
            advances_revision: true,
            ..Default::default()
        };
        let budget = super::super::CommitBudget::new(
            super::super::CommitBudgetLimit::Unbounded,
            super::super::CommitBudgetLimit::Unbounded,
        );

        commit_runtime_state_verified(
            &store,
            RuntimeCommit::persisted_state_for_test_with_budget(&state, &[], budget),
        )
        .await
        .expect("the unbounded commit should be admitted");

        assert_eq!(store.commit_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            metrics.histogram_count("lash.runtime_commit.budgeted_size"),
            0,
            "an unbounded byte budget skips measurement"
        );
    }

    #[tokio::test]
    async fn verified_commit_rejects_a_store_that_lies_about_materializing_admission() {
        let store = FacadeTestStore::default();
        let mut state = crate::RuntimeSessionState {
            session_id: SessionId::from("loose-store-session"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();

        let err = commit_runtime_state_verified(
            &store,
            RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .expect_err("a loose store must fail before its first commit");

        assert!(matches!(
            err,
            StoreError::SessionBindingNotMaterialized { session_id }
                if session_id == state.session_id
        ));
        assert_eq!(
            store.commit_attempts.load(Ordering::SeqCst),
            0,
            "the loose store must not receive the commit"
        );
    }

    #[tokio::test]
    async fn verified_commit_rejects_node_budget_before_calling_a_non_validating_store() {
        let state = crate::RuntimeSessionState {
            session_id: SessionId::from("boundary-budget"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let store = FacadeTestStore {
            materialized_session: Some(state.session_id.clone()),
            ..Default::default()
        };
        let node = crate::SessionNodeRecord {
            node_id: "node".into(),
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
        commit.graph = super::super::GraphAppend::Extend {
            nodes: (0..=RuntimeCommit::MAX_COMMIT_NODE_COUNT)
                .map(|index| crate::SessionNodeRecord {
                    node_id: format!("node-{index}").into(),
                    ..node.clone()
                })
                .collect(),
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
    #[tokio::test]
    #[should_panic(expected = "committed head revision must advance")]
    async fn verified_commit_rejects_nonadvancing_store_receipt() {
        let mut state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        state.ensure_agent_frame_initialized();
        let store = FacadeTestStore {
            materialized_session: Some(state.session_id.clone()),
            ..Default::default()
        };
        let _ = commit_runtime_state_verified(
            &store,
            RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await;
    }

    #[tokio::test]
    async fn verified_commit_preserves_nonadvancing_receipt_replay() {
        let mut state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
        ));
        state.ensure_agent_frame_initialized();
        let store = FacadeTestStore {
            materialized_session: Some(state.session_id.clone()),
            replayed: true,
            ..Default::default()
        };
        let receipt = commit_runtime_state_verified(
            &store,
            RuntimeCommit::persisted_state_for_test(&state, &[]),
        )
        .await
        .unwrap();
        assert!(receipt.receipt_replayed);
        assert_eq!(receipt.head_revision, state.head_revision);
    }
}
