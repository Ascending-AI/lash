//! Neutral engine failure evidence shared by hosts, stores and worker transports.
use crate::VersionRange;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodecRefusal {
    #[error("frame is truncated: {needed} bytes needed, {available} available")]
    Truncated { needed: u64, available: u64 },
    #[error("frame does not start with the protocol magic")]
    BadMagic,
    #[error("frame declares {declared} bytes, over the {limit}-byte bound")]
    FrameTooLarge { limit: u64, declared: u64 },
    #[error("payload nests deeper than {limit}")]
    DepthExceeded { limit: u32 },
    #[error("payload holds more than {limit} values")]
    NodeLimitExceeded { limit: u64 },
    #[error("payload would allocate {requested} bytes, over the {limit}-byte bound")]
    AllocationExceeded { limit: u64, requested: u64 },
    #[error("frame is malformed: {reason}")]
    Malformed { reason: Detail },
    #[error("frame is followed by {extra} unread bytes")]
    TrailingBytes { extra: u64 },
    /// A page exchange names more than [`crate::PAGE_BATCH_PAGES`] pages or
    /// carries more than [`crate::PAGE_BATCH_BYTES`] envelope bytes.
    #[error("a page exchange of {pages} pages and {bytes} bytes exceeds its batch bound")]
    PageBatch { pages: u64, bytes: u64 },
}
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[error(
    "worker protocol {worker_version} is outside parent range {minimum_supported_version}..={parent_version} (parent crate {parent_crate_version}, worker crate {worker_crate_version})"
)]
pub struct ProtocolVersionRefusal {
    pub parent_version: u32,
    pub minimum_supported_version: u32,
    pub worker_version: u32,
    pub parent_crate_version: String,
    pub worker_crate_version: String,
}
/// Why a checkpoint root's header failed the parent's or the worker's
/// structural check.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRefusal {
    #[error("the checkpoint was written under kernel version {found}, outside read range {reads}")]
    KernelOutsideReadRange { found: u32, reads: VersionRange },
    #[error("the checkpoint was parked under document {found}, expected {expected}")]
    WrongDocument { expected: String, found: String },
    #[error("the checkpoint's root is page kind {found}, not a checkpoint root")]
    WrongRootKind { found: u32 },
    #[error("the checkpoint is bound to execution {found}, expected {expected}")]
    WrongOwner { expected: String, found: String },
    /// The root pins another execution closure than its document's.
    #[error("the checkpoint pins execution closure {found}, expected its document's {expected}")]
    WrongClosure { expected: String, found: String },
    #[error("the checkpoint's root does not decode: {problem}")]
    Malformed { problem: String },
}

/// The kernel's precise refusal of a checkpoint it will not resume, carried
/// typed from the worker to the parent and into the store's graph refusals.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KernelRestoreRefusal {
    /// A live continuation needs an exact build helper absent from this image.
    #[error("build helper {function} is missing")]
    MissingHelper {
        function: lash_vocabulary::FunctionId,
    },
    #[error("the document is not admitted: {reason}")]
    NotAdmitted { reason: String },
    #[error("the state was parked under document {parked}, not {given}")]
    DocumentMismatch { parked: String, given: String },
    #[error(
        "the state was parked under kernel version {parked}; the document is written for {supported}"
    )]
    KernelVersion { parked: u32, supported: u32 },
    #[error("a parked wait names effect {effect}, which no admission under this execution holds")]
    AdmissionUnresolved { effect: String },
    #[error("the parked state is malformed: {problem}")]
    Malformed { problem: String },
    /// The run's closure lists a function that only a native implements,
    /// and this worker has no native for it.
    #[error("function {function} needs a native implementation this worker lacks")]
    MissingNative { function: String },
    /// The run's stored definition of a function is refused at admission:
    /// it is not stored, does not decode, is another function's, is written
    /// for another kernel version, or the checker refuses it. Every
    /// definition a run pins is stored before it parks, so one neither held
    /// nor stored is a store that lost it.
    #[error("the stored definition of function {function} is refused: {problem}")]
    DefinitionRefused { function: String, problem: String },
    /// The admission under this execution holds a request of another kind
    /// than the wait's.
    #[error("the admission of effect {effect} is not the kind of the parked wait")]
    AdmissionMismatch { effect: String },
}

