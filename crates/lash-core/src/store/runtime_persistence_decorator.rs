use super::*;

/// Delegating base for [`RuntimePersistence`] decorators.
///
/// Implementors supply one inner persistence handle and override only the
/// operations they intercept. Every other operation, including convenience
/// methods with defaults on the component traits, is forwarded to the inner
/// handle by the blanket component-trait implementations below.
///
/// A decorator must not implement the component traits directly; doing so
/// would overlap these blanket implementations.
#[async_trait::async_trait]
pub trait RuntimePersistenceDecorator: Send + Sync {
    fn inner(&self) -> &(dyn RuntimePersistence + '_);

    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        self.inner().record_intent(intent)
    }

    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, StoreError> {
        self.inner().begin_attachment_write(intent)
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), StoreError> {
        self.inner().commit_refs(session_id, attachment_ids)
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        self.inner().list_uncommitted(older_than_epoch_ms)
    }

    fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        self.inner()
            .forget_aged_uncommitted_intents(intent_grace_cutoff_epoch_ms)
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner()
            .has_live_ref_for_id(attachment_id, intent_grace_cutoff_epoch_ms)
    }

    fn forget(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner().forget(session_id, attachment_id)
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<bool, StoreError> {
        self.inner().holds_ref(session_id, attachment_id)
    }

    fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError> {
        self.inner().list_all_refs()
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        self.inner().load_session().await
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.inner().load_session_head_meta().await
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError> {
        self.inner().load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        self.inner().commit_runtime_state(commit).await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionAdmission, StoreError> {
        self.inner().admit_and_bind_session(binding).await
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.inner().save_session_meta(meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        self.inner().load_session_meta().await
    }

    async fn enqueue_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError> {
        self.inner().enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::PendingTurnInput>, StoreError> {
        self.inner().list_pending_turn_inputs(session_id).await
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::TurnInputApplication>, StoreError> {
        self.inner().list_turn_input_applications(session_id).await
    }

    async fn cancel_pending_turn_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, StoreError> {
        self.inner()
            .cancel_pending_turn_input(session_id, input_id)
            .await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, StoreError> {
        self.inner()
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError> {
        self.inner()
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        self.inner()
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
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        self.inner()
            .claim_next_turn_inputs(session_id, session_execution_lease, owner, max_inputs)
            .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>,
    ) -> Result<(), StoreError> {
        self.inner().abandon_turn_input_claim(claim).await
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>],
    ) -> Result<(), StoreError> {
        self.inner().abandon_turn_input_claims(claims).await
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: OrphanedTurnInputScope<'_>,
    ) -> Result<usize, StoreError> {
        self.inner()
            .defer_orphaned_active_turn_inputs(session_id, session_execution_lease, scope)
            .await
    }

    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        self.inner()
            .try_claim_session_execution_lease(session_id, owner, executor_id, lease_ttl_ms)
            .await
    }

    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        self.inner()
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
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        self.inner()
            .renew_session_execution_lease(fence, lease_ttl_ms)
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        self.inner()
            .release_session_execution_lease(completion)
            .await
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError> {
        self.inner().get_session_execution_lease(session_id).await
    }

    async fn enqueue_queued_work(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, StoreError> {
        self.inner().enqueue_queued_work(batch).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError> {
        self.inner().enqueue_queued_work_with_outcome(batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError> {
        self.inner()
            .claim_leading_ready_session_command(session_id, session_execution_lease, owner)
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::QueuedWorkClaimOutcome, StoreError> {
        self.inner()
            .claim_ready_queued_work(session_id, session_execution_lease, owner, boundary, policy)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>,
            Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>,
        ),
        StoreError,
    > {
        self.inner()
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
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError> {
        self.inner()
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
        claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>,
    ) -> Result<(), StoreError> {
        self.inner().abandon_queued_work_claim(claim).await
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>],
    ) -> Result<(), StoreError> {
        self.inner().abandon_queued_work_claims(claims).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, StoreError> {
        self.inner()
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn queued_work_batch_completed(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        self.inner()
            .queued_work_batch_completed(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &str,
    ) -> Result<PendingSessionWorkOrdering, StoreError> {
        self.inner().pending_session_work_ordering(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        self.inner().list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        self.inner().list_pending_queued_work(session_id).await
    }

    async fn vacuum(&self) -> MaintenanceResult<VacuumReport> {
        self.inner().vacuum().await
    }

    async fn gc_unreachable(&self) -> MaintenanceResult<GcReport> {
        self.inner().gc_unreachable().await
    }

    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError> {
        self.inner()
            .seed_session_trigger_manifest_ref_for_testing(session_id)
            .await
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.inner()
            .raw_session_owned_artifact_refs_for_testing(session_id)
            .await
    }
}

impl<T> AttachmentManifest for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::record_intent(self, intent)
    }

    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, StoreError> {
        RuntimePersistenceDecorator::begin_attachment_write(self, intent)
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[crate::AttachmentId],
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::commit_refs(self, session_id, attachment_ids)
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        RuntimePersistenceDecorator::list_uncommitted(self, older_than_epoch_ms)
    }

    fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::forget_aged_uncommitted_intents(
            self,
            intent_grace_cutoff_epoch_ms,
        )
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        RuntimePersistenceDecorator::has_live_ref_for_id(
            self,
            attachment_id,
            intent_grace_cutoff_epoch_ms,
        )
    }

    fn forget(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::forget(self, session_id, attachment_id)
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &crate::AttachmentId,
    ) -> Result<bool, StoreError> {
        RuntimePersistenceDecorator::holds_ref(self, session_id, attachment_id)
    }

    fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError> {
        RuntimePersistenceDecorator::list_all_refs(self)
    }
}

#[async_trait::async_trait]
impl<T> SessionCommitStore for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        RuntimePersistenceDecorator::load_session(self).await
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        RuntimePersistenceDecorator::load_session_head_meta(self).await
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError> {
        RuntimePersistenceDecorator::load_node(self, node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        RuntimePersistenceDecorator::commit_runtime_state(self, commit).await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionAdmission, StoreError> {
        RuntimePersistenceDecorator::admit_and_bind_session(self, binding).await
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::save_session_meta(self, meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        RuntimePersistenceDecorator::load_session_meta(self).await
    }
}

#[async_trait::async_trait]
impl<T> TurnInputStore for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    async fn enqueue_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError> {
        RuntimePersistenceDecorator::enqueue_pending_turn_input(self, input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::PendingTurnInput>, StoreError> {
        RuntimePersistenceDecorator::list_pending_turn_inputs(self, session_id).await
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::TurnInputApplication>, StoreError> {
        RuntimePersistenceDecorator::list_turn_input_applications(self, session_id).await
    }

    async fn cancel_pending_turn_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, StoreError> {
        RuntimePersistenceDecorator::cancel_pending_turn_input(self, session_id, input_id).await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, StoreError> {
        RuntimePersistenceDecorator::cancel_pending_turn_inputs(self, session_id, targets).await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError> {
        RuntimePersistenceDecorator::cancel_pending_turn_input_suffix(self, session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        RuntimePersistenceDecorator::claim_active_turn_inputs(
            self,
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
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError> {
        RuntimePersistenceDecorator::claim_next_turn_inputs(
            self,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
        )
        .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>,
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::abandon_turn_input_claim(self, claim).await
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>],
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::abandon_turn_input_claims(self, claims).await
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: OrphanedTurnInputScope<'_>,
    ) -> Result<usize, StoreError> {
        RuntimePersistenceDecorator::defer_orphaned_active_turn_inputs(
            self,
            session_id,
            session_execution_lease,
            scope,
        )
        .await
    }
}

#[async_trait::async_trait]
impl<T> SessionExecutionLeaseStore for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        RuntimePersistenceDecorator::try_claim_session_execution_lease(
            self,
            session_id,
            owner,
            executor_id,
            lease_ttl_ms,
        )
        .await
    }

    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        RuntimePersistenceDecorator::try_claim_session_execution_lease_with_token(
            self,
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
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        RuntimePersistenceDecorator::renew_session_execution_lease(self, fence, lease_ttl_ms).await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::release_session_execution_lease(self, completion).await
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError> {
        RuntimePersistenceDecorator::get_session_execution_lease(self, session_id).await
    }
}

#[async_trait::async_trait]
impl<T> QueuedWorkStore for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    async fn enqueue_queued_work(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, StoreError> {
        RuntimePersistenceDecorator::enqueue_queued_work(self, batch).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError> {
        RuntimePersistenceDecorator::enqueue_queued_work_with_outcome(self, batch).await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError> {
        RuntimePersistenceDecorator::claim_leading_ready_session_command(
            self,
            session_id,
            session_execution_lease,
            owner,
        )
        .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::QueuedWorkClaimOutcome, StoreError> {
        RuntimePersistenceDecorator::claim_ready_queued_work(
            self,
            session_id,
            session_execution_lease,
            owner,
            boundary,
            policy,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>,
            Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>,
        ),
        StoreError,
    > {
        RuntimePersistenceDecorator::claim_checkpoint_work(
            self,
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
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError> {
        RuntimePersistenceDecorator::claim_ready_queued_work_by_batch_ids(
            self,
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
        claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>,
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::abandon_queued_work_claim(self, claim).await
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>],
    ) -> Result<(), StoreError> {
        RuntimePersistenceDecorator::abandon_queued_work_claims(self, claims).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, StoreError> {
        RuntimePersistenceDecorator::cancel_queued_work_batch(self, session_id, batch_id).await
    }

    async fn queued_work_batch_completed(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        RuntimePersistenceDecorator::queued_work_batch_completed(self, session_id, batch_id).await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &str,
    ) -> Result<PendingSessionWorkOrdering, StoreError> {
        RuntimePersistenceDecorator::pending_session_work_ordering(self, session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        RuntimePersistenceDecorator::list_queued_work(self, session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        RuntimePersistenceDecorator::list_pending_queued_work(self, session_id).await
    }
}

#[async_trait::async_trait]
impl<T> StoreMaintenance for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    async fn vacuum(&self) -> MaintenanceResult<VacuumReport> {
        RuntimePersistenceDecorator::vacuum(self).await
    }

    async fn gc_unreachable(&self) -> MaintenanceResult<GcReport> {
        RuntimePersistenceDecorator::gc_unreachable(self).await
    }

    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError> {
        RuntimePersistenceDecorator::seed_session_trigger_manifest_ref_for_testing(self, session_id)
            .await
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        RuntimePersistenceDecorator::raw_session_owned_artifact_refs_for_testing(self, session_id)
            .await
    }
}
