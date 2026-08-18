use crate::support::*;
use lash_core::facade_support::{
    AgentFrameReasonFacadeOps, RuntimeSessionStateFacadeOps, SessionGraphFacadeOps,
    SessionNodeRecordFacadeOps, ToolStateFacadeOps,
};
use lash_sansio::sync::MutexExt;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::LlmOutputPart;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, ResponseTextMeta,
};
#[cfg(feature = "rlm")]
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
    executor_id: &str,
    lease_ttl_ms: u64,
    fencing_token: u64,
) -> lash_core::SessionExecutionLease {
    let claimed_at_epoch_ms = now_epoch_ms();
    lash_core::SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: owner.clone(),
        executor_id: executor_id.to_string(),
        lease_token: format!(
            "test-session-lease-{}",
            TEST_SESSION_LEASE_TOKEN.fetch_add(1, Ordering::Relaxed)
        ),
        fencing_token,
        claimed_at_epoch_ms,
        lease_term_ms: lease_ttl_ms,
        expires_at_epoch_ms: claimed_at_epoch_ms.saturating_add(lease_ttl_ms),
    }
}

fn session_fence_matches(
    lease: &lash_core::SessionExecutionLease,
    fence: &lash_core::SessionExecutionLeaseAuthority,
) -> bool {
    lease.session_id == fence.session_id
        && lease.owner == fence.owner
        && lease.executor_id == fence.executor_id
        && lease.lease_token == fence.lease_token
}

fn session_completion_matches(
    lease: &lash_core::SessionExecutionLease,
    completion: &lash_core::SessionExecutionLeaseAuthority,
) -> bool {
    lease.session_id == completion.session_id
        && lease.owner == completion.owner
        && lease.executor_id == completion.executor_id
        && lease.lease_token == completion.lease_token
}

#[derive(Default)]
struct SnapshotStore {
    read: std::sync::Mutex<Option<lash_core::store::PersistedSessionRead>>,
    session_meta: std::sync::Mutex<Option<lash_core::SessionMeta>>,
    runtime_turn_commits: std::sync::Mutex<
        std::collections::HashMap<
            (String, String),
            (String, lash_core::store::RuntimeCommitResult),
        >,
    >,
    usage_delta_identities:
        std::sync::Mutex<std::collections::HashSet<lash_core::store::RuntimeUsageDeltaIdentity>>,
    session_execution_leases: std::sync::Mutex<HashMap<String, lash_core::SessionExecutionLease>>,
    /// Highest generation ever minted per session, retained across release.
    ///
    /// `SessionExecutionLeaseStore` is a fencing trait: ADR 0029 requires every
    /// fresh acquisition after release or expiry to mint `previous + 1`, and a
    /// double is not exempt. This store drops the live lease row on release, so
    /// generation authority has to live somewhere that survives it, or a stale
    /// generation would be reissued and fencing would silently stop working.
    session_execution_lease_generations: std::sync::Mutex<HashMap<String, u64>>,
}

impl SnapshotStore {
    fn with_state(state: RuntimeSessionState) -> Self {
        let config = lash_core::PersistedSessionConfig::from(&state.policy);
        Self::with_state_and_config(state, config)
    }

    fn with_state_and_config(
        state: RuntimeSessionState,
        config: lash_core::PersistedSessionConfig,
    ) -> Self {
        let turn_state = state.turn_state();
        let session_meta = lash_core::SessionMeta {
            session_id: state.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
        };
        let mut components = std::collections::BTreeMap::new();
        if let Some(tool_state) = state.tool_state_snapshot() {
            components.insert(
                lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
                lash_core::HydratedCheckpointComponent::changed(
                    rmp_serde::to_vec_named(tool_state).expect("encode test tool state"),
                ),
            );
        }
        if let Some(execution_state) = state.execution_state_snapshot() {
            components.insert(
                lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                lash_core::HydratedCheckpointComponent::changed(execution_state.to_vec()),
            );
        }
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
                    components,
                    ..Default::default()
                }),
                token_ledger: Vec::new(),
            })),
            session_meta: std::sync::Mutex::new(Some(session_meta)),
            runtime_turn_commits: std::sync::Mutex::new(std::collections::HashMap::new()),
            usage_delta_identities: std::sync::Mutex::new(std::collections::HashSet::new()),
            session_execution_leases: std::sync::Mutex::new(HashMap::new()),
            session_execution_lease_generations: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn set_head_provider_id(&self, provider_id: impl Into<String>) {
        let mut read = self.read.lock_recover();
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
        read.graph = lash_core::SessionGraph::from_nodes(nodes, leaf_node_id)
            .expect("snapshot fixture graph is valid");
        read.head_revision += 1;
    }
}