/// Why the parent refused a page exchange. Every refusal ends the
/// activation as infrastructure; none is a guest outcome.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageRefusal {
    #[error("a capability is forged, foreign or of another epoch")]
    Unauthorized,
    #[error("the activation's root or stage lease is no longer valid")]
    LeaseLost,
    #[error("a page is missing from the store")]
    Missing,
    #[error("a page does not check against its digest or envelope")]
    Corrupt,
    #[error("the request exceeds the batch bound of {pages} pages or {bytes} bytes")]
    BatchTooLarge { pages: u64, bytes: u64 },
    #[error("the activation's staging quota of {bytes} bytes is exhausted")]
    StagingExhausted { bytes: u64 },
    #[error("the activation's page deadline passed")]
    Deadline,
    #[error("the store is unavailable")]
    StoreUnavailable,
}

impl PageRefusal {
    /// Whether redriving the owning invocation can succeed: a page the
    /// store lacks or holds corrupt is so on every attempt; a lease,
    /// deadline, quota or transport refusal is the host's moment.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::Missing | Self::Corrupt)
    }
}

/// A heap fault the worker met itself (FIG-6422), as it crosses to the
/// parent: infrastructure, never a guest outcome. The wire mirrors the
/// kernel's `lash_kernel_doc::heap::HeapFault`, which this crate cannot
/// name: the worker carries each fault's `Display`, which names the page,
/// as its `detail`.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeapFault {
    #[error("{detail}")]
    Missing { detail: Detail },
    #[error("{detail}")]
    Corrupt { detail: Detail },
    #[error("{detail}")]
    Unauthorized { detail: Detail },
    #[error("the activation's root or stage lease is no longer valid")]
    LeaseLost,
    #[error("host resource {resource:?} is exhausted")]
    Exhausted { resource: HostResource },
    #[error("{detail}")]
    Transport { detail: Detail },
    #[error("{detail}")]
    Malformed { detail: Detail },
}

/// The host resources a heap draws on beside guest memory, as they cross.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostResource {
    /// Quiescent census bitmap, work-list and root scratch budget.
    TraceBudget,
    /// Resident clean-page cache.
    CleanResidency,
    /// Resident dirty-page budget.
    DirtyResidency,
    /// Pages pinned for an activation.
    PinnedPages,
    /// Uncommitted staged page bytes.
    StagingBytes,
    /// One page-read batch.
    ReadBatch,
    /// Host work granted to an activation.
    HostWork,
}

impl HostResource {
    /// Whether redriving from committed state can succeed once the host
    /// resource is available. The engine owns redrive; this is no guarantee
    /// that the same resource grant will suffice on another attempt.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        match self {
            Self::TraceBudget
            | Self::CleanResidency
            | Self::DirtyResidency
            | Self::PinnedPages
            | Self::StagingBytes
            | Self::ReadBatch
            | Self::HostWork => true,
        }
    }
}

impl HeapFault {
    /// Whether redriving the owning invocation can succeed: missing and
    /// corrupt content is so on every attempt, and the heap's own stored
    /// state is no different; a lease, grant, host resource or transport
    /// fault is the host's moment.
    pub fn is_retryable(&self) -> bool {
        !matches!(
            self,
            Self::Missing { .. } | Self::Corrupt { .. } | Self::Malformed { .. }
        )
    }
}

/// What the supervisor observed of a worker's end. Evidence, never testimony:
/// a worker cannot claim how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEvidence {
    /// The pipe closed with no exit status observed yet.
    EndOfStream,
    Exited {
        code: i32,
    },
    Signalled {
        signal: i32,
    },
    /// The worker made a system call its confinement does not allow, and
    /// the kernel killed it (`SIGSYS`).
    ForbiddenSyscall,
}

