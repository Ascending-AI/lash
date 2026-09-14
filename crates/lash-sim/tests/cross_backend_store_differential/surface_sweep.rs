//! The operation inventory over the fallible store-trait surface (FIG-2841).
//!
//! `trait_surface_gate.rs` refuses any fallible method of the gated store
//! traits that this binary never calls. The drivers live here: one
//! [`SurfaceMethod`] per trait method, each driven as its own compared step so
//! the error class, the mutated-or-not verdict, and the post-state digest are
//! all compared across the three backends, and so the no-residue law is
//! checked on any step that refuses.
//!
//! Answers are summarized coarsely and deliberately. Backend-generated ids,
//! sequence counters, and wall-clock stamps are not cross-backend comparable;
//! what a read *found* is. The durable contract itself is carried by the
//! digest comparison the step loop already performs.

use super::*;
use corrupt_input_cases::CorruptBackup;

/// A syntactically valid but never-granted lease authority, for the inventory
/// steps that take a fence in a case that holds no lease. Presenting it is
/// itself a refusal driver: no backend may act on unheld authority.
fn unheld_lease_fence(session_id: &SessionId) -> lash_core::SessionExecutionLeaseAuthority {
    lash_core::SessionExecutionLeaseAuthority {
        session_id: session_id.clone(),
        owner: LeaseOwnerIdentity::opaque("fig-2841-no-lease", "fig-2841-no-lease:incarnation"),
        executor_id: "fig-2841-no-lease-executor".to_string(),
        lease_token: "fig-2841-no-lease-token".to_string(),
        fencing_token: 0,
    }
}

/// Scratch state the sweep threads between its own steps.
#[derive(Default)]
pub(super) struct SurfaceScratch {
    pub(super) answer: Option<String>,
    pub(super) corrupt_backup: Option<CorruptBackup>,
    pub(super) corrupt_target: Option<corrupt_input_cases::CorruptTarget>,
    pub(super) batch_id: Option<String>,
    pub(super) queued_work_claim: Option<QueuedWorkClaim>,
    pub(super) turn_input_claim: Option<TurnInputClaim>,
}

/// One fallible store-trait method, driven as a compared differential step.
#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceMethod {
    LoadSession,
    ListPendingTurnInputs,
    ListTurnInputApplications,
    ClaimNextTurnInputs,
    ClaimReadyQueuedWork,
    ReadSessionStateVersion,
    AdmitSessionState,
    LoadKnownNode,
    LoadUnknownNode,
    GetSessionExecutionLease,
    RenewSessionExecutionLease,
    ListQueuedWork,
    ListPendingQueuedWork,
    PendingSessionWorkOrdering,
    EnqueueQueuedWorkWithOutcome,
    ClaimLeadingReadySessionCommand,
    ClaimReadyQueuedWorkByUnknownBatchIds,
    ClaimCheckpointWork,
    AbandonQueuedWorkClaims,
    QueuedWorkBatchCompleted,
    CancelQueuedWorkBatch,
    ClaimActiveTurnInputs,
    AbandonTurnInputClaim,
    AbandonTurnInputClaims,
    CancelUnknownPendingTurnInput,
    CancelPendingTurnInputs,
    CancelPendingTurnInputSuffix,
    OrphanedActiveTurnIds,
    CommittedTurnExists,
    UncommittedTurnExists,
    AbortUnknownAttachmentWrite,
    CommitUnknownAttachmentRefs,
    ForgetUnknownAttachment,
    Vacuum,
}

