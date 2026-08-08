//! Backend-neutral decisions for the atomic runtime-commit sequence.
//!
//! Store backends own transaction mechanics and durable row access. This
//! module owns the semantic decisions made from those rows: request
//! validation, receipt adjudication, head and graph checks, the ordered write
//! set, and the receipt/result projection. A backend therefore reads typed
//! facts, asks the planner once, and executes the returned plan.

use std::collections::HashSet;

use super::{
    BlobRef, RuntimeCommit, RuntimeCommitReceiptDecision, RuntimeCommitResult,
    RuntimeUsageDeltaIdentity, SessionCheckpoint, SessionHeadMeta, SessionHeadPayload, StoreError,
    decide_runtime_commit_receipt,
};

/// Durable receipt fields read by a backend before attempting a commit.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct RuntimeCommitReceiptRecord {
    /// Canonical hash stored by the first commit attempt.
    pub turn_commit_hash: String,
    /// Canonical result stored by the first commit attempt.
    pub result: RuntimeCommitResult,
    /// Versioned append-request identity, when this is an append receipt.
    pub request_identity_hash: Option<String>,
    /// Canonical append identity encoding version.
    pub identity_encoding_version: Option<u32>,
    /// Semantic node count recorded for append corruption detection.
    pub requested_node_count: Option<u64>,
}

/// Backend observations needed to validate a fresh runtime commit.
///
/// Claim-row authority is deliberately absent: claim eligibility and rollback
/// remain backend work until the follow-up extraction. The plan still owns the
/// settlement hooks and their ordering after head publication.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct FreshRuntimeCommitFacts {
    /// Revision observed under the backend's commit authority.
    pub actual_head_revision: u64,
    /// Leaf published by the existing head, when one exists.
    pub old_leaf_node_id: Option<String>,
    /// Whether the requested append ancestor is on the active path. Backends
    /// may pass `true` when the request has no ancestor fence.
    pub requested_ancestor_is_active: bool,
    /// Incoming node ids already occupied in durable history, including
    /// tombstoned rows.
    pub occupied_node_ids: HashSet<String>,
    /// Whether a selected leaf not included in this append resolves to a live
    /// durable node.
    pub selected_leaf_is_live: bool,
    /// Whether this session owns any live durable graph-node row, including a
    /// live row outside the active path.
    pub has_live_nodes: bool,
    /// Whether the previously published leaf is still live. Backends derive
    /// this from an active-path read or a targeted liveness query under their
    /// commit authority.
    pub old_leaf_is_live: bool,
    /// Nearest live FrameOpen ancestor of the selected leaf after applying the
    /// append, or `None` when the resulting graph is empty.
    pub derived_frame_node_id: Option<String>,
}

/// A replay decision for an already committed operation.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct RuntimeCommitReplay {
    result: RuntimeCommitResult,
    release_session_execution_lease: Option<crate::store::SessionExecutionLeaseAuthority>,
}

impl RuntimeCommitReplay {
    /// Ancillary execution-lease release to attempt before returning replay.
    pub fn release_session_execution_lease(
        &self,
    ) -> Option<&crate::store::SessionExecutionLeaseAuthority> {
        self.release_session_execution_lease.as_ref()
    }

    /// Consume the replay prescription and return its canonical stored result.
    pub fn into_result(self) -> RuntimeCommitResult {
        self.result
    }
}

/// Receipt row prescribed after all writes in a fresh plan succeed.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct RuntimeCommitReceiptWrite<'a> {
    /// Session namespace for the receipt.
    pub session_id: &'a str,
    /// Canonical operation storage key.
    pub operation_key: &'a str,
    /// Canonical hash of the committed semantic content.
    pub turn_commit_hash: &'a str,
    /// Canonical result encoded into the durable receipt.
    pub result: &'a RuntimeCommitResult,
    /// Versioned append-request identity, when applicable.
    pub request_identity_hash: Option<&'a str>,
    /// Semantic append node count, when applicable.
    pub requested_node_count: Option<usize>,
    /// Append ancestor fence recorded for diagnostics and replay identity.
    pub requested_ancestor_node_id: Option<&'a str>,
    /// Canonical append identity encoding version.
    pub identity_encoding_version: Option<u32>,
}

