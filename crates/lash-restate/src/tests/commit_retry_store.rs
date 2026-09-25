//! A pass-through store over the shared in-memory recovery store that counts
//! lease claims, for the laws that probe a commit retry.

use super::*;

pub(super) struct CommitRetryStore {
    pub(super) inner: Arc<dyn lash_core::RuntimePersistence>,
    pub(super) lease_claim_count: Arc<AtomicUsize>,
}

impl CommitRetryStore {
    pub(super) fn new(inner: Arc<dyn lash_core::RuntimePersistence>) -> Self {
        Self {
            inner,
            lease_claim_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

lash_core::impl_noop_attachment_manifest!(CommitRetryStore);

// Pass-through wrapper over the shared in-memory recovery store; every
// segment delegates to `inner`.
#[async_trait::async_trait]
impl lash_core::SessionCommitStore for CommitRetryStore {
    async fn raise_pending_follow_on_attempts(
        &self,
        lease: &lash_core::SessionExecutionLeaseAuthority,
        follow_on_turn_id: &lash_core::TurnId,
    ) -> Result<lash_core::store::PendingFollowOn, lash_core::StoreError> {
        self.inner
            .raise_pending_follow_on_attempts(lease, follow_on_turn_id)
            .await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, lash_core::StoreError> {
        self.inner.admit_and_bind_session(binding).await
    }

    async fn load_session(
        &self,
    ) -> Result<Option<lash_core::store::PersistedSessionRead>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<lash_core::store::SessionHeadMeta>, lash_core::StoreError> {
        Ok(None)
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, lash_core::StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        self.inner.commit_runtime_state(commit).await
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<lash_core::SessionMeta>, lash_core::StoreError> {
        self.inner.load_session_meta().await
    }
}

#[async_trait::async_trait]
impl lash_core::SessionExecutionLeaseStore for CommitRetryStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &SessionId,
        owner: &lash_core::LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLeaseClaimOutcome, lash_core::StoreError> {
        self.lease_claim_count.fetch_add(1, Ordering::SeqCst);
        self.inner
            .try_claim_session_execution_lease_with_token(
                session_id,
                owner,
                executor_id,
                claim_nonce,
                lease_ttl_ms,
            )
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<lash_core::SessionExecutionLease, lash_core::StoreError> {
        self.inner
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &lash_core::SessionExecutionLeaseAuthority,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.release_session_execution_lease(completion).await
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::SessionExecutionLeaseObservation, lash_core::StoreError> {
        self.inner.get_session_execution_lease(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::store::DriveEpochStore for CommitRetryStore {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &lash_core::store::AdmissionId,
        observed_epoch: u64,
    ) -> Result<lash_core::store::DriveEpochSeal, lash_core::StoreError> {
        self.inner
            .seal_drive_epoch(session_id, admission, observed_epoch)
            .await
    }

    async fn drive_epoch(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::store::StoredDriveEpoch, lash_core::StoreError> {
        self.inner.drive_epoch(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::QueuedWorkStore for CommitRetryStore {
    async fn select_queued_run(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        scope: &lash_core::ExecutionScope,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &lash_core::PersistedSessionConfig,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> std::result::Result<lash_core::store::SelectedQueuedRun, lash_core::StoreError> {
        self.inner
            .select_queued_run(fence, scope, owner, max_inputs, configuration, policy)
            .await
    }
    async fn pending_queued_run(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<Option<lash_core::store::QueuedRunAdmission>, lash_core::StoreError>
    {
        self.inner.pending_queued_run(session_id).await
    }
    async fn queued_run(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> std::result::Result<Option<lash_core::store::QueuedRunAdmission>, lash_core::StoreError>
    {
        self.inner.queued_run(scope).await
    }
    async fn settle_queued_run(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        settlement: lash_core::store::QueuedRunCommit,
    ) -> std::result::Result<lash_core::store::QueuedRunAdmission, lash_core::StoreError> {
        self.inner.settle_queued_run(fence, settlement).await
    }
    async fn begin_or_resume_queued_run(
        &self,
        fence: &lash_core::SessionExecutionLeaseAuthority,
        request: lash_core::store::BeginQueuedRun,
    ) -> std::result::Result<lash_core::store::QueuedRunAdmission, lash_core::StoreError> {
        self.inner.begin_or_resume_queued_run(fence, request).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: lash_core::runtime::QueuedWorkBatchDraft,
    ) -> Result<lash_core::runtime::QueuedWorkEnqueueOutcome, lash_core::StoreError> {
        self.inner.enqueue_queued_work_with_outcome(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
    ) -> Result<Option<lash_core::runtime::QueuedWorkClaim>, lash_core::StoreError> {
        self.inner
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<lash_core::QueuedWorkClaimOutcome, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work(session_id, session_execution_lease, owner, boundary, policy)
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<lash_core::runtime::TurnInputClaim>,
            Option<lash_core::runtime::QueuedWorkClaim>,
        ),
        lash_core::StoreError,
    > {
        self.inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        boundary: lash_core::runtime::QueuedWorkClaimBoundary,
        batch_ids: &[lash_core::BatchId],
        policy: lash_core::QueuedWorkClaimPolicy,
    ) -> Result<lash_core::SelectedQueuedWorkClaimOutcome, lash_core::StoreError> {
        self.inner
            .claim_ready_queued_work_by_batch_ids(
                session_id,
                session_execution_lease,
                owner,
                boundary,
                batch_ids,
                policy,
            )
            .await
    }

    async fn abandon_queued_work_claim(
        &self,
        claim: &lash_core::runtime::QueuedWorkClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, lash_core::StoreError> {
        self.inner
            .queued_work_batch_completed(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::store::PendingSessionWorkOrdering, lash_core::StoreError> {
        self.inner.pending_session_work_ordering(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::runtime::QueuedWorkBatch>, lash_core::StoreError> {
        self.inner.list_pending_queued_work(session_id).await
    }
}

#[async_trait::async_trait]
impl lash_core::TurnInputStore for CommitRetryStore {
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::StoreError> {
        self.inner
            .validate_turn_cancellation_binding(
                session_id,
                session_execution_lease,
                binding_id,
                admitted_scope,
            )
            .await
    }

    async fn authorize_turn_cancel_closure(
        &self,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        authorization: &lash_core::TurnCancelClosureAuthorization,
    ) -> Result<lash_core::TurnCancelClosureAuthorizationOutcome, lash_core::StoreError> {
        self.inner
            .authorize_turn_cancel_closure(session_execution_lease, authorization)
            .await
    }

    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &lash_core::ExecutionScope,
    ) -> Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError> {
        self.inner
            .pending_turn_cancel_closures(
                session_id,
                session_execution_lease,
                binding_id,
                admitted_scope,
            )
            .await
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError> {
        self.inner.pending_turn_cancel_closure_pins().await
    }

    async fn turn_is_committed(
        &self,
        address: &lash_core::runtime::TurnAddress,
    ) -> Result<bool, lash_core::StoreError> {
        self.inner.turn_is_committed(address).await
    }

    async fn record_turn_cancel_request(
        &self,
        request: lash_core::runtime::TurnCancelRequest,
    ) -> Result<lash_core::TurnCancelRequestRecord, lash_core::StoreError> {
        self.inner.record_turn_cancel_request(request).await
    }

    async fn turn_cancel_request(
        &self,
        address: &lash_core::runtime::TurnAddress,
    ) -> Result<Option<lash_core::TurnCancelRequestRecord>, lash_core::StoreError> {
        self.inner.turn_cancel_request(address).await
    }

    async fn turn_cancel_request_intent(
        &self,
        address: &lash_core::runtime::TurnAddress,
    ) -> Result<lash_core::TurnCancelIntentSnapshot, lash_core::StoreError> {
        self.inner.turn_cancel_request_intent(address).await
    }

    async fn reconcile_turn_cancel_winner(
        &self,
        address: &lash_core::runtime::TurnAddress,
        observed: &lash_core::TurnCancelIntentSnapshot,
        evidence: &lash_core::runtime::TurnCancellationEvidence,
    ) -> Result<bool, lash_core::StoreError> {
        self.inner
            .reconcile_turn_cancel_winner(address, observed, evidence)
            .await
    }

    async fn enqueue_pending_turn_input(
        &self,
        input: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, lash_core::StoreError> {
        self.inner.enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::PendingTurnInputRead>, lash_core::StoreError> {
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::TurnInputApplication>, lash_core::StoreError> {
        self.inner.list_turn_input_applications(session_id).await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelReceipt>, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, lash_core::StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_active_turn_inputs(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
            )
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::runtime::TurnInputClaim,
    ) -> Result<(), lash_core::StoreError> {
        self.inner.abandon_turn_input_claim(claim).await
    }

    async fn bind_turn_input_claim(
        &self,
        claim: &lash_core::runtime::TurnInputClaim,
        turn_id: &lash_core::TurnId,
        receipt_input_id: &lash_core::InputId,
    ) -> Result<(), lash_core::StoreError> {
        self.inner
            .bind_turn_input_claim(claim, turn_id, receipt_input_id)
            .await
    }

    async fn bind_turn_input_claim_of_receipt(
        &self,
        session_id: &SessionId,
        receipt_input_id: &lash_core::InputId,
        generation: u64,
        turn_id: &lash_core::TurnId,
    ) -> Result<(), lash_core::StoreError> {
        self.inner
            .bind_turn_input_claim_of_receipt(session_id, receipt_input_id, generation, turn_id)
            .await
    }

    async fn reclaim_turn_bound_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        owner: &lash_core::LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
    ) -> Result<Option<lash_core::runtime::TurnInputClaim>, lash_core::StoreError> {
        self.inner
            .reclaim_turn_bound_inputs(session_id, session_execution_lease, owner, turn_id)
            .await
    }

    async fn orphaned_active_turn_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<lash_core::TurnId>, lash_core::StoreError> {
        self.inner
            .orphaned_active_turn_ids(session_id, session_execution_lease, scope)
            .await
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &lash_core::SessionExecutionLeaseAuthority,
        turn_id: &lash_core::TurnId,
        observed: &lash_core::TurnCancelIntentSnapshot,
        settlement: Option<&lash_core::TurnCancelClosureSettlement>,
    ) -> Result<lash_core::TurnCancelRepairResult, lash_core::StoreError> {
        self.inner
            .repair_orphaned_active_turn_inputs(
                session_id,
                session_execution_lease,
                turn_id,
                observed,
                settlement,
            )
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::StoreMaintenance for CommitRetryStore {
    async fn vacuum(&self) -> lash_core::MaintenanceResult<lash_core::VacuumReport> {
        self.inner.vacuum().await
    }

    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<lash_core::GcReport> {
        self.inner.gc_unreachable().await
    }
}
