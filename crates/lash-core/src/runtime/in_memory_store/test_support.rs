use lash_sansio::sync::MutexExt;
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
    pub(super) fn refuse_injected_counter_defect(
        &self,
        field: &'static str,
    ) -> Result<(), crate::StoreError> {
        if let Some(value) = self.raw_counter_defects.lock_recover().get(field).copied() {
            let (record_kind, stored_field) = match field {
                "queued_work_claim_fencing_token" => ("QueuedWorkBatch", "claim_fencing_token"),
                "session_head_revision" => ("SessionHeadMeta", "head_revision"),
                "session_lease_fencing_token" => ("SessionExecutionLease", "fencing_token"),
                _ => ("InMemoryDurableRecord", field),
            };
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind,
                message: format!("{stored_field} must be non-negative, got {value}"),
            });
        }
        Ok(())
    }

    pub(crate) fn inject_raw_counter_for_testing(
        &self,
        field: &'static str,
        record_id: &str,
        value: i64,
    ) {
        if value < 0 {
            self.raw_counter_defects
                .lock_recover()
                .insert(field.to_string(), value);
            return;
        }
        let value = value as u64;
        match field {
            "queued_work_claim_fencing_token" => {
                let mut queued = self.queued_work.lock_recover();
                let row = queued
                    .iter_mut()
                    .find(|row| row.batch.batch_id == record_id)
                    .expect("queued-work counter injection row");
                row.claim_fencing_token = value;
            }
            "session_head_revision" => {
                self.session_head_meta
                    .lock_recover()
                    .as_mut()
                    .expect("session-head counter injection row")
                    .head_revision = value;
            }
            "session_lease_fencing_token" => {
                self.session_execution_leases
                    .lock_recover()
                    .get_mut(record_id)
                    .expect("session-lease counter injection row")
                    .fencing_token = value;
            }
            other => panic!("unsupported in-memory raw counter injection field: {other}"),
        }
    }

    pub(crate) fn raw_counter_snapshot_for_testing(
        &self,
        field: &'static str,
        record_id: &str,
    ) -> String {
        if let Some(value) = self.raw_counter_defects.lock_recover().get(field) {
            return format!("defect:{field}:{value}");
        }
        match field {
            "queued_work_claim_fencing_token" => {
                let queued = self.queued_work.lock_recover();
                let row = queued
                    .iter()
                    .find(|row| row.batch.batch_id == record_id)
                    .expect("queued-work counter snapshot row");
                format!(
                    "{}:{:?}:{:?}:{}",
                    row.claim_fencing_token,
                    row.claim_id,
                    row.claim_token,
                    row.claim_session_lease_generation
                )
            }
            "session_head_revision" => {
                let head = self.session_head_meta.lock_recover();
                let row = head.as_ref().expect("session-head counter snapshot row");
                format!(
                    "{}:{:?}:{:?}",
                    row.head_revision, row.checkpoint_ref, row.leaf_node_id
                )
            }
            "session_lease_fencing_token" => {
                let leases = self.session_execution_leases.lock_recover();
                let row = leases
                    .get(record_id)
                    .expect("session-lease counter snapshot row");
                format!(
                    "{}:{:?}:{:?}:{}:{}",
                    row.fencing_token,
                    row.owner,
                    row.lease_token,
                    row.claimed_at_epoch_ms,
                    row.expires_at_epoch_ms
                )
            }
            other => panic!("unsupported in-memory raw counter snapshot field: {other}"),
        }
    }
    pub(super) fn run_claim_after_lease_validation_hook(&self) {
        let hook = self.claim_after_lease_validation_hook.lock_recover().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn set_claim_after_lease_validation_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.claim_after_lease_validation_hook.lock_recover() = Some(hook);
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
        *self.fail_next_runtime_commit.lock_recover() = Some(error);
    }

    pub(crate) fn fail_next_runtime_commit_after_first_mutation(&self, error: crate::StoreError) {
        *self
            .fail_next_runtime_commit_after_first_mutation
            .lock_recover() = Some(error);
    }

    pub(super) fn fail_after_first_runtime_commit_mutation_if_requested(
        &self,
        session_meta_before_commit: Option<crate::SessionMeta>,
    ) -> Result<(), crate::StoreError> {
        if let Some(error) = self
            .fail_next_runtime_commit_after_first_mutation
            .lock_recover()
            .take()
        {
            *self.session_meta.lock_recover() = session_meta_before_commit;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn save_session_head_meta(&self, meta: crate::SessionHeadMeta) {
        *self.session_head_meta.lock_recover() = Some(meta);
    }

    pub(crate) fn load_session_count(&self) -> usize {
        self.load_session_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn fail_load_session_on_call(&self, call: usize) {
        *self.fail_load_session_on_call.lock_recover() = Some(call);
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
            .lock_recover() = Some(error);
    }

    /// Make the next already-validated conditional renewal mutation match no
    /// lease, mirroring a zero-row compare-and-set result in a SQL backend.
    pub(crate) fn force_next_session_execution_lease_renewal_zero_match(&self) {
        self.force_next_session_execution_lease_renewal_zero_match
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Replace the next successful backend renewal response without changing
    /// the durable row, emulating a corrupt or mis-targeted backend result.
    pub(crate) fn respond_to_next_session_execution_lease_renewal_with(
        &self,
        response: crate::SessionExecutionLease,
    ) {
        *self
            .next_session_execution_lease_renewal_response
            .lock_recover() = Some(response);
    }

    /// Suspend every subsequent `release_session_execution_lease` at its
    /// backend await until the returned gate admits it.
    pub(crate) fn gate_session_execution_lease_release(
        &self,
    ) -> Arc<SessionExecutionLeaseReleaseGate> {
        let gate = Arc::new(SessionExecutionLeaseReleaseGate::default());
        *self.session_execution_lease_release_gate.lock_recover() = Some(Arc::clone(&gate));
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
        let _transaction = self.write_transaction.lock_recover();
        let mut meta = self.session_head_meta.lock_recover();
        let meta = meta.as_mut().expect("branch switch requires session head");
        meta.head_revision += 1;
        meta.leaf_node_id = Some(leaf_node_id.clone());
        self.session_graph
            .lock_recover()
            .set_leaf_node_id(Some(leaf_node_id));
    }

    pub(crate) fn tombstone_node_for_testing(&self, node_id: String) {
        let _transaction = self.write_transaction.lock_recover();
        self.tombstoned_node_ids.lock_recover().insert(node_id);
    }
}

#[cfg(test)]
mod tests {
    use lash_sansio::sync::MutexExt;

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
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
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
            .lock_recover()
            .get("second")
            .cloned()
            .expect("second concrete store");
        let mut second_state = RuntimeSessionState {
            session_id: "second".to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            .lock_recover()
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
            second_concrete.usage_deltas.lock_recover().len(),
            0,
            "a rejected ownership collision must not leak usage deltas"
        );
    }

    #[tokio::test]
    async fn budget_rejection_happens_before_backend_transaction_work() {
        let store = super::InMemorySessionStore::new();
        let state = RuntimeSessionState {
            session_id: "budget-before-backend".to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
        let mut commit = RuntimeCommit::persisted_state_for_test_with_budget(
            &state,
            &[],
            crate::CommitBudget::new(
                crate::CommitBudgetLimit::Unbounded,
                crate::CommitBudgetLimit::bounded(2),
            ),
        );
        commit.graph = GraphAppend {
            nodes: (0..=2)
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

        assert!(matches!(
            error,
            StoreError::CommitNodeBudgetExceeded {
                node_count: 3,
                max_nodes: 2,
            }
        ));
        assert_eq!(
            store.commit_write_transaction_count(),
            0,
            "budget validation must reject before the backend transaction boundary"
        );
    }

    #[test]
    fn queued_work_claim_refuses_exhausted_in_memory_fence() {
        let store = super::super::InMemorySessionStore::new();
        store
            .queued_work
            .lock_recover()
            .push(super::super::InMemoryQueuedBatch {
                batch: QueuedWorkBatch {
                    batch_id: "exhausted-batch".to_string(),
                    session_id: "session".to_string(),
                    enqueue_seq: 1,
                    source_key: None,
                    delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
                    slot_policy: SlotPolicy::Exclusive,
                    merge_key: MergeKey::Never,
                    available_at_ms: 0,
                    enqueued_at_ms: 0,
                    items: vec![crate::runtime::QueuedWorkItem {
                        item_id: "exhausted-item".to_string(),
                        payload: crate::runtime::QueuedWorkPayload::agent_frame_task(
                            "frame", "task", None,
                        ),
                    }],
                },
                claim_id: None,
                claim_token: None,
                claim_owner: None,
                claim_fencing_token: i64::MAX as u64,
                claim_session_lease_generation: 0,
            });
        let owner = crate::LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
        let authority = crate::SessionExecutionLeaseAuthority {
            session_id: "session".to_string(),
            owner: owner.clone(),
            lease_token: "session-lease".to_string(),
            fencing_token: 1,
        };

        let error = store
            .claim_ready_queued_work_after_lease_validation(
                "session",
                &authority,
                &owner,
                super::super::InMemoryQueuedWorkClaimKind::TurnWork {
                    boundary: crate::QueuedWorkClaimBoundary::Idle,
                    max_batches: 1,
                },
                store.clock.timestamp_ms(),
            )
            .expect_err("exhausted in-memory fence must refuse");
        assert!(matches!(
            error,
            StoreError::MonotonicCounterOverflow {
                counter: "queued_work_claim_fencing_token",
                current,
            } if current == i64::MAX as u64
        ));
    }

    #[tokio::test]
    async fn stale_claim_validation_cannot_partially_mutate_a_commit() {
        let store = super::InMemorySessionStore::new();
        store
            .queued_work
            .lock_recover()
            .push(super::super::InMemoryQueuedBatch {
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
            });
        let state = RuntimeSessionState {
            session_id: "session".to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        store
            .admit_and_bind_session(&crate::SessionBinding::root(state.session_id.clone()))
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
            store.queued_work.lock_recover().len(),
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
            .lock_recover()
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
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
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
            *store.runtime_commit_count.lock_recover(),
            0,
            "a rejected rewound outbox must not increment the commit counter"
        );
        assert!(
            store.runtime_turn_commits.lock_recover().is_empty(),
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
                policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
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
