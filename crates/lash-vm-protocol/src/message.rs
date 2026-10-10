//! The typed messages a parent and its worker exchange.
//!
//! The worker hosts one kernel machine per execution, and the parent drives
//! it through the machine's own interface: [`ParentMessage::Start`] compiles
//! the document, [`ParentMessage::Run`] runs ready tasks until the run parks,
//! spends its slice or ends, [`ParentMessage::Deliver`] hands it one
//! committed outcome and [`ParentMessage::Export`] asks for its state. While
//! a slice runs the worker asks the parent what the machine reads from its
//! host ([`WorkerMessage::HostRead`]) and hands over what it prints
//! ([`WorkerMessage::Printed`]). Every message travels under a
//! [`MessageHeader`], and a receiver admits it through a [`MessageFence`].
//!
//! Documents, requests, outcomes and ends cross as [`EncodedPayload`] bytes
//! in the kernel's own encoding. A request is a *request*: its effect name
//! and arguments confer no authority. The parent admits every one against
//! the execution it admitted, with its own grants and identities.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::{OpaqueVmState, VmOwner};

/// The lease one checkout of a worker runs under. The parent mints it and
/// fences it when the worker is lost, so a late message under an old lease is
/// refused rather than applied.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ExecutionLease(pub u64);

/// The owner's epoch: advances when ownership of the session or durable
/// process moves, so a message from a superseded owner is refused.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct OwnerEpoch(pub u64);

/// The context frame's epoch: advances when a frame opens (F5), which fences
/// every response from the frame before it.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct FrameEpoch(pub u64);

/// The transport sequence: each direction numbers its messages from zero,
/// one apart, so a dropped, duplicated or reordered message is refused.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
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
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
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

    pub fn next_header_copy(&self) -> MessageHeader {
        MessageHeader {
            lease: self.lease,
            owner_epoch: self.owner_epoch,
            frame_epoch: self.frame_epoch,
            sequence: self.next,
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

/// Encoded bytes of the kernel's own vocabulary. The protocol does not know
/// it; the parent's broker and the worker's machine agree on it under the
/// kernel version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EncodedPayload(#[serde(with = "serde_bytes")] pub Vec<u8>);

/// The bounds a run is held to, as the machine counts them. Passing one ends
/// the run with a typed bound error; it is the run's own end, not a worker
/// failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBounds {
    /// The most the run may be charged, in the kernel's charge units.
    pub charge: u64,
    /// The most heap the run may hold, in bytes as the machine accounts them.
    pub memory: u64,
    pub call_depth: u32,
    pub live_tasks: u32,
    pub requests_per_park: u32,
    pub join_members: u32,
}

/// What a run starts from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartFrom {
    /// A new run: its target, arguments and session bindings, in the
    /// kernel's encoding.
    Fresh(EncodedPayload),
    /// A parked run, as the worker that exported it sealed it.
    Parked(OpaqueVmState),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub owner: VmOwner,
    /// The admitted document, in its JSON encoding. The worker validates
    /// and compiles it: the parent never compiles model code.
    pub document: EncodedPayload,
    pub from: StartFrom,
    pub bounds: RunBounds,
}

/// Names one host read within a run, so its answer answers exactly it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostReadId(pub u64);

/// What a machine reads from its host while it runs. None is a wait: the
/// parent answers at once, and a read in a stretch that was not saved is
/// drawn again after a crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostReadKind {
    Clock,
    Random,
    /// A read through a projection handle: the payload names the handle and
    /// the request, and the answer is kernel data or a kernel error.
    Projection,
}

