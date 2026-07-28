use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(crate) fn fail_next_session_execution_lease_renewal(&self) {
        self.fail_next_session_execution_lease_renewal
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn session_execution_lease_renewal_count(&self) -> usize {
        self.session_execution_lease_renewal_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn abandoned_claim_counts(&self) -> (usize, usize) {
        (
            self.abandoned_queued_work_claim_count
                .load(std::sync::atomic::Ordering::SeqCst),
            self.abandoned_turn_input_claim_count
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    pub(crate) fn commit_write_transaction_count(&self) -> usize {
        self.commit_write_transaction_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use crate::store::{GraphCommitDelta, SessionCommitStore};
    use crate::{
        DeliveryPolicy, MergeKey, QueuedWorkBatch, QueuedWorkCompletion, RuntimeCommit,
        RuntimeSessionState, SessionStoreCreateRequest, SessionStoreFactory, SlotPolicy,
        StoreError, TokenLedgerEntry, TokenUsage, TurnInputCompletion,
    };

    #[tokio::test]
    async fn factory_enforces_global_node_ids_across_sessions() {
        let factory = super::super::InMemorySessionStoreFactory::new();
        let request = |session_id: &str| SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::default(),
        };
        let first = factory
            .create_store(&request("first"))
            .await
            .expect("first store");
        let second = factory
            .create_store(&request("second"))
            .await
            .expect("second store");
        let second_concrete = factory
            .stores
            .lock()
            .expect("lock stores")
            .get("second")
            .cloned()
            .expect("second concrete store");
        let node = crate::SessionNodeRecord {
            node_id: "factory-global-node".to_string(),
            parent_node_id: None,
            caused_by: None,
            agent_frame_id: None,
            timestamp: "2026-07-26T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("global-node-id", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let commit = |session_id: &str| {
            let state = RuntimeSessionState {
                session_id: session_id.to_string(),
                ..Default::default()
            };
            let usage = TokenLedgerEntry {
                source: "rollback-probe".to_string(),
                model: "test".to_string(),
                usage: TokenUsage {
                    input_tokens: 1,
                    ..Default::default()
                },
            };
            let mut commit = RuntimeCommit::persisted_state(&state, &[usage]);
            commit.graph = GraphCommitDelta::Append {
                nodes: vec![node.clone()],
                leaf_node_id: Some(node.node_id.clone()),
            };
            commit
        };

        first
            .commit_runtime_state(commit("first"))
            .await
            .expect("first node insert");
        let error = second
            .commit_runtime_state(commit("second"))
            .await
            .expect_err("second session must not reuse a global node id");

        assert!(matches!(
            error,
            crate::StoreError::NodeIdCollision { node_id }
                if node_id == "factory-global-node"
        ));
        assert_eq!(
            second_concrete
                .usage_deltas
                .lock()
                .expect("lock usage")
                .len(),
            0,
            "the usage mutation preceding node ownership must not leak from a rejected commit"
        );
    }

    #[tokio::test]
    async fn budget_rejection_happens_before_backend_transaction_work() {
        let store = super::InMemorySessionStore::new();
        let state = RuntimeSessionState {
            session_id: "budget-before-backend".to_string(),
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

        let error = store
            .commit_runtime_state(commit)
            .await
            .expect_err("over-budget commit");

        assert!(matches!(error, StoreError::CommitNodeBudgetExceeded { .. }));
        assert_eq!(
            store.commit_write_transaction_count(),
            0,
            "budget validation must reject before the backend transaction boundary"
        );
    }

    #[tokio::test]
    async fn stale_claim_validation_cannot_partially_mutate_a_commit() {
        let store = super::InMemorySessionStore::new();
        store.queued_work.lock().expect("lock queued work").push(
            super::super::InMemoryQueuedBatch {
                batch: QueuedWorkBatch {
                    batch_id: "batch".to_string(),
                    session_id: "session".to_string(),
                    enqueue_seq: 1,
                    source_key: None,
                    delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
                    slot_policy: SlotPolicy::Join,
                    merge_key: MergeKey::Never,
                    available_at_ms: 0,
                    enqueued_at_ms: 0,
                    items: Vec::new(),
                },
                claim_id: Some("queue-claim".to_string()),
                claim_token: Some("queue-token".to_string()),
                claim_owner: None,
                claim_fencing_token: 1,
                claim_session_lease_generation: 1,
            },
        );
        let state = RuntimeSessionState {
            session_id: "session".to_string(),
            ..Default::default()
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
        commit.completed_queue_claims = vec![QueuedWorkCompletion {
            session_id: "session".to_string(),
            claim_id: "queue-claim".to_string(),
            lease_token: "queue-token".to_string(),
            batch_ids: vec!["batch".to_string()],
        }];
        commit.completed_turn_input_claims = vec![TurnInputCompletion {
            session_id: "session".to_string(),
            claim_id: "stale-input-claim".to_string(),
            lease_token: "stale-input-token".to_string(),
            input_ids: vec!["missing-input".to_string()],
            applications: Vec::new(),
        }];

        let error = store
            .commit_runtime_state(commit)
            .await
            .expect_err("stale turn-input claim");

        assert!(matches!(error, StoreError::TurnInputClaimSuperseded { .. }));
        assert_eq!(
            store.queued_work.lock().expect("lock queued work").len(),
            1,
            "a later validation failure must not consume an earlier queue claim"
        );
        assert!(
            store
                .load_session(crate::SessionReadScope::FullGraph)
                .await
                .expect("load session")
                .is_none(),
            "the failed commit must not create a session head"
        );
    }
}
