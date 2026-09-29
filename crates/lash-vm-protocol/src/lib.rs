//! The protocol between a parent and the worker process that runs model code
//! (ADR 0123).
//!
//! Model code runs in a resettable worker. The worker owns the guest heap,
//! roots and scratch; the parent owns every grant, binding, route, ordinal and
//! ledger, and brokers every effect. This crate is what the two exchange:
//!
//! - [`message`]: the typed messages, each under a [`MessageHeader`] naming its
//!   execution lease, owner and frame epochs and transport sequence.
//! - [`state`]: [`OpaqueVmState`], VM state as the parent sees it. The parent
//!   checks size, owner, VM contract and hash, and nothing else: no function
//!   here decodes the bytes, and this crate depends on nothing that could.
//!   Semantic restore, regexp compilation and artifact validation happen only
//!   in the worker.
//! - [`codec`]: length-framed encoding under an exact [`BuildIdentity`], and
//!   bounded decoding that charges frame size, depth, node count and
//!   cumulative allocation before anything is allocated.
//! - [`outcome`]: the typed infrastructure outcomes a worker's failure maps
//!   to, kept apart from guest errors.
//! - [`bounds`]: [`ProtocolBounds`], every bound a host states for its
//!   workers, with the measured `standard()` preset.
//!
//! There is no transport and no pool here; both belong to the worker entry.

pub mod bounds;
pub mod codec;
pub mod identity;
pub mod message;
pub mod outcome;
pub mod state;

pub use bounds::ProtocolBounds;
pub use codec::{
    CodecRefusal, DecodeLimits, FRAME_HEADER_BYTES, FRAME_MAGIC, FrameCodec, FrameReader,
};
pub use identity::BuildIdentity;
pub use message::{
    ContextDescription, EffectKind, EffectOutcome, EffectRequest, EffectRequestId, EffectResult,
    EncodedPayload, ExecutionLease, FrameEpoch, HeaderRefusal, MessageFence, MessageHeader,
    OwnerEpoch, ParentFrame, ParentMessage, ProgramSource, Start, StartState, TransportSequence,
    VmLimits, WorkerFrame, WorkerMessage,
};
pub use outcome::{InfrastructureOutcome, SupervisorEvidence, WorkerLimit};
pub use state::{
    OpaqueStateRefusal, OpaqueVmState, StateDigest, StateExpectation, VmOwner, VmStateKind,
};