use crate::worker_limit::{WorkerFrameKind, WorkerLimit};

/// A diagnostic beside a typed cause, cut to [`Self::MAX_BYTES`]. The variant
/// that carries it decides the class; the bound keeps a refusal within any
/// frame whatever its text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(from = "String", into = "String")]
pub struct Detail(String);

/// Diagnostic retention within the refusal frame's 512-byte safety ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "usize", into = "usize")]
pub struct DetailLimit(usize);
impl DetailLimit {
    pub const fn standard() -> Self {
        Self(512)
    }
    pub fn new(bytes: usize) -> Option<Self> {
        (bytes <= 512).then_some(Self(bytes))
    }
    pub const fn bytes(self) -> usize {
        self.0
    }
}
impl Default for DetailLimit {
    fn default() -> Self {
        Self::standard()
    }
}
impl TryFrom<usize> for DetailLimit {
    type Error = &'static str;
    fn try_from(bytes: usize) -> Result<Self, Self::Error> {
        Self::new(bytes).ok_or("diagnostic limit exceeds the 512-byte refusal ceiling")
    }
}
impl From<DetailLimit> for usize {
    fn from(limit: DetailLimit) -> usize {
        limit.0
    }
}

impl Detail {
    pub const MAX_BYTES: usize = 512;

    pub fn new(text: impl std::fmt::Display) -> Self {
        Self::with_limit(text, DetailLimit::standard())
    }
    pub fn with_limit(text: impl std::fmt::Display, limit: DetailLimit) -> Self {
        let mut text = text.to_string();
        let mut end = text.len().min(limit.bytes());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Detail {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<Detail> for String {
    fn from(detail: Detail) -> Self {
        detail.0
    }
}

impl std::fmt::Display for Detail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The exchange a worker answered with a message it does not admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Exchange {
    /// The first message of a checkout, which is the handshake.
    Handshake,
    /// A step of the run.
    Run,
    /// The parent was answering the machine's host read.
    HostRead,
    /// The parent was answering the machine's page exchange.
    Pages,
    Deliver,
    Seal,
    Prepare,
    Reset,
}

impl std::fmt::Display for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Handshake => "as its handshake",
            Self::Run => "mid-run",
            Self::HostRead => "while its host read was answered",
            Self::Pages => "while its page exchange was answered",
            Self::Deliver => "in answer to a delivery",
            Self::Seal => "in answer to a seal",
            Self::Prepare => "in answer to pure work",
            Self::Reset => "in answer to a reset",
        })
    }
}

/// A message that arrived out of order, or an exchange whose accounting does
/// not add up.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SequenceFault {
    #[error("an idle worker expects Start or lifecycle control")]
    IdleWithoutStart,
    #[error("the exchange runs under no lease")]
    MissingLease,
    #[error("the exchange has no owner")]
    MissingOwner,
    #[error("Start before reset")]
    StartBeforeReset,
    #[error("Prepare before reset")]
    PrepareBeforeReset,
    #[error("the checkout already started")]
    CheckoutAlreadyStarted,
    #[error("an answer answers no pending host read")]
    NoPendingRead,
    #[error("an answer names another host read than the pending one")]
    WrongReadId,
    #[error("a host read repeats an id the run already used")]
    RepeatedReadId,
    #[error("a host read was answered with another message")]
    ReadOtherControl,
    #[error("an answer answers no pending page exchange")]
    NoPendingPages,
    #[error("a page answer names another exchange than the pending one")]
    WrongPageExchange,
    #[error("the exchange needs a started run")]
    NoRun,
    #[error("the run already ended")]
    RunEnded,
    #[error("a worker phase is out of order")]
    InvalidPhase,
    #[error("worker CPU accounting regressed")]
    CpuAccountingRegressed,
    #[error("a failed worker cannot reset")]
    FailedWorkerReset,
    #[error("the stream ended inside a frame")]
    EndInPartialFrame,
    #[error("a frame header is incomplete")]
    IncompleteFrameHeader,
    #[error("lease space is exhausted")]
    LeaseSpaceExhausted,
    #[error("pure work was answered with another response")]
    UnexpectedServiceResponse,
    #[error("the worker receipt probe overflowed")]
    ReceiptProbeOverflow,
}

