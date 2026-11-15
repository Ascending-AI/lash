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

use lash_kernel_doc::heap::{HeapFault, LogicalBytes, PageId, PageRef};

use lash_kernel_value::names::DocumentId;

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

/// Whether a run meters its charge. Chosen at start and retained across resume.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Charge {
    /// Exact accounting and the configured charge bound.
    #[default]
    Exact,
    /// No charge accounting or charge bound.
    Off,
}

/// Whether a run meters its memory. Chosen at start and retained across
/// resume: a root whose saved memory limit is none is unmetered.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMeter {
    /// The conservative meter and the configured memory bound.
    #[default]
    On,
    /// No memory meter, reservations or bound.
    Off,
}

/// The meters a root saves exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedMeters {
    /// None is Off; a present meter is Exact, including a meter of zero.
    pub charged: Option<u64>,
    pub waits_issued: u64,
    pub next_join: u64,
    pub live_tasks: u32,
}

impl SavedMeters {
    /// The run's saved charge policy, retained even before its first charge.
    pub const fn charge(&self) -> Charge {
        if self.charged.is_some() {
            Charge::Exact
        } else {
            Charge::Off
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellTargetKind {
    Main,
    Entry,
}

/// The limits a run is held to now: those it started with, as its host's
/// limiters have grown them since (FIG-6768). A resume continues with them,
/// whatever bounds the machine that opens it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedLimits {
    /// The memory bound in bytes; none when memory is unmetered.
    pub memory: Option<u64>,
    /// The charge bound in charge units, held whether or not charge is
    /// metered ([`SavedMeters::charge`]).
    pub charge: u64,
}

/// Kind 1000: one execution's complete root.
#[derive(Clone, Debug)]
pub struct CheckpointRoot {
    pub limits: SavedLimits,
    pub kernel: u32,
    pub document: DocumentId,
    pub link: lash_kernel_value::content_ids::LinkId,
    /// Ordinary stored definitions selected by the document. Build helpers
    /// are carried by the binary and never staged in this closure.
    pub closure: PageId,
    /// The immutable build helper image this root executed with.
    pub helper_library: lash_kernel_value::HelperLibraryId,
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
    /// Operation uses the run performs as reads (`K-EFF-012`).
    pub reads: KeyedRoot,
    /// A session cell's bindings: [`BindingRecord`]s by name.
    pub session: KeyedRoot,
    /// The bindings the host projected into a session cell's `main`, which
    /// its carry leaves out: the host's, not the session's.
    /// Admitted by `CellStart` within [`Self::HOST_METADATA_LIMIT`].
    pub host: Vec<lash_kernel_value::names::Name>,
    pub meters: SavedMeters,
}

impl CheckpointRoot {
    /// The encoded host-name list's byte budget (S1, FIG-6499).
    ///
    /// The 4096-byte root reserves 865 token bytes for its fixed fields:
    /// struct header (2), kernel (6), link (34), document (34), closure/owner (66), helper library (34),
    /// limits (24), target (3), nine full tree roots (9 * 58), heap (33), largest ended
    /// status (66), and meters (41). All integers use their maximum widths.
    /// Another 256 bytes cover the envelope: each of at most 13 distinct
    /// child references adds at most one byte when moved to the child table,
    /// plus the magic, kind, child count and body length.
    ///
    /// Names include their UTF-8 bytes and token headers, as does the list.
    /// Even empty names cost two bytes, so this bounds their count as well
    /// as each name. No guest allocation or park can grow the list.
    pub const HOST_METADATA_LIMIT: usize =
        4096 - (2 + 6 + 34 + 34 + 66 + 34 + 24 + 3 + 9 * 58 + 33 + 66 + 41) - 256;

    /// The canonical host-name list's size, without allocating its encoding.
    /// Overflow saturates so admission refuses it.
    pub fn host_metadata_bytes<'a>(names: impl IntoIterator<Item = &'a str>) -> usize {
        use lash_kernel_value::tokens::varint_len;
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
    /// root it read through its page source. Its closure and owner are
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
        let mut root: Self = lash_kernel_value::tokens::decode_body(&answer.tokens)
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
