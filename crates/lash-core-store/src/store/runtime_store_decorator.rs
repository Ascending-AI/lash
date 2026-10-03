use super::*;
use crate::SessionId;
use crate::session_catalog::{SessionListFilter, SessionView};
use crate::session_store_factory_types::{
    ForkSessionReceipt, ForkSessionRequest, RetainedRevision, Retention, SessionLookup,
    SessionStoreCreateRequest, Target,
};
use std::num::NonZeroU32;

/// Every [`RuntimeStore`] operation, grouped by the segment trait that
/// declares it (ADR 0112 §2).
///
/// This list is the single place each operation's signature is written. It
/// generates four surfaces, so none of them can drift from the traits:
///
/// - the defaulted forwarders of [`RuntimeStoreDecorator`];
/// - each segment trait's blanket implementation for a decorator;
/// - the inherent forwarders of [`SessionStore`](super::SessionStore);
/// - in test builds, `StoreOp` and the scripted store a law arms faults and
///   pauses on (`crate::testing::script`).
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
/// The `@inner` segment closes the list. It is the deployment's control-intent
/// ledger, which a [`RuntimeStore`] does not keep: a decorator has these
/// operations when the store it wraps is one, and the view has none of them.
///
/// Adding an operation to a segment trait and not to this list is the drift
/// this shape exists to prevent; the
/// `decorator_surface_covers_every_component_trait_method` lint in
/// `store::tests` fails when the two disagree, for the `@inner` segment too.
macro_rules! runtime_store_operations {
    ($emit:ident) => {
        $emit! {
            AttachmentReferrers {
                [catalog] fn begin_attachment_write(&self, write: &AttachmentWrite) -> Result<AttachmentWriteFence, StoreError>;
                [catalog] fn complete_attachment_write(&self, write: &AttachmentWrite, permit: AttachmentWritePermit) -> Result<(), StoreError>;
                [catalog] fn abort_attachment_write(&self, write: &AttachmentWrite, permit: AttachmentWritePermit) -> Result<(), StoreError>;
                [catalog] fn acquire_attachment_refs(&self, claim: &crate::artifact_referrer::ReferrerClaim, attachment_ids: &[crate::AttachmentId]) -> Result<(), StoreError>;
                [catalog] fn forget_attachment_ref(&self, referrer: &crate::artifact_referrer::ArtifactReferrer, attachment_id: &crate::AttachmentId) -> Result<(), StoreError>;
                [catalog] fn end_attachment_referrer(&self, referrer: &crate::artifact_referrer::ArtifactReferrer) -> Result<(), StoreError>;
                [session] fn session_referrer_state(&self, session_id: &SessionId) -> Result<SessionReferrerState, StoreError>;
                [catalog] fn attachment_referrers(&self, attachment_id: &crate::AttachmentId) -> Result<Vec<crate::artifact_referrer::ArtifactReferrer>, StoreError>;
            }
            SessionCatalogStore {
                [catalog] fn admit_session(&self, request: &SessionStoreCreateRequest) -> Result<SessionAdmission, StoreError>;
                [catalog] fn lookup_session(&self, session_id: &SessionId) -> Result<SessionLookup, StoreError>;
                [catalog] fn list_sessions(&self, filter: &SessionListFilter) -> Result<Vec<SessionView>, StoreError>;
                [catalog] fn fork_session(&self, request: &ForkSessionRequest) -> Result<ForkSessionReceipt, StoreError>;
                [session] fn resolve_target(&self, session_id: &SessionId, target: &Target) -> Result<RetainedRevision, StoreError>;
                [session] fn revisions(&self, session_id: &SessionId) -> Result<Vec<RetainedRevision>, StoreError>;
                [session] fn pin(&self, session_id: &SessionId, target: &Target) -> Result<(), StoreError>;
                [session] fn unpin(&self, session_id: &SessionId, target: &Target) -> Result<(), StoreError>;
                [session] fn retention(&self, session_id: &SessionId) -> Result<Retention, StoreError>;
                [session] fn set_retention(&self, session_id: &SessionId, retention: Retention) -> Result<(), StoreError>;
                [catalog] fn delete_session(&self, session_id: &SessionId) -> MaintenanceResult<SessionBlobReclaimReport>;
            }
            WaitReceiptStore {
                [catalog] fn record_wait_request(&self, request: &WaitRequestReceipt) -> Result<StoreTransition<WaitRequestReceipt>, StoreError>;
                [catalog] fn record_wait_resolution(&self, resolution: &WaitResolutionReceipt) -> Result<StoreTransition<WaitResolutionReceipt>, StoreError>;
                [catalog] fn retire_observation_receipts(&self, owner_key: &str, retired_at_ms: u64) -> Result<(), StoreError>;
            }
            SessionCommitStore {
                [catalog] fn tool_request_receipt(&self, request_key: &str) -> Result<Option<ToolRequestReceipt>, StoreError>;
                [catalog] fn record_tool_request(&self, request: &ToolRequestReceipt) -> Result<StoreTransition<ToolRequestReceipt>, StoreError>;
                [catalog] fn record_tool_completion(&self, completion: &ToolCompletionReceipt) -> Result<StoreTransition<ToolCompletionReceipt>, StoreError>;
                [session] fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError>;
                [carried fence] fn admit_session_state(&self, fence: &ShiftFence) -> Result<SessionStateAdmission, StoreError>;
                [session] fn load_session_head_meta(&self, session_id: &SessionId) -> Result<Option<SessionHeadMeta>, StoreError>;
                [carried fence] fn retain_admission_base(&self, fence: &ShiftFence, base: &SessionHeadRef) -> Result<(), StoreError>;
                [session] fn committed_turn_exists(&self, session_id: &SessionId, turn_id: &crate::TurnId) -> Result<bool, StoreError>;
                [carried commit] fn commit_runtime_state(&self, commit: RuntimeCommit) -> Result<RuntimeCommitReceipt, StoreError>;
                [carried fence] fn raise_pending_follow_on_attempts(&self, fence: &ShiftFence, follow_on_turn_id: &crate::TurnId, recovering: &crate::build_generation::BuildGeneration) -> Result<PendingFollowOn, StoreError>;
                [session] fn settle_observer_intents(&self, session_id: &SessionId, remaining: Vec<crate::SessionObserverIntent>) -> Result<(), StoreError>;
                [session] fn load_session_meta(&self, session_id: &SessionId) -> Result<Option<SessionMeta>, StoreError>;
                [session] fn load_session_meta_for_commit(&self, session_id: &SessionId) -> Result<Option<SessionMeta>, StoreError>;
                [carried park] fn record_turn_park(&self, park: &TurnParkWrite) -> Result<StoreTransition<TurnPark>, StoreError>;
                [session] fn load_turn_park(&self, session_id: &SessionId) -> Result<Option<TurnPark>, StoreError>;
                provided:
                [session] fn load_pending_follow_on(&self, session_id: &SessionId) -> Result<Option<PendingFollowOn>, StoreError>;
            }
            SessionHistoryStore {
                [session] fn load_session_window(&self, session_id: &SessionId, selector: WindowSelector) -> Result<Option<SessionWindowRead>, StoreError>;
                [session] fn load_ancestors(&self, session_id: &SessionId, anchor: HistoryAnchor, budget: HistoryBudget) -> Result<HistoryPage, StoreError>;
                [session] fn contains_active_ancestor(&self, session_id: &SessionId, node_id: &crate::NodeId) -> Result<bool, StoreError>;
                [session] fn load_failure_evidence_page(&self, session_id: &SessionId, after: Option<&FailureEvidenceCursor>, limit: NonZeroU32) -> Result<FailureEvidencePage, StoreError>;
            }
            TurnInputStore {
                [carried address] fn turn_is_committed(&self, address: &crate::TurnAddress) -> Result<bool, StoreError>;
                [carried address] fn reconcile_turn_cancel_winner(&self, address: &crate::TurnAddress, observed: &crate::TurnCancelIntentSnapshot, evidence: &crate::TurnCancellationEvidence) -> Result<bool, StoreError>;
                [carried request] fn record_turn_cancel_request(&self, request: crate::TurnCancelRequest) -> Result<crate::TurnCancelRequestRecord, StoreError>;
                [carried address] fn turn_cancel_request(&self, address: &crate::TurnAddress) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError>;
                [carried address] fn turn_cancel_request_intent(&self, address: &crate::TurnAddress) -> Result<crate::TurnCancelIntentSnapshot, StoreError>;
                [session fence] fn validate_turn_cancellation_binding(&self, session_id: &SessionId, fence: &ShiftFence, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<(), StoreError>;
                [carried fence, authorization] fn authorize_turn_cancel_closure(&self, fence: &ShiftFence, authorization: &crate::TurnCancelClosureAuthorization) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;
                [session fence] fn pending_turn_cancel_closures(&self, session_id: &SessionId, fence: &ShiftFence, binding_id: &str, admitted_scope: &crate::ExecutionScope) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                [session] fn pending_turn_cancel_closure_pins(&self, session_id: &SessionId) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;
                [carried batch] fn enqueue_pending_turn_inputs(&self, batch: crate::PendingTurnInputBatch) -> Result<Vec<crate::PendingTurnInput>, StoreError>;
                [carried batch] fn admit_pending_turn_inputs(&self, batch: crate::PendingTurnInputBatch, ingress_claim_ttl_ms: u64) -> Result<TurnInputAdmission, StoreError>;
                [session] fn load_run_spec(&self, session_id: &SessionId, hash: &crate::run_spec::RunSpecHash) -> Result<Option<crate::run_spec::RunSpec>, StoreError>;
                [session] fn list_pending_turn_inputs(&self, session_id: &SessionId) -> Result<Vec<crate::PendingTurnInputRead>, StoreError>;
                [session] fn pending_turn_input(&self, session_id: &SessionId, input_id: &crate::InputId) -> Result<Option<crate::PendingTurnInputRead>, StoreError>;
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
                [carried fence] fn open_session_command_run(&self, fence: &ShiftFence) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                [session] fn cancel_queued_work_batch(&self, session_id: &SessionId, batch_id: &str) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;
                [session] fn queued_work_batch_completion(&self, session_id: &SessionId, batch_id: &str) -> Result<Option<RuntimeCommitReceipt>, StoreError>;
                [session] fn pending_session_work_ordering(&self, session_id: &SessionId) -> Result<PendingSessionWorkOrdering, StoreError>;
                [session] fn list_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                [session] fn list_open_queued_work(&self, session_id: &SessionId) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
                [session] fn has_admissible_queued_work(&self, session_id: &SessionId) -> Result<bool, StoreError>;
            }
            ShiftEpochStore {
                [session] fn seal_shift_epoch(&self, session_id: &SessionId, admission: &AdmissionId, observed_epoch: u64, run_start: &RunStartNonce, hold: Option<&RunHold>) -> Result<ShiftEpochSeal, StoreError>;
                [session] fn shift_epoch(&self, session_id: &SessionId) -> Result<StoredShiftEpoch, StoreError>;
                [session] fn record_session_fault(&self, session_id: &SessionId, record: &SessionFaultRecord, at_ms: u64) -> Result<Option<SessionFault>, StoreError>;
                [session] fn session_fault(&self, session_id: &SessionId) -> Result<Option<SessionFault>, StoreError>;
                [catalog] fn list_session_faults(&self, after: Option<&SessionId>, limit: std::num::NonZeroUsize) -> Result<Vec<SessionFault>, StoreError>;
                [session] fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError>;
            }
            RunStore {
                [session] fn unfinished_run(&self, session_id: &SessionId) -> Result<Option<UnfinishedRun>, StoreError>;
                [carried request] fn prepare_run_admission(&self, request: &AdmitRunRequest) -> Result<Option<PreparedRunAdmission>, StoreError>;
                [carried prepared] fn commit_run_admission(&self, prepared: &PreparedRunAdmission, anchor: &lash_trace::TraceAnchor) -> Result<Option<RunAdmission>, StoreError>;
                [carried request] fn admit_run(&self, request: &AdmitRunRequest) -> Result<Option<RunAdmission>, StoreError>;
                [carried request] fn admit_at_checkpoint(&self, request: &CheckpointAdmissionRequest) -> Result<CheckpointAdmission, StoreError>;
                [session] fn run_terminal(&self, session_id: &SessionId, run: &crate::TurnId) -> Result<Option<RunTerminal>, StoreError>;
                [carried fence] fn end_refused_run(&self, fence: &ShiftFence, run: &crate::TurnId, refusal: &crate::RuntimeError, at_ms: u64) -> Result<RunEndOutcome, StoreError>;
                [carried fence] fn end_command_run(&self, fence: &ShiftFence, run: &crate::TurnId, at_ms: u64) -> Result<RunEndOutcome, StoreError>;
                [session] fn run_of_input(&self, session_id: &SessionId, input: &crate::InputId) -> Result<Option<crate::TurnId>, StoreError>;
                [session] fn run_binding(&self, session_id: &SessionId, input: &crate::InputId) -> Result<Option<crate::TurnId>, StoreError>;
                [session] fn bound_turn_scopes(&self, session_id: &SessionId, run: &crate::TurnId) -> Result<Vec<crate::TurnId>, StoreError>;
                [session] fn bind_run_inputs(&self, session_id: &SessionId, run: &crate::TurnId, inputs: &[crate::InputId]) -> Result<(), StoreError>;
            }
            StoreMaintenance {
                [session] fn vacuum(&self, session_id: &SessionId) -> MaintenanceResult<VacuumReport>;
                [catalog] fn gc_unreachable(&self) -> MaintenanceResult<GcReport>;
            }
            @inner ControlIntentStore {
                fn begin_session_close(&self, session_id: &SessionId, at_ms: u64) -> Result<Option<ControlIntent>, StoreError>;
                fn claim_intent_application(&self, id: ControlIntentId, at_ms: u64) -> Result<IntentApplication, StoreError>;
                fn acknowledge_intent(&self, id: ControlIntentId, claim: &ClaimToken, at_ms: u64) -> Result<IntentSettle, StoreError>;
                fn refuse_intent(&self, id: ControlIntentId, claim: &ClaimToken, cause: &DeliveryError, at_ms: u64) -> Result<IntentSettle, StoreError>;
                fn load_intent(&self, id: ControlIntentId) -> Result<Option<ControlIntent>, StoreError>;
                fn open_run_intent(&self, request: &RunIntentRequest, at_ms: u64) -> Result<ControlIntent, RunIntentRefused>;
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
    )*
    @inner $inner:ident {
        $(
            $(#[$imeta:meta])*
            fn $iname:ident(&self $(, $iarg:ident: $iarg_ty:ty)*) -> $iret:ty;
        )*
    }) => {
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
        /// attachment root set forwards wholesale, and its control-intent
        /// ledger through the `@inner` operations, which a decorator
        /// overrides like any other operation.
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

            $(
                $(#[$imeta])*
                async fn $iname(&self $(, $iarg: $iarg_ty)*) -> $iret
                where
                    Self::Inner: $inner,
                {
                    self.inner().$iname($($iarg),*).await
                }
            )*
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
    )*
    @inner $inner:ident {
        $(
            $(#[$imeta:meta])*
            fn $iname:ident(&self $(, $iarg:ident: $iarg_ty:ty)*) -> $iret:ty;
        )*
    }) => {
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

        /// A deployment's control-intent ledger answers through the
        /// decorator, which forwards to an inner store that keeps one unless
        /// it intercepts the operation.
        #[async_trait::async_trait]
        impl<T> $inner for T
        where
            T: RuntimeStoreDecorator + ?Sized,
            T::Inner: $inner,
        {
            $(
                $(#[$imeta])*
                async fn $iname(&self $(, $iarg: $iarg_ty)*) -> $iret {
                    RuntimeStoreDecorator::$iname(self $(, $iarg)*).await
                }
            )*
        }
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
    )*
    @inner $inner:ident {
        $(
            $(#[$imeta:meta])*
            fn $iname:ident(&self $(, $iarg:ident: $iarg_ty:ty)*) -> $iret:ty;
        )*
    }) => {
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

        /// The `@inner` segment's operations.
        pub(crate) const CONTROL_INTENT_OPERATIONS: &[&str] = &[$(stringify!($iname)),*];
    };
}

/// `StoreOp` and the scripted store: every operation a decorator can
/// intercept, routed through the law's script. The `provided` conveniences
/// have no entry: a decorator answers them through their primitive, which is
/// scripted.
#[cfg(any(test, feature = "testing"))]
macro_rules! emit_scripted_decorator {
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
    )*
    @inner $inner:ident {
        $(
            $(#[$imeta:meta])*
            fn $iname:ident(&self $(, $iarg:ident: $iarg_ty:ty)*) -> $iret:ty;
        )*
    }) => {
        /// A store operation a law scripts, named as the operation list
        /// names it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[expect(
            non_camel_case_types,
            reason = "each variant is spelled as the operation it names"
        )]
        pub enum StoreOp {
            $($($name,)*)*
            $($iname,)*
        }

        impl StoreOp {
            /// Every scriptable store operation, in list order.
            pub const ALL: &[StoreOp] = &[
                $($(StoreOp::$name,)*)*
                $(StoreOp::$iname,)*
            ];

            pub const fn name(self) -> &'static str {
                match self {
                    $($(Self::$name => stringify!($name),)*)*
                    $(Self::$iname => stringify!($iname),)*
                }
            }
        }

        impl From<StoreOp> for crate::testing::Op {
            fn from(op: StoreOp) -> Self {
                Self::listed(op.name())
            }
        }

        #[async_trait::async_trait]
        impl<S> RuntimeStoreDecorator for crate::testing::Scripted<S>
        where
            S: RuntimeStore + ?Sized,
        {
            type Inner = S;

            fn inner(&self) -> &S {
                crate::testing::Scripted::inner(self)
            }

            $($(
                $(#[$meta])*
                async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                    self.call(StoreOp::$name, self.inner().$name($($arg),*)).await
                }
            )*)*

            $(
                $(#[$imeta])*
                async fn $iname(&self $(, $iarg: $iarg_ty)*) -> $iret
                where
                    Self::Inner: $inner,
                {
                    self.call(StoreOp::$iname, self.inner().$iname($($iarg),*)).await
                }
            )*
        }
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
#[cfg(any(test, feature = "testing"))]
runtime_store_operations!(emit_scripted_decorator);
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

    fn plugin_writers(&self) -> super::PluginWriterRangesFuture<'_> {
        self.inner().plugin_writers()
    }

    fn provision_plugin_writers<'a>(
        &'a self,
        registrations: &'a [super::plugin_writers::PluginWriterRegistration],
    ) -> super::PluginWriterRangesFuture<'a> {
        self.inner().provision_plugin_writers(registrations)
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
    async fn attachment_root_page(
        &self,
        source: crate::attachments::AttachmentRootSource,
        after: Option<&crate::AttachmentId>,
    ) -> Result<crate::attachments::AttachmentRootPage, StoreError> {
        self.inner().attachment_root_page(source, after).await
    }
    async fn list_condemnations(&self) -> Result<Vec<AttachmentCondemnationRecord>, StoreError> {
        self.inner().list_condemnations().await
    }

    async fn has_live_attachment_ref(&self, id: &crate::AttachmentId) -> Result<bool, StoreError> {
        self.inner().has_live_attachment_ref(id).await
    }

    fn fence(&self) -> crate::attachments::AttachmentGcFence {
        self.inner().fence()
    }

    async fn begin_attachment_sweep(&self) -> Result<AttachmentSweepGeneration, StoreError> {
        self.inner().begin_attachment_sweep().await
    }

    async fn adopt_attachment_condemnations(
        &self,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnationAdoption, StoreError> {
        self.inner()
            .adopt_attachment_condemnations(generation)
            .await
    }

    async fn condemn_attachment(
        &self,
        id: &crate::AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentCondemnation, StoreError> {
        self.inner().condemn_attachment(id, generation).await
    }

    async fn arm_attachment_delete(
        &self,
        id: &crate::AttachmentId,
        generation: &AttachmentSweepGeneration,
    ) -> Result<AttachmentDeleteArming, StoreError> {
        self.inner().arm_attachment_delete(id, generation).await
    }

    async fn settle_attachment_condemnation(
        &self,
        id: &crate::AttachmentId,
        generation: &AttachmentSweepGeneration,
        settlement: AttachmentCondemnationSettlement,
    ) -> Result<AttachmentSettlementOutcome, StoreError> {
        self.inner()
            .settle_attachment_condemnation(id, generation, settlement)
            .await
    }

    async fn recover_abandoned_attachment_write(
        &self,
        id: &crate::AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner().recover_abandoned_attachment_write(id).await
    }
}