/// Prepared semantic identity for one attempted runtime commit.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
pub struct RuntimeCommitPlanner {
    commit: RuntimeCommit,
    turn_commit_hash: String,
    operation_key: String,
    realized_node_timestamps: Vec<crate::session_graph::RealizedNodeTimestamp>,
    committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    turn_input_applications: Vec<crate::TurnInputApplication>,
}

impl RuntimeCommitPlanner {
    /// Validate request-only invariants and compute stable commit projections.
    pub fn prepare(commit: RuntimeCommit) -> Result<Self, StoreError> {
        commit.validate_budget()?;
        validate_session_execution_lease_plan(&commit)?;
        commit.validate_operation_session()?;

        let turn_commit_hash = commit.turn_commit_hash()?;
        let operation_key = commit.turn_commit.operation.storage_key()?;
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| crate::session_graph::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect();
        let committed_usage_delta_identities = commit
            .usage_deltas
            .iter()
            .map(|delta| delta.identity.clone())
            .collect();
        let turn_input_applications = commit.turn_input_applications();

        Ok(Self {
            commit,
            turn_commit_hash,
            operation_key,
            realized_node_timestamps,
            committed_usage_delta_identities,
            turn_input_applications,
        })
    }

    /// Borrow the validated request whose backend writes the plan prescribes.
    pub fn commit(&self) -> &RuntimeCommit {
        &self.commit
    }

    /// Return the canonical durable operation key used for receipt lookup.
    pub fn operation_key(&self) -> &str {
        &self.operation_key
    }

    /// Validate the session carried by an existing head. Backends call this
    /// immediately after reading the head and before creating session metadata,
    /// preserving the binding fence ahead of receipt replay.
    pub fn validate_session_binding(
        &self,
        bound_session_id: Option<&str>,
    ) -> Result<(), StoreError> {
        if let Some(bound_session_id) = bound_session_id
            && bound_session_id != self.commit.session_id
        {
            return Err(StoreError::SessionBindingMismatch {
                bound_session_id: bound_session_id.to_string(),
                attempted_session_id: self.commit.session_id.clone(),
            });
        }
        Ok(())
    }

    /// Validate node identities after the backend has established session
    /// authority and metadata, but before it adjudicates a receipt.
    pub fn validate_node_derivation(&self) -> Result<(), StoreError> {
        self.commit.validate_node_derivation()
    }

    /// Apply the shared head-CAS decision to a revision read under backend
    /// authority. PostgreSQL uses this after taking its row lock; serialized
    /// backends need it only through [`Self::plan`].
    pub fn validate_head_revision(&self, actual: u64) -> Result<(), StoreError> {
        validate_head_revision(self.commit.expected_head_revision, actual)
    }