/// A typed payload inside a frame that did not encode or decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Bootstrap,
    Document,
    Start,
    Park,
    End,
    Outcome,
    HostRead,
    HostAnswer,
    Printed,
    /// Document position hints (`entered`, each task's last) alongside
    /// exact statement-start count deltas (`ran`) since the previous frame.
    Sites,
    Checkpoint,
    ServiceRequest,
    ServiceResponse,
}

/// Why a freshly exec'd worker could not take its place on the pipe.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapFault {
    #[error("the worker environment is not empty")]
    EnvironmentNotEmpty,
    #[error("the IPC descriptor is missing")]
    MissingDescriptor,
    #[error("the IPC descriptor is not a stream socket above stdio")]
    InvalidDescriptor,
    #[error("inherited descriptors cannot be enumerated")]
    DescriptorEnumeration,
    #[error("the IPC bounds are missing")]
    MissingBounds,
    #[error("the IPC bounds do not decode")]
    InvalidBounds,
    #[error("the worker could not confine itself")]
    Confinement,
}

/// A failure of the supervising pool itself, met while it drove a worker.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PoolFault {
    /// A typed refusal of the native library held by a worker.
    #[error("the worker refused the parent's native library: {refusal}")]
    Library { refusal: WorkerLibraryRefusal },
    #[error("the worker recovery store failed")]
    Recovery,
    #[error("the worker queue refuses {bytes} bytes")]
    QueueFull { bytes: u64 },
    #[error("worker checkout exceeded its bounded wait")]
    CheckoutTimedOut,
    #[error("repeated worker failures exhausted the restart window")]
    RestartStorm,
    #[error("worker pool configuration is invalid")]
    InvalidConfiguration,
    #[error("this platform has no worker descriptor adapter")]
    UnsupportedPlatform,
    #[error("worker I/O failed with OS error {code:?}")]
    Io { code: Option<i32> },
    /// The host's cgroup v2 subtree cannot hold the pool's workers under
    /// the limits it configured (FIG-6787).
    #[error("{refusal}")]
    Cgroup { refusal: CgroupRefusal },
}

/// Why a pool cannot place its workers in the cgroup v2 subtree its host
/// named (FIG-6787). The host's deployment, never the run's.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CgroupRefusal {
    /// The subtree is not a cgroup v2 directory.
    #[error("the worker cgroup subtree is not a cgroup v2 directory")]
    NotCgroupV2,
    /// The subtree cannot hand `controller` to the workers' cgroups.
    #[error("the worker cgroup subtree does not delegate its {controller:?} controller")]
    ControllerNotDelegated { controller: CgroupController },
    /// The subtree refused a worker's cgroup, a limit in it or the worker's
    /// move into it.
    #[error("the worker cgroup subtree refused a worker with OS error {code:?}")]
    Refused { code: Option<i32> },
}

/// A cgroup v2 controller a worker's limits need.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CgroupController {
    Memory,
    Cpu,
}

/// A parent, its worker or the pool between them broke the protocol. A fresh
/// worker may keep it, so the owning invocation is redriven.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolBreach {
    #[error("{refusal}")]
    Frame { refusal: CodecRefusal },
    #[error("{refusal}")]
    Header { refusal: HeaderRefusal },
    #[error("{refusal}")]
    Version { refusal: ProtocolVersionRefusal },
    #[error("{fault}")]
    Sequence { fault: SequenceFault },
    #[error("the worker sent {found:?} {exchange}")]
    Unexpected {
        exchange: Exchange,
        found: WorkerFrameKind,
    },
    #[error("{payload:?} payload is refused: {detail}")]
    Payload {
        payload: PayloadKind,
        detail: Detail,
    },
    /// A checkpoint root the worker itself sealed fails the parent's check.
    #[error("the worker's checkpoint is refused: {refusal}")]
    Checkpoint { refusal: CheckpointRefusal },
    #[error("the machine refused a step the protocol drove: {detail}")]
    Machine { detail: Detail },
    #[error("{fault}")]
    Bootstrap { fault: BootstrapFault },
    /// Worker code, or the parent task driving it, panicked.
    #[error("worker code panicked: {detail}")]
    Panicked { detail: Detail },
    #[error("{fault}")]
    Pool { fault: PoolFault },
}

