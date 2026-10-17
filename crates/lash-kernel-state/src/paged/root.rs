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
//! register window are derived caches, never persisted. A frame standing
//! at an action nested in its statement's expression keeps the operand
//! values its statement already evaluated by their sites, in its pending
//! continuation (`K-EVAL-008`), never as registers.
//!
//! These records are the decoded shapes of their pages. Their canonical
//! token encodings are layer hp-heap's (FIG-6188), in this crate; the
//! envelope around them is the generic node store's.

use super::{NameRef, SiteRef};
use lash_kernel_doc::heap::{EntityId, HeapFault, LogicalBytes, PageId, PageRef, PageValue};
use lash_kernel_doc::{DocumentId, JoinMode, TaskId};

use super::census::AccountingCertificate;
use super::family::FrozenRef;

/// The root of an ordered-key tree: lookup by a canonical key. An empty
/// tree has no page and no summary; a nonempty one has both, and its
/// summary is the root page's authenticated one (FIG-6197 F3). Neither
/// construction nor decoding admits a page-less tree with rows, or a
/// paged one without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyedRoot {
    Empty,
    NonEmpty {
        page: PageRef,
        /// At least one.
        len: u64,
        /// The rows' aggregate logical cost (S7 summaries).
        cost: LogicalBytes,
    },
}

/// The root of an order-statistic sequence tree: access by position. Its
/// occupancy is checked as a [`KeyedRoot`]'s is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceRoot {
    Empty,
    NonEmpty {
        page: PageRef,
        len: u64,
        cost: LogicalBytes,
    },
}

macro_rules! tree_root {
    ($root:ident) => {
        impl $root {
            /// The descriptor of a tree of `len` rows costing `cost` under
            /// `page`.
            ///
            /// # Errors
            ///
            /// [`HeapFault::Malformed`] for rows without a sealed page, or a
            /// page without rows.
            pub fn of(
                page: Option<PageRef>,
                len: u64,
                cost: LogicalBytes,
            ) -> Result<Self, HeapFault> {
                match (page, len) {
                    (None, 0) if cost.0 == 0 => Ok(Self::Empty),
                    (Some(page), 1..) => Ok(Self::NonEmpty { page, len, cost }),
                    _ => Err(HeapFault::Malformed(format!(
                        "a tree root of {len} rows costing {} with{} a page",
                        cost.0,
                        if page.is_some() { "" } else { "out" }
                    ))),
                }
            }

            #[must_use]
            pub fn page(&self) -> Option<PageRef> {
                match self {
                    Self::Empty => None,
                    Self::NonEmpty { page, .. } => Some(*page),
                }
            }

            /// The root page, for a reader that rewrites its grant.
            pub fn page_mut(&mut self) -> Option<&mut PageRef> {
                match self {
                    Self::Empty => None,
                    Self::NonEmpty { page, .. } => Some(page),
                }
            }

            #[must_use]
            pub fn len(&self) -> u64 {
                match self {
                    Self::Empty => 0,
                    Self::NonEmpty { len, .. } => *len,
                }
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                matches!(self, Self::Empty)
            }

            #[must_use]
            pub fn cost(&self) -> LogicalBytes {
                match self {
                    Self::Empty => LogicalBytes(0),
                    Self::NonEmpty { cost, .. } => *cost,
                }
            }
        }
    };
}
tree_root!(KeyedRoot);
tree_root!(SequenceRoot);

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
    /// Live-size estimate at the last collection (or first carry). Growth
    /// above it pays for the next carry's collection, across cold opens.
    pub collection_live: Option<LogicalBytes>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellTargetKind {
    Main,
    Entry,
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
    pub target: CellTargetKind,
    pub functions: KeyedRoot,
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
    /// The bindings the host projected into a session cell's `main`, which
    /// its carry leaves out: the host's, not the session's.
    /// Admitted by `CellStart` within [`Self::HOST_METADATA_LIMIT`].
    pub host: Vec<lash_kernel_doc::Name>,
    pub meters: SavedMeters,
}