lash_core::impl_noop_attachment_manifest!(SnapshotStore);

#[async_trait]
impl lash_core::SessionCommitStore for SnapshotStore {
    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> std::result::Result<lash_core::SessionAdmission, lash_core::store::StoreError> {
        binding.validate()?;
        let mut meta = self.session_meta.lock_recover();
        if let Some(meta) = meta.as_ref() {
            if meta.session_id != binding.session_id {
                return Err(lash_core::store::StoreError::SessionBindingMismatch {
                    bound_session_id: meta.session_id.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
            return Ok(lash_core::SessionAdmission::Rebound);
        }
        *meta = Some(lash_core::SessionMeta {
            session_id: binding.session_id.clone(),
            relation: binding.relation.clone(),
        });
        Ok(lash_core::SessionAdmission::Created)
    }

    async fn load_session(
        &self,
    ) -> std::result::Result<
        Option<lash_core::store::PersistedSessionRead>,
        lash_core::store::StoreError,
    > {
        Ok(self.read.lock_recover().clone())
    }

    async fn load_session_head_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::store::SessionHeadMeta>, lash_core::store::StoreError>
    {
        Ok(self.read.lock_recover().as_ref().map(|read| {
            lash_core::store::SessionHeadMeta::assemble(
                lash_core::store::SessionHeadPayload {
                    schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                    session_id: read.session_id.clone(),
                    config: read.config.clone(),
                    current_frame_node_id: read.current_frame_node_id.clone(),
                },
                read.head_revision,
                read.checkpoint_ref.clone(),
                read.graph.leaf_node_id.clone(),
            )
        }))
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
        let turn_commit_hash = commit.turn_commit_hash()?;
        let session_id = commit.session_id.clone();
        let mut read = self.read.lock_recover();
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| lash_core::session_graph::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect();
        let completed = &commit.turn_commit;
        let operation_key = completed.operation.storage_key()?;
        let key = (session_id.clone(), operation_key.clone());
        if let Some((stored_hash, result)) =
            self.runtime_turn_commits.lock_recover().get(&key).cloned()
        {
            if stored_hash == turn_commit_hash {
                return Ok(result);
            }
            return Err(lash_core::store::StoreError::RuntimeTurnCommitConflict {
                session_id,
                turn_id: operation_key,
            });
        }
        if let Some(required_node_id) = commit.turn_commit.requested_ancestor_node_id.as_deref()
            && !read
                .as_ref()
                .is_some_and(|read| read.graph.active_path_contains(required_node_id))
        {
            return Err(lash_core::store::StoreError::AppendAncestorNotActive {
                required_node_id: required_node_id.to_string(),
            });
        }
        {
            let mut session_meta = self.session_meta.lock_recover();
            if session_meta.is_none() {
                *session_meta = Some(lash_core::SessionMeta {
                    session_id: commit.session_id.clone(),
                    relation: lash_core::SessionRelation::Root,
                });
            }
        }
        let existing_graph = read
            .as_ref()
            .map(|read| read.graph.clone())
            .unwrap_or_default();
        let mut graph = existing_graph;
        graph.extend_node_records(commit.graph.nodes.iter().cloned());
        graph.set_leaf_node_id(commit.graph.leaf_node_id.clone());
        let mut token_ledger = read
            .as_ref()
            .map(|read| read.token_ledger.clone())
            .unwrap_or_default();
        let mut usage_delta_identities = self.usage_delta_identities.lock_recover();
        for delta in &commit.usage_deltas {
            if usage_delta_identities.insert(delta.identity.clone()) {
                if let Some(existing) = token_ledger.iter_mut().find(|entry| {
                    entry.source == delta.entry.source && entry.model == delta.entry.model
                }) {
                    existing.usage.input_tokens = existing
                        .usage
                        .input_tokens
                        .saturating_add(delta.entry.usage.input_tokens);
                    existing.usage.output_tokens = existing
                        .usage
                        .output_tokens
                        .saturating_add(delta.entry.usage.output_tokens);
                    existing.usage.cache_read_input_tokens = existing
                        .usage
                        .cache_read_input_tokens
                        .saturating_add(delta.entry.usage.cache_read_input_tokens);
                    existing.usage.cache_write_input_tokens = existing
                        .usage
                        .cache_write_input_tokens
                        .saturating_add(delta.entry.usage.cache_write_input_tokens);
                    existing.usage.reasoning_output_tokens = existing
                        .usage
                        .reasoning_output_tokens
                        .saturating_add(delta.entry.usage.reasoning_output_tokens);
                } else {
                    token_ledger.push(delta.entry.clone());
                }
            }
        }
        drop(usage_delta_identities);
        *read = Some(lash_core::store::PersistedSessionRead {
            session_id: commit.session_id.clone(),
            head_revision: 8,
            config: commit.config,
            current_frame_node_id: commit.current_frame_node_id,
            graph,
            checkpoint_ref: Some(lash_core::BlobRef("checkpoint".to_string())),
            checkpoint: Some(commit.checkpoint),
            token_ledger,
        });
        let result = lash_core::store::RuntimeCommitResult {
            head_revision: 8,
            checkpoint_ref: lash_core::BlobRef("checkpoint".to_string()),
            manifest: lash_core::store::SessionCheckpoint::default(),
            committed_leaf_node_id: commit.graph.leaf_node_id.clone(),
            realized_node_timestamps,
            committed_usage_delta_identities: commit
                .usage_deltas
                .iter()
                .map(|delta| delta.identity.clone())
                .collect(),
            enqueued_queue_batches: Vec::new(),
            turn_input_applications: Vec::new(),
            receipt_replayed: false,
        };
        self.runtime_turn_commits.lock_recover().insert(
            (session_id, completed.operation.storage_key()?),
            (turn_commit_hash, result.clone()),
        );
        if let Some(completion) = &commit.release_session_execution_lease {
            let mut leases = self.session_execution_leases.lock_recover();
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
        *self.session_meta.lock_recover() = Some(meta);
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::SessionMeta>, lash_core::store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}

#[async_trait]
impl lash_core::SessionExecutionLeaseStore for SnapshotStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        let lease_token = claim_nonce.as_str();
        let mut leases = self.session_execution_leases.lock_recover();
        if let Some(existing) = leases.get(session_id)
            && existing.expires_at_epoch_ms > now_epoch_ms()
        {
            if existing.owner.same_incarnation(owner) && existing.executor_id == executor_id {
                let mut lease = existing.clone();
                if lease.lease_token != lease_token {
                    lease.lease_token = lease_token.to_string();
                }
                lease.expires_at_epoch_ms = now_epoch_ms().saturating_add(lease_ttl_ms);
                leases.insert(session_id.to_string(), lease.clone());
                return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
                    lash_core::SessionExecutionLeaseAcquisition::fresh(lease),
                ));
            }
            return Ok(lash_core::SessionExecutionLeaseClaimOutcome::Busy {
                holder: existing.clone(),
            });
        }
        // The lapsed holder this claim takes the lane from, read before the
        // overwrite. A double that reports no displacement would silently
        // disable the takeover event for every facade test that runs on it.
        let displaced = leases.get(session_id).and_then(|previous| {
            (!previous.owner.same_incarnation(owner) || previous.executor_id != executor_id).then(
                || {
                    (
                        previous.owner.clone(),
                        previous.executor_id.clone(),
                        previous.fencing_token,
                        previous.expires_at_epoch_ms,
                    )
                },
            )
        });
        // Mint from the retained counter, not from the live row: the row is gone
        // after a release, and restarting the fence there would reissue a
        // generation a stale claim still pins.
        let mut generations = self.session_execution_lease_generations.lock_recover();
        let next_fencing_token = generations
            .get(session_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        generations.insert(session_id.to_string(), next_fencing_token);
        drop(generations);
        let mut lease = test_session_execution_lease(
            session_id,
            owner,
            executor_id,
            lease_ttl_ms,
            next_fencing_token,
        );
        lease.lease_token = lease_token.to_string();
        leases.insert(session_id.to_string(), lease.clone());
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, previous_executor_id, generation, expired_at_epoch_ms)) => {
                    lash_core::SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        previous_executor_id,
                        generation,
                        expired_at_epoch_ms,
                    )
                }
                None => lash_core::SessionExecutionLeaseAcquisition::fresh(lease),
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> std::result::Result<lash_core::SessionExecutionLease, lash_core::store::StoreError> {
        let mut leases = self.session_execution_leases.lock_recover();
        let Some(existing) = leases.get_mut(&fence.session_id) else {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !session_fence_matches(existing, fence) {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "facade_test_double_lock",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    Some(&existing.owner),
                    Some(existing.executor_id.as_str()),
                    Some(existing.lease_token.as_str()),
                ),
            );
            return Err(
                lash_core::store::StoreError::SessionExecutionLeaseRenewalRefused {
                    session_id: fence.session_id.clone(),
                },
            );
        }
        if existing.expires_at_epoch_ms <= now_epoch_ms() {
            return Err(lash_core::store::StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        existing.expires_at_epoch_ms = now_epoch_ms().saturating_add(lease_ttl_ms);
        Ok(existing.clone())
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        let mut leases = self.session_execution_leases.lock_recover();
        if leases
            .get(&completion.session_id)
            .is_some_and(|lease| session_completion_matches(lease, completion))
        {
            // The live row goes; the generation counter deliberately stays, so
            // the next claim mints `previous + 1` (ADR 0029).
            leases.remove(&completion.session_id);
            Ok(())
        } else {
            let current = leases.get(&completion.session_id);
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                "token_scoped_release_did_not_match",
                "facade_test_double_lock",
                completion,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.map(|lease| &lease.owner),
                    current.map(|lease| lease.executor_id.as_str()),
                    current.map(|lease| lease.lease_token.as_str()),
                ),
            );
            Err(
                lash_core::store::StoreError::SessionExecutionLeaseReleaseRefused {
                    session_id: completion.session_id.clone(),
                },
            )
        }
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> std::result::Result<Option<lash_core::SessionExecutionLease>, lash_core::store::StoreError>
    {
        Ok(self
            .session_execution_leases
            .lock_recover()
            .get(session_id)
            .cloned())
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _policy: lash_core::QueuedWorkClaimPolicy,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn claim_checkpoint_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &lash_core::TurnId,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
        _policy: lash_core::QueuedWorkClaimPolicy,
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _batch_ids: &[String],
        _policy: lash_core::QueuedWorkClaimPolicy,
    ) -> std::result::Result<lash_core::SelectedQueuedWorkClaimOutcome, lash_core::store::StoreError>
    {
        Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
            None,
            Vec::new(),
        ))
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

    async fn pending_session_work_ordering(
        &self,
        _session_id: &str,
    ) -> std::result::Result<
        lash_core::store::PendingSessionWorkOrdering,
        lash_core::store::StoreError,
    > {
        Ok(lash_core::store::PendingSessionWorkOrdering {
            session_command: None,
            turn_input: None,
        })
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &lash_core::TurnId,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        Ok(None)
    }

    async fn claim_next_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
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
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        _session_id: &str,
    ) -> std::result::Result<bool, lash_core::store::StoreError> {
        Ok(false)
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<(String, String)>, lash_core::store::StoreError> {
        Ok(Vec::new())
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
}

#[derive(Clone)]
struct ReusableStoreFactory {
    store: Arc<dyn lash_core::RuntimePersistence>,
}

// The reusable mock store uses a no-op attachment manifest; this fixture
// explicitly owns no attachment roots.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for ReusableStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for ReusableStoreFactory {
    async fn create_store(
        &self,
        _request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        Ok(Arc::clone(&self.store))
    }

    // The single reused store is never dropped and no tombstone is recorded.
    async fn session_was_deleted(&self, _session_id: &str) -> std::result::Result<bool, String> {
        Ok(false)
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
    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> std::result::Result<lash_core::SessionAdmission, lash_core::store::StoreError> {
        let meta = self
            .load_session_meta()
            .await?
            .expect("bound test store metadata");
        if meta.session_id != binding.session_id {
            return Err(lash_core::store::StoreError::SessionBindingMismatch {
                bound_session_id: meta.session_id,
                attempted_session_id: binding.session_id.clone(),
            });
        }
        Ok(lash_core::SessionAdmission::Rebound)
    }

    async fn load_session(
        &self,
    ) -> std::result::Result<
        Option<lash_core::store::PersistedSessionRead>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::store::SessionHeadMeta>, lash_core::store::StoreError>
    {
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
            relation: lash_core::SessionRelation::Root,
        }))
    }
}

