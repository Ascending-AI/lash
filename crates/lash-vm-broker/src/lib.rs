//! The parent side of lash's worker boundary (ADR 0123).
//!
//! Model code runs in a worker process that holds guest state only. The
//! parent holds every grant, binding, route, admission and ledger, and brokers
//! every effect the worker asks for. This crate is that broker:
//!
//! - [`authority`]: what a worker may request, resolved against the
//!   parent's admitted context. Worker ids, bytes and claims confer nothing.
//! - [`ledger`]: the parent-issued admissions and grants of an execution,
//!   and the [`Checkpoint`](ledger::Checkpoint) that commits them atomically
//!   with the VM state they match.
//! - [`snapshot`]: quiet points, admitted operation identities and the
//!   [`SnapshotStore`] a VM's snapshots commit to.
//! - [`identity`]: the one derivation of a code command's `ToolCallId`.
//! - [`effects`]: the parent's admission and body behind every operation.
//! - [`broker`]: the run loop, quiet points, restore by identity, terminal
//!   precedence, cancellation, frame fencing and slot release.
//! - [`session`]: an owner's frames: opening one (F5) fences old responses,
//!   resets persisted state, then retires live state.
//! - [`transport`]: the seams a worker pool implements.
//!
//! With the `testing` feature, [`testing`] provides an in-process fake worker
//! and pool behind the protocol types.

pub mod authority;
pub mod broker;
pub mod effects;
pub mod identity;
pub mod ledger;
pub mod session;
pub mod snapshot;
pub mod transport;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use authority::{
    AdmittedContext, ArgumentContract, AuthorityRefusal, BoundOperation, FrozenBindings,
    HandleGrant, Invocation, OperationRequest, OperationRequestCodec, RequestFingerprint,
    ToolRoute,
};
pub use broker::{Broker, BrokerBounds, BrokerFailure, BrokeredEnd, FrameFence, RunStart};
pub use effects::{Admission, ParentEffects, ParentFault, Performed, operation_draft, waits_only};
pub use identity::CodeCallIdentities;
pub use lash_core_execution::runtime::actor::round::ExecutionDraft;
pub use lash_core_execution::runtime::actor::waits::{PinnedKey, WaitRef, WaitSpec};
pub use lash_durable::domain::{CellId, ExecKey, SnapshotRev};
pub use ledger::{
    AdmittedCall, AdmittedKind, AdmittedOperation, Checkpoint, ParentLedger, QuietPointRefusal,
    RecordedEnd,
};
pub use session::VmSession;
pub use snapshot::{
    BrokerLedger, Committed, DurableSnapshotStore, OperationAdmission, OperationId,
    PendingOperation, QuietPoint, Recovered, SnapshotStore, outcomes_to_inject,
};
pub use transport::{CheckoutRefusal, WorkerCheckout, WorkerRead, WorkerSlots, WorkerTransport};
