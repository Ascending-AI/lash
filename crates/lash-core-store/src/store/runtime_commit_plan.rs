//! Backend-neutral decisions for the atomic runtime-commit sequence.
//!
//! Store backends own transaction mechanics and durable row access. This
//! module owns the semantic decisions made from those rows: request
//! validation, receipt adjudication, head and graph checks, the ordered write
//! set, and the receipt/result projection. A backend therefore reads typed
//! facts, asks the planner once, and executes the returned plan.

use crate::SessionId;
use std::collections::HashSet;

use super::{
    AppendRequestIdentity, BlobRef, RuntimeCommit, RuntimeCommitReceipt,
    RuntimeCommitReceiptDecision, RuntimeUsageDeltaIdentity, SessionCheckpoint, SessionHeadMeta,
    StoreError, decide_runtime_commit_receipt,
};

/// Durable receipt fields read by a backend before attempting a commit.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct RuntimeCommitReceiptRecord {
    /// Canonical hash stored by the first commit attempt.
    pub turn_commit_hash: String,
    /// Canonical result stored by the first commit attempt.
    pub result: RuntimeCommitReceipt,
    /// Plain-commit or versioned append-request replay identity.
    pub append_request_identity: AppendRequestIdentity,
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
    pub published_leaf: PublishedLeafFacts,
    /// Backends may pass `true` when the request has no ancestor fence.
    pub requested_ancestor_is_active: bool,
    /// Incoming node ids already occupied in durable history, including
    /// tombstoned rows.
    pub occupied_node_ids: HashSet<crate::NodeId>,
    /// The follow-on the head owes, read under the same authority as
    /// `actual_head_revision` (ADR 0101 §3).
    pub existing_pending_follow_on: Option<super::PendingFollowOn>,
}

/// The previously published leaf observed under commit authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishedLeafFacts {
    /// No leaf has been published.
    Absent,
    /// A live leaf with its immutable ancestry seed.
    Live(ParentNodeFacts),
    /// The published identity no longer names a live node.
    Retired { node_id: crate::NodeId },
}

/// Immutable derived facts for an append's durable parent node.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentNodeFacts {
    /// Parent identity cross-checked against the append edge.
    pub node_id: crate::NodeId,
    /// Zero-based distance from the graph root.
    pub generation: u64,
    /// Nearest frame boundary at or above the parent.
    pub frame_node_id: crate::NodeId,
}

/// Immutable facts the core planner prescribes for one appended node.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedNodeFacts {
    /// Appended node identity.
    pub node_id: crate::NodeId,
    /// Zero-based distance from the graph root.
    pub generation: u64,
    /// Nearest frame boundary at or above this node.
    pub frame_node_id: crate::NodeId,
}

/// A replay decision for an already committed operation.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct RuntimeCommitReplay {
    result: RuntimeCommitReceipt,
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
    pub fn into_result(self) -> RuntimeCommitReceipt {
        self.result
    }
}

/// Receipt row prescribed after all writes in a fresh plan succeed.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
pub struct RuntimeCommitReceiptWrite<'a> {
    /// Session namespace for the receipt.
    pub session_id: &'a SessionId,
    /// Canonical operation storage key.
    pub operation_key: &'a str,
    /// Canonical hash of the committed semantic content.
    pub turn_commit_hash: &'a str,
    /// Canonical result encoded into the durable receipt.
    pub result: &'a RuntimeCommitReceipt,
    /// Plain-commit or versioned append-request replay identity.
    pub append_request_identity: &'a AppendRequestIdentity,
}

/// Prepared semantic identity for one attempted runtime commit.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
pub struct RuntimeCommitPlanner {
    commit: RuntimeCommit,
    /// The fleet format this store's writers emit (ADR 0106 §1 `F`), read from
    /// the deployment's fleet-format row at open. Every version a commit
    /// stamps is mapped through it rather than bound to a build constant.
    fleet_format: super::FleetFormat,
    turn_commit_hash: String,
    operation_key: String,
    realized_node_timestamps: Vec<crate::session_graph::RealizedNodeTimestamp>,
    committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    turn_input_applications: Vec<crate::TurnInputApplication>,
}