#[async_trait]
impl lash_core::SessionExecutionLeaseStore for BoundSessionStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> std::result::Result<
        lash_core::SessionExecutionLeaseClaimOutcome,
        lash_core::store::StoreError,
    > {
        let mut lease =
            test_session_execution_lease(session_id, owner, executor_id, lease_ttl_ms, 1);
        lease.lease_token = claim_nonce.as_str().to_string();
        Ok(lash_core::SessionExecutionLeaseClaimOutcome::Acquired(
            lash_core::SessionExecutionLeaseAcquisition::fresh(lease),
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> std::result::Result<lash_core::SessionExecutionLease, lash_core::store::StoreError> {
        Ok(test_session_execution_lease(
            &fence.session_id,
            &fence.owner,
            &fence.executor_id,
            lease_ttl_ms,
            fence.fencing_token,
        ))
    }

    async fn release_session_execution_lease(
        &self,
        _completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn get_session_execution_lease(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Option<lash_core::SessionExecutionLease>, lash_core::store::StoreError>
    {
        Ok(None)
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &lash_core::TurnId,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
    ) -> std::result::Result<Option<lash_core::TurnInputClaim>, lash_core::store::StoreError> {
        unreachable!("BoundSessionStore does not serve pending turn input")
    }

    async fn claim_next_turn_inputs(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _policy: lash_core::QueuedWorkClaimPolicy,
    ) -> std::result::Result<
        Option<lash_core::runtime::QueuedWorkClaim>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn claim_checkpoint_work(
        &self,
        _session_id: &str,
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _turn_id: &lash_core::TurnId,
        _checkpoint: lash_core::CheckpointKind,
        _max_inputs: usize,
        _policy: lash_core::QueuedWorkClaimPolicy,
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
        _session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        _owner: &lash_core::LeaseOwnerIdentity,
        _boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        _batch_ids: &[String],
        _policy: lash_core::QueuedWorkClaimPolicy,
    ) -> std::result::Result<lash_core::SelectedQueuedWorkClaimOutcome, lash_core::store::StoreError>
    {
        Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
            None,
            Vec::new(),
        ))
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

    async fn pending_session_work_ordering(
        &self,
        _session_id: &str,
    ) -> std::result::Result<
        lash_core::store::PendingSessionWorkOrdering,
        lash_core::store::StoreError,
    > {
        Ok(lash_core::store::PendingSessionWorkOrdering {
            session_command: None,
            turn_input: None,
        })
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
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        _session_id: &str,
    ) -> std::result::Result<bool, lash_core::store::StoreError> {
        Ok(false)
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        _session_id: &str,
    ) -> std::result::Result<Vec<(String, String)>, lash_core::store::StoreError> {
        Ok(Vec::new())
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
}

#[derive(Default)]
struct RecordingStoreFactory {
    requests: std::sync::Mutex<Vec<lash_core::SessionStoreCreateRequest>>,
}

impl RecordingStoreFactory {
    fn session_ids(&self) -> Vec<String> {
        self.requests
            .lock_recover()
            .iter()
            .map(|request| request.session_id.clone())
            .collect()
    }
}

// SnapshotStore has a no-op attachment manifest; this request-recording
// fixture explicitly owns no attachment roots.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for RecordingStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for RecordingStoreFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        self.requests.lock_recover().push(request.clone());
        Ok(Arc::new(SnapshotStore::default()))
    }

    // Every create_store hands back a fresh store; nothing is ever tombstoned.
    async fn session_was_deleted(&self, _session_id: &str) -> std::result::Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(&self, _session_id: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct DeletingStoreFactory {
    stores: std::sync::Mutex<std::collections::HashMap<String, Arc<SnapshotStore>>>,
    tombstones: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

// SnapshotStore has a no-op attachment manifest; this deletion fixture
// explicitly owns no attachment roots.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for DeletingStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for DeletingStoreFactory {
    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        let store = self
            .stores
            .lock_recover()
            .entry(request.session_id.clone())
            .or_insert_with(|| Arc::new(SnapshotStore::default()))
            .clone();
        Ok(store as Arc<dyn lash_core::RuntimePersistence>)
    }

    async fn open_existing_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, String> {
        Ok(self
            .stores
            .lock_recover()
            .get(&request.session_id)
            .cloned()
            .map(|store| store as Arc<dyn lash_core::RuntimePersistence>))
    }

    // This fixture really deletes, so it really keeps the tombstone.
    async fn session_was_deleted(&self, session_id: &str) -> std::result::Result<bool, String> {
        Ok(self.tombstones.lock_recover().contains(session_id))
    }

    async fn delete_session(&self, session_id: &str) -> std::result::Result<(), String> {
        self.stores.lock_recover().remove(session_id);
        self.tombstones
            .lock_recover()
            .insert(session_id.to_string());
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

#[cfg(feature = "rlm")]
struct FailingAppTools;

#[cfg(feature = "rlm")]
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

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == app_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "app_lookup");
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(err) => return lash_core::ToolResult::err_fmt(err),
        };
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(key);
        }
        lash_core::ToolResult::pending(lash_core::PendingCompletion::new())
    }
}

