use crate::support::*;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::LlmOutputPart;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, ResponseTextMeta,
};
use lash_lashlang_runtime::ToolDefinitionLashlangExt;
use tokio::sync::{Mutex as TokioMutex, oneshot};

static TEST_SESSION_LEASE_TOKEN: AtomicUsize = AtomicUsize::new(1);

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as u64
}

fn test_session_execution_lease(
    session_id: &str,
    owner: &lash_core::LeaseOwnerIdentity,
    lease_ttl_ms: u64,
    fencing_token: u64,
) -> lash_core::SessionExecutionLease {
    let claimed_at_epoch_ms = now_epoch_ms();
    lash_core::SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: owner.clone(),
        lease_token: format!(
            "test-session-lease-{}",
            TEST_SESSION_LEASE_TOKEN.fetch_add(1, Ordering::Relaxed)
        ),
        fencing_token,
        claimed_at_epoch_ms,
        expires_at_epoch_ms: claimed_at_epoch_ms.saturating_add(lease_ttl_ms),
    }
}

fn session_fence_matches(
    lease: &lash_core::SessionExecutionLease,
    fence: &lash_core::SessionExecutionLeaseFence,
) -> bool {
    lease.session_id == fence.session_id
        && lease.owner == fence.owner
        && lease.lease_token == fence.lease_token
        && lease.fencing_token == fence.fencing_token
        && lease.expires_at_epoch_ms > now_epoch_ms()
}

fn session_completion_matches(
    lease: &lash_core::SessionExecutionLease,
    completion: &lash_core::SessionExecutionLeaseCompletion,
) -> bool {
    lease.session_id == completion.session_id
        && lease.owner == completion.owner
        && lease.lease_token == completion.lease_token
        && lease.fencing_token == completion.fencing_token
}

#[derive(Default)]
struct SnapshotStore {
    read: std::sync::Mutex<Option<lash_core::store::PersistedSessionRead>>,
    session_meta: std::sync::Mutex<Option<lash_core::SessionMeta>>,
    scopes: std::sync::Mutex<Vec<lash_core::SessionReadScope>>,
    runtime_turn_commits: std::sync::Mutex<
        std::collections::HashMap<
            (String, String),
            (String, lash_core::store::RuntimeCommitResult),
        >,
    >,
    session_execution_leases: std::sync::Mutex<HashMap<String, lash_core::SessionExecutionLease>>,
}

