use super::*;
use crate::SessionId;
use crate::session_catalog::{SessionListFilter, SessionSummary};
use crate::session_store_factory_types::{
    ForkPoint, ForkSessionReceipt, ForkSessionRequest, SessionLookup, SessionStoreCreateRequest,
};
use std::num::NonZeroU32;

/// Every [`RuntimeStore`] operation, grouped by the segment trait that
/// declares it (ADR 0112 §2).
///
/// This list is the single place each operation's signature is written. It
/// generates three surfaces, so none of them can drift from the traits:
///
/// - the defaulted forwarders of [`RuntimeStoreDecorator`];
/// - each segment trait's blanket implementation for a decorator;
/// - the inherent forwarders of [`SessionStore`](super::SessionStore).
///
/// Each entry carries its session scope in brackets:
///
/// - `[session]` takes `session_id: &SessionId` as its first parameter; the
///   view supplies its own id and drops the parameter;
/// - `[carried a, b]` takes requests that carry their session id; the view
///   refuses a request naming another session with
///   [`StoreError::ForeignSessionRequest`] before anything reaches the store;
/// - `[session a]` does both;
/// - `[catalog]` spans the catalog and is not on the view.
///
/// A segment's `provided` block lists provided methods that compose the
/// segment's own required primitives. A decorator answers them through its
/// own primitive instead of forwarding them to `inner()`, so a decorator that
/// intercepts the primitive intercepts every call of the convenience too; the
/// view still forwards them.
///
/// Adding an operation to a segment trait and not to this list is the drift
/// this shape exists to prevent; the
/// `decorator_surface_covers_every_component_trait_method` lint in
/// `store::tests` fails when the two disagree.
macro_rules! runtime_store_operations {
    ($emit:ident) => {
        $emit! {
            AttachmentManifest {
                [carried intent] fn begin_attachment_write(&self, intent: AttachmentIntent) -> Result<AttachmentWriteFence, StoreError>;
                [carried intent] fn complete_attachment_write(&self, intent: &AttachmentIntent, permit: AttachmentWritePermit) -> Result<(), StoreError>;
                [carried intent] fn abort_attachment_write(&self, intent: &AttachmentIntent, permit: AttachmentWritePermit) -> Result<(), StoreError>;
                [session] fn commit_refs(&self, session_id: &SessionId, attachment_ids: &[crate::AttachmentId]) -> Result<(), StoreError>;
                [catalog] fn list_uncommitted(&self, older_than_epoch_ms: u64) -> Result<Vec<AttachmentManifestEntry>, StoreError>;
                [catalog] fn forget_aged_uncommitted_intents(&self, intent_grace_cutoff_epoch_ms: u64) -> Result<(), StoreError>;
                [catalog] fn has_live_ref_for_id(&self, attachment_id: &crate::AttachmentId, intent_grace_cutoff_epoch_ms: u64) -> Result<bool, StoreError>;
                [session] fn forget(&self, session_id: &SessionId, attachment_id: &crate::AttachmentId) -> Result<(), StoreError>;
                [catalog] fn list_all_refs(&self) -> Result<Vec<crate::AttachmentId>, StoreError>;
            }
            SessionCatalogStore {
                [catalog] fn admit_session(&self, request: &SessionStoreCreateRequest) -> Result<SessionAdmission, StoreError>;
                [catalog] fn lookup_session(&self, session_id: &SessionId) -> Result<SessionLookup, StoreError>;
                [catalog] fn list_sessions(&self, filter: &SessionListFilter) -> Result<Vec<SessionSummary>, StoreError>;
                [catalog] fn fork_session(&self, request: &ForkSessionRequest) -> Result<ForkSessionReceipt, StoreError>;
                [catalog] fn pin(&self, node_id: &crate::NodeId) -> Result<ForkPoint, StoreError>;
                [catalog] fn unpin(&self, node_id: &crate::NodeId) -> Result<(), StoreError>;
                [catalog] fn fork_points(&self) -> Result<Vec<ForkPoint>, StoreError>;
                [catalog] fn delete_session(&self, session_id: &SessionId) -> MaintenanceResult<SessionBlobReclaimReport>;
            }
            SessionCommitStore {
                [session] fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError>;
                [carried fence] fn admit_session_state(&self, fence: &DriveFence) -> Result<SessionStateAdmission, StoreError>;
                [session] fn load_session_head_meta(&self, session_id: &SessionId) -> Result<Option<SessionHeadMeta>, StoreError>;
                [carried fence] fn retain_admission_base(&self, fence: &DriveFence, base: &SessionHeadRef) -> Result<(), StoreError>;
                [session] fn committed_turn_exists(&self, session_id: &SessionId, turn_id: &crate::TurnId) -> Result<bool, StoreError>;
                [session] fn drain_end_exists(&self, session_id: &SessionId, drain_id: &str) -> Result<bool, StoreError>;
                [carried commit] fn commit_runtime_state(&self, commit: RuntimeCommit) -> Result<RuntimeCommitReceipt, StoreError>;
                [carried fence] fn raise_pending_follow_on_attempts(&self, fence: &DriveFence, follow_on_turn_id: &crate::TurnId) -> Result<PendingFollowOn, StoreError>;
                [carried meta] fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
                [session] fn load_session_meta(&self, session_id: &SessionId) -> Result<Option<SessionMeta>, StoreError>;
                [session] fn load_session_meta_for_commit(&self, session_id: &SessionId) -> Result<Option<SessionMeta>, StoreError>;
                [carried park] fn record_turn_park(&self, park: &TurnParkWrite) -> Result<TurnPark, StoreError>;
                [session] fn load_turn_park(&self, session_id: &SessionId) -> Result<Option<TurnPark>, StoreError>;
                provided:
                [session] fn load_pending_follow_on(&self, session_id: &SessionId) -> Result<Option<PendingFollowOn>, StoreError>;
            }
            SessionHistoryStore {
                [session] fn load_session_window(&self, session_id: &SessionId, selector: WindowSelector) -> Result<Option<SessionWindowRead>, StoreError>;
                [session] fn load_ancestors(&self, session_id: &SessionId, anchor: HistoryAnchor, budget: HistoryBudget) -> Result<HistoryPage, StoreError>;
                [session] fn contains_active_ancestor(&self, session_id: &SessionId, node_id: &crate::NodeId) -> Result<bool, StoreError>;
                [session] fn load_usage_totals(&self, session_id: &SessionId) -> Result<crate::usage::SessionUsageTotals, StoreError>;
                [session] fn load_usage_ledger_page(&self, session_id: &SessionId, after: Option<&UsageLedgerCursor>, limit: NonZeroU32) -> Result<UsageLedgerPage, StoreError>;
                [session] fn load_failure_evidence_page(&self, session_id: &SessionId, after: Option<&FailureEvidenceCursor>, limit: NonZeroU32) -> Result<FailureEvidencePage, StoreError>;
            }
            TurnInputStore {
                [carried address] fn turn_is_committed(&self, address: &crate::TurnAddress) -> Result<bool, StoreError>;
                [carried address] fn reconcile_turn_cancel_winner(&self, address: &crate::TurnAddress, observed: &crate::TurnCancelIntentSnapshot, evidence: &crate::TurnCancellationEvidence) -> Result<bool, StoreError>;
                [carried request] fn record_turn_cancel_request(&self, request: crate::TurnCancelRequest) -> Result<crate::TurnCancelRequestRecord, StoreError>;
                [carried address] fn turn_cancel_request(&self, address: &crate::TurnAddress) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError>;
                [carried address] fn turn_cancel_request_intent(&self, address: &crate::TurnAddress) -> Result<crate::TurnCancelIntentSnapshot, StoreError>;
                [session fence] fn validate_turn_cancellation_binding(&self, session_id: &SessionId, fence: &DriveFence, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<(), StoreError>;
                [carried fence, authorization] fn authorize_turn_cancel_closure(&self, fence: &DriveFence, authorization: &crate::TurnCancelClosureAuthorization) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;
                [session fence] fn pending_turn_cancel_closures(&self, session_id: &SessionId, fence: &DriveFence, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                [session] fn pending_turn_cancel_closure_pins(&self, session_id: &SessionId) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                [carried batch] fn enqueue_pending_turn_inputs(&self, batch: crate::PendingTurnInputBatch) -> Result<Vec<crate::PendingTurnInput>, StoreError>;
                [carried batch] fn admit_pending_turn_inputs(&self, batch: crate::PendingTurnInputBatch, ingress_claim_ttl_ms: u64) -> Result<TurnInputAdmission, StoreError>;
                [session] fn load_run_spec(&self, session_id: &SessionId, hash: &crate::run_spec::RunSpecHash) -> Result<Option<crate::run_spec::RunSpec>, StoreError>;
                [session] fn list_pending_turn_inputs(&self, session_id: &SessionId) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;
                [session] fn list_turn_input_applications(&self, session_id: &SessionId) -> Result<Vec<crate::TurnInputApplication>, StoreError>;
                [session] fn cancel_pending_turn_inputs(&self, session_id: &SessionId, targets: &[crate::PendingTurnInputCancelTarget]) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, StoreError>;
                [session] fn cancel_pending_turn_input_suffix(&self, session_id: &SessionId, anchor: &crate::PendingTurnInputCancelTarget) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError>;
                provided:
                [carried input] fn enqueue_pending_turn_input(&self, input: crate::PendingTurnInputDraft) -> Result<crate::PendingTurnInput, StoreError>;
                [session] fn cancel_pending_turn_input(&self, session_id: &SessionId, input_id: &str) -> Result<crate::PendingTurnInputCancelOutcome, StoreError>;
            }
            QueuedWorkStore {
                [carried batch] fn enqueue_queued_work(&self, batch: crate::QueuedWorkBatchDraft) -> Result<crate::QueuedWorkBatch, StoreError>;
                [carried batch] fn enqueue_queued_work_with_outcome(&self, batch: crate::QueuedWorkBatchDraft) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError>;
                [carried fence] fn open_session_command_run(&self, fence: &DriveFence) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                [session] fn cancel_queued_work_batch(&self, session_id: &SessionId, batch_id: &str) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;
                [session] fn queued_work_batch_completed(&self, session_id: &SessionId, batch_id: &str) -> Result<bool, StoreError>;
                [session] fn pending_session_work_ordering(&self, session_id: &SessionId) -> Result<PendingSessionWorkOrdering, StoreError>;
                [session] fn list_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                [session] fn list_open_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                [session] fn has_claimable_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError>;
            }
            DriveEpochStore {
                [session] fn seal_drive_epoch(&self, session_id: &SessionId, admission: &AdmissionId, observed_epoch: u64, root_start: &RootStartNonce) -> Result<DriveEpochSeal, StoreError>;
                [session] fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError>;
            }
            RootStore {
                [session] fn unfinished_root(&self, session_id: &SessionId) -> Result<Option<UnfinishedRoot>, StoreError>;
                [carried request] fn admit_root(&self, request: &AdmitRootRequest) -> Result<Option<RootAdmission>, StoreError>;
                [carried request] fn admit_at_checkpoint(&self, request: &CheckpointAdmissionRequest) -> Result<CheckpointAdmission, StoreError>;
                [session] fn root_terminal(&self, session_id: &SessionId, root: &crate::TurnId) -> Result<Option<RootTerminal>, StoreError>;
                [session] fn root_of_input(&self, session_id: &SessionId, input: &crate::InputId) -> Result<Option<crate::TurnId>, StoreError>;
                [session] fn root_binding(&self, session_id: &SessionId, input: &crate::InputId) -> Result<Option<crate::TurnId>, StoreError>;
                [session] fn bind_root_inputs(&self, session_id: &SessionId, root: &crate::TurnId, inputs: &[crate::InputId]) -> Result<(), StoreError>;
            }
            StoreMaintenance {
                [session] fn vacuum(&self, session_id: &SessionId) -> MaintenanceResult<VacuumReport>;
                [catalog] fn gc_unreachable(&self) -> MaintenanceResult<GcReport>;
            }
        }
    };
}

macro_rules! emit_decorator_trait {
    ($(
        $component:ident {
            $(
                $(#[$meta:meta])*
                [$($scope:tt)*] fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
            )*
            $(provided:
                $(
                    $(#[$pmeta:meta])*
                    [$($pscope:tt)*] fn $pname:ident(&self $(, $parg:ident: $parg_ty:ty)*) -> $pret:ty;
                )*
            )?
        }
    )*) => {
        /// Delegating base for [`RuntimeStore`] decorators.
        ///
        /// Implementors supply one inner store and override only the
        /// operations they intercept. Every other operation is forwarded to
        /// the inner store by the blanket segment implementations generated
        /// below; the `provided` conveniences alone run their segment default
        /// over this decorator's own primitive.
        ///
        /// `Inner` is the store the decorator wraps. A decorator over a
        /// deployment names its deployment store here, so the deployment's
        /// attachment root set and control-intent ledger forward wholesale.
        ///
        /// A decorator must not implement the segment traits directly; doing
        /// so would overlap those blanket implementations.
        #[async_trait::async_trait]
        pub trait RuntimeStoreDecorator: Send + Sync {
            type Inner: RuntimeStore + ?Sized;

            fn inner(&self) -> &Self::Inner;

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
                [$($scope:tt)*] fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
            )*
            $(provided:
                $(
                    $(#[$pmeta:meta])*
                    [$($pscope:tt)*] fn $pname:ident(&self $(, $parg:ident: $parg_ty:ty)*) -> $pret:ty;
                )*
            )?
        }
    )*) => {
        $(
            #[async_trait::async_trait]
            impl<T> $component for T
            where
                T: RuntimeStoreDecorator + ?Sized,
            {
                $(
                    $(#[$meta])*
                    async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                        RuntimeStoreDecorator::$name(self $(, $arg)*).await
                    }
                )*
            }
        )*
    };
}

/// The operation names of the list, by routing, for the surface lints.
#[cfg(test)]
macro_rules! emit_operation_table {
    ($(
        $component:ident {
            $(
                $(#[$meta:meta])*
                [$($scope:tt)*] fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
            )*
            $(provided:
                $(
                    $(#[$pmeta:meta])*
                    [$($pscope:tt)*] fn $pname:ident(&self $(, $parg:ident: $parg_ty:ty)*) -> $pret:ty;
                )*
            )?
        }
    )*) => {
        /// Every listed operation, with its segment, its routing, its scope
        /// marker, its parameter types and its return type, as written in
        /// the list.
        pub(crate) const RUNTIME_STORE_OPERATIONS: &[OperationSignature] = &[
            $(
                $(
                    OperationSignature {
                        component: stringify!($component),
                        name: stringify!($name),
                        provided: false,
                        scope: stringify!($($scope)*),
                        params: &[$(stringify!($arg_ty)),*],
                        returns: stringify!($ret),
                    },
                )*
                $($(
                    OperationSignature {
                        component: stringify!($component),
                        name: stringify!($pname),
                        provided: true,
                        scope: stringify!($($pscope)*),
                        params: &[$(stringify!($parg_ty)),*],
                        returns: stringify!($pret),
                    },
                )*)?
            )*
        ];
    };
}

/// One entry of the operation list, as text (test builds only).
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
#[allow(
    dead_code,
    reason = "`component` and `returns` are read by the history gate test (ADR 0112 §13)"
)]
pub(crate) struct OperationSignature {
    pub component: &'static str,
    pub name: &'static str,
    pub provided: bool,
    pub scope: &'static str,
    pub params: &'static [&'static str],
    pub returns: &'static str,
}

runtime_store_operations!(emit_decorator_trait);
runtime_store_operations!(emit_component_impls);
#[cfg(test)]
runtime_store_operations!(emit_operation_table);
pub(super) use runtime_store_operations;

/// Provided conveniences a decorator answers through its own primitive, for
/// the surface lint.
#[cfg(test)]
pub(super) fn self_routed_conveniences() -> Vec<&'static str> {
    RUNTIME_STORE_OPERATIONS
        .iter()
        .filter(|operation| operation.provided)
        .map(|operation| operation.name)
        .collect()
}

/// `FleetFormatStore` is a synchronous segment the operation list cannot
/// express — its entries are emitted as `async fn`. The same
/// wholesale-delegation norm holds: a decorator answers its inner store's
/// fleet format rather than maintaining one of its own.
impl<T> FleetFormatStore for T
where
    T: RuntimeStoreDecorator + ?Sized,
{
    fn fleet_format(&self) -> FleetFormat {
        self.inner().fleet_format()
    }
}

/// A deployment's attachment root set forwards wholesale from a decorator
/// whose inner store keeps one.
#[async_trait::async_trait]
impl<T> crate::attachments::AttachmentRootSet for T
where
    T: RuntimeStoreDecorator + ?Sized,
    T::Inner: crate::attachments::AttachmentRootSet,
{
    fn can_prove_process_owner_death(&self) -> bool {
        self.inner().can_prove_process_owner_death()
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, StoreError> {
        self.inner()
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn list_condemnations(&self) -> Result<Vec<AttachmentCondemnationRecord>, StoreError> {
        self.inner().list_condemnations().await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner()
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }

    fn fence(&self) -> crate::attachments::AttachmentGcFence {
        self.inner().fence()
    }

    async fn condemn_attachment(
        &self,
        id: &crate::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<AttachmentCondemnation, StoreError> {
        self.inner()
            .condemn_attachment(id, intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn arm_attachment_delete(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        self.inner().arm_attachment_delete(id).await
    }

    async fn release_attachment_condemnation(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner().release_attachment_condemnation(id).await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner().recover_abandoned_attachment_write(id).await
    }

    async fn retire_attachment_condemnation(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner().retire_attachment_condemnation(id).await
    }
}

/// A deployment's control-intent ledger forwards wholesale from a decorator
/// whose inner store keeps one.
#[async_trait::async_trait]
impl<T> ControlIntentStore for T
where
    T: RuntimeStoreDecorator + ?Sized,
    T::Inner: ControlIntentStore,
{
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<ControlIntent>, StoreError> {
        self.inner().begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: ControlIntentId,
        at_ms: u64,
    ) -> Result<IntentApplication, StoreError> {
        self.inner().claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        self.inner().acknowledge_intent(id, claim, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: ControlIntentId,
        claim: &ClaimToken,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> Result<IntentSettle, StoreError> {
        self.inner()
            .record_intent_failure(id, claim, error, retryable, at_ms)
            .await
    }

    async fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError> {
        self.inner().load_intent(id).await
    }

    async fn open_root_intent(
        &self,
        request: &RootIntentRequest,
        at_ms: u64,
    ) -> Result<ControlIntent, RootIntentRefused> {
        self.inner().open_root_intent(request, at_ms).await
    }
}