#[cfg(feature = "rlm")]
struct DurableInputTools {
    key_tx:
        StdMutex<Option<oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>>>,
    attempt_count: Arc<AtomicUsize>,
}

#[cfg(feature = "rlm")]
struct RetryingDirectTools;

#[cfg(feature = "rlm")]
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
                lash_core::facade_support::DirectRequest::text(
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

#[cfg(feature = "rlm")]
fn retrying_direct_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_lashlang_binding(
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
        .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 0, 0)),
        "retrying_direct",
    )
}

#[cfg(feature = "rlm")]
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
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(result);
        }
    }
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for DurableInputTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![durable_input_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "mock_input_request").then(|| Arc::new(durable_input_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == durable_input_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolResult {
        assert_eq!(call.name, "mock_input_request");
        let question = call
            .args
            .get("question")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("answer")
            .to_string();
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(err) => {
                self.send_key_result(Err(err.to_string()));
                return lash_core::ToolResult::err_fmt(err);
            }
        };
        self.attempt_count.fetch_add(1, Ordering::SeqCst);
        // The attempt body cannot append process events. It declares the
        // announcement instead, and the runtime appends it when the call parks.
        let announcement = lash_core::PendingAnnouncement::new(
            "process.yield",
            serde_json::json!({
                "type": "work.input_request.opened",
                "request_id": "request-1",
                "question": question,
                "await_key_id": key.key_id,
            }),
            "mock-input-request:request-1",
        );
        self.send_key_result(Ok(key));
        lash_core::ToolResult::pending(lash_core::PendingCompletion::new().announcing(announcement))
    }
}

