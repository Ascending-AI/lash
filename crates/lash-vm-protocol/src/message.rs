//! The typed messages a parent and its worker exchange.
//!
//! The worker hosts one kernel machine per execution, and the parent drives
//! it through the machine's own interface: [`ParentMessage::Run`] compiles
//! the document when it starts the run, hands the machine the committed
//! outcomes it waits on and runs ready tasks until the run parks, spends
//! its slice or ends, and a park that requests something answers with the
//! run's checkpoint candidate ([`ParentMessage::Start`],
//! [`ParentMessage::Deliver`] and [`ParentMessage::Seal`] do each part
//! alone). While
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

use crate::page::{PageCapability, PageExchangeId, PageReply, PageRequest, ReadPage, SealBatch};
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

/// Whether a run meters charge, selected by the host at start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Charge {
    #[default]
    Exact,
    Off,
}

/// The run's response to conservative memory pressure, carried to every worker.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLimit {
    #[default]
    Recount,
    Refuse,
    Off,
}

/// The bounds a run is held to, as the machine counts them. Passing one ends
/// the run with a typed bound error; it is the run's own end, not a worker
/// failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBounds {
    pub charge: Charge,
    pub memory_limit: MemoryLimit,
    /// The most the run may be charged, in the kernel's charge units.
    pub charge_limit: u64,
    /// The most heap the run may hold, in bytes as the machine accounts them.
    pub memory: u64,
    pub call_depth: u32,
    pub live_tasks: u32,
    pub requests_per_park: u32,
    pub join_members: u32,
    /// Whether the run can be saved. A durable run is the default and
    /// states nothing.
    #[serde(default, skip_serializing_if = "Durability::is_durable")]
    pub durability: Durability,
}

/// Whether a run can be saved, chosen when it starts and the same on every
/// worker that resumes it.
///
/// An ephemeral run is never saved: it keeps its whole state in its
/// worker's memory and does none of the work a save needs. It cannot be
/// checkpointed, moved or recovered, and exactly once across crashes is not
/// offered for its effects. A run whose host would have to save it (a park
/// that requests an effect, or a session cell's end that carries its
/// session) is refused with [`crate::RunRefusal::Ephemeral`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// The run can be saved at every park and resumed from it.
    #[default]
    Durable,
    /// The run is never saved.
    Ephemeral,
}

impl Durability {
    /// Whether this is [`Durability::Durable`].
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Durable)
    }
}

/// What a run starts from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartFrom {
    /// A new run: its target, arguments and carried session, in the
    /// kernel's encoding. A carried session's pages are read through
    /// [`WorkerMessage::Pages`] under the capabilities it names.
    Fresh(EncodedPayload),
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
    /// The admitted document, as its canonical token body (`K-ID-003`),
    /// the body of its checkpoint graph node. The worker decodes, validates
    /// and compiles it: the parent never compiles model code.
    pub document: EncodedPayload,
    pub from: StartFrom,
    pub bounds: RunBounds,
    /// The OwnerBinding node of the execution every checkpoint root of the
    /// run is bound to: the parent stages the node, the worker writes its
    /// digest into each root it seals.
    #[serde(with = "serde_bytes")]
    pub binding: [u8; 32],
    /// How a session cell's end saves the functions its carry would drop,
    /// in the kernel embedding's encoding; none for a run that saves none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<EncodedPayload>,
    /// Pages the run reads first, as a read of them answers: its carried
    /// session's or checkpoint root's pages and what lies nearest under
    /// them, so a fresh activation does not fault each in on its own
    /// exchange (FIG-6321). The worker checks each as a read reply.
    pub warm: Vec<ReadPage>,
    /// Whether the parent observes the document sites the run enters. When
    /// it does, each run frame the worker sends ([`WorkerMessage::HostRead`],
    /// `Parked`, `Slice` and `Ended`) carries the sites entered since the
    /// previous one; no exchange of its own reports them.
    pub observe_sites: bool,
    /// Enables exact document statement-start counts. Live deltas travel
    /// with Slice and HostRead only when observe_sites is true; parks and
    /// ends always carry the remaining counts.
    pub observe_counts: bool,
    /// Records the run's statement path for a replay: each Slice, Parked
    /// and Ended frame carries the document statement starts and
    /// empty-block entries since the previous one, by task, in order.
    pub observe_path: bool,
}

