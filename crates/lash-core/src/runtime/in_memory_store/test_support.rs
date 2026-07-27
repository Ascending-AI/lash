use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(crate) async fn save_session_head_meta(&self, meta: crate::SessionHeadMeta) {
        *self.session_head_meta.lock().expect("lock store") = Some(meta);
    }

    pub(crate) fn load_session_count(&self) -> usize {
        self.load_session_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn checkpoint_claim_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

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
    use crate::store::{GraphAppend, SessionCommitStore, StoreMaintenance};
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
        let first_incarnation = first
            .load_session_meta()
            .await
            .expect("load first metadata")
            .expect("first metadata")
            .incarnation_id;
        let second_incarnation = second
            .load_session_meta()
            .await
            .expect("load second metadata")
            .expect("second metadata")
            .incarnation_id;
        let commit = |session_id: &str, incarnation_id: crate::IncarnationId| {
            let mut state = RuntimeSessionState {
                session_id: session_id.to_string(),
                session_lifetime: crate::SessionLifetime::durable(incarnation_id),
                ..Default::default()
            };
            state.ensure_agent_frame_initialized();
            let node = crate::SessionNodeRecord {
                node_id: "factory-global-node".to_string(),
                parent_node_id: state.session_graph.leaf_node_id.clone(),
                timestamp: "2026-07-26T00:00:00Z".to_string(),
                payload: crate::SessionNodePayload::Event {
                    event: crate::SessionHistoryRecord::Protocol(
                        crate::ProtocolEvent::typed("global-collision", serde_json::Value::Null)
                            .expect("protocol event"),
                    ),
                },
            };
            state.session_graph.push_node_record(node.clone());
            state.session_graph.set_leaf_node_id(Some(node.node_id));
            let usage = TokenLedgerEntry {
                source: "rollback-probe".to_string(),
                model: "test".to_string(),
                usage: TokenUsage {
                    input_tokens: 1,
                    ..Default::default()
                },
            };
            RuntimeCommit::persisted_state_for_test(&state, &[usage])
        };

        first
            .commit_runtime_state(commit("first", first_incarnation))
            .await
            .expect("first node insert");
        let error = second
            .commit_runtime_state(commit("second", second_incarnation))
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
    async fn head_move_zero_confirmation_aborts_a_corrupt_low_count() {
        let factory = super::super::InMemorySessionStoreFactory::new();
        let request = SessionStoreCreateRequest {
            session_id: "head-move-refcount-drift".to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::default(),
        };
        let store = factory.create_store(&request).await.expect("create store");
        let concrete = factory
            .stores
            .lock()
            .expect("lock stores")
            .get(&request.session_id)
            .cloned()
            .expect("concrete store");
        let mut state = RuntimeSessionState {
            session_id: request.session_id.clone(),
            session_lifetime: crate::SessionLifetime::durable(
                store
                    .load_session_meta()
                    .await
                    .expect("load metadata")
                    .expect("metadata")
                    .incarnation_id,
            ),
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();
        let first = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("commit root frame");
        let leaf = concrete
            .raw_leaf_node_id_for_testing()
            .expect("persisted leaf");
        concrete.corrupt_node_refcount_for_testing(&leaf, 0);

        let child = crate::SessionNodeRecord {
            node_id: "head-move-refcount-drift-child".to_string(),
            parent_node_id: Some(leaf.clone()),
            timestamp: "2026-07-27T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Event {
                event: crate::SessionHistoryRecord::Protocol(
                    crate::ProtocolEvent::typed("child", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state_with_graph_commit(
            &state,
            GraphAppend {
                nodes: vec![child.clone()],
                leaf_node_id: Some(child.node_id.clone()),
            },
            &[],
        );
        commit.expected_head_revision = first.head_revision;
        let error = store
            .commit_runtime_state(commit.clone())
            .await
            .expect_err("low cached count must abort the head move");

        assert!(matches!(
            error,
            StoreError::NodeRefcountDrift {
                node_id,
                cached: 0,
                derived: 1,
            } if node_id == leaf
        ));
        assert_eq!(concrete.raw_head_revision_for_testing(), Some(1));
        assert!(
            store
                .load_node(&child.node_id)
                .await
                .expect("load child after rejected head move")
                .is_none(),
            "the failed zero-confirmation must roll the append back"
        );

        concrete.corrupt_node_refcount_for_testing(&leaf, 1);
        store
            .commit_runtime_state(commit)
            .await
            .expect("head move after repairing count");
        assert_eq!(concrete.raw_head_revision_for_testing(), Some(2));
    }

    #[tokio::test]
    async fn refcount_scrub_counts_anchor_roots_when_detecting_drift() {
        let store = super::InMemorySessionStore::new();
        let mut state = RuntimeSessionState {
            session_id: "scrub-refcount-drift".to_string(),
            ..Default::default()
        };
        let incarnation_id = store
            .ensure_session_incarnation(&state.session_id, &state.policy)
            .await
            .expect("realize scrub session lifetime");
        state.bind_durable_incarnation(incarnation_id);
        state.ensure_agent_frame_initialized();
        store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("commit root frame");
        let leaf = store
            .raw_leaf_node_id_for_testing()
            .expect("persisted leaf");
        let checkpoint_ref = store
            .session_head_meta
            .lock()
            .expect("lock head")
            .as_ref()
            .and_then(|head| head.checkpoint_ref.clone())
            .expect("checkpoint ref");
        let checkpoint = store
            .checkpoint
            .lock()
            .expect("lock checkpoint")
            .clone()
            .expect("checkpoint");
        store.node_anchors.lock().expect("lock anchors").insert(
            leaf.clone(),
            (
                checkpoint_ref,
                checkpoint,
                "scrub-refcount-drift".to_string(),
            ),
        );
        store.corrupt_node_refcount_for_testing(&leaf, 3);

        let error = store
            .verify_node_refcounts()
            .await
            .expect_err("scrub must detect cached count drift");

        assert!(matches!(
            error,
            StoreError::NodeRefcountDrift {
                node_id,
                cached: 3,
                derived: 2,
            } if node_id == leaf
        ));
    }

    #[tokio::test]
    async fn budget_rejection_happens_before_backend_transaction_work() {
        let store = super::InMemorySessionStore::new();
        let state = RuntimeSessionState {
            session_id: "budget-before-backend".to_string(),
            session_lifetime: crate::SessionLifetime::durable(
                crate::IncarnationId::mint_for_store(),
            ),
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
        commit.graph = GraphAppend {
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
        let mut state = RuntimeSessionState {
            session_id: "session".to_string(),
            ..Default::default()
        };
        let incarnation_id = store
            .ensure_session_incarnation(&state.session_id, &state.policy)
            .await
            .expect("realize stale-claim session lifetime");
        state.bind_durable_incarnation(incarnation_id);
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
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
            store.load_session().await.expect("load session").is_none(),
            "the failed commit must not create a session head"
        );
    }
}