impl SurfaceMethod {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::LoadSession => "surface:load_session",
            Self::ListPendingTurnInputs => "surface:list_pending_turn_inputs",
            Self::ListTurnInputApplications => "surface:list_turn_input_applications",
            Self::ClaimNextTurnInputs => "surface:claim_next_turn_inputs",
            Self::ClaimReadyQueuedWork => "surface:claim_ready_queued_work",
            Self::ReadSessionStateVersion => "surface:read_session_state_version",
            Self::AdmitSessionState => "surface:admit_session_state",
            Self::LoadKnownNode => "surface:load_node_known",
            Self::LoadUnknownNode => "surface:load_node_unknown",
            Self::GetSessionExecutionLease => "surface:get_session_execution_lease",
            Self::RenewSessionExecutionLease => "surface:renew_session_execution_lease",
            Self::ListQueuedWork => "surface:list_queued_work",
            Self::ListPendingQueuedWork => "surface:list_pending_queued_work",
            Self::PendingSessionWorkOrdering => "surface:pending_session_work_ordering",
            Self::EnqueueQueuedWorkWithOutcome => "surface:enqueue_queued_work_with_outcome",
            Self::ClaimLeadingReadySessionCommand => "surface:claim_leading_ready_session_command",
            Self::ClaimReadyQueuedWorkByUnknownBatchIds => {
                "surface:claim_ready_queued_work_by_batch_ids_unknown"
            }
            Self::ClaimCheckpointWork => "surface:claim_checkpoint_work",
            Self::AbandonQueuedWorkClaims => "surface:abandon_queued_work_claims",
            Self::QueuedWorkBatchCompleted => "surface:queued_work_batch_completed",
            Self::CancelQueuedWorkBatch => "surface:cancel_queued_work_batch",
            Self::ClaimActiveTurnInputs => "surface:claim_active_turn_inputs",
            Self::AbandonTurnInputClaim => "surface:abandon_turn_input_claim",
            Self::AbandonTurnInputClaims => "surface:abandon_turn_input_claims",
            Self::CancelUnknownPendingTurnInput => "surface:cancel_pending_turn_input_unknown",
            Self::CancelPendingTurnInputs => "surface:cancel_pending_turn_inputs",
            Self::CancelPendingTurnInputSuffix => "surface:cancel_pending_turn_input_suffix",
            Self::OrphanedActiveTurnIds => "surface:orphaned_active_turn_ids",
            Self::CommittedTurnExists => "surface:committed_turn_exists_committed",
            Self::UncommittedTurnExists => "surface:committed_turn_exists_uncommitted",
            Self::AbortUnknownAttachmentWrite => "surface:abort_attachment_write_unknown",
            Self::CommitUnknownAttachmentRefs => "surface:commit_refs_unknown",
            Self::ForgetUnknownAttachment => "surface:forget_attachment_unknown",
            Self::Vacuum => "surface:vacuum",
        }
    }
}

fn unknown_attachment_id() -> AttachmentId {
    AttachmentId::parse(UNKNOWN_ATTACHMENT_ID).expect("the unknown-attachment id must parse")
}

fn unknown_attachment_intent(session_id: &SessionId) -> lash_core::AttachmentIntent {
    lash_core::AttachmentIntent {
        attachment_id: unknown_attachment_id(),
        session_id: session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{UNKNOWN_ATTACHMENT_ID}"),
        intent_at_epoch_ms: 1_000,
        owner: None,
    }
}

fn surface(method: SurfaceMethod) -> StoreOperation {
    StoreOperation::DriveSurface { method }
}

const UNKNOWN_BATCH_ID: &str = "fig-2841-unknown-batch";
/// A turn id no case ever commits. Paired with [`SURFACE_COMMITTED_TURN_ID`]
/// so the membership read is driven over both answers, not just the one a
/// backend could return by refusing to look.
const UNCOMMITTED_TURN_ID: &str = "fig-2841-uncommitted-turn";
/// The turn id the surface sweep's seed commit stamps.
const SURFACE_COMMITTED_TURN_ID: &str = "fig-2841-surface-committed-turn";
/// An attachment id no case ever writes. The manifest drivers use
/// it so the inventory covers those methods without mutating an attachment the
/// surrounding case depends on: an unknown entity is itself a refusal driver,
/// and whatever a backend answers, the no-residue law still applies.
const UNKNOWN_ATTACHMENT_ID: &str = "fig-2841-unknown-attachment";
const UNKNOWN_INPUT_ID: &str = "fig-2841-unknown-input";

/// Every fallible method in the inventory, driven against a live session.
pub(super) fn surface_sweep_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::StoreSurfaceSweep,
        operations: vec![
            // The seed commit stamps a turn so the inventory can drive
            // `committed_turn_exists` over a turn that is actually committed.
            StoreOperation::Commit {
                label: "seed_surface_sweep_graph",
                expected_head_revision: 0,
                graph: append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: SURFACE_COMMITTED_TURN_ID,
                }),
                checkpoint: CheckpointSpec::Empty,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "surface-sweep-owner",
            },
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::AdmitSessionState),
            surface(SurfaceMethod::LoadKnownNode),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::GetSessionExecutionLease),
            surface(SurfaceMethod::RenewSessionExecutionLease),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::ClaimLeadingReadySessionCommand),
            surface(SurfaceMethod::AbandonQueuedWorkClaims),
            surface(SurfaceMethod::ClaimReadyQueuedWorkByUnknownBatchIds),
            surface(SurfaceMethod::ClaimCheckpointWork),
            surface(SurfaceMethod::AbandonQueuedWorkClaims),
            surface(SurfaceMethod::QueuedWorkBatchCompleted),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::ClaimActiveTurnInputs),
            surface(SurfaceMethod::AbandonTurnInputClaim),
            surface(SurfaceMethod::AbandonTurnInputClaims),
            surface(SurfaceMethod::OrphanedActiveTurnIds),
            surface(SurfaceMethod::CommittedTurnExists),
            surface(SurfaceMethod::UncommittedTurnExists),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::CancelPendingTurnInputSuffix),
            surface(SurfaceMethod::CancelPendingTurnInputs),
            surface(SurfaceMethod::AbortUnknownAttachmentWrite),
            surface(SurfaceMethod::CommitUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            surface(SurfaceMethod::Vacuum),
        ],
    }
}