    /// Adjudicate an optional prior receipt. `None` means execution may
    /// continue; `Some` is the replay plan the backend must return.
    pub fn decide_receipt(
        &self,
        prior: Option<RuntimeCommitReceiptRecord>,
    ) -> Result<Option<RuntimeCommitReplay>, StoreError> {
        let Some(prior) = prior else {
            return Ok(None);
        };
        let attempted_count = self
            .commit
            .turn_commit
            .requested_node_count
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                StoreError::Backend(
                    "attempted append requested-node count does not fit u64".to_string(),
                )
            })?;
        match decide_runtime_commit_receipt(
            &prior.turn_commit_hash,
            &self.turn_commit_hash,
            prior.identity_encoding_version,
            self.commit.turn_commit.identity_encoding_version,
            prior.request_identity_hash.as_deref(),
            self.commit.turn_commit.request_identity_hash.as_deref(),
            prior.requested_node_count,
            attempted_count,
        ) {
            RuntimeCommitReceiptDecision::Replay => {
                let mut result = prior.result;
                result.receipt_replayed = true;
                Ok(Some(RuntimeCommitReplay {
                    result,
                    release_session_execution_lease: self
                        .commit
                        .release_session_execution_lease
                        .clone(),
                }))
            }
            RuntimeCommitReceiptDecision::AppendIdentityConflict => {
                Err(StoreError::AppendOperationIdentityConflict {
                    session_id: self.commit.session_id.clone(),
                    operation_key: self.operation_key.clone(),
                })
            }
            RuntimeCommitReceiptDecision::RuntimeCommitConflict => {
                Err(StoreError::RuntimeTurnCommitConflict {
                    session_id: self.commit.session_id.clone(),
                    turn_id: self.operation_key.clone(),
                })
            }
            RuntimeCommitReceiptDecision::CorruptRequestedNodeCount { stored, attempted } => {
                Err(StoreError::AppendReceiptRequestedNodeCountCorrupt {
                    session_id: self.commit.session_id.clone(),
                    operation_key: self.operation_key.clone(),
                    stored,
                    attempted,
                })
            }
        }
    }

    /// Turn backend-read facts into the ordered write plan for a fresh commit.
    pub fn plan(
        &self,
        facts: FreshRuntimeCommitFacts,
    ) -> Result<RuntimeCommitPlan<'_>, StoreError> {
        if self.commit.turn_commit.requested_ancestor_node_id.is_some()
            && !facts.requested_ancestor_is_active
        {
            return Err(StoreError::AppendAncestorNotActive {
                required_node_id: self
                    .commit
                    .turn_commit
                    .requested_ancestor_node_id
                    .clone()
                    .expect("checked append ancestor"),
            });
        }
        validate_head_revision(
            self.commit.expected_head_revision,
            facts.actual_head_revision,
        )?;
        self.commit.validate_append_node_ids_unique()?;
        self.commit.graph.validate_append_topology()?;
        if let Some(node) = self
            .commit
            .graph
            .nodes
            .iter()
            .find(|node| facts.occupied_node_ids.contains(&node.node_id))
        {
            return Err(StoreError::NodeIdCollision {
                node_id: node.node_id.clone(),
            });
        }

        match self.commit.graph.leaf_node_id() {
            Some(leaf_node_id)
                if !self
                    .commit
                    .graph
                    .appended_nodes()
                    .any(|node| &node.node_id == leaf_node_id)
                    && !facts.selected_leaf_is_live =>
            {
                return Err(StoreError::InvalidGraphLeaf {
                    leaf_node_id: Some(leaf_node_id.clone()),
                });
            }
            None if !self.commit.graph.nodes.is_empty() || facts.has_live_nodes => {
                return Err(StoreError::InvalidGraphLeaf { leaf_node_id: None });
            }
            _ => {}
        }

        let old_leaf_node_id = facts.old_leaf_node_id.clone();
        if let Some(old_leaf_node_id) = old_leaf_node_id.as_ref()
            && !facts.old_leaf_is_live
        {
            return Err(StoreError::InvalidGraphLeaf {
                leaf_node_id: Some(old_leaf_node_id.clone()),
            });
        }
        match self.commit.graph.nodes.first() {
            None if self.commit.graph.leaf_node_id != old_leaf_node_id => {
                return Err(StoreError::InvalidGraphLeaf {
                    leaf_node_id: self.commit.graph.leaf_node_id.clone(),
                });
            }
            Some(first) if first.parent_node_id.as_ref() != old_leaf_node_id.as_ref() => {
                return Err(StoreError::InvalidGraphParent {
                    node_id: first.node_id.clone(),
                    expected: old_leaf_node_id.clone(),
                    actual: first.parent_node_id.clone(),
                });
            }
            _ => {}
        }
        if self.commit.graph.leaf_node_id.is_some() && facts.derived_frame_node_id.is_none() {
            return Err(StoreError::MissingFrameOpenAncestor {
                leaf_node_id: self
                    .commit
                    .graph
                    .leaf_node_id
                    .clone()
                    .expect("checked selected leaf"),
            });
        }
        if self.commit.current_frame_node_id != facts.derived_frame_node_id {
            return Err(StoreError::CurrentFrameNodeMismatch {
                claimed: self.commit.current_frame_node_id.clone(),
                derived: facts.derived_frame_node_id,
            });
        }
        for completion in &self.commit.completed_queue_claims {
            if completion.session_id != self.commit.session_id {
                return Err(StoreError::QueuedWorkClaimSuperseded {
                    session_id: completion.session_id.clone(),
                    claim_id: completion.claim_id.clone(),
                    row_id: None,
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                });
            }
        }
        for completion in &self.commit.completed_turn_input_claims {
            if completion.session_id != self.commit.session_id {
                return Err(StoreError::TurnInputClaimSuperseded {
                    session_id: completion.session_id.clone(),
                    claim_id: completion.claim_id.clone(),
                    row_id: None,
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                });
            }
        }
        for batch in &self.commit.enqueued_queue_batches {
            if batch.session_id != self.commit.session_id {
                return Err(StoreError::SessionBindingMismatch {
                    bound_session_id: self.commit.session_id.clone(),
                    attempted_session_id: batch.session_id.clone(),
                });
            }
            batch
                .validate_process_wake_source()
                .map_err(StoreError::Backend)?;
        }

        let next_head_revision = StoreError::checked_monotonic_increment(
            "session_head_revision",
            facts.actual_head_revision,
        )?;
        Ok(RuntimeCommitPlan {
            commit: &self.commit,
            turn_commit_hash: self.turn_commit_hash.clone(),
            operation_key: self.operation_key.clone(),
            actual_head_revision: facts.actual_head_revision,
            next_head_revision,
            old_leaf_node_id,
            derived_frame_node_id: facts.derived_frame_node_id,
            realized_node_timestamps: self.realized_node_timestamps.clone(),
            committed_usage_delta_identities: self.committed_usage_delta_identities.clone(),
            turn_input_applications: self.turn_input_applications.clone(),
        })
    }
}

