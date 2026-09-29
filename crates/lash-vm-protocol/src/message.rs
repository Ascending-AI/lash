//! The typed messages a parent and its worker exchange.
//!
//! Parent to worker: [`ParentMessage`] (`Start`, `EffectResult`, `Cancel`,
//! `Reset`, `Shutdown`). Worker to parent: [`WorkerMessage`] (`Ready`,
//! `EffectRequest`, `Suspended`, `Complete`, `GuestError`, `Cancelled`,
//! `ResetDone`). Every message travels under a [`MessageHeader`], and a
//! receiver admits it through a [`MessageFence`].
//!
//! Effect requests and results carry their values as [`EncodedPayload`]
//! bytes. The request is a *request*: its operation, receiver and claimed call
//! site confer no authority. The parent resolves every request against the
//! execution context it admitted, with its own grants, bindings and ordinals.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::identity::BuildIdentity;
use crate::state::{OpaqueVmState, VmOwner};

/// The lease one checkout of a worker runs under. The parent mints it and
/// fences it when the worker is lost, so a late message under an old lease is
/// refused rather than applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionLease(pub u64);

/// The owner's epoch: advances when ownership of the session or durable
/// process moves, so a message from a superseded owner is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerEpoch(pub u64);

/// The context frame's epoch: advances when a frame opens (F5), which fences
/// every response from the frame before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrameEpoch(pub u64);

/// The transport sequence: each direction numbers its messages from zero,
/// one apart, so a dropped, duplicated or reordered message is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransportSequence(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageHeader {
    pub lease: ExecutionLease,
    pub owner_epoch: OwnerEpoch,
    pub frame_epoch: FrameEpoch,
    pub sequence: TransportSequence,
}

/// Why a receiver refused a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum HeaderRefusal {
    #[error("message runs under lease {found:?}, expected {expected:?}")]
    StaleLease {
        expected: ExecutionLease,
        found: ExecutionLease,
    },
    #[error("message runs under owner epoch {found:?}, expected {expected:?}")]
    StaleOwnerEpoch {
        expected: OwnerEpoch,
        found: OwnerEpoch,
    },
    #[error("message runs under frame epoch {found:?}, expected {expected:?}")]
    StaleFrameEpoch {
        expected: FrameEpoch,
        found: FrameEpoch,
    },
    #[error("message has transport sequence {found:?}, expected {expected:?}")]
    OutOfSequence {
        expected: TransportSequence,
        found: TransportSequence,
    },
}

/// A receiver's view of one direction of a checkout: the lease and epochs it
/// runs under, and the sequence it expects next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageFence {
    lease: ExecutionLease,
    owner_epoch: OwnerEpoch,
    frame_epoch: FrameEpoch,
    next: TransportSequence,
}

impl MessageFence {
    pub fn new(lease: ExecutionLease, owner_epoch: OwnerEpoch, frame_epoch: FrameEpoch) -> Self {
        Self {
            lease,
            owner_epoch,
            frame_epoch,
            next: TransportSequence(0),
        }
    }

    /// The header this side's next outgoing message carries, advancing the
    /// sequence.
    pub fn next_header(&mut self) -> MessageHeader {
        let header = MessageHeader {
            lease: self.lease,
            owner_epoch: self.owner_epoch,
            frame_epoch: self.frame_epoch,
            sequence: self.next,
        };
        self.next = TransportSequence(self.next.0 + 1);
        header
    }

    /// Admits an incoming header, advancing the expected sequence only when
    /// every field matches.
    pub fn admit(&mut self, header: &MessageHeader) -> Result<(), HeaderRefusal> {
        if header.lease != self.lease {
            return Err(HeaderRefusal::StaleLease {
                expected: self.lease,
                found: header.lease,
            });
        }
        if header.owner_epoch != self.owner_epoch {
            return Err(HeaderRefusal::StaleOwnerEpoch {
                expected: self.owner_epoch,
                found: header.owner_epoch,
            });
        }
        if header.frame_epoch != self.frame_epoch {
            return Err(HeaderRefusal::StaleFrameEpoch {
                expected: self.frame_epoch,
                found: header.frame_epoch,
            });
        }
        if header.sequence != self.next {
            return Err(HeaderRefusal::OutOfSequence {
                expected: self.next,
                found: header.sequence,
            });
        }
        self.next = TransportSequence(self.next.0 + 1);
        Ok(())
    }