/// An input of the run that the worker reads when it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunInput {
    Document,
    Start,
    Checkpoint,
}

impl std::fmt::Display for RunInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Document => "document",
            Self::Start => "start",
            Self::Checkpoint => "checkpoint",
        })
    }
}

/// The run's own inputs are refused. They are the same on every attempt, so
/// redriving the invocation refuses them again: the refusal is terminal.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunRefusal {
    /// A cache miss must name code supplied under this exact run's ownership.
    #[error("the run did not supply document {document}")]
    DocumentNotSupplied {
        document: lash_vocabulary::names::DocumentId,
    },

    /// The run names immutable code the worker's exact image does not own.
    #[error("worker image does not own document {document}")]
    ImageDocumentMissing {
        document: lash_vocabulary::names::DocumentId,
    },

    #[error("unusable payload schema: {source}")]
    UnusableSchema {
        source: Box<lash_vocabulary::json_schema::SchemaAdmissionError>,
    },
    #[error("{refusal}")]
    Checkpoint { refusal: CheckpointRefusal },
    /// The run's session or process recorded a dialect this worker's
    /// embedding does not install: the host that built the worker did not
    /// name it. Refused before the document is read or anything runs.
    #[error("the run is written in the dialect `{dialect}`, which this worker does not install")]
    DialectNotInstalled { dialect: String },
    /// The worker will not resume the checkpoint: its pinned
    /// release, a function or native it needs, or a wait's admission.
    #[error("{refusal}")]
    Restore { refusal: KernelRestoreRefusal },
    #[error("the run's {input} does not decode: {detail}")]
    Undecodable { input: RunInput, detail: Detail },
    #[error("a payload of {size} bytes exceeds the {limit}-byte bound")]
    PayloadTooLarge { limit: u64, size: u64 },
    /// The document is not admitted, names a library function the worker
    /// has not registered, or has no such entry.
    #[error("the machine refuses to start the run: {detail}")]
    Start { detail: Detail },
    /// The parked run is not one this machine resumes.
    #[error("the machine refuses to resume the run: {detail}")]
    Resume { detail: Detail },
    /// The run asks for more of a bound than its worker pool admits
    /// (FIG-6827): the pool's run bounds are a ceiling, never a clamp.
    /// Refused before anything runs.
    #[error("the run asks for a {bound} of {requested}, past the {ceiling} its worker pool admits")]
    AboveCeiling {
        bound: RunBound,
        requested: u64,
        ceiling: u64,
    },
}

/// One of the bounds a run states and its worker pool admits up to: a
/// field of [`crate::RunBounds`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunBound {
    Charge,
    Memory,
    CallDepth,
    LiveTasks,
    RequestsPerPark,
    JoinMembers,
}

impl std::fmt::Display for RunBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Charge => "charge limit",
            Self::Memory => "memory bound",
            Self::CallDepth => "call depth",
            Self::LiveTasks => "live-task bound",
            Self::RequestsPerPark => "per-park request bound",
            Self::JoinMembers => "join-member bound",
        })
    }
}

/// What a worker may say of its own failure: that the protocol was broken,
/// that it refuses the run, or that its heap faulted. How it ended is the
/// supervisor's evidence, and a limit travels as
/// [`crate::WorkerMessage::LimitExceeded`].
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRefusal {
    #[error("{0}")]
    Breach(ProtocolBreach),
    #[error("{0}")]
    Run(RunRefusal),
    /// A heap fault the worker met itself (FIG-6422), which crosses as
    /// itself: the worker's own heap is its own to report.
    #[error("{0}")]
    Heap(HeapFault),
}