/// Backend-independent ordered write set for a fresh runtime commit.
///
/// The plan exposes typed decisions and projections. Backends retain ownership
/// of transactional write and settlement ordering.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
pub struct RuntimeCommitPlan<'a> {
    commit: &'a RuntimeCommit,
    turn_commit_hash: String,
    operation_key: String,
    actual_head_revision: u64,
    next_head_revision: u64,
    old_leaf_node_id: Option<String>,
    derived_frame_node_id: Option<String>,
    realized_node_timestamps: Vec<crate::session_graph::RealizedNodeTimestamp>,
    committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    turn_input_applications: Vec<crate::TurnInputApplication>,
}

impl<'a> RuntimeCommitPlan<'a> {
    /// Head revision observed under backend commit authority.
    pub fn actual_head_revision(&self) -> u64 {
        self.actual_head_revision
    }

    /// Revision prescribed for successful head publication.
    pub fn next_head_revision(&self) -> u64 {
        self.next_head_revision
    }

    /// Previously published leaf from which retirement may begin.
    pub fn old_leaf_node_id(&self) -> Option<&str> {
        self.old_leaf_node_id.as_deref()
    }

    /// Whether head publication selects a leaf different from the prior head.
    pub fn head_changed(&self) -> bool {
        self.old_leaf_node_id != self.commit.graph.leaf_node_id
    }

    /// Error prescribed when a conditional head write affected no row. The
    /// write outcome is authoritative even if a diagnostic re-read happens to
    /// observe the original revision again.
    pub fn head_publication_conflict(&self, actual: u64) -> StoreError {
        StoreError::HeadRevisionConflict {
            expected: self.commit.expected_head_revision,
            actual,
        }
    }

    /// Assemble canonical session-head metadata after checkpoint storage.
    pub fn head_meta(&self, checkpoint_ref: BlobRef) -> SessionHeadMeta {
        SessionHeadMeta::assemble(
            SessionHeadPayload {
                schema_version: super::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: self.commit.session_id.clone(),
                config: self.commit.config.clone(),
                current_frame_node_id: self.derived_frame_node_id.clone(),
            },
            self.next_head_revision,
            Some(checkpoint_ref),
            self.commit.graph.leaf_node_id.clone(),
        )
    }