/// One committed outcome, for the wait the machine named.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub wait: lash_protocol_types::WaitId,
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
    pub charged: Option<u64>,
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
    ///
    /// `sealed` is the stage capability the parent issued for each page of
    /// the last seal batch that crossed with a `Parked`, `Sealed` or
    /// `Ended` frame, in batch order: the worker holds those pages under
    /// these grants before the slice reads or references them (FIG-6288).
    Run {
        start: Option<Box<Start>>,
        deliveries: Vec<Delivery>,
        slice: u64,
        cancel: bool,
        sealed: Vec<PageCapability>,
    },
    /// One committed outcome, for the wait the machine named: a delivery
    /// that does not fit one frame with the slice it precedes.
    Deliver {
        wait: lash_protocol_types::WaitId,
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
    /// Seal the run as it stands at this safe point into a checkpoint
    /// candidate: a run's start before its first slice, or a park whose
    /// candidate did not cross with it.
    Seal,
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
        /// Opt-in diagnostic delta, with no execution or admission authority.
        #[serde(skip_serializing_if = "Option::is_none")]
        work: Option<WorkerWork>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sites: Option<EncodedPayload>,
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
    /// park, and the waits withdrawn since. A park that requests something
    /// is sealed: `candidate` names its `CheckpointRoot` page in the
    /// activation's staging, which the parent publishes with the park's
    /// admissions. `seal` carries the pages sealed since the last batch
    /// crossed, the root among them, so a park adds no page exchange
    /// (FIG-6288); the parent admits them before it handles the park.
    Parked {
        park: EncodedPayload,
        meters: RunMeters,
        seal: Option<SealBatch>,
        candidate: Option<[u8; 32]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sites: Option<EncodedPayload>,
    },
    /// The slice is spent and a task is still ready.
    Slice {
        meters: RunMeters,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sites: Option<EncodedPayload>,
    },
    /// The run is over. A fully received `Ended` wins over a later EOF or
    /// exit.
    /// `seal` carries the pages sealed since the last batch crossed: a
    /// finished cell's carry crosses with its end.
    Ended {
        end: EncodedPayload,
        meters: RunMeters,
        seal: Option<SealBatch>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sites: Option<EncodedPayload>,
    },
    /// The outcome was taken, or dropped when its wait had been
    /// withdrawn.
    Delivered,
    /// The candidate a [`ParentMessage::Seal`] asked for: its
    /// `CheckpointRoot` page in the activation's staging, with the pages
    /// sealed since the last batch crossed.
    Sealed {
        seal: Option<SealBatch>,
        candidate: [u8; 32],
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
            Self::Sealed { .. } => WorkerFrameKind::Sealed,
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
        sealed: &'a [PageCapability],
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

/// Count-only performance receipt for one worker exchange. Never persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerWork {
    pub serialized_body_bytes: u64,
    pub decoded_body_bytes: u64,
    pub vm_steps: u64,
    /// Execution-root publications in this exchange.
    pub root_publishes: u64,
    // Fixed implementation work: calls, copied rows, completed-trace work,
    // and scratch maxima. Never instructions or guest charge. The standard
    // census supplies accounting/trace counts; custom censuses do not.
    pub task_resolutions: u64,
    pub frame_resolutions: u64,
    pub register_slots_initialized: u64,
    pub register_slots_cleared: u64,
    pub tree_leaf_clones: u64,
    pub tree_index_clones: u64,
    pub tree_rows_cloned: u64,
    pub accounting_certificate_reads: u64,
    pub accounting_additions: u64,
    pub accounting_changes: u64,
    pub accounting_reservations: u64,
    pub refinement_attempts: u64,
    pub completed_refinements: u64,
    pub completed_trace_work: u64,
    /// Maximum over completed traces; absent when no trace completed.
    pub trace_bitmap_peak_bytes: Option<u64>,
    /// Maximum over completed traces; absent when no trace completed.
    pub trace_work_list_peak_bytes: Option<u64>,
    /// Maximum over completed traces; absent when no trace completed.
    pub trace_budget_peak_bytes: Option<u64>,

    /// Pages the worker's heap read through its page port (cache misses).
    pub pages_loaded: u64,
    /// Pages the worker's heap sealed into the activation's staging.
    pub pages_sealed: u64,
    pub encoded_nodes: u64,
    pub encoded_bytes: u64,
    pub decoded_nodes: u64,
    pub decoded_bytes: u64,
    /// Largest simultaneously owned frame buffers, excluding kernel heap.
    pub peak_buffer_bytes: u64,
}
impl WorkerWork {
    pub fn accumulate(&mut self, next: Self) {
        self.serialized_body_bytes += next.serialized_body_bytes;
        self.decoded_body_bytes += next.decoded_body_bytes;
        self.vm_steps += next.vm_steps;
        self.root_publishes += next.root_publishes;
        self.task_resolutions += next.task_resolutions;
        self.frame_resolutions += next.frame_resolutions;
        self.register_slots_initialized += next.register_slots_initialized;
        self.register_slots_cleared += next.register_slots_cleared;
        self.tree_leaf_clones += next.tree_leaf_clones;
        self.tree_index_clones += next.tree_index_clones;
        self.tree_rows_cloned += next.tree_rows_cloned;
        self.accounting_certificate_reads += next.accounting_certificate_reads;
        self.accounting_additions += next.accounting_additions;
        self.accounting_changes += next.accounting_changes;
        self.accounting_reservations += next.accounting_reservations;
        self.refinement_attempts += next.refinement_attempts;
        self.completed_refinements += next.completed_refinements;
        self.completed_trace_work += next.completed_trace_work;
        self.trace_bitmap_peak_bytes = self
            .trace_bitmap_peak_bytes
            .max(next.trace_bitmap_peak_bytes);
        self.trace_work_list_peak_bytes = self
            .trace_work_list_peak_bytes
            .max(next.trace_work_list_peak_bytes);
        self.trace_budget_peak_bytes = self
            .trace_budget_peak_bytes
            .max(next.trace_budget_peak_bytes);

        self.pages_loaded += next.pages_loaded;
        self.pages_sealed += next.pages_sealed;
        self.encoded_nodes += next.encoded_nodes;
        self.encoded_bytes += next.encoded_bytes;
        self.decoded_nodes += next.decoded_nodes;
        self.decoded_bytes += next.decoded_bytes;
        self.peak_buffer_bytes = self.peak_buffer_bytes.max(next.peak_buffer_bytes);
    }
}
