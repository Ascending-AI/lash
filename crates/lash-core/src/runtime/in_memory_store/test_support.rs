use std::sync::Arc;

use super::InMemorySessionStore;

/// Test seam that suspends `release_session_execution_lease` at its backend
/// await so a caller can drop the release future at exactly that point.
#[derive(Debug)]
pub(crate) struct SessionExecutionLeaseReleaseGate {
    entered: tokio::sync::Notify,
    admitted: tokio::sync::Semaphore,
}

impl Default for SessionExecutionLeaseReleaseGate {
    fn default() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            admitted: tokio::sync::Semaphore::new(0),
        }
    }
}

impl SessionExecutionLeaseReleaseGate {
    /// Wait until a release attempt has reached the gate.
    pub(crate) async fn wait_entered(&self) {
        self.entered.notified().await;
    }

    /// Let exactly one release attempt through.
    pub(crate) fn admit_one(&self) {
        self.admitted.add_permits(1);
    }

    pub(super) async fn enter(&self) {
        self.entered.notify_one();
        self.admitted
            .acquire()
            .await
            .expect("lease release gate stays open")
            .forget();
    }
}

impl InMemorySessionStore {
    pub(super) fn run_claim_after_lease_validation_hook(&self) {
        let hook = self
            .claim_after_lease_validation_hook
            .lock()
            .expect("lock claim validation hook")
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn set_claim_after_lease_validation_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self
            .claim_after_lease_validation_hook
            .lock()
            .expect("lock claim validation hook") = Some(hook);
    }

    pub(crate) fn fail_next_exact_queue_claim(&self) {
        self.fail_next_exact_queue_claim
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn drop_next_list_queued_work_batch(&self) {
        self.drop_next_list_queued_work_batch
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn drop_next_list_pending_queued_work_batch(&self) {
        self.drop_next_list_pending_queued_work_batch
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn fail_next_runtime_commit(&self, error: crate::StoreError) {
        *self
            .fail_next_runtime_commit
            .lock()
            .expect("lock next runtime commit failure") = Some(error);
    }

    pub(crate) fn fail_next_runtime_commit_after_first_mutation(&self, error: crate::StoreError) {
        *self
            .fail_next_runtime_commit_after_first_mutation
            .lock()
            .expect("lock post-mutation runtime commit failure") = Some(error);
    }

    pub(super) fn fail_after_first_runtime_commit_mutation_if_requested(
        &self,
        session_meta_before_commit: Option<crate::SessionMeta>,
    ) -> Result<(), crate::StoreError> {
        if let Some(error) = self
            .fail_next_runtime_commit_after_first_mutation
            .lock()
            .expect("lock post-mutation runtime commit failure")
            .take()
        {
            *self.session_meta.lock().expect("lock session meta") = session_meta_before_commit;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn save_session_head_meta(&self, meta: crate::SessionHeadMeta) {
        *self.session_head_meta.lock().expect("lock store") = Some(meta);
    }

    pub(crate) fn load_session_count(&self) -> usize {
        self.load_session_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn fail_load_session_on_call(&self, call: usize) {
        *self
            .fail_load_session_on_call
            .lock()
            .expect("lock load-session failure injection") = Some(call);
    }

    pub(crate) fn checkpoint_claim_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Inject a transient renewal rejection (the lease stays durably ours).
    pub(crate) fn fail_next_session_execution_lease_renewal(&self) {
        self.fail_next_session_execution_lease_renewal_with(crate::StoreError::Backend(
            "injected session execution lease renewal rejection".to_string(),
        ));
    }

    /// Inject a specific renewal rejection. Transient errors and a definitive
    /// `SessionExecutionLeaseExpired` mean different things to a lease guard.
    pub(crate) fn fail_next_session_execution_lease_renewal_with(&self, error: crate::StoreError) {
        *self
            .fail_next_session_execution_lease_renewal
            .lock()
            .expect("lock injected renewal failure") = Some(error);
    }

    /// Replace the next successful backend renewal response without changing
    /// the durable row, emulating a corrupt or mis-targeted backend result.
    pub(crate) fn respond_to_next_session_execution_lease_renewal_with(
        &self,
        response: crate::SessionExecutionLease,
    ) {
        *self
            .next_session_execution_lease_renewal_response
            .lock()
            .expect("lock injected renewal response") = Some(response);
    }

    /// Suspend every subsequent `release_session_execution_lease` at its
    /// backend await until the returned gate admits it.
    pub(crate) fn gate_session_execution_lease_release(
        &self,
    ) -> Arc<SessionExecutionLeaseReleaseGate> {
        let gate = Arc::new(SessionExecutionLeaseReleaseGate::default());
        *self
            .session_execution_lease_release_gate
            .lock()
            .expect("lock lease release gate") = Some(Arc::clone(&gate));
        gate
    }

    pub(crate) fn session_execution_lease_release_attempt_count(&self) -> usize {
        self.session_execution_lease_release_attempt_count
            .load(std::sync::atomic::Ordering::SeqCst)
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

    pub(crate) fn force_active_leaf_for_testing(&self, leaf_node_id: String) {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory test branch switch");
        let mut meta = self.session_head_meta.lock().expect("lock session head");
        let meta = meta.as_mut().expect("branch switch requires session head");
        meta.head_revision += 1;
        meta.leaf_node_id = Some(leaf_node_id.clone());
        self.session_graph
            .lock()
            .expect("lock resident graph")
            .set_leaf_node_id(Some(leaf_node_id));
    }

    pub(crate) fn tombstone_node_for_testing(&self, node_id: String) {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory test tombstone");
        self.tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .insert(node_id);
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
            data: crate::QueuedWorkCompletionData {
                batch_ids: vec!["batch".to_string()],
            },
        }];
        commit.completed_turn_input_claims = vec![TurnInputCompletion {
            session_id: "session".to_string(),
            claim_id: "stale-input-claim".to_string(),
            lease_token: "stale-input-token".to_string(),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["missing-input".to_string()],
                applications: Vec::new(),
            },
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