impl RuntimeCommitPlanner {
    /// Validate request-only invariants and compute stable commit projections.
    ///
    /// `fleet_format` is the store's fleet-format row as the backend read it at
    /// open: the planner stamps every durable version it prescribes through
    /// [`super::FleetFormat::writer_version`], so a finalized fleet move needs
    /// no planner change.
    pub fn prepare(
        commit: RuntimeCommit,
        fleet_format: super::FleetFormat,
    ) -> Result<Self, StoreError> {
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
            fleet_format,
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

    /// Backends call this immediately after reading the head and before creating session
    /// metadata, preserving the binding fence ahead of receipt replay.
    pub fn validate_session_binding(
        &self,
        bound_session_id: Option<&SessionId>,
    ) -> Result<(), StoreError> {
        if let Some(bound_session_id) = bound_session_id
            && bound_session_id != self.commit.session_id
        {
            return Err(StoreError::SessionBindingMismatch {
                bound_session_id: SessionId::from(bound_session_id.to_string()),
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
        match decide_runtime_commit_receipt(
            &prior.turn_commit_hash,
            &self.turn_commit_hash,
            &prior.append_request_identity,
            &self.commit.turn_commit.append_request_identity,
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
            RuntimeCommitReceiptDecision::SemanticBoundaryIdentityConflict => {
                Err(StoreError::SemanticBoundaryIdentityConflict {
                    session_id: self.commit.session_id.clone(),
                    operation_key: self.commit.turn_commit.operation.key.clone(),
                })
            }
            RuntimeCommitReceiptDecision::RuntimeCommitConflict => {
                Err(StoreError::RuntimeTurnCommitConflict {
                    session_id: self.commit.session_id.clone(),
                    operation_key: self.operation_key.clone(),
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
        if let AppendRequestIdentity::Append {
            requested_ancestor_node_id: Some(required_node_id),
            ..
        } = &self.commit.turn_commit.append_request_identity
            && !facts.requested_ancestor_is_active
        {
            return Err(StoreError::AppendAncestorNotActive {
                required_node_id: crate::NodeId::from(required_node_id),
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
            .nodes()
            .iter()
            .find(|node| facts.occupied_node_ids.contains(&node.node_id))
        {
            return Err(StoreError::NodeIdCollision {
                node_id: node.node_id.clone(),
            });
        }

        let (old_leaf_node_id, parent_node_facts) = match facts.published_leaf {
            PublishedLeafFacts::Absent => (None, None),
            PublishedLeafFacts::Live(parent) => (Some(parent.node_id.clone()), Some(parent)),
            PublishedLeafFacts::Retired { node_id } => {
                return Err(StoreError::InvalidGraphLeaf {
                    leaf_node_id: Some(node_id),
                });
            }
        };
        let committed_leaf_node_id = self
            .commit
            .graph
            .leaf_node_id()
            .cloned()
            .or_else(|| old_leaf_node_id.clone());
        if let Some(first) = self.commit.graph.nodes().first()
            && first.parent_node_id.as_ref() != old_leaf_node_id.as_ref()
        {
            return Err(StoreError::InvalidGraphParent {
                node_id: first.node_id.clone(),
                expected: old_leaf_node_id.clone(),
                actual: first.parent_node_id.clone(),
            });
        }

        let (planned_node_facts, derived_frame_node_id) =
            derive_appended_node_facts(&self.commit.graph, parent_node_facts)?;
        if let Some(leaf_node_id) = committed_leaf_node_id.clone()
            && derived_frame_node_id.is_none()
        {
            return Err(StoreError::MissingFrameOpenAncestor { leaf_node_id });
        }
        if self.commit.current_frame_node_id.as_deref() != derived_frame_node_id.as_deref() {
            return Err(StoreError::CurrentFrameNodeMismatch {
                claimed: self
                    .commit
                    .current_frame_node_id
                    .clone()
                    .map(crate::FrameNodeId::into_inner),
                derived: derived_frame_node_id.map(crate::NodeId::into_inner),
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
                    claim_id: completion.settlement_identity(),
                    row_id: None,
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                });
            }
        }
        let derived_frame = derived_frame_node_id
            .clone()
            .map(crate::FrameNodeId::new)
            .transpose()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        super::validate_follow_on_head_write(
            &self.commit.session_id,
            facts.existing_pending_follow_on.as_ref(),
            &self.commit.turn_commit.operation,
            self.commit.pending_follow_on.as_ref(),
            derived_frame.as_ref(),
        )?;

        let next_head_revision = StoreError::checked_monotonic_increment(
            "session_head_revision",
            facts.actual_head_revision,
        )?;
        Ok(RuntimeCommitPlan {
            commit: &self.commit,
            fleet_format: self.fleet_format,
            turn_commit_hash: self.turn_commit_hash.clone(),
            operation_key: self.operation_key.clone(),
            actual_head_revision: facts.actual_head_revision,
            next_head_revision,
            old_leaf_node_id,
            committed_leaf_node_id,
            derived_frame_node_id,
            planned_node_facts,
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
    fleet_format: super::FleetFormat,
    turn_commit_hash: String,
    operation_key: String,
    actual_head_revision: u64,
    next_head_revision: u64,
    old_leaf_node_id: Option<crate::NodeId>,
    committed_leaf_node_id: Option<crate::NodeId>,
    derived_frame_node_id: Option<crate::NodeId>,
    planned_node_facts: Vec<PlannedNodeFacts>,
    realized_node_timestamps: Vec<crate::session_graph::RealizedNodeTimestamp>,
    committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    turn_input_applications: Vec<crate::TurnInputApplication>,
}

impl<'a> RuntimeCommitPlan<'a> {
    /// Derived facts to persist beside appended nodes, in append order.
    pub fn planned_node_facts(&self) -> &[PlannedNodeFacts] {
        &self.planned_node_facts
    }
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

    pub fn head_changed(&self) -> bool {
        self.old_leaf_node_id != self.committed_leaf_node_id
    }

    /// Assemble canonical session-head metadata after checkpoint storage.
    #[expect(
        clippy::expect_used,
        reason = "`FrameNodeId::new` rejects only the empty string, and a derived frame node id is never empty"
    )]
    pub fn head_meta(&self, checkpoint_ref: BlobRef) -> SessionHeadMeta {
        SessionHeadMeta {
            schema_version: self
                .fleet_format
                .writer_version(super::SESSION_HEAD_META_SCHEMA_VERSION),
            session_id: self.commit.session_id.clone(),
            head_revision: self.next_head_revision,
            config: self.commit.config.clone(),
            current_frame_node_id: self.derived_frame_node_id.clone().map(|frame_node_id| {
                crate::FrameNodeId::new(frame_node_id)
                    .expect("derived graph node identities are non-empty")
            }),
            checkpoint_ref: Some(checkpoint_ref),
            leaf_node_id: self.committed_leaf_node_id.clone(),
            pending_follow_on: self.commit.pending_follow_on.clone(),
        }
    }

    /// Construct the canonical result after backend writes finish.
    pub fn result(
        &self,
        checkpoint_ref: BlobRef,
        manifest: SessionCheckpoint,
    ) -> RuntimeCommitReceipt {
        RuntimeCommitReceipt {
            schema_version: self
                .fleet_format
                .writer_version(super::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
            head_revision: self.next_head_revision,
            checkpoint_ref,
            manifest,
            committed_leaf_node_id: self.committed_leaf_node_id.clone(),
            realized_node_timestamps: self.realized_node_timestamps.clone(),
            committed_usage_delta_identities: self.committed_usage_delta_identities.clone(),
            failure_evidence: self.commit.failure_evidence.clone(),
            pending_follow_on: self.commit.pending_follow_on.clone(),
            turn_input_applications: self.turn_input_applications.clone(),
            turn_cancel_input_outcome: crate::TurnCancelInputOutcome::default(),
            receipt_replayed: false,
        }
    }

    /// Project the durable receipt row for the supplied canonical result.
    pub fn receipt_write<'b>(
        &'b self,
        result: &'b RuntimeCommitReceipt,
    ) -> RuntimeCommitReceiptWrite<'b> {
        RuntimeCommitReceiptWrite {
            session_id: &self.commit.session_id,
            operation_key: &self.operation_key,
            turn_commit_hash: &self.turn_commit_hash,
            result,
            append_request_identity: &self.commit.turn_commit.append_request_identity,
        }
    }
}

fn derive_appended_node_facts(
    graph: &super::GraphAppend,
    mut parent: Option<ParentNodeFacts>,
) -> Result<(Vec<PlannedNodeFacts>, Option<crate::NodeId>), StoreError> {
    let mut planned = Vec::with_capacity(graph.nodes().len());
    for node in graph.nodes() {
        let generation = match parent.as_ref() {
            Some(parent) => StoreError::checked_monotonic_increment(
                "session_graph_generation",
                parent.generation,
            )?,
            None => 0,
        };
        let frame_node_id = if matches!(node.payload, crate::SessionNodePayload::FrameOpen { .. }) {
            node.node_id.clone()
        } else {
            parent
                .as_ref()
                .map(|parent| parent.frame_node_id.clone())
                .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
                    leaf_node_id: node.node_id.clone(),
                })?
        };
        let node_facts = PlannedNodeFacts {
            node_id: node.node_id.clone(),
            generation,
            frame_node_id,
        };
        parent = Some(ParentNodeFacts {
            node_id: node_facts.node_id.clone(),
            generation: node_facts.generation,
            frame_node_id: node_facts.frame_node_id.clone(),
        });
        planned.push(node_facts);
    }
    Ok((planned, parent.map(|parent| parent.frame_node_id)))
}

fn validate_session_execution_lease_plan(commit: &RuntimeCommit) -> Result<(), StoreError> {
    if commit.session_execution_lease_fence.is_some()
        && commit.release_session_execution_lease.is_some()
    {
        return Err(StoreError::RuntimeCommitLeaseAuthorityConflict {
            session_id: commit.session_id.clone(),
        });
    }
    if commit.interrupted_turn_input_cancellation.is_some()
        && commit.interrupted_turn_input_turn_id.is_none()
    {
        return Err(StoreError::Backend(
            "runtime commit cancellation evidence requires an interrupted turn id".to_string(),
        ));
    }
    if commit.interrupted_turn_cancel_intent.is_some()
        != commit.interrupted_turn_input_turn_id.is_some()
    {
        return Err(StoreError::Backend(
            "runtime commit cancellation intent predicate and interrupted turn id must be present together"
                .to_string(),
        ));
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
            session_id: SessionId::from("lease-plan-conflict"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let authority = crate::SessionExecutionLeaseAuthority {
            session_id: state.session_id.clone(),
            owner: crate::LeaseOwnerIdentity::opaque("owner", "incarnation"),
            executor_id: "executor".to_string(),
            lease_token: "token".to_string(),
            fencing_token: 1,
        };
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
            .borrowing_session_execution_lease(authority.clone())
            .releasing_session_execution_lease(authority);

        let error =
            match RuntimeCommitPlanner::prepare(commit, crate::store::FleetFormat::current()) {
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
    fn commit_rejects_cancellation_evidence_without_an_interrupted_turn() {
        let state = crate::RuntimeSessionState {
            session_id: SessionId::from("orphan-cancellation-evidence"),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        commit.interrupted_turn_input_cancellation = Some(crate::TurnCancellationEvidence {
            request_id: "request".to_string(),
            origin: None,
            reason: None,
            undelivered: crate::TurnCancelDisposition::Defer,
            mode: crate::TurnCancelMode::Immediate,
            honoured_after_step: None,
        });

        let error =
            match RuntimeCommitPlanner::prepare(commit, crate::store::FleetFormat::current()) {
                Ok(_) => panic!("cancellation evidence must name its interrupted turn"),
                Err(error) => error,
            };
        assert!(matches!(
            error,
            StoreError::Backend(message)
                if message == "runtime commit cancellation evidence requires an interrupted turn id"
        ));
    }

    #[test]
    fn fresh_commit_plan_refuses_exhausted_head_revision() {
        let state = crate::RuntimeSessionState {
            session_id: SessionId::from("revision-overflow"),
            head_revision: i64::MAX as u64,
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        let planner = RuntimeCommitPlanner::prepare(commit, crate::store::FleetFormat::current())
            .expect("prepare commit");
        let error = match planner.plan(FreshRuntimeCommitFacts {
            actual_head_revision: i64::MAX as u64,
            published_leaf: PublishedLeafFacts::Absent,
            requested_ancestor_is_active: true,
            occupied_node_ids: HashSet::new(),
            existing_pending_follow_on: None,
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
    #[test]
    fn fresh_commit_plan_retired_old_leaf_is_invalid_graph_leaf() {
        let state = crate::RuntimeSessionState {
            session_id: "retired-leaf".into(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let commit = RuntimeCommit::persisted_state_for_test(&state, &[]);
        let planner = RuntimeCommitPlanner::prepare(commit, crate::store::FleetFormat::current())
            .expect("prepare empty commit");
        let result = planner.plan(FreshRuntimeCommitFacts {
            actual_head_revision: 0,
            published_leaf: PublishedLeafFacts::Retired {
                node_id: "retired-parent".into(),
            },
            requested_ancestor_is_active: true,
            occupied_node_ids: HashSet::new(),
            existing_pending_follow_on: None,
        });
        assert!(
            matches!(result, Err(StoreError::InvalidGraphLeaf { leaf_node_id: Some(id) }) if id == "retired-parent")
        );
    }
}