/// The same surface after the session is gone: a refusal driver whose whole
/// point is that nothing it refuses may leave a durable trace.
pub(super) fn refused_surface_on_deleted_session_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::RefusedSurfaceOnDeletedSession,
        operations: vec![
            commit(
                "seed_deleted_session_surface_graph",
                0,
                append(
                    vec![
                        NodeSpec::new("root", None, "root"),
                        NodeSpec::new("active-frame", Some("root"), "active"),
                    ],
                    Some("active-frame"),
                ),
            ),
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "deleted-surface-owner",
            },
            StoreOperation::DeleteSession,
            surface(SurfaceMethod::ReadSessionStateVersion),
            surface(SurfaceMethod::LoadUnknownNode),
            surface(SurfaceMethod::GetSessionExecutionLease),
            surface(SurfaceMethod::RenewSessionExecutionLease),
            surface(SurfaceMethod::ListQueuedWork),
            surface(SurfaceMethod::ListPendingQueuedWork),
            surface(SurfaceMethod::PendingSessionWorkOrdering),
            surface(SurfaceMethod::EnqueueQueuedWorkWithOutcome),
            surface(SurfaceMethod::ClaimLeadingReadySessionCommand),
            surface(SurfaceMethod::CancelUnknownPendingTurnInput),
            surface(SurfaceMethod::CancelQueuedWorkBatch),
            surface(SurfaceMethod::AbortUnknownAttachmentWrite),
            surface(SurfaceMethod::CommitUnknownAttachmentRefs),
            surface(SurfaceMethod::ForgetUnknownAttachment),
            surface(SurfaceMethod::Vacuum),
        ],
    }
}

