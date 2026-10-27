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
//! - [`checkpoint`]: the typed refusals of a run's checkpoint root, which
//!   the parent checks by its header alone, never a guest frame:
//!   validation, compilation and resumption happen only in the worker.
//! - [`page`]: the page exchange of the persistent heap.
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
/// The page exchange of the persistent heap (FIG-6195, FIG-6190).
pub mod page;
pub mod state;
mod version;

pub use bounds::{PAGE_BATCH_BYTES, PAGE_BATCH_PAGES, ProtocolBounds};
pub use codec::{
    DecodeLimits, FRAME_HEADER_BYTES, FRAME_MAGIC, FrameCodec, FrameReader, ObservationChunker,
};
pub use message::{
    BorrowedParentFrame, BorrowedParentMessage, BoundReached, Charge, CheckpointStart, Delivery,
    EncodedPayload, HostReadId, HostReadKind, LimitDecision, LimitedBound, MemoryMeter,
    MessageFence, MessageHeader, ParentFrame, ParentMessage, PoolStart, PoolStartParts,
    PoolStarted, RunBounds, RunMeters, Start, StartFrom, StoredDefinition, WorkerFrame,
    WorkerMessage, WorkerPhase, WorkerResources, WorkerWork,
};
pub use state::VmOwner;
pub use version::{
    MIN_SUPPORTED_WORKER_PROTOCOL_VERSION, WORKER_PROTOCOL_VERSION, check_worker_protocol_version,
};
