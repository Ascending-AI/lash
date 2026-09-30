use super::*;

#[lash::async_trait]
impl AttachmentManifest for Integrator {
    async fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<AttachmentWriteFence, StoreError> {
        unreachable!("external signature witness")
    }
    async fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn has_live_ref_for_id(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl AttachmentRootSet for Integrator {
    fn can_prove_process_owner_death(&self) -> bool {
        unreachable!("external signature witness")
    }
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_condemnations(&self) -> Result<Vec<AttachmentCondemnationRecord>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
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
        intent_grace_cutoff_epoch_ms: u64,
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
    async fn pin(&self, node_id: &NodeId) -> Result<ForkPoint, StoreError> {
        unreachable!("external signature witness")
    }
    async fn unpin(&self, node_id: &NodeId) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError> {
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
        fence: &DriveFence,
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
        fence: &DriveFence,
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
    async fn drain_end_exists(
        &self,
        session_id: &SessionId,
        drain_id: &str,
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
        fence: &DriveFence,
        follow_on_turn_id: &TurnId,
    ) -> Result<PendingFollowOn, StoreError> {
        unreachable!("external signature witness")
    }
    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
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
    async fn record_turn_park(&self, park: &TurnParkWrite) -> Result<TurnPark, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_turn_park(&self, session_id: &SessionId) -> Result<Option<TurnPark>, StoreError> {
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
    async fn load_usage_totals(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionUsageTotals, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_usage_ledger_page(
        &self,
        session_id: &SessionId,
        after: Option<&UsageLedgerCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageLedgerPage, StoreError> {
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
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        fence: &DriveFence,
        binding_id: &str,
        admitted_scope: &ExecutionScope,
    ) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn authorize_turn_cancel_closure(
        &self,
        fence: &DriveFence,
        authorization: &TurnCancelClosureAuthorization,
    ) -> Result<TurnCancelClosureAuthorizationOutcome, StoreError> {
        unreachable!("external signature witness")
    }
    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        fence: &DriveFence,
        binding_id: &str,
        admitted_scope: &ExecutionScope,
    ) -> Result<Vec<TurnCancelClosureAuthorization>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<TurnCancelClosureAuthorization>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn turn_is_committed(&self, address: &TurnAddress) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn record_turn_cancel_request(
        &self,
        request: TurnCancelRequest,
    ) -> Result<TurnCancelRequestRecord, StoreError> {
        unreachable!("external signature witness")
    }
    async fn turn_cancel_request(
        &self,
        address: &TurnAddress,
    ) -> Result<Option<TurnCancelRequestRecord>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn turn_cancel_request_intent(
        &self,
        address: &TurnAddress,
    ) -> Result<TurnCancelIntentSnapshot, StoreError> {
        unreachable!("external signature witness")
    }
    async fn reconcile_turn_cancel_winner(
        &self,
        address: &TurnAddress,
        observed: &TurnCancelIntentSnapshot,
        evidence: &TurnCancellationEvidence,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
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
        ingress_claim_ttl_ms: u64,
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
        fence: &DriveFence,
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
    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
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
    async fn has_claimable_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl DriveEpochStore for Integrator {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
    ) -> Result<DriveEpochSeal, StoreError> {
        unreachable!("external signature witness")
    }
    async fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl RootStore for Integrator {
    async fn unfinished_root(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRoot>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn admit_root(
        &self,
        request: &AdmitRootRequest,
    ) -> Result<Option<RootAdmission>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn admit_at_checkpoint(
        &self,
        request: &CheckpointAdmissionRequest,
    ) -> Result<CheckpointAdmission, StoreError> {
        unreachable!("external signature witness")
    }
    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Option<RootTerminal>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn end_refused_root(
        &self,
        fence: &DriveFence,
        root: &TurnId,
        refusal: &RuntimeError,
        at_ms: u64,
    ) -> Result<RefusedRootEnd, StoreError> {
        unreachable!("external signature witness")
    }
    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn root_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &TurnId,
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
    async fn claim_intent_application(
        &self,
        id: ControlIntentId,
        at_ms: u64,
    ) -> Result<IntentApplication, StoreError> {
        unreachable!("external signature witness")
    }
    async fn acknowledge_intent(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        unreachable!("external signature witness")
    }
    async fn record_intent_failure(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        unreachable!("external signature witness")
    }
    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn open_root_intent(
        &self,
        _request: &RootIntentRequest,
        _at_ms: u64,
    ) -> Result<ControlIntent, RootIntentRefused> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl DeploymentStore for Integrator {
    fn bind_effect_host(&self, effect_host: &Arc<dyn EffectHost>) {
        unreachable!("external signature witness")
    }
    async fn count_unsettled_turns(&self) -> Result<UnsettledTurnCounts, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_turn_parks(&self, query: &TurnParkQuery) -> Result<Vec<TurnPark>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn turn_park_feed(
        &self,
        after: ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<ParkFeedPage<TurnParkTarget>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn compact_turn_park_feed(&self, through: ParkFeedCursor) -> Result<(), StoreError> {
        unreachable!("external signature witness")
    }
    async fn non_terminal_roots_page(
        &self,
        after: Option<&RootRef>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<RootRef>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn end_lost_root(
        &self,
        target: &RootRef,
        at_ms: u64,
    ) -> Result<Option<RootTerminal>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn list_control_intents(
        &self,
        after: Option<ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ControlIntent>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), StoreError> {
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
impl GenerationDrainStore for Integrator {
    async fn mark_draining(
        &self,
        generation: &BuildGeneration,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn clear_draining(&self, generation: &BuildGeneration) -> Result<bool, StoreError> {
        unreachable!("external signature witness")
    }
    async fn draining_generations(&self) -> Result<Vec<DrainingGeneration>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn generation_work(
        &self,
        generation: &BuildGeneration,
    ) -> Result<GenerationWork, StoreError> {
        unreachable!("external signature witness")
    }
    async fn live_processes(
        &self,
        generation: &BuildGeneration,
        after: Option<&ProcessId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ProcessId>, StoreError> {
        unreachable!("external signature witness")
    }
}

#[lash::async_trait]
impl ObligationLedger for Integrator {
    fn kind(&self) -> ObligationKind {
        unreachable!("external signature witness")
    }
    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
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
impl SessionDeleteLedger for Integrator {
    async fn delete_obligation(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionDeleteObligation>, StoreError> {
        unreachable!("external signature witness")
    }
    async fn undelivered_cleanup(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionCleanup, StoreError> {
        unreachable!("external signature witness")
    }
    async fn count_closing(&self) -> Result<u64, StoreError> {
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
