//! The typed messages a parent and its worker exchange.
//!
//! The worker hosts one kernel machine per execution, and the parent drives
//! it through the machine's own interface: [`ParentMessage::Run`] compiles
//! the document when it starts the run, hands the machine the committed
//! outcomes it waits on and runs ready tasks until the run parks, spends
//! its slice or ends, and a park that requests something answers with the
//! run's state ([`ParentMessage::Start`], [`ParentMessage::Deliver`] and
//! [`ParentMessage::ExportGraph`] do each part alone). While
//! a slice runs the worker asks the parent what the machine reads from its
//! host ([`WorkerMessage::HostRead`]) and hands over what it prints
//! ([`WorkerMessage::Printed`]). A run on the persistent heap reads and
//! seals its pages the same way, mid-slice ([`WorkerMessage::Pages`]).
//! Every message travels under a
//! [`MessageHeader`], and a receiver admits it through a [`MessageFence`].
//!
//! Documents, requests, outcomes and ends cross as [`EncodedPayload`] bytes
//! in the kernel's own encoding. A request is a *request*: its effect name
//! and arguments confer no authority. The parent admits every one against
//! the execution it admitted, with its own grants and identities.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::graph_state::SealedGraphState;
use crate::page::{PageCapability, PageExchangeId, PageReply, PageRequest};
use crate::state::VmOwner;

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
    /// A parked run as checkpoint graph state, with the admission of every
    /// wait it holds.
    ParkedGraph(ParkedGraph),
    /// A committed checkpoint root on the persistent heap: the worker reads
    /// its control header, then only the pages its next steps touch.
    Checkpoint(CheckpointStart),
}

/// A run resumed from a committed checkpoint root (FIG-6190): the root's
/// capability under the activation's root lease, and the admission of every
/// wait it holds. Nothing else crosses: the heap faults its pages in
/// through [`WorkerMessage::Pages`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointStart {
    /// The `CheckpointRoot` page, readable under the activation's root
    /// lease.
    pub root: PageCapability,
    /// Each an admitted wait in the kernel's binary encoding
    /// (`lash_kernel_state::AdmittedWait`); the protocol does not read it.
    pub admissions: Vec<EncodedPayload>,
}

/// A parked run as the parent hands it back: the graph state a worker
/// exported (a KernelWhole root and the nodes it needs), and, for every wait
/// the run holds, the request the parent's admission froze when the park
/// handed the wait out. The state holds a wait as its effect identity
/// alone, so a worker resumes it only from these (FIG-5887 §3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkedGraph {
    pub state: SealedGraphState,
    /// Each an admitted wait in the kernel's binary encoding
    /// (`lash_kernel_state::AdmittedWait`); the protocol does not read it.
    pub admissions: Vec<EncodedPayload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub owner: VmOwner,
    /// The dialect the run's document is written in, as its session or
    /// process recorded it at creation. A worker whose embedding lacks it
    /// refuses the run before it reads the document
    /// ([`crate::RunRefusal::DialectNotInstalled`]), and a new run pins
    /// that dialect's helper release (FIG-5990).
    pub dialect: String,
    /// The admitted document, in its JSON encoding. The worker validates
    /// and compiles it: the parent never compiles model code.
    pub document: EncodedPayload,
    pub from: StartFrom,
    pub bounds: RunBounds,
    /// The execution the run's parked state is bound to (an OwnerBinding
    /// body in the graph's token encoding), when the parent saves the
    /// state at every park that requests something: the worker exports it
    /// there and [`WorkerMessage::Parked`] carries it, so no separate
    /// [`ParentMessage::ExportGraph`] exchange asks for it (FIG-5921).
    pub export: Option<EncodedPayload>,
}