impl BackendRunner {
    /// Drive one inventory method. Returns `Err` verbatim: the step loop owns
    /// the refusal comparison and the no-residue check.
    pub(super) async fn drive_surface(
        &mut self,
        method: SurfaceMethod,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        let store = self.store();
        let session_id = self.session_id.clone();
        // Lease credentials are copied out up front: the sweep mutates its own
        // scratch state inside these arms, which cannot hold a borrow of self.
        let lease_credentials = self.first_lease.as_ref().map(|lease| {
            (
                lease.owner.clone(),
                lease.fence(),
                lease.completion().fencing_token,
            )
        });
        let (lease_owner, lease_fence) = lease_credentials
            .map(|(owner, fence, _)| (owner, fence))
            .unwrap_or_else(|| {
                (
                    LeaseOwnerIdentity::opaque(
                        "fig-2841-no-lease",
                        "fig-2841-no-lease:incarnation",
                    ),
                    unheld_lease_fence(&session_id),
                )
            });
        let answer = match method {
            SurfaceMethod::LoadSession => {
                format!("present={}", store.load_session().await?.is_some())
            }
            SurfaceMethod::ListPendingTurnInputs => {
                format!(
                    "rows={}",
                    store.list_pending_turn_inputs(&session_id).await?.len()
                )
            }
            SurfaceMethod::ListTurnInputApplications => {
                format!(
                    "rows={}",
                    store.list_turn_input_applications(&session_id).await?.len()
                )
            }
            SurfaceMethod::ClaimNextTurnInputs => {
                let claim = store
                    .claim_next_turn_inputs(&session_id, &lease_fence, &lease_owner, 1)
                    .await?;
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::ClaimReadyQueuedWork => {
                let outcome = store
                    .claim_ready_queued_work(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        QueuedWorkClaimBoundary::Idle,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                let claim = outcome.claim();
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.queued_work_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::ReadSessionStateVersion => {
                format!("version={}", store.read_session_state_version().await?)
            }
            SurfaceMethod::AdmitSessionState => {
                let admission = store.admit_session_state(&lease_fence).await?;
                format!("version={}", admission.version)
            }
            SurfaceMethod::LoadKnownNode => {
                let node_id = scoped_node_id(&session_id, "root");
                let node = store.load_node(&node_id).await?;
                format!("present={}", node.is_some())
            }
            SurfaceMethod::LoadUnknownNode => {
                let node = store.load_node("fig-2841-unknown-node").await?;
                format!("present={}", node.is_some())
            }
            SurfaceMethod::GetSessionExecutionLease => {
                let observation = store.get_session_execution_lease(&session_id).await?;
                format!("lease_present={}", observation.lease.is_some())
            }
            SurfaceMethod::RenewSessionExecutionLease => {
                let renewed = store
                    .renew_session_execution_lease(&lease_fence, SESSION_LEASE_TTL_MS)
                    .await?;
                format!("fencing_token={}", renewed.fencing_token)
            }
            SurfaceMethod::ListQueuedWork => {
                format!("rows={}", store.list_queued_work(&session_id).await?.len())
            }
            SurfaceMethod::ListPendingQueuedWork => {
                format!(
                    "rows={}",
                    store.list_pending_queued_work(&session_id).await?.len()
                )
            }
            SurfaceMethod::PendingSessionWorkOrdering => {
                let ordering = store.pending_session_work_ordering(&session_id).await?;
                format!(
                    "session_command={} turn_input={}",
                    ordering.session_command.is_some(),
                    ordering.turn_input.is_some()
                )
            }
            SurfaceMethod::EnqueueQueuedWorkWithOutcome => {
                let outcome = store
                    .enqueue_queued_work_with_outcome(
                        QueuedWorkBatchDraft::new(
                            &session_id,
                            DeliveryPolicy::EarliestSafeBoundary,
                            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                                reason: "fig-2841 surface sweep".to_string(),
                            },
                        )
                        .with_source_key("fig-2841-surface-sweep"),
                    )
                    .await?;
                let batch = outcome.into_batch();
                self.surface.batch_id = Some(batch.batch_id.to_string());
                format!("source_key={:?}", batch.source_key)
            }
            SurfaceMethod::ClaimLeadingReadySessionCommand => {
                let claim = store
                    .claim_leading_ready_session_command(&session_id, &lease_fence, &lease_owner)
                    .await?;
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.queued_work_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::ClaimReadyQueuedWorkByUnknownBatchIds => {
                let outcome = store
                    .claim_ready_queued_work_by_batch_ids(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        QueuedWorkClaimBoundary::Idle,
                        &[UNKNOWN_BATCH_ID.into()],
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                format!(
                    "claimed={} already_satisfied={}",
                    outcome.claim.is_some(),
                    outcome.already_satisfied_batch_ids.len()
                )
            }
            SurfaceMethod::ClaimCheckpointWork => {
                let (input_claim, work_claim) = store
                    .claim_checkpoint_work(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        &lash_core::TurnId::from("fig-2841-surface-turn"),
                        lash_core::CheckpointKind::AfterWork,
                        1,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await?;
                let summary = format!(
                    "input_claim={} work_claim={}",
                    input_claim.is_some(),
                    work_claim.is_some()
                );
                if let Some(claim) = input_claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                if let Some(claim) = work_claim {
                    self.surface.queued_work_claim = Some(claim);
                }
                summary
            }
            SurfaceMethod::AbandonQueuedWorkClaims => {
                let claims: Vec<QueuedWorkClaim> =
                    self.surface.queued_work_claim.take().into_iter().collect();
                let count = claims.len();
                store.abandon_queued_work_claims(&claims).await?;
                format!("abandoned={count}")
            }
            SurfaceMethod::QueuedWorkBatchCompleted => {
                let completed = store
                    .queued_work_batch_completed(&session_id, UNKNOWN_BATCH_ID)
                    .await?;
                format!("completed={completed}")
            }
            SurfaceMethod::CancelQueuedWorkBatch => {
                let batch_id = self
                    .surface
                    .batch_id
                    .clone()
                    .unwrap_or_else(|| UNKNOWN_BATCH_ID.to_string());
                let cancelled = store
                    .cancel_queued_work_batch(&session_id, &batch_id)
                    .await?;
                format!("cancelled={}", cancelled.is_some())
            }
            SurfaceMethod::ClaimActiveTurnInputs => {
                let claim = store
                    .claim_active_turn_inputs(
                        &session_id,
                        &lease_fence,
                        &lease_owner,
                        &lash_core::TurnId::from("fig-2841-surface-turn"),
                        lash_core::CheckpointKind::AfterWork,
                        1,
                    )
                    .await?;
                let claimed = claim.is_some();
                if let Some(claim) = claim {
                    self.surface.turn_input_claim = Some(claim);
                }
                format!("claimed={claimed}")
            }
            SurfaceMethod::AbandonTurnInputClaim => match self.surface.turn_input_claim.take() {
                Some(claim) => {
                    store.abandon_turn_input_claim(&claim).await?;
                    "abandoned=1".to_string()
                }
                None => "abandoned=0".to_string(),
            },
            SurfaceMethod::AbandonTurnInputClaims => {
                let claims: Vec<TurnInputClaim> =
                    self.surface.turn_input_claim.take().into_iter().collect();
                let count = claims.len();
                store.abandon_turn_input_claims(&claims).await?;
                format!("abandoned={count}")
            }
            SurfaceMethod::CancelUnknownPendingTurnInput => {
                let outcome = store
                    .cancel_pending_turn_input(&session_id, UNKNOWN_INPUT_ID)
                    .await?;
                format!(
                    "not_found={}",
                    matches!(outcome, lash_core::PendingTurnInputCancelOutcome::NotFound)
                )
            }
            SurfaceMethod::CancelPendingTurnInputs => {
                let targets = vec![lash_core::PendingTurnInputCancelTarget::input_id(format!(
                    "{session_id}:input"
                ))];
                let receipts = store
                    .cancel_pending_turn_inputs(&session_id, &targets)
                    .await?;
                format!("receipts={}", receipts.len())
            }
            SurfaceMethod::CancelPendingTurnInputSuffix => {
                let anchor = lash_core::PendingTurnInputCancelTarget::input_id(UNKNOWN_INPUT_ID);
                let outcome = store
                    .cancel_pending_turn_input_suffix(&session_id, &anchor)
                    .await?;
                match outcome {
                    lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { .. } => {
                        "anchor_not_found".to_string()
                    }
                    lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes {
                        outcomes, ..
                    } => format!("outcomes={}", outcomes.len()),
                }
            }
            SurfaceMethod::OrphanedActiveTurnIds => {
                let turn_ids = store
                    .orphaned_active_turn_ids(
                        &session_id,
                        &lease_fence,
                        lash_core::store::OrphanedTurnInputScope::LaneGeneration {
                            resumable_turn_id: None,
                        },
                    )
                    .await?;
                format!("turn_ids={}", turn_ids.len())
            }
            SurfaceMethod::CommittedTurnExists => {
                let exists = store
                    .committed_turn_exists(&lash_core::TurnId::from(SURFACE_COMMITTED_TURN_ID))
                    .await?;
                format!("exists={exists}")
            }
            SurfaceMethod::UncommittedTurnExists => {
                let exists = store
                    .committed_turn_exists(&lash_core::TurnId::from(UNCOMMITTED_TURN_ID))
                    .await?;
                format!("exists={exists}")
            }
            SurfaceMethod::AbortUnknownAttachmentWrite => {
                let intent = unknown_attachment_intent(&session_id);
                let outcome = store
                    .begin_attachment_write(intent.clone())
                    .and_then(|fence| match fence {
                        lash_core::AttachmentWriteFence::Granted(permit) => store
                            .abort_attachment_write(&intent, permit)
                            .map(|()| "aborted"),
                        _ => Ok("not_granted"),
                    })?;
                format!("outcome={outcome}")
            }
            SurfaceMethod::CommitUnknownAttachmentRefs => store
                .commit_refs(&session_id, &[unknown_attachment_id()])
                .map(|()| "committed".to_string())?,
            SurfaceMethod::ForgetUnknownAttachment => store
                .forget(&session_id, &unknown_attachment_id())
                .map(|()| "forgotten".to_string())?,
            SurfaceMethod::Vacuum => {
                let report = store
                    .vacuum()
                    .await
                    .map_err(|_| StoreError::Backend("vacuum_failed".to_string()))?;
                format!(
                    "removed_nodes={} removed_input_tombstones={}",
                    report.removed_node_count, report.removed_pending_turn_input_tombstone_count
                )
            }
        };
        self.surface.answer = Some(answer);
        Ok(None)
    }
}