impl SnapshotStore {
    fn with_state(state: RuntimeSessionState) -> Self {
        let turn_state = state.turn_state();
        let session_meta = lash_core::SessionMeta {
            session_id: state.session_id.clone(),
            incarnation_id: state.incarnation_id.clone(),
            session_name: state.session_id.clone(),
            created_at: "test".to_string(),
            model: state.policy.model.id.clone(),
            cwd: None,
            relation: lash_core::SessionRelation::Root,
        };
        let config = lash_core::PersistedSessionConfig {
            provider_id: state.policy.recorded_provider_id().to_string(),
            model: state.policy.model.clone(),
        };
        Self {
            read: std::sync::Mutex::new(Some(lash_core::store::PersistedSessionRead {
                session_id: state.session_id,
                head_revision: 7,
                config,
                current_frame_node_id: state.current_frame_node_id,
                graph: state.session_graph,
                checkpoint_ref: None,
                checkpoint: Some(lash_core::store::HydratedSessionCheckpoint {
                    turn_state,
                    tool_state: state.tool_state_snapshot,
                    ..Default::default()
                }),
                token_ledger: Vec::new(),
            })),
            session_meta: std::sync::Mutex::new(Some(session_meta)),
            scopes: std::sync::Mutex::new(Vec::new()),
            runtime_turn_commits: std::sync::Mutex::new(std::collections::HashMap::new()),
            session_execution_leases: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn scopes(&self) -> Vec<lash_core::SessionReadScope> {
        self.scopes.lock().expect("snapshot scopes lock").clone()
    }

    fn set_head_provider_id(&self, provider_id: impl Into<String>) {
        let mut read = self.read.lock().expect("snapshot store lock");
        let Some(read) = read.as_mut() else {
            panic!("snapshot store has no session head");
        };
        let provider_id = provider_id.into();
        read.config.provider_id = provider_id.clone();
        let leaf_node_id = read.graph.leaf_node_id.clone();
        let mut nodes = read.graph.nodes.clone();
        for node in &mut nodes {
            if let lash_core::SessionNodePayload::FrameOpen { assignment, .. } = &mut node.payload {
                assignment.policy.provider_id = provider_id.clone();
            }
        }
        read.graph = lash_core::SessionGraph::from_nodes(nodes, leaf_node_id);
        read.head_revision += 1;
    }
}

lash_core::impl_noop_attachment_manifest!(SnapshotStore);

#[async_trait]
impl lash_core::SessionCommitStore for SnapshotStore {
    async fn load_session(
        &self,
        scope: lash_core::SessionReadScope,
    ) -> std::result::Result<
        Option<lash_core::store::PersistedSessionRead>,
        lash_core::store::StoreError,
    > {
        self.scopes
            .lock()
            .expect("snapshot scopes lock")
            .push(scope.clone());
        let mut read = self.read.lock().expect("snapshot store lock").clone();
        if let Some(read) = read.as_mut()
            && let lash_core::SessionReadScope::ActivePath { leaf_node_id } = scope
        {
            if let Some(leaf_node_id) = leaf_node_id {
                read.graph.set_leaf_node_id(Some(leaf_node_id));
            }
            read.graph = read.graph.trim_to_active_path();
        }
        Ok(read)
    }

    async fn load_node(
        &self,
        _node_id: &str,
    ) -> std::result::Result<Option<lash_core::SessionNodeRecord>, lash_core::store::StoreError>
    {
        Ok(None)
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> std::result::Result<lash_core::store::RuntimeCommitResult, lash_core::store::StoreError>
    {
        {
            let mut session_meta = self.session_meta.lock().expect("session metadata lock");
            if session_meta.is_none() {
                *session_meta = Some(lash_core::SessionMeta {
                    session_id: commit.session_id.clone(),
                    incarnation_id: commit.incarnation_id.clone(),
                    session_name: commit.session_id.clone(),
                    created_at: "test".to_string(),
                    model: commit.config.model.id.clone(),
                    cwd: None,
                    relation: lash_core::SessionRelation::Root,
                });
            }
        }
        let mut read = self.read.lock().expect("snapshot store lock");
        let realization_digest = lash_core::store::graph_realization_digest(&commit.graph);
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| lash_core::store::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect();
        if let Some(completed) = &commit.turn_commit {
            let operation_key = completed.operation.storage_key()?;
            if completed.session_id != commit.session_id {
                return Err(lash_core::store::StoreError::RuntimeTurnCommitConflict {
                    session_id: completed.session_id.clone(),
                    turn_id: operation_key,
                });
            }
            let key = (
                completed.session_id.clone(),
                completed.operation.storage_key()?,
            );
            if let Some((stored_hash, result)) = self
                .runtime_turn_commits
                .lock()
                .expect("runtime turn commits lock")
                .get(&key)
                .cloned()
            {
                if stored_hash == completed.turn_commit_hash {
                    return Ok(result);
                }
                return Err(lash_core::store::StoreError::RuntimeTurnCommitConflict {
                    session_id: completed.session_id.clone(),
                    turn_id: completed.operation.storage_key()?,
                });
            }
        }
        let Some(fence) = &commit.session_execution_lease else {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: commit.session_id.clone(),
            });
        };
        let live_lease = {
            self.session_execution_leases
                .lock()
                .expect("session execution leases lock")
                .get(&commit.session_id)
                .cloned()
        };
        if !live_lease
            .as_ref()
            .is_some_and(|lease| session_fence_matches(lease, fence))
        {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: commit.session_id.clone(),
            });
        }
        let existing_graph = read
            .as_ref()
            .map(|read| read.graph.clone())
            .unwrap_or_default();
        let graph = match commit.graph.clone() {
            lash_core::store::GraphCommitDelta::Unchanged { leaf_node_id } => {
                let mut graph = existing_graph;
                graph.set_leaf_node_id(leaf_node_id);
                graph
            }
            lash_core::store::GraphCommitDelta::Append {
                nodes,
                leaf_node_id,
            } => {
                let mut graph = existing_graph;
                graph.extend_node_records(nodes);
                graph.set_leaf_node_id(leaf_node_id);
                graph
            }
        };
        *read = Some(lash_core::store::PersistedSessionRead {
            session_id: commit.session_id.clone(),
            head_revision: 8,
            config: commit.config,
            current_frame_node_id: commit.current_frame_node_id,
            graph,
            checkpoint_ref: Some(lash_core::BlobRef("checkpoint".to_string())),
            checkpoint: Some(commit.checkpoint),
            token_ledger: commit.usage_deltas,
        });
        let result = lash_core::store::RuntimeCommitResult {
            head_revision: 8,
            checkpoint_ref: lash_core::BlobRef("checkpoint".to_string()),
            manifest: lash_core::store::SessionCheckpoint::default(),
            realization_digest,
            realized_node_timestamps,
            enqueued_queue_batches: Vec::new(),
            turn_input_applications: Vec::new(),
        };
        if let Some(completed) = &commit.turn_commit {
            self.runtime_turn_commits
                .lock()
                .expect("runtime turn commits lock")
                .insert(
                    (
                        completed.session_id.clone(),
                        completed.operation.storage_key()?,
                    ),
                    (completed.turn_commit_hash.clone(), result.clone()),
                );
        }
        if let Some(completion) = &commit.release_session_execution_lease {
            let mut leases = self
                .session_execution_leases
                .lock()
                .expect("session execution leases lock");
            if leases
                .get(&completion.session_id)
                .is_some_and(|lease| session_completion_matches(lease, completion))
            {
                leases.remove(&completion.session_id);
            }
        }
        Ok(result)
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        *self.session_meta.lock().expect("session metadata lock") = Some(meta);
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::SessionMeta>, lash_core::store::StoreError> {
        Ok(self
            .session_meta
            .lock()
            .expect("session metadata lock")
            .clone())
    }
}

