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
//! - [`codec`]: length-framed encoding and protocol-version admission, with
//!   bounded decoding that charges frame size, depth, node count and
//!   cumulative allocation before anything is allocated.
//! - [`outcome`]: the typed infrastructure outcomes a worker's failure maps
//!   to, kept apart from guest errors: a [`ProtocolBreach`] is retried, and a
//!   [`RunRefusal`] of the run's own inputs is terminal.
//! - [`bounds`]: [`ProtocolBounds`], every bound a host states for its
//!   workers, with the measured `standard()` preset.
//!
//! There is no transport and no pool here; both belong to the worker entry.

pub mod bounds;
pub mod codec;
pub mod message;
pub mod outcome;
pub mod state;
mod version;

pub use bounds::ProtocolBounds;
pub use codec::{
    CodecRefusal, DecodeLimits, FRAME_HEADER_BYTES, FRAME_MAGIC, FrameCodec, FrameReader,
    ObservationChunker,
};
pub use message::{
    ContextDescription, EffectKind, EffectOutcome, EffectRequest, EffectRequestId, EffectResponse,
    EncodedPayload, ExecutionLease, FrameEpoch, HeaderRefusal, MessageFence, MessageHeader,
    OwnerEpoch, ParentFrame, ParentMessage, ProgramEntry, ProgramSource, Start, StartState,
    TransportSequence, VmLimits, WorkerFrame, WorkerMessage, WorkerPhase,
};
pub use outcome::{
    BootstrapFault, Detail, Exchange, InfrastructureOutcome, PayloadKind, PoolFault,
    ProtocolBreach, RunInput, RunRefusal, SequenceFault, SupervisorEvidence, WorkerFrameKind,
    WorkerLimit, WorkerRefusal,
};
pub use state::{
    OpaqueStateRefusal, OpaqueVmState, StateDigest, StateExpectation, VmOwner, VmStateKind,
};
pub use version::{
    MIN_SUPPORTED_WORKER_PROTOCOL_VERSION, ProtocolVersionRefusal, WORKER_PROTOCOL_VERSION,
    check_worker_protocol_version,
};
mod contract;
pub use contract::{VmContract, VmContractComponent, VmContractReads};