/// What a run has used so far, as its machine meters it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunMeters {
    pub charged: u64,
    pub memory: u64,
    pub live_tasks: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentMessage {
    Start(Box<Start>),
    /// Pure compiler or state work. The payload has no parent authority.
    Prepare {
        owner: VmOwner,
        request: EncodedPayload,
    },
    /// Run ready tasks until none is ready, the run ends, or `slice` charge
    /// units are spent. With `cancel`, the machine observes a run cancel at
    /// its next safe point and ends cancelled; the durable winner is still
    /// the parent's to decide (ADR 0039).
    Run {
        slice: u64,
        cancel: bool,
    },
    /// One committed outcome, for the wait the machine named.
    Deliver {
        wait: u64,
        outcome: EncodedPayload,
    },
    /// The answer to the worker's pending [`WorkerMessage::HostRead`].
    HostAnswer {
        id: HostReadId,
        answer: EncodedPayload,
    },
    /// Write the run's state as it stands. Legal between runs: every task
    /// is between statements.
    Export,
    /// Physical cancellation: drop the machine where it stands.
    Cancel,
    /// Drop the machine and everything guest-derived. Sent only after a
    /// clean end or release; any failure discards the worker instead.
    Reset,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMessage {
    /// The worker refuses the exchange or the run. How a worker ended is
    /// never its own to say.
    Refused {
        refusal: crate::WorkerRefusal,
    },
    /// A bounded phase, with cumulative process CPU usage. It does not grant
    /// extra time when repeated: the parent owns the absolute phase deadline.
    Progress {
        phase: WorkerPhase,
        cpu_nanos: u64,
    },
    LimitExceeded {
        limit: crate::WorkerLimit,
    },
    /// The handshake. The parent checks only the protocol version.
    Ready {
        protocol_version: u32,
        crate_version: String,
    },
    /// The machine exists and has run nothing.
    Started,
    /// The machine reads its host; it runs on once the parent answers.
    HostRead {
        id: HostReadId,
        kind: HostReadKind,
        request: EncodedPayload,
    },
    /// What the run printed, in order, with no authority.
    Printed {
        payload: EncodedPayload,
    },
    /// No task is ready: the effects and sleeps requested since the last
    /// park, and the waits withdrawn since.
    Parked {
        park: EncodedPayload,
        meters: RunMeters,
    },
    /// The slice is spent and a task is still ready.
    Slice {
        meters: RunMeters,
    },
    /// The run is over. A fully received `Ended` wins over a later EOF or
    /// exit.
    Ended {
        end: EncodedPayload,
        meters: RunMeters,
    },
    /// The outcome was taken; `dropped` when its wait had been withdrawn.
    Delivered {
        dropped: bool,
    },
    Exported {
        state: OpaqueVmState,
    },
    Cancelled,
    ResetDone {
        cpu_nanos: u64,
    },
    /// A complete response to pure worker work.
    Prepared {
        response: EncodedPayload,
    },
}

impl WorkerMessage {
    pub fn kind(&self) -> crate::WorkerFrameKind {
        use crate::WorkerFrameKind;
        match self {
            Self::Refused { .. } => WorkerFrameKind::Refused,
            Self::Progress { .. } => WorkerFrameKind::Progress,
            Self::LimitExceeded { .. } => WorkerFrameKind::LimitExceeded,
            Self::Ready { .. } => WorkerFrameKind::Ready,
            Self::Started => WorkerFrameKind::Started,
            Self::HostRead { .. } => WorkerFrameKind::HostRead,
            Self::Printed { .. } => WorkerFrameKind::Printed,
            Self::Parked { .. } => WorkerFrameKind::Parked,
            Self::Slice { .. } => WorkerFrameKind::Slice,
            Self::Ended { .. } => WorkerFrameKind::Ended,
            Self::Delivered { .. } => WorkerFrameKind::Delivered,
            Self::Exported { .. } => WorkerFrameKind::Exported,
            Self::Cancelled => WorkerFrameKind::Cancelled,
            Self::ResetDone { .. } => WorkerFrameKind::ResetDone,
            Self::Prepared { .. } => WorkerFrameKind::Prepared,
        }
    }
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

/// Computation and serialization have deadlines separate from IPC silence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    Computing,
    Serializing,
    Responding,
}