#[async_trait]
impl lash_core::SessionExecutionLeaseStore for SnapshotStore {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("session execution leases lock");
        if let Some(existing) = leases.get(session_id)
            && existing.expires_at_epoch_ms > now_epoch_ms()
        {
            if existing.owner.same_incarnation(owner) {
                let mut lease = existing.clone();
                lease.expires_at_epoch_ms = now_epoch_ms().saturating_add(lease_ttl_ms);
                leases.insert(session_id.to_string(), lease.clone());
                return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
                    lease,
                ));
            }
            return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Busy {
                holder: existing.clone(),
            });
        }
        let next_fencing_token = leases
            .get(session_id)
            .map(|lease| lease.fencing_token.saturating_add(1))
            .unwrap_or(1);
        let lease =
            test_session_execution_lease(session_id, owner, lease_ttl_ms, next_fencing_token);
        leases.insert(session_id.to_string(), lease.clone());
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            lease,
        ))
    }

    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        observed_holder: &lash_core::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("session execution leases lock");
        if let Some(existing) = leases.get(session_id)
            && existing.expires_at_epoch_ms > now_epoch_ms()
            && !session_fence_matches(existing, observed_holder)
        {
            return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Busy {
                holder: existing.clone(),
            });
        }
        if let Some(existing) = leases.get(session_id)
            && existing.expires_at_epoch_ms > now_epoch_ms()
            && !existing.owner.is_definitely_dead_for_claimant(owner)
        {
            return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Busy {
                holder: existing.clone(),
            });
        }
        let next_fencing_token = leases
            .get(session_id)
            .map(|lease| lease.fencing_token.saturating_add(1))
            .unwrap_or(1);
        let lease =
            test_session_execution_lease(session_id, owner, lease_ttl_ms, next_fencing_token);
        leases.insert(session_id.to_string(), lease.clone());
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            lease,
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> std::result::Result<lash_core::SessionExecutionLease, lash_core::store::StoreError> {
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("session execution leases lock");
        let Some(existing) = leases.get_mut(&fence.session_id) else {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !session_fence_matches(existing, fence) {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        existing.claimed_at_epoch_ms = now_epoch_ms();
        existing.expires_at_epoch_ms = existing.claimed_at_epoch_ms.saturating_add(lease_ttl_ms);
        Ok(existing.clone())
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseCompletion,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        let mut leases = self
            .session_execution_leases
            .lock()
            .expect("session execution leases lock");
        if leases
            .get(&completion.session_id)
            .is_some_and(|lease| session_completion_matches(lease, completion))
        {
            leases.remove(&completion.session_id);
        }
        Ok(())
    }
}

#[async_trait]
impl lash_core::QueuedWorkStore for SnapshotStore {
    async fn enqueue_queued_work(
        &self,
        _batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> std::result::Result<lash_core::runtime::QueuedWorkBatch, lash_core::store::StoreError>
    {
        Err(lash_core::store::StoreError::Backend(
            "queued work is not supported by SnapshotStore".to_string(),
        ))
    }

    async fn claim_leading_ready_session_command(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn claim_ready_queued_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _max_batches: usize,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn claim_checkpoint_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &str,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
        _max_batches: usize,
    ) -> std::result::Result<
        (
            Option<lash_core::runtime::TurnInputClaim>,
            Option<lash_core::runtime::QueuedWorkClaim>,
        ),
        lash_core::store::StoreError,
    > {
        Ok((None, None))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _batch_ids: &[String],
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn abandon_queued_work_claim(
        &self,
        _claim: &lash_core::runtime::QueuedWorkClaim,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        _session_id: &str,
        _batch_id: &str,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkBatch>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn list_queued_work(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }

    async fn list_pending_queued_work(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }
}

// SnapshotStore serves no pending turn input: idle dispatch may probe the
// (always empty) queue, but nothing in these tests enqueues or claims input.
#[async_trait]
impl lash_core::TurnInputStore for SnapshotStore {
    async fn enqueue_pending_turn_input(
        &self,
        _input: lash_core::PendingTurnInputDraft,
    ) -> std::result::Result<lash_core::PendingTurnInput, lash_core::store::StoreError> {
        unreachable!("SnapshotStore does not serve pending turn input")
    }

    async fn list_pending_turn_inputs(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInput>, lash_core::store::StoreError> {
        Ok(Vec::new())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        _session_id: &str,
        _targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> std::result::Result<
        Vec<lash_core::PendingTurnInputCancelResult>,
        lash_core::store::StoreError,
    > {
        unreachable!("SnapshotStore does not serve pending turn input")
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        _session_id: &str,
        _anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> std::result::Result<
        lash_core::PendingTurnInputSuffixCancelOutcome,
        lash_core::store::StoreError,
    > {
        unreachable!("SnapshotStore does not serve pending turn input")
    }

    // Turn checkpoints and idle dispatch probe the input queue on every
    // turn; this store's queue is always empty, so claims find nothing.
    async fn claim_active_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &str,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        Ok(None)
    }

    async fn claim_next_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        Ok(None)
    }

    async fn abandon_turn_input_claim(
        &self,
        _claim: &lash_core::TurnInputClaim,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }
}

#[async_trait]
impl lash_core::StoreMaintenance for SnapshotStore {
    async fn tombstone_nodes(
        &self,
        _ids: &[String],
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn vacuum(
        &self,
    ) -> std::result::Result<lash_core::VacuumReport, lash_core::store::StoreError> {
        Ok(lash_core::VacuumReport::default())
    }

    async fn gc_unreachable(
        &self,
    ) -> std::result::Result<lash_core::GcReport, lash_core::store::StoreError> {
        Ok(lash_core::GcReport::default())
    }

    async fn verify_node_refcounts(
        &self,
    ) -> std::result::Result<lash_core::NodeRefcountVerification, lash_core::store::StoreError>
    {
        Ok(lash_core::NodeRefcountVerification::default())
    }
}

#[derive(Clone)]
struct ReusableStoreFactory {
    store: Arc<dyn lash_core::RuntimePersistence>,
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for ReusableStoreFactory {
    async fn create_store(
        &self,
        _request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, String> {
        Ok(Arc::clone(&self.store))
    }

    async fn delete_session(&self, _session_id: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

struct BoundSessionStore {
    session_id: String,
}

lash_core::impl_noop_attachment_manifest!(BoundSessionStore);

#[async_trait]
impl lash_core::SessionCommitStore for BoundSessionStore {
    async fn load_session(
        &self,
        _scope: lash_core::SessionReadScope,
    ) -> std::result::Result<
        Option<lash_core::store::PersistedSessionRead>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn load_node(
        &self,
        _node_id: &str,
    ) -> std::result::Result<Option<lash_core::SessionNodeRecord>, lash_core::store::StoreError>
    {
        Ok(None)
    }

    async fn commit_runtime_state(
        &self,
        _commit: lash_core::store::RuntimeCommit,
    ) -> std::result::Result<lash_core::store::RuntimeCommitResult, lash_core::store::StoreError>
    {
        unreachable!("test should fail before committing to the reused child store")
    }

    async fn save_session_meta(
        &self,
        _meta: lash_core::SessionMeta,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::SessionMeta>, lash_core::store::StoreError> {
        Ok(Some(lash_core::SessionMeta {
            session_id: self.session_id.clone(),
            incarnation_id: lash_core::IncarnationId::fresh(),
            session_name: self.session_id.clone(),
            created_at: "test".to_string(),
            model: "mock-model".to_string(),
            cwd: None,
            relation: lash_core::SessionRelation::Root,
        }))
    }
}

#[async_trait]
impl lash_core::SessionExecutionLeaseStore for BoundSessionStore {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            test_session_execution_lease(session_id, owner, lease_ttl_ms, 1),
        ))
    }

    async fn reclaim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        _observed_holder: &lash_core::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            test_session_execution_lease(session_id, owner, lease_ttl_ms, 1),
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> std::result::Result<lash_core::SessionExecutionLease, lash_core::store::StoreError> {
        Ok(test_session_execution_lease(
            &fence.session_id,
            &fence.owner,
            lease_ttl_ms,
            fence.fencing_token,
        ))
    }

    async fn release_session_execution_lease(
        &self,
        _completion: &lash_core::SessionExecutionLeaseCompletion,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }
}

// The reuse test fails before any turn runs, so this double serves neither
// pending turn input nor queued work.
#[async_trait]
impl lash_core::TurnInputStore for BoundSessionStore {
    async fn enqueue_pending_turn_input(
        &self,
        _input: lash_core::PendingTurnInputDraft,
    ) -> std::result::Result<lash_core::PendingTurnInput, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn list_pending_turn_inputs(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<lash_core::PendingTurnInput>, lash_core::store::StoreError> {
        Ok(Vec::new())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        _session_id: &str,
        _targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> std::result::Result<
        Vec<lash_core::PendingTurnInputCancelResult>,
        lash_core::store::StoreError,
    > {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        _session_id: &str,
        _anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> std::result::Result<
        lash_core::PendingTurnInputSuffixCancelOutcome,
        lash_core::store::StoreError,
    > {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn claim_active_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &str,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn claim_next_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn abandon_turn_input_claim(
        &self,
        _claim: &lash_core::TurnInputClaim,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }
}

#[async_trait]
impl lash_core::QueuedWorkStore for BoundSessionStore {
    async fn enqueue_queued_work(
        &self,
        _batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> std::result::Result<lash_core::runtime::QueuedWorkBatch, lash_core::store::StoreError>
    {
        unreachable!("BoundSessionStore does not serve queued work")
    }

    async fn claim_leading_ready_session_command(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn claim_ready_queued_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _max_batches: usize,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn claim_checkpoint_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &str,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
        _max_batches: usize,
    ) -> std::result::Result<
        (
            Option<lash_core::runtime::TurnInputClaim>,
            Option<lash_core::runtime::QueuedWorkClaim>,
        ),
        lash_core::store::StoreError,
    > {
        Ok((None, None))
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseFence,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _batch_ids: &[String],
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn abandon_queued_work_claim(
        &self,
        _claim: &lash_core::runtime::QueuedWorkClaim,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        _session_id: &str,
        _batch_id: &str,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkBatch>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn list_queued_work(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }

    async fn list_pending_queued_work(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::store::StoreError>
    {
        Ok(Vec::new())
    }
}

#[async_trait]
impl lash_core::StoreMaintenance for BoundSessionStore {
    async fn tombstone_nodes(
        &self,
        _ids: &[String],
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn vacuum(
        &self,
    ) -> std::result::Result<lash_core::VacuumReport, lash_core::store::StoreError> {
        Ok(lash_core::VacuumReport::default())
    }

    async fn gc_unreachable(
        &self,
    ) -> std::result::Result<lash_core::GcReport, lash_core::store::StoreError> {
        Ok(lash_core::GcReport::default())
    }

    async fn verify_node_refcounts(
        &self,
    ) -> std::result::Result<lash_core::NodeRefcountVerification, lash_core::store::StoreError>
    {
        Ok(lash_core::NodeRefcountVerification::default())
    }
}

#[derive(Default)]
struct RecordingStoreFactory {
    requests: std::sync::Mutex<Vec<lash_core::SessionStoreCreateRequest>>,
}

impl RecordingStoreFactory {
    fn session_ids(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("recording factory lock")
            .iter()
            .map(|request| request.session_id.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for RecordingStoreFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, String> {
        self.requests
            .lock()
            .expect("recording factory lock")
            .push(request.clone());
        Ok(Arc::new(SnapshotStore::default()))
    }

    async fn delete_session(&self, _session_id: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct DeletingStoreFactory {
    stores: std::sync::Mutex<std::collections::HashMap<String, Arc<SnapshotStore>>>,
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for DeletingStoreFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, String> {
        let store = self
            .stores
            .lock()
            .expect("deleting factory lock")
            .entry(request.session_id.clone())
            .or_insert_with(|| Arc::new(SnapshotStore::default()))
            .clone();
        Ok(store as Arc<dyn lash_core::RuntimePersistence>)
    }

    async fn delete_session(&self, session_id: &str) -> std::result::Result<(), String> {
        self.stores
            .lock()
            .expect("deleting factory lock")
            .remove(session_id);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingEvents {
    events: TokioMutex<Vec<TurnActivity>>,
}

impl RecordingEvents {
    async fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl TurnActivitySink for RecordingEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock().await.push(activity);
    }
}

fn test_activity(correlation_id: &str, event: TurnEvent) -> TurnActivity {
    TurnActivity::new(TurnActivityId::new(correlation_id.to_string()), event)
}

fn assistant_prose(events: &[TurnActivity]) -> String {
    events
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.as_ref()),
            _ => None,
        })
        .collect()
}

struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        lash_core::ToolResult::ok(serde_json::json!({ "ok": true }))
    }
}

struct FailingAppTools;

#[async_trait]
impl ToolProvider for FailingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        lash_core::ToolResult::err_fmt("lookup failed but Lashlang recovered")
    }
}

struct PendingAppTools {
    key_tx: StdMutex<Option<oneshot::Sender<lash_core::AwaitEventKey>>>,
}

impl PendingAppTools {
    fn new(key_tx: oneshot::Sender<lash_core::AwaitEventKey>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
        }
    }
}

#[async_trait]
impl ToolProvider for PendingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "app_lookup");
        let key = match call.context.completion_key().await {
            Ok(key) => key,
            Err(err) => return lash_core::ToolResult::err_fmt(err),
        };
        if let Some(tx) = self.key_tx.lock().expect("pending tool key tx").take() {
            let _ = tx.send(key);
        }
        lash_core::ToolResult::pending(lash_core::PendingCompletion::new())
    }
}

struct DurableInputTools {
    key_tx:
        StdMutex<Option<oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>>>,
    attempt_count: Arc<AtomicUsize>,
}

struct RetryingDirectTools;

#[async_trait]
impl ToolProvider for RetryingDirectTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retrying_direct_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "retrying_direct").then(|| Arc::new(retrying_direct_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "retrying_direct");
        let model = match call.context.sessions().model().await {
            Ok(model) => model,
            Err(err) => return lash_core::ToolResult::err_fmt(err),
        };
        let completion = match call
            .context
            .direct_completions()
            .complete(
                lash_core::DirectRequest::text(
                    model.model,
                    format!(
                        "retrying direct completion attempt {}",
                        call.context.attempt_number()
                    ),
                ),
                "retrying_direct",
            )
            .await
        {
            Ok(completion) => completion,
            Err(err) => return lash_core::ToolResult::err_fmt(err),
        };
        if call.context.attempt_number() == 1 {
            return lash_core::ToolResult::failure(lash_core::ToolFailure::safe_retry(
                lash_core::ToolFailureClass::Execution,
                "retrying_direct_first_attempt",
                "retry the complete atomic attempt",
                Some(0),
            ));
        }
        lash_core::ToolResult::ok(serde_json::json!(completion.text))
    }
}

fn retrying_direct_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:retrying_direct",
        "retrying_direct",
        "Call a direct completion and retry the complete attempt once.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    )
    .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 0, 0))
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["tools"],
        "retrying_direct",
    ))
}

impl DurableInputTools {
    fn new(key_tx: oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
            attempt_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempt_count(&self) -> usize {
        self.attempt_count.load(Ordering::SeqCst)
    }

    fn send_key_result(&self, result: std::result::Result<lash_core::AwaitEventKey, String>) {
        if let Some(tx) = self.key_tx.lock().expect("durable input key tx").take() {
            let _ = tx.send(result);
        }
    }
}

#[async_trait]
impl ToolProvider for DurableInputTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![durable_input_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "mock_input_request").then(|| Arc::new(durable_input_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "mock_input_request");
        let question = call
            .args
            .get("question")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("answer")
            .to_string();
        let key = match call.context.completion_key().await {
            Ok(key) => key,
            Err(err) => {
                self.send_key_result(Err(err.to_string()));
                return lash_core::ToolResult::err_fmt(err);
            }
        };
        self.attempt_count.fetch_add(1, Ordering::SeqCst);
        let event = lash_core::ProcessEventAppendRequest::new(
            "process.yield",
            serde_json::json!({
                "type": "work.input_request.opened",
                "request_id": "request-1",
                "question": question,
                "await_key_id": key.key_id,
            }),
        )
        .with_replay_key("mock-input-request:request-1");
        if let Err(err) = call.context.process_events().emit_request(event).await {
            self.send_key_result(Err(err.to_string()));
            return lash_core::ToolResult::err_fmt(err);
        }
        self.send_key_result(Ok(key));
        lash_core::ToolResult::pending(lash_core::PendingCompletion::new())
    }
}

fn durable_input_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:mock_input_request",
        "mock_input_request",
        "Open a durable input request and wait for the answer.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string" },
                "answer": {}
            },
            "required": ["request_id", "answer"],
            "additionalProperties": true
        }),
    )
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["tools"],
        "mock_input_request",
    ))
}

