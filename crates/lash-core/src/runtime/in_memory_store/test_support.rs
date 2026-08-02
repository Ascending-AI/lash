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
    use std::sync::Arc;

    use crate::store::{GraphAppend, SessionCommitStore};
    use crate::{
        DeliveryPolicy, MergeKey, QueuedWorkBatch, QueuedWorkCompletion, RuntimeCommit,
        RuntimeSessionState, SessionStoreCreateRequest, SessionStoreFactory, SlotPolicy,
        StoreError, TokenLedgerEntry, TokenUsage, TurnInputCompletion,
    };

    #[tokio::test]
    async fn factory_rejects_occupied_global_node_id_without_partial_usage() {
        let factory = super::super::InMemorySessionStoreFactory::new();
        let request = |session_id: &str| SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::default(),
        };
        factory
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
        let mut second_state = RuntimeSessionState {
            session_id: "second".to_string(),
            ..Default::default()
        };
        second_state.ensure_agent_frame_initialized();
        let usage = TokenLedgerEntry {
            source: "rollback-probe".to_string(),
            model: "test".to_string(),
            usage: TokenUsage {
                input_tokens: 1,
                ..Default::default()
            },
        };
        let commit = RuntimeCommit::persisted_state_for_test(&second_state, &[usage]);
        let occupied_node_id = commit
            .graph
            .nodes
            .first()
            .expect("derived frame node")
            .node_id
            .clone();
        factory
            .global_node_owners
            .lock()
            .expect("lock global node owners")
            .insert(occupied_node_id.clone(), "first".to_string());

        let error = second
            .commit_runtime_state(commit)
            .await
            .expect_err("second session must not reuse an occupied global node id");

        assert!(matches!(
            error,
            StoreError::NodeIdCollision { node_id } if node_id == occupied_node_id
        ));
        assert_eq!(
            second_concrete
                .usage_deltas
                .lock()
                .expect("lock usage")
                .len(),
            0,
            "a rejected ownership collision must not leak usage deltas"
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
        let state = RuntimeSessionState {
            session_id: "session".to_string(),
            ..Default::default()
        };
        store
            .admit_and_bind_session(&crate::SessionBinding::root(
                state.session_id.clone(),
                &state.policy,
            ))
            .await
            .expect("bind stale-claim session");
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

    #[tokio::test]
    async fn rewound_commit_outbox_is_rejected_before_any_state_is_published() {
        let store = super::InMemorySessionStore::new();
        let session_id = "rewound-commit-atomic";
        let process_id = "rewound-commit-process";
        store
            .wake_redelivery_fences
            .lock()
            .expect("lock receiver floors")
            .insert((session_id.to_string(), process_id.to_string()), 7);
        let wake = crate::ProcessWakeDelivery {
            wake_id: "rewound-commit-wake".to_string(),
            target_session_id: session_id.to_string(),
            process_id: process_id.to_string(),
            sequence: 7,
            event_type: "producer.wake".to_string(),
            event_invocation: crate::RuntimeInvocation {
                scope: crate::RuntimeScope::new(session_id),
                subject: crate::RuntimeSubject::ProcessEvent {
                    process_id: process_id.to_string(),
                    sequence: 7,
                    event_type: "producer.wake".to_string(),
                },
                caused_by: None,
                replay: None,
            },
            process_caused_by: None,
            input: "rewound".to_string(),
            created_at_ms: 1,
        };
        let state = RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::default()
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.enqueued_queue_batches = vec![crate::process_wake_batch_draft(wake)];

        let error = store
            .commit_runtime_state(commit)
            .await
            .expect_err("rewound wake must reject the complete commit");

        assert!(matches!(
            error,
            StoreError::ProcessWakeSequenceRewound { .. }
        ));
        assert!(
            store
                .load_session()
                .await
                .expect("load rejected commit")
                .is_none(),
            "a rejected rewound outbox must not publish a session head"
        );
        assert_eq!(
            *store
                .runtime_commit_count
                .lock()
                .expect("lock runtime commit count"),
            0,
            "a rejected rewound outbox must not increment the commit counter"
        );
        assert!(
            store
                .runtime_turn_commits
                .lock()
                .expect("lock runtime turn commits")
                .is_empty(),
            "a rejected rewound outbox must not publish a turn receipt"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_racing_four_creates_never_resurrects_a_session_in_5000_rounds() {
        const ROUNDS: usize = 5_000;

        for round in 0..ROUNDS {
            let factory = Arc::new(super::super::InMemorySessionStoreFactory::new());
            let request = SessionStoreCreateRequest {
                session_id: format!("delete-create-race-{round}"),
                relation: crate::SessionRelation::Root,
                policy: crate::SessionPolicy::default(),
            };
            factory
                .create_store(&request)
                .await
                .expect("materialize session before the race");

            let barrier = Arc::new(tokio::sync::Barrier::new(5));
            let delete_factory = Arc::clone(&factory);
            let delete_barrier = Arc::clone(&barrier);
            let deleted_session_id = request.session_id.clone();
            let delete = crate::task::spawn(async move {
                delete_barrier.wait().await;
                delete_factory.delete_session(&deleted_session_id).await
            });
            let mut creates = Vec::with_capacity(4);
            for _ in 0..4 {
                let create_factory = Arc::clone(&factory);
                let create_barrier = Arc::clone(&barrier);
                let create_request = request.clone();
                creates.push(crate::task::spawn(async move {
                    create_barrier.wait().await;
                    create_factory.create_store(&create_request).await
                }));
            }

            delete
                .await
                .expect("delete task joined")
                .expect("delete completed");
            for create in creates {
                let _ = create.await.expect("create task joined");
            }

            assert!(
                factory
                    .open_existing_store(&request)
                    .await
                    .expect("inspect factory")
                    .is_none(),
                "round {round} left a stale store reachable"
            );
            assert!(matches!(
                factory.create_store(&request).await,
                Err(StoreError::SessionDeleted { session_id })
                    if session_id == request.session_id
            ));
        }
    }
}