/// One committed outcome, for the wait the machine named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub wait: u64,
    pub outcome: EncodedPayload,
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
    /// A `perform` the run binds as a read (`K-EFF-012`): the payload names
    /// the effect, its arguments and the site of the `perform`, and the
    /// answer is kernel data or a kernel error.
    Effect,
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
    /// Build the run's machine. A run's start crosses with its first
    /// [`ParentMessage::Run`], and on its own only when the two outgrow one
    /// frame.
    Start(Box<Start>),
    /// Pure compiler or state work. The payload has no parent authority.
    /// A lease's one `Prepare` may be followed by the `Start` of the run it
    /// prepared, with no reset between them (FIG-5921).
    Prepare {
        owner: VmOwner,
        request: EncodedPayload,
    },
    /// Build the run's machine from `start` when this exchange begins the
    /// run, hand it `deliveries` in order, then run ready tasks until none
    /// is ready, the run ends, or `slice` charge units are spent. With
    /// `cancel`, the machine observes a run cancel at its next safe point
    /// and ends cancelled; the durable winner is still the parent's to
    /// decide (ADR 0039). One exchange does what a start, its deliveries
    /// and a slice need, under one compute deadline (FIG-5921).
    Run {
        start: Option<Box<Start>>,
        deliveries: Vec<Delivery>,
        slice: u64,
        cancel: bool,
    },
    /// One committed outcome, for the wait the machine named: a delivery
    /// that does not fit one frame with the slice it precedes.
    Deliver {
        wait: u64,
        outcome: EncodedPayload,
    },
    /// The answer to the worker's pending [`WorkerMessage::HostRead`].
    HostAnswer {
        id: HostReadId,
        answer: EncodedPayload,
    },
    /// The answer to the worker's pending [`WorkerMessage::Pages`].
    Pages {
        id: PageExchangeId,
        reply: PageReply,
    },
    /// Write the run's state at a park as checkpoint graph state, bound to
    /// the execution `binding` names (an OwnerBinding body in the graph's
    /// token encoding).
    ExportGraph {
        binding: EncodedPayload,
    },
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
    /// The machine's heap reads or seals pages; it runs on once the parent
    /// answers. The machine's statement and native stack wait intact: a
    /// page fault never parks, retries or reruns anything.
    Pages {
        id: PageExchangeId,
        request: PageRequest,
    },
    /// What the run printed, in order, with no authority.
    Printed {
        payload: EncodedPayload,
    },
    /// No task is ready: the effects and sleeps requested since the last
    /// park, and the waits withdrawn since. `state` is the run's state at
    /// the park, bound to its start's `export`, when the park requests
    /// something and the two fit one frame; otherwise the parent asks for
    /// it with [`ParentMessage::ExportGraph`].
    Parked {
        park: EncodedPayload,
        meters: RunMeters,
        state: Option<SealedGraphState>,
        /// On the persistent heap, the sealed candidate root: a stage
        /// capability for its `CheckpointRoot` page in the activation's
        /// spool, which the parent publishes.
        candidate: Option<PageCapability>,
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
    /// The outcome was taken, or dropped when its wait had been
    /// withdrawn.
    Delivered,
    ExportedGraph {
        state: SealedGraphState,
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
            // A page exchange is a host read of the heap's storage.
            Self::HostRead { .. } | Self::Pages { .. } => WorkerFrameKind::HostRead,
            Self::Printed { .. } => WorkerFrameKind::Printed,
            Self::Parked { .. } => WorkerFrameKind::Parked,
            Self::Slice { .. } => WorkerFrameKind::Slice,
            Self::Ended { .. } => WorkerFrameKind::Ended,
            Self::Delivered => WorkerFrameKind::Delivered,
            Self::ExportedGraph { .. } => WorkerFrameKind::Exported,
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

/// A [`ParentFrame`] whose message borrows what it carries: how a parent
/// encodes or measures a message whose parts it keeps, to send them apart
/// should they not fit one frame together. It encodes exactly as the owned
/// frame does.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct BorrowedParentFrame<'a> {
    pub header: MessageHeader,
    pub message: BorrowedParentMessage<'a>,
}

/// The [`ParentMessage`]s a parent encodes from parts it keeps.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BorrowedParentMessage<'a> {
    Start(&'a Start),
    Run {
        start: Option<&'a Start>,
        deliveries: &'a [Delivery],
        slice: u64,
        cancel: bool,
    },
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