struct AgentFrameSwitchTools;

#[async_trait]
impl ToolProvider for AgentFrameSwitchTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![agent_frame_switch_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "switch_frame").then(|| Arc::new(agent_frame_switch_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "switch_frame");
        let task = call
            .args
            .get("task")
            .and_then(serde_json::Value::as_str)
            .expect("task arg")
            .to_string();
        lash_core::ToolResult::ok(serde_json::json!({ "ok": true })).with_control(
            lash_core::ToolControl::SwitchAgentFrame {
                frame_id: "durable-follow-frame".to_string(),
                initial_nodes: Vec::new(),
                task: Some(task),
            },
        )
    }
}

fn agent_frame_switch_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:switch_frame",
        "switch_frame",
        "Switch to a fresh agent frame.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
}

fn app_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:app_lookup",
        "app_lookup",
        "Look up app state.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["tools"],
        "app_lookup",
    ))
}

struct LongTextTools;

#[async_trait]
impl ToolProvider for LongTextTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![long_text_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(long_text_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        lash_core::ToolResult::ok(serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789"))
    }
}

fn long_text_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:app_lookup",
        "app_lookup",
        "Look up verbose app state.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    )
    .with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["tools"],
        "app_lookup",
    ))
}

struct SurfacePluginFactory;