    /// Construct the canonical result after backend writes and enqueues finish.
    pub fn result(
        &self,
        checkpoint_ref: BlobRef,
        manifest: SessionCheckpoint,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    ) -> RuntimeCommitResult {
        RuntimeCommitResult {
            head_revision: self.next_head_revision,
            checkpoint_ref,
            manifest,
            committed_leaf_node_id: self.commit.graph.leaf_node_id.clone(),
            realized_node_timestamps: self.realized_node_timestamps.clone(),
            committed_usage_delta_identities: self.committed_usage_delta_identities.clone(),
            enqueued_queue_batches,
            turn_input_applications: self.turn_input_applications.clone(),
            receipt_replayed: false,
        }
    }

    /// Project the durable receipt row for the supplied canonical result.
    pub fn receipt_write<'b>(
        &'b self,
        result: &'b RuntimeCommitResult,
    ) -> RuntimeCommitReceiptWrite<'b> {
        RuntimeCommitReceiptWrite {
            session_id: &self.commit.session_id,
            operation_key: &self.operation_key,
            turn_commit_hash: &self.turn_commit_hash,
            result,
            request_identity_hash: self.commit.turn_commit.request_identity_hash.as_deref(),
            requested_node_count: self.commit.turn_commit.requested_node_count,
            requested_ancestor_node_id: self
                .commit
                .turn_commit
                .requested_ancestor_node_id
                .as_deref(),
            identity_encoding_version: self.commit.turn_commit.identity_encoding_version,
        }
    }
}

fn validate_session_execution_lease_plan(commit: &RuntimeCommit) -> Result<(), StoreError> {
    if commit.session_execution_lease_fence.is_some()
        && commit.release_session_execution_lease.is_some()
    {
        return Err(StoreError::RuntimeCommitLeaseAuthorityConflict {
            session_id: commit.session_id.clone(),
        });
    }
    Ok(())
}

fn validate_head_revision(expected: u64, actual: u64) -> Result<(), StoreError> {
    if expected != actual {
        return Err(StoreError::HeadRevisionConflict { expected, actual });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_plan_rejects_borrow_xor_release_violation_with_typed_error() {
        let state = crate::RuntimeSessionState {
            session_id: "lease-plan-conflict".to_string(),
            ..crate::RuntimeSessionState::default()
        };
        let authority = crate::SessionExecutionLeaseAuthority {
            session_id: state.session_id.clone(),
            owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            lease_token: "token".to_string(),
            fencing_token: 1,
        };
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .borrowing_session_execution_lease(authority.clone())
            .releasing_session_execution_lease(authority);

        let error = match RuntimeCommitPlanner::prepare(commit) {
            Ok(_) => panic!("one commit must not borrow and release the lane"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            StoreError::RuntimeCommitLeaseAuthorityConflict { session_id }
                if session_id == "lease-plan-conflict"
        ));
    }

    #[test]
    fn fresh_commit_plan_refuses_exhausted_head_revision() {
        let state = crate::RuntimeSessionState {
            session_id: "revision-overflow".to_string(),
            head_revision: i64::MAX as u64,
            ..crate::RuntimeSessionState::default()
        };
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        let planner = RuntimeCommitPlanner::prepare(commit).expect("prepare commit");
        let error = match planner.plan(FreshRuntimeCommitFacts {
            actual_head_revision: i64::MAX as u64,
            old_leaf_node_id: None,
            requested_ancestor_is_active: true,
            occupied_node_ids: HashSet::new(),
            selected_leaf_is_live: false,
            has_live_nodes: false,
            old_leaf_is_live: false,
            derived_frame_node_id: None,
        }) {
            Ok(_) => panic!("exhausted head revision must refuse"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            StoreError::MonotonicCounterOverflow {
                counter: "session_head_revision",
                current,
            } if current == i64::MAX as u64
        ));
    }
}