    /// Fences the frame: every message under an earlier frame epoch is
    /// refused from here on.
    pub fn open_frame(&mut self, frame_epoch: FrameEpoch) {
        self.frame_epoch = frame_epoch;
    }
}

/// Encoded value bytes. The protocol does not know the value model; the
/// parent's broker and the worker's VM agree on it under the shared build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EncodedPayload(#[serde(with = "serde_bytes")] pub Vec<u8>);

/// Where the program to run comes from. The worker parses, links and compiles
/// it: the parent never compiles model code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgramSource {
    /// Model-authored source in a front end's dialect.
    Source { dialect: String, text: String },
    /// A stored module artifact and the entry to compile from it.
    Artifact {
        module_ref: String,
        entry: String,
        #[serde(with = "serde_bytes")]
        artifact: Vec<u8>,
    },
}

/// One explicit description of the context the program runs in: a tool
/// contract, a projected binding's shape, a module. Descriptions, never
/// handles: nothing in one lets the worker act.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDescription {
    pub kind: String,
    pub name: String,
    pub body: EncodedPayload,
}

/// The VM state a run starts from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartState {
    Fresh,
    Snapshot(OpaqueVmState),
    Continuation(OpaqueVmState),
}

/// The VM limits a run is held to. `None` is unbounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmLimits {
    pub instruction_budget: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub max_frame_depth: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub owner: VmOwner,
    pub program: ProgramSource,
    pub contexts: Vec<ContextDescription>,
    pub state: StartState,
    pub limits: VmLimits,
}

/// Names one effect request within a run, so its result answers exactly it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectRequestId(pub u64);

/// The kind of operation a worker requests. Mirrors the VM's suspension
/// vocabulary; the payload carries the operation itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    ResourceOperation,
    ResourceOperationBatch,
    Await,
    Print,
    Finish,
    Fail,
    ProcessEvent,
    Sleep,
    WaitSignal,
    /// The run reached a cancel checkpoint; the parent answers with its
    /// journaled observation of cancellation.
    CancelCheckpoint,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectRequest {
    pub id: EffectRequestId,
    pub kind: EffectKind,
    pub payload: EncodedPayload,
}

/// What the parent answers an effect request with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectOutcome {
    Value(EncodedPayload),
    Unit,
    /// The parent handed a signal wait to a successor segment.
    HandedOver,
    /// The effect failed; the payload is the host error the guest may catch.
    Failed(EncodedPayload),
    /// A cancel checkpoint's journaled observation.
    Checkpoint {
        cancelled: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectResult {
    pub id: EffectRequestId,
    pub outcome: EffectOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentMessage {
    Start(Box<Start>),
    EffectResult(EffectResult),
    /// Cooperative cancellation. It never decides the durable winner: that is
    /// the journaled checkpoint observation (ADR 0039).
    Cancel,
    /// Drop the VM instance and install a pristine one. Sent only after a
    /// clean completion or release; any failure discards the worker instead.
    Reset,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMessage {
    /// The handshake: the worker's exact build, which must equal the
    /// parent's.
    Ready {
        build: BuildIdentity,
    },
    EffectRequest(EffectRequest),
    /// The run parked; the state resumes it.
    Suspended {
        state: OpaqueVmState,
    },
    /// The run finished. A fully received `Complete` wins over a later EOF or
    /// exit.
    Complete {
        state: OpaqueVmState,
        value: EncodedPayload,
    },
    /// The guest raised an error the VM surfaced; the state is what the cell's
    /// error semantics keep.
    GuestError {
        state: Option<OpaqueVmState>,
        error: EncodedPayload,
    },
    Cancelled,
    ResetDone,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentFrame {
    pub header: MessageHeader,
    pub message: ParentMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFrame {
    pub header: MessageHeader,
    pub message: WorkerMessage,
}
