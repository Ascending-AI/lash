//! The protocol between a parent and the worker process that runs model code
//! (ADR 0123).
//!
//! Model code runs in a resettable worker. The worker owns the guest heap,
//! roots and scratch; the parent owns every grant, binding, route, ordinal and
//! ledger, and brokers every effect. This crate is what the two exchange:
//!
//! - [`message`]: the typed messages, each under a [`MessageHeader`] naming its
//!   execution lease, owner and frame epochs and transport sequence.
//! - [`state`]: [`VmOwner`], whose state a worker holds.
//! - [`graph_state`]: [`SealedGraphState`], a parked run as checkpoint graph
//!   nodes under a seal binding its root to its structural facts; the parent
//!   checks the facts and the seal, never a guest frame: validation,
//!   compilation and resumption happen only in the worker.
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
pub mod graph_state;
pub mod message;
pub mod outcome;
pub mod state;
mod version;

pub use bounds::ProtocolBounds;
pub use codec::{
    CodecRefusal, DecodeLimits, FRAME_HEADER_BYTES, FRAME_MAGIC, FrameCodec, FrameReader,
    ObservationChunker,
};
pub use graph_state::{
    GraphNodeBytes, GraphRoot, GraphStateExpectation, GraphStateRefusal, KernelRestoreRefusal,
    SealedGraphState,
};
pub use message::{
    EncodedPayload, ExecutionLease, FrameEpoch, HeaderRefusal, HostReadId, HostReadKind,
    MessageFence, MessageHeader, OwnerEpoch, ParentFrame, ParentMessage, ParkedGraph, RunBounds,
    RunMeters, Start, StartFrom, TransportSequence, WorkerFrame, WorkerMessage, WorkerPhase,
};
pub use outcome::{
    BootstrapFault, Detail, Exchange, InfrastructureOutcome, PayloadKind, PoolFault,
    ProtocolBreach, RunInput, RunRefusal, SequenceFault, SupervisorEvidence, WorkerDeploymentFault,
    WorkerFrameKind, WorkerLimit, WorkerRefusal,
};
pub use state::{StateDigest, VmOwner};
pub use version::{
    MIN_SUPPORTED_WORKER_PROTOCOL_VERSION, ProtocolVersionRefusal, WORKER_PROTOCOL_VERSION,
    check_worker_protocol_version,
};