/// Why this deployment cannot launch its configured worker executable.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDeploymentFault {
    #[error("the configured executable was not found")]
    NotFound,
    #[error("the configured file cannot be executed")]
    NotExecutable,
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InfrastructureOutcome {
    #[error("the worker executable {executable:?} is unavailable: {fault}")]
    WorkerDeployment {
        executable: std::path::PathBuf,
        fault: WorkerDeploymentFault,
    },
    #[error("the worker crashed ({evidence:?})")]
    WorkerCrashed { evidence: SupervisorEvidence },
    /// The worker died after it began computing the run, before the run
    /// released it: an end the run may have caused, such as an allocation
    /// abort at the address-space ceiling, a kill for memory or a crash in
    /// a native helper. Another attempt may pass, so it is retryable, but
    /// it spends one of the execution's attempts and is never a capacity
    /// verdict: a run that keeps killing its worker runs out of attempts
    /// (FIG-6786). A worker lost before it computed the run is
    /// [`Self::WorkerCrashed`].
    #[error("the worker died running the run ({evidence:?})")]
    RunCrashedWorker { evidence: SupervisorEvidence },
    #[error("the worker sent nothing for {silent_ms} ms")]
    WorkerUnresponsive { silent_ms: u64 },
    #[error("the worker protocol was broken: {breach}")]
    ProtocolViolation { breach: ProtocolBreach },
    #[error("the run is refused: {refusal}")]
    RunRefused { refusal: RunRefusal },
    #[error("the run exhausted its {limit:?} limit")]
    WorkerLimitExceeded { limit: WorkerLimit },
    /// The parent's page service refused the run's page exchange (FIG-6190):
    /// infrastructure, never a guest outcome. The activation ends and its
    /// execution resumes from its last committed root.
    #[error("the run's page exchange is refused: {refusal}")]
    PageRefused { refusal: PageRefusal },
    /// A heap fault the worker met itself (FIG-6422): infrastructure,
    /// never a guest outcome. The activation ends and its execution
    /// resumes from its last committed root.
    #[error("the run's heap faulted: {fault}")]
    HeapFault { fault: HeapFault },
}

impl InfrastructureOutcome {
    /// Whether redriving the owning invocation can succeed. A refusal of the
    /// run's inputs and a limit the run itself exhausted fail the same way on
    /// every attempt; a host's deadline or budget does not
    /// ([`WorkerLimit::is_host_verdict`]).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::WorkerLimitExceeded { limit } => limit.is_host_verdict(),
            Self::PageRefused { refusal } => refusal.is_retryable(),
            Self::HeapFault { fault } => fault.is_retryable(),
            Self::RunRefused { .. } | Self::WorkerDeployment { .. } => false,
            Self::WorkerCrashed { .. }
            | Self::RunCrashedWorker { .. }
            | Self::WorkerUnresponsive { .. }
            | Self::ProtocolViolation { .. } => true,
        }
    }

    /// The deployment fault, whose repair lets an operator redrive retained work.
    pub fn deployment_fault(&self) -> Option<(&std::path::Path, WorkerDeploymentFault)> {
        match self {
            Self::WorkerDeployment { executable, fault } => Some((executable, *fault)),
            _ => None,
        }
    }

    /// A committed checkpoint's header fails the structural check before
    /// a worker is handed it: the same on every attempt.
    pub fn input_checkpoint(refusal: CheckpointRefusal) -> Self {
        RunRefusal::Checkpoint { refusal }.into()
    }

    /// A checkpoint a worker sealed fails the parent's structural check:
    /// the worker's breach.
    pub fn output_checkpoint(refusal: CheckpointRefusal) -> Self {
        ProtocolBreach::Checkpoint { refusal }.into()
    }
}

