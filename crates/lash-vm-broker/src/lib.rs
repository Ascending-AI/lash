//! The parent side of lash's worker boundary (ADR 0123).
//!
//! Model code runs in a worker process that holds guest state only. The
//! parent holds every grant, binding, route, ordinal and ledger, and brokers
//! every effect the worker asks for. This crate is that broker:
//!
//! - [`authority`]: what a worker may request, resolved against the
//!   parent's admitted context. Worker ids, bytes and claims confer nothing.
//! - [`ledger`]: the parent-issued ordinals and grants of a run, and the
//!   [`Checkpoint`](ledger::Checkpoint) that commits them atomically with the
//!   VM state they match.
//! - [`identity`]: the one derivation of a code command's `ToolCallId`.
//! - [`effects`]: the journaled parent work behind every admitted operation.
//! - [`broker`]: the run loop, worker-loss recovery through the substrate,
//!   terminal precedence, cancellation, frame fencing and slot release.
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
pub mod transport;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use authority::{
    AdmittedContext, ArgumentContract, AuthorityRefusal, BoundOperation, FrozenBindings,
    HandleGrant, Invocation, OperationRequest, RequestFingerprint, ToolRoute,
};
pub use broker::{
    Broker, BrokerBounds, BrokerFailure, BrokeredEnd, FrameFence, ParkedOperation, RunStart,
    SettledOperation, Settlement, StateContract,
};
pub use effects::{ParentEffects, ParentFault, Performed};
pub use identity::CodeCallIdentities;
pub use ledger::{
    AdmittedCall, AdmittedKind, AdmittedOperation, Checkpoint, CheckpointRefusal, CheckpointStore,
    LedgerSnapshot, ParentLedger,
};
pub use session::VmSession;
pub use transport::{CheckoutRefusal, WorkerCheckout, WorkerRead, WorkerSlots, WorkerTransport};