#[cfg(feature = "rlm")]
fn durable_input_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_lashlang_binding(
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
        ),
        "mock_input_request",
    )
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
                frame_key: lash_core::FrameKey::from_caller_material("durable-follow-frame")
                    .expect("non-empty caller material"),
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
    test_tool_definition_with_lashlang_binding(
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
        ),
        "app_lookup",
    )
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
    test_tool_definition_with_lashlang_binding(
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
        ),
        "app_lookup",
    )
}

#[cfg(feature = "rlm")]
fn test_tool_definition_with_lashlang_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition.with_lashlang_binding(lash_lashlang_runtime::LashlangToolBinding::new(
        ["tools"],
        name,
    ))
}

#[cfg(not(feature = "rlm"))]
fn test_tool_definition_with_lashlang_binding(
    definition: lash_core::ToolDefinition,
    _name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition
}

struct SurfacePluginFactory;

impl lash_core::facade_support::PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(SurfacePlugin))
    }
}

struct SurfacePlugin;

impl lash_core::facade_support::SessionPlugin for SurfacePlugin {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.output().response(Arc::new(|ctx| {
            Box::pin(async move {
                Ok(lash_core::facade_support::AssistantResponseTransform {
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

#[cfg(feature = "rlm")]
fn lashlang_block(source: &str) -> String {
    format!("<lashlang>\n{}\n</lashlang>", source.trim())
}

#[cfg(feature = "rlm")]
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

#[cfg(feature = "rlm")]
fn native_tool_call_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("native-tool-call-under-rlm")
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "native-call-1".to_string(),
                    tool_name: "native_lookup".to_string(),
                    input_json: r#"{"query":"forbidden"}"#.to_string(),
                    replay: None,
                }],
                terminal_reason: lash_core::LlmTerminalReason::ToolUse,
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
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
                seen.lock_recover()
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

#[cfg(feature = "rlm")]
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
                seen.lock_recover().push(system_text(&request));
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

#[cfg(feature = "rlm")]
fn recording_request_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("request-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover().push(request_text(&request));
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
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
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
                    if let Some(tx) = entered_tx.lock_recover().take() {
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
    explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
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
        lash_protocol_rlm::RlmProtocolPluginConfig::new(
            lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
            lash_protocol_rlm::ExecutionBound::secs(30),
            lash_protocol_rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        inmem_artifact_store(),
    )
}

/// A [`LashCoreBuilder`] pre-seeded with the default RLM factory.
#[cfg(feature = "rlm")]
fn rlm_core_builder() -> crate::core::LashCoreBuilder {
    LashCore::rlm_builder(crate::TurnBudget::Unbounded, rlm_factory())
}

fn inline_scope(scope: lash_core::ExecutionScope) -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::InlineRuntimeEffectController::default()),
        scope,
    )
    .expect("inline execution scope")
}

fn turn_scope(session_id: &str) -> lash_core::ScopedEffectController<'static> {
    inline_scope(lash_core::ExecutionScope::turn(
        session_id,
        lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string())
            .0
            .to_string(),
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

async fn session_delete_scope(
    core: &LashCore,
    session_id: &str,
) -> lash_core::ScopedEffectController<'static> {
    inline_scope(
        core.session_delete_scope(session_id)
            .await
            .expect("session delete execution scope"),
    )
}

fn explicit_ephemeral_facets(
    builder: crate::core::LashCoreBuilder,
) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets_with_budget(builder, crate::CommitBudget::bounded(1024 * 1024, 512))
}

fn explicit_ephemeral_facets_with_budget(
    builder: crate::core::LashCoreBuilder,
    commit_budget: crate::CommitBudget,
) -> crate::core::LashCoreBuilder {
    builder
        .commit_budget(commit_budget)
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
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
        parts: lash_core::facade_support::shared_parts(vec![lash_core::Part::text(
            format!("{id}.p0"),
            text.to_string(),
            None,
        )]),
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
#[cfg(feature = "rlm")]
mod processes_endstate;
#[cfg(feature = "rlm")]
mod rebuild_conformance;
mod rolling_history_persistence;
#[cfg(feature = "rlm")]
mod stack_budget;
mod tool_intent_ingress;
mod turn_streaming;

/// `SnapshotStore` backs the facade tests, so it owes the displacement contract
/// too: a double that reports no displacement would let a facade-level regression
/// in the takeover event pass unnoticed.
#[tokio::test]
async fn snapshot_store_reports_the_holder_a_claim_displaces() {
    let store = SnapshotStore::default();
    lash_core::testing::conformance::session_execution_lease_displacement(
        &store,
        "snapshot-lease-displacement",
    )
    .await;
}

#[tokio::test]
async fn deployment_drain_status_keeps_waiting_process_non_drained() {
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::memory()
            .await
            .expect("open in-memory process registry"),
    );
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(crate::TurnBudget::Unbounded)
            .model(mock_model_spec())
            .store_factory(Arc::new(
                crate::persistence::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(registry.clone()),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("build core with a process registry");
    let process_id = "deployment-drain-status-waiting";
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register waiting process");
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id,
        "deployment-drain-status-waiting-run",
    )
    .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("attempt-bound invocation has a start fact");
    registry
        .record_first_started_with_authority(process_id, started, &authority)
        .await
        .expect("record process start");
    registry
        .set_process_wait_with_authority(
            process_id,
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "deployment-drain-status".to_string(),
                    event_type: "deployment.drain_status".to_string(),
                    key: "deployment-drain-status-waiting:signal".to_string(),
                    ordinal: 1,
                },
            },
            &authority,
        )
        .await
        .expect("set process waiting");

    let status = core
        .drain_status(false)
        .await
        .expect("read deployment drain status");
    assert_eq!(status.remaining_invocations, 1);
    assert!(!status.drained);
}
