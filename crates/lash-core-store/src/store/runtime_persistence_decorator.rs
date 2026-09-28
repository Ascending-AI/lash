use super::*;
use crate::SessionId;

/// Provided conveniences a decorator answers through its own primitive
/// instead of forwarding them to `inner()`: a decorator that intercepts the
/// primitive intercepts every call of the convenience too. They are left out
/// of `persistence_operations!`, so the component trait's default applies;
/// the surface lint reads this list.
#[cfg(test)]
pub(super) const SELF_ROUTED_CONVENIENCES: &[&str] = &[
    // Exactly `enqueue_pending_turn_inputs` of a batch of one (FIG-3842).
    "enqueue_pending_turn_input",
];

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
/// `store::tests` fails when the two disagree, except for the self-routed
/// conveniences named above.
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
                fn admit_session_state(&self, lease: &ClaimAuthority) -> Result<SessionStateAdmission, StoreError>;
                fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError>;
                fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError>;
                fn load_session_at(&self, base: &SessionHeadRef) -> Result<PersistedSessionRead, StoreError>;
                fn retain_admission_base(&self, lease: &ClaimAuthority, base: &SessionHeadRef) -> Result<(), StoreError>;
                fn committed_turn_exists(&self, turn_id: &crate::TurnId) -> Result<bool, StoreError>;
                fn drain_end_exists(&self, drain_id: &str) -> Result<bool, StoreError>;
                fn load_node(&self, node_id: &str) -> Result<Option<crate::SessionNodeRecord>, StoreError>;
                fn commit_runtime_state(&self, commit: RuntimeCommit) -> Result<RuntimeCommitReceipt, StoreError>;
                fn load_pending_follow_on(&self) -> Result<Option<PendingFollowOn>, StoreError>;
                fn raise_pending_follow_on_attempts(&self, lease: &ClaimAuthority, follow_on_turn_id: &crate::TurnId) -> Result<PendingFollowOn, StoreError>;
                fn admit_and_bind_session(&self, binding: &SessionBinding) -> Result<SessionAdmission, StoreError>;
                fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
                fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError>;
                fn load_session_meta_for_commit(&self) -> Result<Option<SessionMeta>, StoreError>;
                fn record_turn_park(&self, park: &crate::store::TurnParkWrite) -> Result<crate::store::TurnPark, StoreError>;
                fn load_turn_park(&self, session_id: &SessionId) -> Result<Option<crate::store::TurnPark>, StoreError>;
            }
            TurnInputStore {
                fn turn_is_committed(&self, address: &crate::TurnAddress) -> Result<bool, StoreError>;
                fn reconcile_turn_cancel_winner(&self, address: &crate::TurnAddress, observed: &crate::TurnCancelIntentSnapshot, evidence: &crate::TurnCancellationEvidence) -> Result<bool, StoreError>;
                fn record_turn_cancel_request(&self, request: crate::TurnCancelRequest) -> Result<crate::TurnCancelRequestRecord, StoreError>;
                fn turn_cancel_request(&self, address: &crate::TurnAddress) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError>;
                fn turn_cancel_request_intent(&self, address: &crate::TurnAddress) -> Result<crate::TurnCancelIntentSnapshot, StoreError>;
                fn validate_turn_cancellation_binding(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<(), StoreError>;
                fn authorize_turn_cancel_closure(&self, session_execution_lease: &ClaimAuthority, authorization: &crate::TurnCancelClosureAuthorization) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;
                fn pending_turn_cancel_closures(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                fn pending_turn_cancel_closure_pins(&self) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                fn enqueue_pending_turn_inputs(&self, batch: crate::PendingTurnInputBatch) -> Result<Vec<crate::PendingTurnInput>, StoreError>;
                fn load_run_spec(&self, session_id: &SessionId, hash: &crate::run_spec::RunSpecHash) -> Result<Option<crate::run_spec::RunSpec>, StoreError>;
                fn list_pending_turn_inputs(&self, session_id: &SessionId) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;
                fn list_turn_input_applications(&self, session_id: &SessionId) -> Result<Vec<crate::TurnInputApplication>, StoreError>;
                fn cancel_pending_turn_input(&self, session_id: &SessionId, input_id: &str) -> Result<crate::PendingTurnInputCancelOutcome, StoreError>;
                fn cancel_pending_turn_inputs(&self, session_id: &SessionId, targets: &[crate::PendingTurnInputCancelTarget]) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, StoreError>;
                fn cancel_pending_turn_input_suffix(&self, session_id: &SessionId, anchor: &crate::PendingTurnInputCancelTarget) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError>;
                fn claim_active_turn_inputs(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, owner: &LeaseOwnerIdentity, turn_id: &crate::TurnId, checkpoint: crate::CheckpointKind, max_inputs: usize) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;
                fn claim_next_turn_inputs(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, owner: &LeaseOwnerIdentity, max_inputs: usize) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;
                fn abandon_turn_input_claim(&self, claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>) -> Result<(), StoreError>;
                fn abandon_turn_input_claims(&self, claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>]) -> Result<(), StoreError>;
                fn orphaned_active_turn_ids(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, scope: OrphanedTurnInputScope<'_>) -> Result<Vec<crate::TurnId>, StoreError>;
                fn repair_orphaned_active_turn_inputs(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, turn_id: &crate::TurnId, observed: &crate::TurnCancelIntentSnapshot, settlement: Option<&crate::TurnCancelClosureSettlement>) -> Result<crate::store::TurnCancelRepairResult, StoreError>;
            }
            QueuedWorkStore {
                fn enqueue_queued_work(&self, batch: crate::QueuedWorkBatchDraft) -> Result<crate::QueuedWorkBatch, StoreError>;
                fn enqueue_queued_work_with_outcome(&self, batch: crate::QueuedWorkBatchDraft) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError>;
                fn claim_leading_ready_session_command(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, owner: &LeaseOwnerIdentity) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError>;
                fn claim_ready_queued_work(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, owner: &LeaseOwnerIdentity, boundary: crate::QueuedWorkClaimBoundary, policy: crate::QueuedWorkClaimPolicy) -> Result<crate::QueuedWorkClaimOutcome, StoreError>;
                #[allow(clippy::too_many_arguments)]
                fn claim_checkpoint_work(&self, session_id: &SessionId, session_execution_lease: &ClaimAuthority, owner: &LeaseOwnerIdentity, turn_id: &crate::TurnId, checkpoint: crate::CheckpointKind, max_inputs: usize, policy: crate::QueuedWorkClaimPolicy) -> Result< ( Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, ), StoreError, >;
                fn abandon_queued_work_claim(&self, claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>) -> Result<(), StoreError>;
                fn abandon_queued_work_claims(&self, claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>]) -> Result<(), StoreError>;
                fn cancel_queued_work_batch(&self, session_id: &SessionId, batch_id: &str) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;
                fn queued_work_batch_completed(&self, session_id: &SessionId, batch_id: &str) -> Result<bool, StoreError>;
                fn pending_session_work_ordering(&self, session_id: &SessionId) -> Result<PendingSessionWorkOrdering, StoreError>;
                fn list_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                fn list_pending_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
            }
            DriveEpochStore {
                fn seal_drive_epoch(&self, session_id: &SessionId, admission: &AdmissionId, observed_epoch: u64, root_start: &RootStartNonce) -> Result<DriveEpochSeal, StoreError>;
                fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError>;
            }
            RootStore {
                fn unfinished_root(&self, session_id: &SessionId) -> Result<Option<UnfinishedRoot>, StoreError>;
                fn admit_root(&self, request: &AdmitRootRequest) -> Result<Option<RootAdmission>, StoreError>;
                fn root_terminal(&self, session_id: &SessionId, root: &crate::TurnId) -> Result<Option<RootTerminal>, StoreError>;
                fn root_of_input(&self, session_id: &SessionId, input: &crate::InputId) -> Result<Option<crate::TurnId>, StoreError>;
                fn root_binding(&self, session_id: &SessionId, input: &crate::InputId) -> Result<Option<crate::TurnId>, StoreError>;
                fn bind_root_inputs(&self, session_id: &SessionId, root: &crate::TurnId, inputs: &[crate::InputId]) -> Result<(), StoreError>;
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
        /// implementations generated below; the self-routed conveniences
        /// alone run their trait default over this decorator's own primitive.
        ///
        /// A decorator must not implement the component traits directly; doing
        /// so would overlap those blanket implementations.
        #[async_trait::async_trait]
        pub trait RuntimePersistenceDecorator: Send + Sync {
            fn inner(&self) -> &(dyn RuntimePersistence + '_);

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

/// `FleetFormatStore` is a synchronous segment the `persistence_operations!`
/// list cannot express — its entries are emitted as `async fn`. The same
/// wholesale-delegation norm holds: a decorator answers its inner handle's
/// fleet format rather than maintaining one of its own.
impl<T> FleetFormatStore for T
where
    T: RuntimePersistenceDecorator + ?Sized,
{
    fn fleet_format(&self) -> FleetFormat {
        self.inner().fleet_format()
    }
}