impl WorkerRefusal {
    /// Apply the deployment's diagnostic cut without changing its typed cause.
    pub fn with_detail_limit(mut self, limit: DetailLimit) -> Self {
        let detail = match &mut self {
            Self::Breach(
                ProtocolBreach::Payload { detail, .. }
                | ProtocolBreach::Machine { detail }
                | ProtocolBreach::Panicked { detail },
            )
            | Self::Run(
                RunRefusal::Undecodable { detail, .. }
                | RunRefusal::Start { detail }
                | RunRefusal::Resume { detail },
            )
            | Self::Heap(
                HeapFault::Missing { detail }
                | HeapFault::Corrupt { detail }
                | HeapFault::Unauthorized { detail }
                | HeapFault::Transport { detail }
                | HeapFault::Malformed { detail },
            ) => Some(detail),
            Self::Breach(ProtocolBreach::Frame {
                refusal: CodecRefusal::Malformed { reason },
            }) => Some(reason),
            _ => None,
        };
        if let Some(detail) = detail {
            *detail = Detail::with_limit(detail.as_str(), limit);
        }
        self
    }

    /// The parent's reading of a worker's refusal: the one mapping every
    /// reader of the pipe shares.
    pub fn into_outcome(self) -> InfrastructureOutcome {
        match self {
            Self::Breach(breach) => InfrastructureOutcome::ProtocolViolation { breach },
            Self::Run(refusal) => InfrastructureOutcome::RunRefused { refusal },
            Self::Heap(fault) => InfrastructureOutcome::HeapFault { fault },
        }
    }
}

impl From<ProtocolBreach> for InfrastructureOutcome {
    fn from(breach: ProtocolBreach) -> Self {
        Self::ProtocolViolation { breach }
    }
}

impl From<RunRefusal> for InfrastructureOutcome {
    fn from(refusal: RunRefusal) -> Self {
        Self::RunRefused { refusal }
    }
}

impl From<SequenceFault> for ProtocolBreach {
    fn from(fault: SequenceFault) -> Self {
        Self::Sequence { fault }
    }
}

impl From<HeaderRefusal> for ProtocolBreach {
    fn from(refusal: HeaderRefusal) -> Self {
        Self::Header { refusal }
    }
}

impl From<BootstrapFault> for ProtocolBreach {
    fn from(fault: BootstrapFault) -> Self {
        Self::Bootstrap { fault }
    }
}

/// A size bound refuses the same payload on every attempt; any other codec
/// refusal is a broken frame.
impl From<CodecRefusal> for InfrastructureOutcome {
    fn from(refusal: CodecRefusal) -> Self {
        match refusal {
            CodecRefusal::FrameTooLarge { limit, declared } => RunRefusal::PayloadTooLarge {
                limit,
                size: declared,
            }
            .into(),
            CodecRefusal::AllocationExceeded { limit, requested } => RunRefusal::PayloadTooLarge {
                limit,
                size: requested,
            }
            .into(),
            refusal @ (CodecRefusal::Truncated { .. }
            | CodecRefusal::BadMagic
            | CodecRefusal::DepthExceeded { .. }
            | CodecRefusal::NodeLimitExceeded { .. }
            | CodecRefusal::Malformed { .. }
            | CodecRefusal::TrailingBytes { .. }
            | CodecRefusal::PageBatch { .. }) => ProtocolBreach::Frame { refusal }.into(),
        }
    }
}

/// The native library's typed validation refusal, independent of VM transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Error)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLibraryRefusal {
    #[error("the native library is malformed: {problem}")]
    Malformed { problem: String },
    #[error("native {name} ({function}) is missing")]
    NativeMissing {
        function: lash_vocabulary::FunctionId,
        name: lash_vocabulary::FunctionName,
    },
    /// A build's generated helper plan has no exact body in the held image.
    #[error("generated plan {name} ({function}) has no registered body")]
    PlanMissing {
        function: lash_vocabulary::FunctionId,
        name: lash_vocabulary::FunctionName,
    },
    #[error("native {function} is unused")]
    NativeUnused {
        function: lash_vocabulary::FunctionId,
    },
}
