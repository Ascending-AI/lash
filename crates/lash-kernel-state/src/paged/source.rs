//! S3: the synchronous page port an embedder implements, and the semantic
//! page kinds.
//!
//! The kernel does no I/O (`K-MACH-001`): a page miss blocks inside this
//! port with the statement's and the native call's Rust stack intact, and
//! returns when the page arrives. Nothing is replayed, restarted or
//! re-charged. A page fault costs host work, never guest charge.
//!
//! The kernel sees logical page bodies only: graph tokens whose child
//! references are full page digests. The embedder frames a body in the
//! generic node envelope, computes its digest, sorts its child table and
//! authenticates what it reads; the kernel never hashes.

use std::sync::Arc;

use lash_kernel_doc::heap::{HeapFault, PageRef};

/// A semantic page kind: a generic envelope kind number in the range the
/// node registry reserved for a complete object graph, 1000 to 1999.
/// Every semantic kind is direct-bodied (no inline/stream wrapper, no
/// placement of large tokens) and node-bounded, except
/// [`PageKind::CHECKPOINT_ROOT`], which is root-bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageKind(pub u32);

impl PageKind {
    /// A [`CheckpointRoot`](super::CheckpointRoot) (root-bounded, 4096 bytes).
    pub const CHECKPOINT_ROOT: Self = Self(1000);
    /// An order-statistic sequence index: child pages with their counts
    /// and summaries.
    pub const SEQUENCE_INDEX: Self = Self(1001);
    /// A sequence leaf: bounded cells of values or records.
    pub const SEQUENCE_LEAF: Self = Self(1002);
    /// An ordered-key index: separator keys and child pages with summaries.
    pub const KEYED_INDEX: Self = Self(1003);
    /// An ordered-key leaf: bounded rows.
    pub const KEYED_LEAF: Self = Self(1004);
    /// An immutable payload leaf: text (ending on a scalar boundary),
    /// bytes or integer digits.
    pub const PAYLOAD_LEAF: Self = Self(1005);
    /// An immutable payload index with byte, character and UTF-16 counts.
    pub const PAYLOAD_INDEX: Self = Self(1006);
    /// A frozen value's root: its structure pages and preorder entity
    /// counts (S8).
    pub const FROZEN_ROOT: Self = Self(1007);
    /// A session root: carried heap, bindings and functions (S8).
    pub const SESSION_ROOT: Self = Self(1008);
    /// The first number past the semantic kinds a format may add.
    pub const END: u32 = 2000;
}

/// Index fanout of every semantic tree (start value, `D-CAP`).
pub const INDEX_FANOUT: u64 = 32;
/// Cells per sequence leaf (start value, `D-CAP`).
pub const SEQUENCE_LEAF_CELLS: u64 = 64;
/// Rows per keyed leaf (start value, `D-CAP`).
pub const KEYED_LEAF_ROWS: u64 = 64;
/// A leaf stops packing at this many envelope bytes, whichever of its
/// count bound comes first (start value, `D-CAP`). The format's absolute
/// node bound, 65 536 bytes, is never exceeded.
pub const PAGE_TARGET_BYTES: u64 = 8_192;
/// An object body of at most this many token bytes lives inline in its
/// directory row; a larger one is paged under the same schema.
pub const INLINE_BODY_MAX: u64 = 256;
/// The most pages one read request names (start value, `D-CAP`).
pub const READ_BATCH_PAGES: usize = 64;
/// The most answer bytes one read request returns (start value, `D-CAP`).
pub const READ_BATCH_BYTES: u64 = 4 << 20;

/// A page the heap seals: its kind, its logical body and the pages it
/// references with the grants that authorize them.
#[derive(Clone, Debug)]
pub struct PageBody {
    pub kind: PageKind,
    /// Graph tokens; a child reference is the child's full digest.
    pub tokens: Vec<u8>,
    /// Every page the tokens reference, each once, with its grant: a
    /// committed page the activation read, or a page it sealed earlier.
    pub children: Vec<PageRef>,
}

/// A page as the embedder hands it back, authenticated.
#[derive(Clone, Debug)]
pub struct PageAnswer {
    pub page: PageRef,
    pub kind: PageKind,
    /// The logical body (child references as full digests).
    pub tokens: Arc<[u8]>,
    /// Every child the envelope declares, ascending by digest, with a grant
    /// derived from this page's authority.
    pub children: Vec<PageRef>,
}

/// The embedder's synchronous page port: one per activation, owned by the
/// heap from open to drop.
///
/// Implemented by the worker over its host exchange and the parent's page
/// service, and in process over a local store (layer hp-paging, FIG-6190).
pub trait PageSource {
    /// Reads `pages`, at most [`READ_BATCH_PAGES`], answering in request
    /// order. Every answer was authenticated against its digest and kind.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Missing`], [`HeapFault::Corrupt`],
    /// [`HeapFault::Unauthorized`], [`HeapFault::LeaseLost`],
    /// [`HeapFault::Exhausted`] or [`HeapFault::Transport`]: the activation
    /// ends and resumes from its last committed root.
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault>;

    /// Seals `page`: frames it, computes its identity and keeps its bytes
    /// readable through [`PageSource::read`] for the rest of the activation
    /// (buffered or spilled to the activation's private staging). Sealing
    /// publishes nothing and admits no effect.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Exhausted`] for staging quota, [`HeapFault::Unauthorized`]
    /// for a child no grant authorizes, and transport faults.
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault>;
}
