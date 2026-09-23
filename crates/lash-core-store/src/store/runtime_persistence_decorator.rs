use super::*;
use crate::SessionId;

/// Every persistence operation a decorator forwards, grouped by the component
/// trait that declares it.
///
/// This list is the single place each operation's signature is written. The
/// two shapes that used to hold a hand-kept copy each -- the defaulted
/// forwarder on [`RuntimePersistenceDecorator`] and the component trait's
/// blanket implementation -- are both generated from it, so the two can no
/// longer drift apart (ADR 0082: decorators delegate wholesale rather than
/// forwarding method by method).
///
/// Adding an operation to a component trait and not to this list is the drift
/// this shape exists to prevent; the
/// `decorator_surface_covers_every_component_trait_method` lint in
/// `store::tests` fails when the two disagree.
macro_rules! persistence_operations {
    ($emit:ident) => {
        $emit! {
            AttachmentManifest {
                fn begin_attachment_write(&self, intent: AttachmentIntent) -> Result<AttachmentWriteFence, StoreError>;
                fn complete_attachment_write(&self, intent: &AttachmentIntent, permit: AttachmentWritePermit) -> Result<(), StoreError>;
                fn abort_attachment_write(&self, intent: &AttachmentIntent, permit: AttachmentWritePermit) -> Result<(), StoreError>;
                fn commit_refs(&self, session_id: &SessionId, attachment_ids: &[crate::AttachmentId]) -> Result<(), StoreError>;
                fn list_uncommitted(&self, older_than_epoch_ms: u64) -> Result<Vec<AttachmentManifestEntry>, StoreError>;
                fn forget_aged_uncommitted_intents(&self, intent_grace_cutoff_epoch_ms: u64) -> Result<(), StoreError>;
                fn has_live_ref_for_id(&self, attachment_id: &crate::AttachmentId, intent_grace_cutoff_epoch_ms: u64) -> Result<bool, StoreError>;
                fn forget(&self, session_id: &SessionId, attachment_id: &crate::AttachmentId) -> Result<(), StoreError>;
                fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError>;
            }
            SessionCommitStore {
                fn read_session_state_version(&self) -> Result<u32, StoreError>;
                fn admit_session_state(&self, lease: &SessionExecutionLeaseAuthority) -> Result<SessionStateAdmission, StoreError>;
                fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError>;
                fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError>;
                fn committed_turn_exists(&self, turn_id: &crate::TurnId) -> Result<bool, StoreError>;
                fn drain_end_exists(&self, drain_id: &str) -> Result<bool, StoreError>;
                fn load_node(&self, node_id: &str) -> Result<Option<crate::SessionNodeRecord>, StoreError>;
                fn commit_runtime_state(&self, commit: RuntimeCommit) -> Result<RuntimeCommitReceipt, StoreError>;
                fn admit_and_bind_session(&self, binding: &SessionBinding) -> Result<SessionAdmission, StoreError>;
                fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
                fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError>;
            }
            TurnInputStore {
                sync fn turn_cancellation_authority(&self) -> Option<std::sync::Arc<dyn crate::StoreTurnCancellationAuthority>>;
                fn turn_is_committed(&self, address: &crate::TurnAddress) -> Result<bool, StoreError>;
                fn reconcile_turn_cancel_winner(&self, address: &crate::TurnAddress, observed: &crate::TurnCancelIntentSnapshot, evidence: &crate::TurnCancellationEvidence) -> Result<bool, StoreError>;
                fn record_turn_cancel_request(&self, request: crate::TurnCancelRequest) -> Result<crate::TurnCancelRequestRecord, StoreError>;
                fn turn_cancel_request(&self, address: &crate::TurnAddress) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError>;
                fn turn_cancel_request_intent(&self, address: &crate::TurnAddress) -> Result<crate::TurnCancelIntentSnapshot, StoreError>;
                fn validate_turn_cancellation_binding(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<(), StoreError>;
                fn authorize_turn_cancel_closure(&self, session_execution_lease: &SessionExecutionLeaseAuthority, authorization: &crate::TurnCancelClosureAuthorization) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;
                fn pending_turn_cancel_closures(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                fn pending_turn_cancel_closure_pins(&self) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                fn enqueue_pending_turn_input(&self, input: crate::PendingTurnInputDraft) -> Result<crate::PendingTurnInput, StoreError>;
                fn list_pending_turn_inputs(&self, session_id: &SessionId) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;
                fn list_turn_input_applications(&self, session_id: &SessionId) -> Result<Vec<crate::TurnInputApplication>, StoreError>;
                fn cancel_pending_turn_input(&self, session_id: &SessionId, input_id: &str) -> Result<crate::PendingTurnInputCancelOutcome, StoreError>;
                fn cancel_pending_turn_inputs(&self, session_id: &SessionId, targets: &[crate::PendingTurnInputCancelTarget]) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, StoreError>;
                fn cancel_pending_turn_input_suffix(&self, session_id: &SessionId, anchor: &crate::PendingTurnInputCancelTarget) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError>;
                fn claim_active_turn_inputs(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity, turn_id: &crate::TurnId, checkpoint: crate::CheckpointKind, max_inputs: usize) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;
                fn claim_next_turn_inputs(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity, max_inputs: usize) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;
                fn abandon_turn_input_claim(&self, claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>) -> Result<(), StoreError>;
                fn abandon_turn_input_claims(&self, claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>]) -> Result<(), StoreError>;
                fn bind_turn_input_claim(&self, claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>, turn_id: &crate::TurnId, receipt_input_id: &crate::InputId) -> Result<(), StoreError>;
                fn bind_turn_input_claim_of_receipt(&self, session_id: &SessionId, receipt_input_id: &crate::InputId, generation: u64, turn_id: &crate::TurnId) -> Result<(), StoreError>;
                fn reclaim_turn_bound_inputs(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity, turn_id: &crate::TurnId) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;
                fn orphaned_active_turn_ids(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, scope: OrphanedTurnInputScope<'_>) -> Result<Vec<crate::TurnId>, StoreError>;
                fn repair_orphaned_active_turn_inputs(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, turn_id: &crate::TurnId, observed: &crate::TurnCancelIntentSnapshot, settlement: Option<&crate::TurnCancelClosureSettlement>) -> Result<crate::store::TurnCancelRepairResult, StoreError>;
            }
            SessionExecutionLeaseStore {
                fn try_claim_session_execution_lease(&self, session_id: &SessionId, owner: &LeaseOwnerIdentity, executor_id: &str, lease_ttl_ms: u64) -> Result<SessionExecutionLeaseClaimOutcome, StoreError>;
                fn try_claim_session_execution_lease_with_token(&self, session_id: &SessionId, owner: &LeaseOwnerIdentity, executor_id: &str, claim_nonce: &LeaseClaimNonce, lease_ttl_ms: u64) -> Result<SessionExecutionLeaseClaimOutcome, StoreError>;
                fn renew_session_execution_lease(&self, fence: &SessionExecutionLeaseAuthority, lease_ttl_ms: u64) -> Result<SessionExecutionLease, StoreError>;
                fn release_session_execution_lease(&self, completion: &SessionExecutionLeaseAuthority) -> Result<(), StoreError>;
                fn get_session_execution_lease(&self, session_id: &SessionId) -> Result<crate::SessionExecutionLeaseObservation, StoreError>;
            }
            QueuedWorkStore {
                fn select_queued_run(&self, fence: &SessionExecutionLeaseAuthority, scope: &crate::ExecutionScope, owner: &LeaseOwnerIdentity, max_inputs: usize, configuration: &crate::PersistedSessionConfig, policy: crate::QueuedWorkClaimPolicy) -> Result<SelectedQueuedRun, StoreError>;
                fn pending_queued_run(&self, session_id: &SessionId) -> Result<Option<QueuedRunAdmission>, StoreError>;
                fn queued_run(&self, scope: &crate::ExecutionScope) -> Result<Option<QueuedRunAdmission>, StoreError>;
                fn settle_queued_run(&self, fence: &SessionExecutionLeaseAuthority, settlement: QueuedRunCommit) -> Result<QueuedRunAdmission, StoreError>;
                fn begin_or_resume_queued_run(&self, fence: &SessionExecutionLeaseAuthority, request: BeginQueuedRun) -> Result<QueuedRunAdmission, StoreError>;
                fn enqueue_queued_work(&self, batch: crate::QueuedWorkBatchDraft) -> Result<crate::QueuedWorkBatch, StoreError>;
                fn enqueue_queued_work_with_outcome(&self, batch: crate::QueuedWorkBatchDraft) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError>;
                fn claim_leading_ready_session_command(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError>;
                fn claim_ready_queued_work(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity, boundary: crate::QueuedWorkClaimBoundary, policy: crate::QueuedWorkClaimPolicy) -> Result<crate::QueuedWorkClaimOutcome, StoreError>;
                #[allow(clippy::too_many_arguments)]
                fn claim_checkpoint_work(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity, turn_id: &crate::TurnId, checkpoint: crate::CheckpointKind, max_inputs: usize, policy: crate::QueuedWorkClaimPolicy) -> Result< ( Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, ), StoreError, >;
                fn claim_ready_queued_work_by_batch_ids(&self, session_id: &SessionId, session_execution_lease: &SessionExecutionLeaseAuthority, owner: &LeaseOwnerIdentity, boundary: crate::QueuedWorkClaimBoundary, batch_ids: &[crate::BatchId], policy: crate::QueuedWorkClaimPolicy) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError>;
                fn abandon_queued_work_claim(&self, claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>) -> Result<(), StoreError>;
                fn abandon_queued_work_claims(&self, claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>]) -> Result<(), StoreError>;
                fn cancel_queued_work_batch(&self, session_id: &SessionId, batch_id: &str) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;
                fn queued_work_batch_completed(&self, session_id: &SessionId, batch_id: &str) -> Result<bool, StoreError>;
                fn pending_session_work_ordering(&self, session_id: &SessionId) -> Result<PendingSessionWorkOrdering, StoreError>;
                fn list_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                fn list_pending_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
            }
            StoreMaintenance {
                fn vacuum(&self) -> MaintenanceResult<VacuumReport>;
                fn gc_unreachable(&self) -> MaintenanceResult<GcReport>;
            }
        }
    };
}

macro_rules! emit_decorator_trait {
    ($(
        $component:ident {
            $(
                sync fn $sync_name:ident(&self) -> $sync_ret:ty;
            )*
            $(
                $(#[$meta:meta])*
                fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
            )*
        }
    )*) => {
        /// Delegating base for [`RuntimePersistence`] decorators.
        ///
        /// Implementors supply one inner persistence handle and override only
        /// the operations they intercept. Every other operation, including
        /// convenience methods with defaults on the component traits, is
        /// forwarded to the inner handle by the blanket component-trait
        /// implementations generated below.
        ///
        /// A decorator must not implement the component traits directly; doing
        /// so would overlap those blanket implementations.
        #[async_trait::async_trait]
        pub trait RuntimePersistenceDecorator: Send + Sync {
            fn inner(&self) -> &(dyn RuntimePersistence + '_);

            $($(
                fn $sync_name(&self) -> $sync_ret {
                    self.inner().$sync_name()
                }
            )*)*

            $($(
                $(#[$meta])*
                async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                    self.inner().$name($($arg),*).await
                }
            )*)*
        }
    };
}

macro_rules! emit_component_impls {
    ($(
        $component:ident {
            $(
                sync fn $sync_name:ident(&self) -> $sync_ret:ty;
            )*
            $(
                $(#[$meta:meta])*
                fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
            )*
        }
    )*) => {
        $(
            #[async_trait::async_trait]
            impl<T> $component for T
            where
                T: RuntimePersistenceDecorator + ?Sized,
            {
                $(
                    fn $sync_name(&self) -> $sync_ret {
                        RuntimePersistenceDecorator::$sync_name(self)
                    }
                )*

                $(
                    $(#[$meta])*
                    async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                        RuntimePersistenceDecorator::$name(self $(, $arg)*).await
                    }
                )*
            }
        )*
    };
}

persistence_operations!(emit_decorator_trait);
persistence_operations!(emit_component_impls);