impl CheckpointRoot {
    /// The encoded host-name list's byte budget (S1, FIG-6499).
    ///
    /// The 4096-byte root reserves 773 token bytes for its fixed fields:
    /// struct header (2), kernel (6), document (34), release/owner (66),
    /// target (3), nine full tree roots (9 * 58), heap (33), largest ended
    /// status (66), and meters (41). All integers use their maximum widths.
    /// Another 256 bytes cover the envelope: each of at most 13 distinct
    /// child references adds at most one byte when moved to the child table,
    /// plus the magic, kind, child count and body length.
    ///
    /// Names include their UTF-8 bytes and token headers, as does the list.
    /// Even empty names cost two bytes, so this bounds their count as well
    /// as each name. No guest allocation or park can grow the list.
    pub const HOST_METADATA_LIMIT: usize =
        4096 - (2 + 6 + 34 + 66 + 3 + 9 * 58 + 33 + 66 + 41) - 256;

    /// The canonical host-name list's size, without allocating its encoding.
    /// Overflow saturates so admission refuses it.
    pub fn host_metadata_bytes<'a>(names: impl IntoIterator<Item = &'a str>) -> usize {
        use lash_kernel_doc::graph_tokens::varint_len;
        let mut count = 0_u64;
        let mut bytes = 0_usize;
        for name in names {
            count = count.saturating_add(1);
            bytes = bytes.saturating_add(1 + varint_len(name.len() as u64));
            bytes = bytes.saturating_add(name.len());
        }
        bytes.saturating_add(1 + varint_len(count))
    }

    /// The root a checkpoint-root page holds, each page it names granted
    /// what the page's answer grants that child: how an embedder opens a
    /// root it read through its page source. Its release and owner are
    /// host nodes, named by identity and never read through the heap.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Corrupt`] for a page that is not a checkpoint root or
    /// names a page its envelope does not declare.
    pub fn of_answer(answer: &super::PageAnswer) -> Result<Self, HeapFault> {
        let corrupt = |reason: String| HeapFault::Corrupt {
            page: answer.page.id,
            reason,
        };
        if answer.kind != super::PageKind::CHECKPOINT_ROOT {
            return Err(corrupt("not a checkpoint root".into()));
        }
        let mut root: Self = lash_kernel_doc::graph_tokens::decode_body(&answer.tokens)
            .map_err(|error| corrupt(error.to_string()))?;
        let grant = |page: &mut PageRef| super::tree::grant(page, &answer.children);
        grant(&mut root.heap)?;
        for tree in [
            &mut root.functions,
            &mut root.tasks,
            &mut root.waits,
            &mut root.joins,
            &mut root.withdrawn,
            &mut root.reads,
            &mut root.session,
        ] {
            if let Some(page) = tree.page_mut() {
                grant(page)?;
            }
        }
        for sequence in [&mut root.ready, &mut root.unreported] {
            if let Some(page) = sequence.page_mut() {
                grant(page)?;
            }
        }
        if let RootStatus::Ended(
            EndedRecord::Returned(frozen)
            | EndedRecord::Finished(frozen)
            | EndedRecord::Failed(frozen),
        ) = &mut root.status
        {
            grant(&mut frozen.root)?;
        }
        Ok(root)
    }
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
    /// Ready, with what its pending action continues with. `requeue`: the
    /// action waited `then yield` and its outcome is in, but the task has
    /// not yet taken the one more turn of the queue it takes before it
    /// continues (`K-TASK-005`): it stands *delivered*.
    Resuming {
        incoming: IncomingRecord,
        requeue: bool,
    },
    /// In a `perform` or a `sleep`: the wait's number.
    Waiting(u64),
    Joining(TaskId),
    JoiningMany(u64),
    Ended(EndedTask),
}

/// What a ready task's pending statement continues with.
#[derive(Clone, Debug)]
pub enum IncomingRecord {
    Value(PageValue),
    Raise(PageValue),
    Joined(TaskId),
    /// A frozen committed outcome waiting for semantic delivery.
    Outcome(crate::WaitId),
}

/// How a task ended. Its result counts toward memory only while a handle
/// reaches the task (S7).
#[derive(Clone, Debug)]
pub enum EndedTask {
    Returned(PageValue),
    Raised(PageValue),
}