impl lash_core::PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn build(
        &self,
        _ctx: &lash_core::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(SurfacePlugin))
    }
}

struct SurfacePlugin;

impl lash_core::SessionPlugin for SurfacePlugin {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn register(
        &self,
        reg: &mut lash_core::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.output().response(Arc::new(|ctx| {
            Box::pin(async move {
                Ok(lash_core::AssistantResponseTransform {
                    response: ctx.response,
                    events: vec![lash_core::PluginRuntimeEvent::Status {
                        key: "surface".to_string(),
                        label: "working".to_string(),
                        detail: Some("details".to_string()),
                    }],
                })
            })
        }));
        Ok(())
    }
}

fn mock_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(|request| async move {
            let user_text = last_user_text(&request);
            let reply = format!("echo: {user_text}");
            if let Some(events) = request.stream_events.as_ref() {
                events.send(LlmStreamEvent::Delta(reply.clone()));
            }
            Ok(LlmResponse {
                full_text: reply.clone(),
                parts: vec![LlmOutputPart::Text {
                    text: reply,
                    response_meta: None,
                }],
                usage: lash_core::llm::types::LlmUsage {
                    input_tokens: user_text.split_whitespace().count() as i64,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn tool_roundtrip_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "app_lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        LlmResponse {
            full_text: "done".to_string(),
            parts: vec![LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn agent_frame_switch_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "switch-call".to_string(),
                tool_name: "switch_frame".to_string(),
                input_json: serde_json::json!({
                    "task": "finish in the next frame"
                })
                .to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        text_response("done after frame switch"),
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        full_text: text.to_string(),
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn lashlang_block(source: &str) -> String {
    format!("<lashlang>\n{}\n</lashlang>", source.trim())
}

fn queued_text_provider(texts: Vec<impl Into<String>>) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(
        texts
            .into_iter()
            .map(|text| {
                let text = text.into();
                LlmResponse {
                    full_text: text.clone(),
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }
            })
            .collect::<Vec<_>>(),
    )));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn semantic_group_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(|_request| async move {
            Ok(LlmResponse {
                full_text: "firstsecond".to_string(),
                parts: vec![
                    LlmOutputPart::Text {
                        text: "first".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:first".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                    LlmOutputPart::Text {
                        text: "second".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:second".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn text_provider(kind: &'static str, _model: &'static str, text: &'static str) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request| async move {
            Ok(LlmResponse {
                full_text: text.to_string(),
                parts: vec![LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

type SeenModels = Arc<std::sync::Mutex<Vec<(String, lash_core::ReasoningSelection)>>>;

fn recording_text_provider(
    kind: &'static str,
    _model: &'static str,
    _variant: Option<&'static str>,
    text: &'static str,
    seen: SeenModels,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock()
                    .expect("seen requests")
                    .push((request.model, request.model_variant));
                Ok(LlmResponse {
                    full_text: text.to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn last_user_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == LlmRole::User)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn system_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .find(|message| message.role == LlmRole::System)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn recording_prompt_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("prompt-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock()
                    .expect("seen prompts")
                    .push(system_text(&request));
                Ok(LlmResponse {
                    full_text: "ok".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn recording_request_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("request-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock()
                    .expect("seen prompts")
                    .push(request_text(&request));
                Ok(text_response(&lashlang_block("finish \"ok\"")))
            }
        })
        .build()
        .into_handle()
}

fn retry_once_provider() -> ProviderHandle {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("retry-test")
        .requires_streaming(true)
        .options(lash_core::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::ProviderOptions::default()
        })
        .complete(move |_request| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return Err(LlmTransportError::new("retry me").retryable(true));
                }
                Ok(LlmResponse {
                    full_text: "retried".to_string(),
                    parts: vec![LlmOutputPart::Text {
                        text: "retried".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn checkpoint_gated_provider(
    entered_tx: oneshot::Sender<()>,
    release_rx: oneshot::Receiver<()>,
) -> ProviderHandle {
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(TokioMutex::new(Some(release_rx)));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("checkpoint-gated")
        .complete(move |request| {
            let entered_tx = Arc::clone(&entered_tx);
            let release_rx = Arc::clone(&release_rx);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    if let Some(tx) = entered_tx.lock().expect("entered tx").take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    Ok(text_response("first"))
                } else {
                    Ok(text_response(&format!(
                        "after {}",
                        last_user_text(&request)
                    )))
                }
            }
        })
        .build()
        .into_handle()
}

fn standard_core() -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build()
        .expect("standard core")
}

/// In-memory Lashlang artifact store for RLM test factories.
#[cfg(feature = "rlm")]
fn inmem_artifact_store() -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
    Arc::new(crate::persistence::InMemoryLashlangArtifactStore::new())
}

/// Default RLM protocol factory for tests (in-memory artifact store).
#[cfg(feature = "rlm")]
fn rlm_factory() -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        inmem_artifact_store(),
    )
}

/// A [`LashCoreBuilder`] pre-seeded with the default RLM factory.
#[cfg(feature = "rlm")]
fn rlm_core_builder() -> crate::core::LashCoreBuilder {
    LashCore::rlm_builder(rlm_factory())
}

fn inline_scope(scope: lash_core::ExecutionScope) -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::InlineRuntimeEffectController::default()),
        scope,
    )
    .expect("inline execution scope")
}

fn turn_scope(session_id: &str) -> lash_core::ScopedEffectController<'static> {
    inline_scope(lash_core::ExecutionScope::turn(
        session_id,
        lash_core::TurnActivityId::fresh().0.to_string(),
    ))
}

fn runtime_operation_scope(
    core: &LashCore,
    scope_id: impl Into<String>,
) -> lash_core::ScopedEffectController<'static> {
    core.effect_host()
        .scoped_static(lash_core::ExecutionScope::runtime_operation(scope_id))
        .expect("runtime operation scope")
        .expect("effect host supplies an owned runtime operation scope")
}

fn session_delete_scope(session_id: &str) -> lash_core::ScopedEffectController<'static> {
    inline_scope(lash_core::ExecutionScope::session_delete(session_id))
}

fn explicit_ephemeral_facets(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    builder
        .effect_host(Arc::new(
            crate::durability::InlineEffectHost::default().allow_process_lifetime_completion_keys(),
        ))
        .attachment_store(Arc::new(crate::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            crate::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
}

fn text_message(role: lash_core::MessageRole, text: &str) -> lash_core::Message {
    let id = "stored-message".to_string();
    lash_core::Message {
        id: id.clone(),
        role,
        parts: lash_core::shared_parts(vec![lash_core::Part {
            id: format!("{id}.p0"),
            kind: lash_core::PartKind::Text,
            content: text.to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: lash_core::PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: None,
    }
}

mod control_admin;
mod core_session_builder;
mod harness;
use harness::{
    mock_model_spec, model_spec, run_async_test_on_stack_budget, run_async_test_on_stack_size,
};
mod agent_scenarios;
mod plugin_stack;
mod processes_endstate;
mod rebuild_conformance;
mod stack_budget;
mod turn_streaming;
