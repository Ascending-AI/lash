//! S4: the execution root and the persistent execution records.
//!
//! A [`CheckpointRoot`] is one root-bounded page (kind
//! [`PageKind::CHECKPOINT_ROOT`](super::PageKind::CHECKPOINT_ROOT), at most
//! 4096 bytes) naming small metadata and the persistent roots of the heap,
//! tasks, ready queue, waits, list joins and session bindings. It is the
//! whole machine: no chronological delta, executable pointer, earlier root
//! or worker cache is needed to resume it. A table of unbounded size is a
//! [`KeyedRoot`] or a [`SequenceRoot`] whose pages fault in on demand.
//!
//! Frames carry semantic coordinates only, matching the flat-KIR machine
//! (FIG-6161): a frame is its pending statement's [`Site`], its bindings by
//! declaring site, its loops, its `finally` departures and its held values.
//! Compiled code, statement and slot indexes, registers and the KIR
//! register window are derived caches, never persisted: a slice clears its
//! registers before any park, and an inline frame runs to its end inside
//! its expression, so neither exists at a checkpoint.
//!
//! These records are the decoded shapes of their pages. Their canonical
//! token encodings are layer hp-heap's (FIG-6188), in this crate; the
//! envelope around them is the generic node store's.

use lash_kernel_doc::heap::{EntityId, LogicalBytes, PageId, PageRef, Value};
use lash_kernel_doc::{DocumentId, EffectIdentity, JoinMode, Name, Site, TaskId};

use super::census::AccountingCertificate;
use super::family::FrozenRef;

/// The root of an ordered-key tree: lookup by a canonical key.
#[derive(Clone, Copy, Debug)]
pub struct KeyedRoot {
    /// `None` for an empty tree.
    pub page: Option<PageRef>,
    pub len: u64,
    /// The rows' aggregate logical cost (S7 summaries).
    pub cost: LogicalBytes,
}

/// The root of an order-statistic sequence tree: access by position.
#[derive(Clone, Copy, Debug)]
pub struct SequenceRoot {
    pub page: Option<PageRef>,
    pub len: u64,
    pub cost: LogicalBytes,
}

/// A heap's persistent root.
#[derive(Clone, Copy, Debug)]
pub struct HeapRoot {
    /// The object directory: [`EntityId`] to object head (kind, inline or
    /// paged body, local cost, identity-edge summary). It is the
    /// allocation census: it may hold entities no root reaches (S7).
    pub directory: KeyedRoot,
    /// [`FamilyRecord`](super::FamilyRecord)s by family, for lazy families
    /// (S8). The primary family has none.
    pub families: KeyedRoot,
    /// The next ordinal of the primary family.
    pub next_ordinal: u64,
    /// The next family a lazy instantiation takes.
    pub next_family: u64,
    /// The census's persisted accounting (S7).
    pub accounting: AccountingCertificate,
    pub population: super::DataOnlyCertificate,
}

/// Whether the execution is parked or ended.
#[derive(Clone, Debug)]
pub enum RootStatus {
    Parked,
    /// The run ended. A terminal root keeps the result and the session
    /// carry it ended with; a session's next cell opens the carry (S8).
    Ended(EndedRecord),
}

/// A run's end as its root holds it.
#[derive(Clone, Debug)]
pub enum EndedRecord {
    Returned(FrozenRef),
    Finished(FrozenRef),
    Failed(FrozenRef),
    /// Ended by a bound or a cancel: what it states is the host record's.
    Stopped,
}

/// The meters a root saves exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedMeters {
    pub charged: u64,
    pub waits_issued: u64,
    pub next_join: u64,
    pub live_tasks: u32,
}

/// Kind 1000: one execution's complete root.
#[derive(Clone, Debug)]
pub struct CheckpointRoot {
    pub kernel: u32,
    pub document: DocumentId,
    /// The ReleaseManifest node the run pins; checked before executing,
    /// never decoded by the parent beyond this header.
    pub release: PageId,
    /// The OwnerBinding node of the execution.
    pub owner: PageId,
    pub status: RootStatus,
    pub heap: PageRef,
    /// [`TaskRecord`](super::TaskRecord)s by task number; `main` is task 0.
    pub tasks: KeyedRoot,
    /// Task numbers in the order they run (`K-TASK-003`).
    pub ready: SequenceRoot,
    /// [`WaitRecord`]s by wait number: every wait handed out and not yet
    /// run with.
    pub waits: KeyedRoot,
    /// [`ListJoinRecord`]s by join number.
    pub joins: KeyedRoot,
    /// Wait numbers withdrawn after being handed out (`K-MACH-004`).
    pub withdrawn: KeyedRoot,
    /// Withdrawn waits the next park reports, in order (`K-MACH-003`).
    pub unreported: SequenceRoot,
    /// Effects the run performs as reads (`K-EFF-012`), by name.
    pub reads: KeyedRoot,
    /// A session cell's bindings: [`BindingRecord`]s by name.
    pub session: KeyedRoot,
    pub meters: SavedMeters,
}

/// A waiting join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinerRecord {
    Single(TaskId),
    List(u64),
}

/// Where a task is in its life.
#[derive(Clone, Debug)]
pub enum TaskStateRecord {
    Ready,
    Resuming(IncomingRecord),
    /// In a `perform` or a `sleep`: the wait's number.
    Waiting(u64),
    Joining(TaskId),
    JoiningMany(u64),
    Ended(EndedTask),
}

/// What a ready task's pending statement continues with.
#[derive(Clone, Debug)]
pub enum IncomingRecord {
    Value(Value),
    Raise(Value),
    Joined(TaskId),
}

/// How a task ended. Its result counts toward memory only while a handle
/// reaches the task (S7).
#[derive(Clone, Debug)]
pub enum EndedTask {
    Returned(Value),
    Raised(Value),
}

#[derive(Clone, Debug)]
pub struct BindingRecord {
    pub name: Name,
    pub declared: Site,
    pub value: BoundRecord,
}

#[derive(Clone, Debug)]
pub enum BoundRecord {
    Value(Value),
    /// A variable a closure shares.
    Shared(EntityId),
}

#[derive(Clone, Debug)]
pub struct LoopRecord {
    pub site: Site,
    pub started: u64,
    /// A `for`'s position; a map or set position is its
    /// [`TableCursor`](super::TableCursor), not a count.
    pub position: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct FinallyRecord {
    pub site: Site,
    pub entered: EnteredRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnteredRecord {
    Normal,
    Throw,
    Return,
    Break,
    Continue,
}

/// One wait handed out. Its request is the admission the parent froze;
/// the record names it by effect identity and never copies its arguments.
#[derive(Clone, Debug)]
pub struct WaitRecord {
    pub wait: u64,
    pub task: TaskId,
    pub identity: EffectIdentity,
    pub kind: WaitKindRecord,
    pub state: WaitStateRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKindRecord {
    Effect,
    Sleep,
}

#[derive(Clone, Debug)]
pub enum WaitStateRecord {
    Admitted,
    Committed(CommittedOutcomeRecord),
}

/// A committed outcome: a frozen value the task will receive as a fresh
/// family (S8), or a sleep's elapse.
#[derive(Clone, Debug)]
pub enum CommittedOutcomeRecord {
    Completed(FrozenRef),
    Failed(FrozenRef),
    Elapsed,
}

#[derive(Clone)]
pub struct ListJoinRecord {
    pub joiner: TaskId,
    pub mode: JoinMode,
    pub members: super::tree::Tree<TaskId>,
}