#[derive(Clone, Debug)]
pub struct BindingRecord {
    pub name: NameRef,
    pub declared: SiteRef,
    pub value: BoundRecord,
}

/// A saved function a session holds (`K-SES-004`), in the session's
/// functions tree under its declared name: its code and what its dialect
/// wrote of it as the text `meta`, the release its code calls into, and
/// the frozen captures each call instantiates as a fresh family.
#[derive(Clone, Debug)]
pub struct FunctionRecord {
    /// The name the function is declared under.
    pub name: NameRef,
    /// A bytes value: the saved function's canonical description, a token
    /// body that holds its document's token body, which its dialect reads
    /// back to declare it in a later cell's document.
    pub meta: PageValue,
    pub release: PageRef,
    /// Each variable the function reads, by name.
    pub captures: std::collections::BTreeMap<lash_kernel_doc::Name, FrozenRef>,
    /// The token's frozen data before and after the function, when a token
    /// holds it.
    pub before: Vec<FrozenRef>,
    pub after: Vec<FrozenRef>,
}

#[derive(Clone, Debug)]
pub enum BoundRecord {
    Value(PageValue),
    /// A variable a closure shares.
    Shared(EntityId),
}

#[derive(Clone, Debug)]
pub struct LoopRecord {
    pub site: SiteRef,
    pub started: u64,
    /// Where a `for`'s header stands; a `while` is always in its body.
    pub header: HeaderRecord,
    /// What a `for` walks and where; `None` for a `while` and for a `for`
    /// still opening its value.
    pub cursor: Option<CursorRecord>,
}

/// Where a `for`'s header stands (`K-ITER-005`): in the body, or in the
/// protocol hook whose frame runs above the loop's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRecord {
    Body,
    /// `open` takes the iterated value to what the loop walks.
    Open,
    /// `next` steps the driver.
    Pull,
    /// `check` runs with a stepped item held.
    Check,
    /// `check` runs at exhaustion, no item held.
    Exhausted,
    /// `item` takes the held item to the value bound.
    Item,
}

/// A `for`'s cursor. What it walks is the loop's held value; a stepped item
/// awaiting its hook is held after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorRecord {
    pub kind: CursorKind,
    /// A list's or a tuple's next index; a map's or a set's last visited
    /// [`TableCursor`](super::TableCursor), `u64::MAX` before the first; a
    /// text's next code point, by index; 0 for a driver.
    pub position: u64,
    /// A map's or a set's size when the loop opened it and the items the
    /// protocol's `check` still allows, when it has one.
    pub guard: Option<GuardRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    List,
    Tuple,
    /// A map's keys or a set's members.
    Table,
    /// A text's code points.
    Text,
    /// A value the protocol's `next` steps.
    Driver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuardRecord {
    pub size: u64,
    pub remaining: u64,
}

#[derive(Clone, Debug)]
pub struct FinallyRecord {
    pub site: SiteRef,
    pub entered: EnteredRecord,
}

#[derive(Clone, Debug)]
pub enum EnteredRecord {
    /// The return site supplies the policy after cleanup has unwound.
    ReturnVia(Box<SiteRef>),
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
    pub task: TaskId,
    pub kind: WaitKindRecord,
    pub phase: WaitPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKindRecord {
    Effect,
    Sleep,
}

#[derive(Clone, Debug)]
pub enum WaitPhase {
    /// Frozen request intent, not yet reported or admitted by a park.
    Issued {
        request: FrozenRef,
    },
    Admitted,
    Committed(CommittedOutcomeRecord),
}

/// A committed outcome: a frozen value the task will receive as a fresh
/// family (S8), or a sleep's elapse.
#[derive(Clone, Debug)]
pub enum CommittedOutcomeRecord {
    Completed(FrozenRef),
    Failed(FrozenRef),
    /// A Completed input rejected by the perform type; no inbound work charge.
    Rejected(FrozenRef),
    Elapsed,
}

#[derive(Clone)]
pub struct ListJoinRecord {
    pub joiner: TaskId,
    pub mode: JoinMode,
    pub members: super::tree::Tree<TaskId>,
}
