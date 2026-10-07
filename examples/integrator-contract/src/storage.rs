use super::*;

#[lash::async_trait]
impl AttachmentReferrers for Integrator {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, StoreError> {
        unreachable!("external signature witness")
    }
    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn end_attachment_referrer(&self, referrer: &ArtifactReferrer) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn session_referrer_state(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReferrerState, StoreError> {
        unreachable!("external signature witness")
    }
    async fn attachment_referrers(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl AttachmentRootSet for Integrator {
    async fn attachment_root_page(
        &self,
        source: lash::persistence::AttachmentRootSource,
        after: Option<&AttachmentId>,
    ) -> Result<lash::persistence::AttachmentRootPage, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_condemnations(&self) -> Result<Vec<AttachmentCondemnationRecord>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn has_live_attachment_ref(&self, id: &AttachmentId) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    fn fence(&self) -> AttachmentGcFence {
        unreachable!("external signature witness")
    }
    async fn begin_attachment_sweep(&self) -> Result<AttachmentSweepGeneration, StoreError> {
        unreachable!("external signature witness")
    }
    async fn adopt_attachment_condemnations(
        &self,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnationAdoption, StoreError> {
        unreachable!("external signature witness")
    }
    async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnation, StoreError> {
        unreachable!("external signature witness")
    }
    async fn arm_attachment_delete(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        unreachable!("external signature witness")
    }
    async fn settle_attachment_condemnation(
        &self,
        id: &AttachmentId,
        generation: &AttachmentSweepGeneration,
        settlement: AttachmentCondemnationSettlement,
    ) -> Result<AttachmentSettlementOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn recover_abandoned_attachment_write(
        &self,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl SessionCatalogStore for Integrator {
    async fn admit_session(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<SessionAdmission, StoreError> {
        unreachable!("external signature witness")
    }
    async fn lookup_session(&self, session_id: &SessionId) -> Result<SessionLookup, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_sessions(
        &self,
        filter: &SessionListFilter,
    ) -> Result<Vec<SessionView>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn fork_session(
        &self,
        request: &ForkSessionRequest,
    ) -> Result<ForkSessionReceipt, StoreError> {
        unreachable!("external signature witness")
    }
    async fn resolve_target(
        &self,
        session_id: &SessionId,
        target: &Target,
    ) -> Result<RetainedRevision, StoreError> {
        unreachable!("external signature witness")
    }
    async fn revisions(&self, session_id: &SessionId) -> Result<Vec<RetainedRevision>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn pin(&self, session_id: &SessionId, target: &Target) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn unpin(&self, session_id: &SessionId, target: &Target) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn retention(&self, session_id: &SessionId) -> Result<Retention, StoreError> {
        unreachable!("external signature witness")
    }
    async fn set_retention(
        &self,
        session_id: &SessionId,
        retention: Retention,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> MaintenanceResult<SessionBlobReclaimReport> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl SessionCommitStore for Integrator {
    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError> {
        unreachable!("external signature witness")
    }
    async fn admit_session_state(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionStateAdmission, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn retain_admission_base(
        &self,
        session_id: &SessionId,
        base: &SessionHeadRef,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_pending_follow_on(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PendingFollowOn>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn raise_pending_follow_on_attempts(
        &self,
        session_id: &SessionId,
        follow_on_turn_id: &TurnId,
    ) -> Result<PendingFollowOn, StoreError> {
        unreachable!("external signature witness")
    }
    async fn settle_observer_intents(
        &self,
        session_id: &SessionId,
        remaining: Vec<SessionObserverIntent>,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl SessionHistoryStore for Integrator {
    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_ancestors(
        &self,
        session_id: &SessionId,
        anchor: HistoryAnchor,
        budget: HistoryBudget,
    ) -> Result<HistoryPage, StoreError> {
        unreachable!("external signature witness")
    }
    async fn contains_active_ancestor(
        &self,
        session_id: &SessionId,
        node_id: &NodeId,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_failure_evidence_page(
        &self,
        session_id: &SessionId,
        after: Option<&FailureEvidenceCursor>,
        limit: NonZeroU32,
    ) -> Result<FailureEvidencePage, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl TurnInputStore for Integrator {
    async fn enqueue_pending_turn_inputs(
        &self,
        batch: PendingTurnInputBatch,
    ) -> Result<Vec<PendingTurnInput>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn enqueue_pending_turn_input(
        &self,
        input: PendingTurnInputDraft,
    ) -> Result<PendingTurnInput, StoreError> {
        unreachable!("external signature witness")
    }
    async fn admit_pending_turn_inputs(
        &self,
        batch: PendingTurnInputBatch,
    ) -> Result<TurnInputAdmission, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_run_spec(
        &self,
        session_id: &SessionId,
        hash: &RunSpecHash,
    ) -> Result<Option<RunSpec>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<PendingTurnInputRead>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &InputId,
    ) -> Result<Option<PendingTurnInputRead>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<TurnInputApplication>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn cancel_pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &str,
    ) -> Result<PendingTurnInputCancelOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[PendingTurnInputCancelTarget],
    ) -> Result<Vec<PendingTurnInputCancelReceipt>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &PendingTurnInputCancelTarget,
    ) -> Result<PendingTurnInputSuffixCancelOutcome, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl QueuedWorkStore for Integrator {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        unreachable!("external signature witness")
    }
    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn open_session_command_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn queued_work_batch_completion(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<RuntimeCommitReceipt>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<PendingSessionWorkOrdering, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_open_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn has_admissible_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl SessionFaultStore for Integrator {
    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &lash::SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<lash::SessionFault>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash::SessionFault>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash::SessionFault>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl RunStore for Integrator {
    async fn unfinished_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRun>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn admit_at_checkpoint(
        &self,
        request: &CheckpointAdmissionRequest,
    ) -> Result<CheckpointAdmission, StoreError> {
        unreachable!("external signature witness")
    }
    async fn run_terminal(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Option<RunTerminal>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn end_refused_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        refusal: &RuntimeError,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn end_command_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn run_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn run_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn bind_run_inputs(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl StoreMaintenance for Integrator {
    async fn vacuum(&self, session_id: &SessionId) -> MaintenanceResult<VacuumReport> {
        unreachable!("external signature witness")
    }
    async fn gc_unreachable(&self) -> MaintenanceResult<GcReport> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ControlIntentStore for Integrator {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn session_close_intent(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ControlIntent>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl DeploymentStore for Integrator {
    async fn artifact_frame_is_retained(
        &self,
        frame: &lash::persistence::FrameEnvironmentId,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }

    async fn count_unsettled_turns(&self) -> Result<UnsettledTurnCounts, StoreError> {
        unreachable!("external signature witness")
    }
    async fn turns_changed_since(
        &self,
        after: lash::persistence::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<lash::persistence::TurnChangePage, StoreError> {
        unreachable!("external signature witness")
    }
    async fn non_terminal_runs_page(
        &self,
        after: Option<&RunRef>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<OpenRun>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn end_lost_run(
        &self,
        target: &RunRef,
        loss: RunLoss,
        at_ms: u64,
    ) -> Result<Option<RunTerminal>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_control_intents(
        &self,
        after: Option<ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ControlIntent>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn reclaim_retained_evidence(
        &self,
        bound: RetentionBound,
    ) -> MaintenanceResult<RetentionReport> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ObligationLedger for Integrator {
    fn kind(&self) -> ObligationKind {
        unreachable!("external signature witness")
    }
    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn count_stalled(&self) -> Result<u64, StoreError> {
        unreachable!("external signature witness")
    }
    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn state(&self, id: &ObligationId) -> Result<Option<ObligationState>, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl RecoveryLeaderStore for Integrator {
    async fn acquire(&self, claim: &LeaseClaim) -> Result<LeaseAnswer, StoreError> {
        unreachable!("external signature witness")
    }
    async fn renew(&self, claim: &LeaseClaim, term: i64) -> Result<LeaseAnswer, StoreError> {
        unreachable!("external signature witness")
    }
    async fn resign(
        &self,
        name: &LeaseName,
        holder: &HolderId,
        term: i64,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    fn due_claims_need_leader(&self) -> bool {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl StorePreflight for Integrator {
    fn backend(&self) -> StoreBackend {
        unreachable!("external signature witness")
    }
    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError> {
        unreachable!("external signature witness")
    }
    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        unreachable!("external signature witness")
    }
}
