//! S5: an activation's private staging spool and the candidate manifest it
//! hands a store (FIG-6195, `docs/design/persistent-heap-seams.md`).
//!
//! Declarations only. The spool is node-local temporary storage owned by
//! the parent for one activation: content-addressed, resident in the
//! parent's memory up to a bound and disk-indexed past it, quota limited,
//! with bounded buffers. It is never a checkpoint, never admits an
//! effect and need not survive the parent. A sealed page may name only
//! committed pages the activation's root lease protects or pages sealed
//! into this spool before it, so the staged graph is acyclic by
//! construction.
//!
//! At a park the spool walks the candidate root's staged DAG only, stopping
//! at protected committed references, and external-sorts what it reaches
//! into a [`CandidateManifest`]: one stream in global digest order, the
//! store's blob lock order. A spool may be shared by the activations of one
//! session until the session's state is published: its owner names the
//! roots it still needs ([`StageSpool::retain`]), and a settled candidate
//! removes only its own pages.

use std::sync::Arc;

use super::commit::CommitId;
use super::node::{EncodedNode, NodeId};

/// One activation's stage lease: it scopes the spool and every stage
/// capability; reset or expiry ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StageLeaseId(pub u64);

/// What a spool holds: its staged pages and their bytes, and the quota
/// those bytes share with each entry's manifest scratch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageUsage {
    pub pages: u64,
    pub bytes: u64,
    pub quota_bytes: u64,
}

/// Why a spool refused. Every variant is a host outcome; none is the
/// guest's memory bound.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StageFault {
    #[error("the staging quota of {quota} bytes is exhausted")]
    Quota { quota: u64 },
    #[error("staged page {0} does not hash to its identity")]
    HashMismatch(NodeId),
    /// A sealed page names a child that is neither protected nor staged.
    #[error("staged page {page} names {child}, which is neither protected nor staged")]
    Unauthorized { page: NodeId, child: NodeId },
    #[error("page {0} is not staged")]
    Missing(NodeId),
    /// The stage lease ended: reset, expiry or parent shutdown.
    #[error("the stage lease ended")]
    Ended,
    #[error("staging storage failed: {0}")]
    Io(String),
}

/// One entry of a manifest, in global digest order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestEntry {
    /// A committed node the candidate references directly: the store
    /// key-share locks it and checks it exists, never walking its closure.
    External(NodeId),
    /// A node the candidate supplies: inserted before any edge.
    Fresh(EncodedNode),
}

/// What a manifest states before it is streamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestSummary {
    /// The candidate's root node (a CheckpointRoot, or the ExecRoot over
    /// it).
    pub root: NodeId,
    /// The committed root the candidate was built against.
    pub base: Option<NodeId>,
    /// The commit identity the candidate publishes under: with `root`, how
    /// an ambiguous publication is resolved against the execution row.
    pub commit: CommitId,
    pub fresh_nodes: u64,
    pub fresh_bytes: u64,
    pub fresh_edges: u64,
    pub external: u64,
}

/// A candidate's bounded-memory stream, restartable by cursor: the store
/// reads it once to lock and insert nodes, and again on a retried
/// transaction. Its edges come back from the install's own [`EdgeLog`]
/// (FIG-6596): the store never reads a body twice.
pub trait CandidateManifest: Send {
    fn summary(&self) -> ManifestSummary;
    /// Up to `limit` entries after `after`, ascending by digest.
    ///
    /// # Errors
    ///
    /// [`StageFault`]: the publication fails as infrastructure and the
    /// candidate stays staged.
    fn entries(
        &mut self,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<ManifestEntry>, StageFault>;
    /// `len` bytes from `offset` of the node `id` it supplies: how an
    /// install reads a checked node's child table again for its edges,
    /// never the node's body.
    ///
    /// # Errors
    ///
    /// [`StageFault`], as for [`CandidateManifest::entries`].
    fn fresh_range(&mut self, id: &NodeId, offset: u64, len: usize) -> Result<Vec<u8>, StageFault>;
    /// An empty edge log for one install of this candidate, in place of
    /// any earlier install's.
    ///
    /// # Errors
    ///
    /// [`StageFault`], as for [`CandidateManifest::entries`].
    fn edge_log(&mut self) -> Result<Box<dyn EdgeLog>, StageFault>;
}

/// An install's private scratch (FIG-6596): the store writes it while it
/// checks a candidate's nodes and reads it back for their edges. The
/// install authenticates every record it reads back under a key only it
/// holds, so a log is never trusted on its own, nor by another install.
pub trait EdgeLog: Send {
    /// Appends `bytes` at the log's end.
    ///
    /// # Errors
    ///
    /// [`StageFault`]: the publication fails as infrastructure.
    fn append(&mut self, bytes: &[u8]) -> Result<(), StageFault>;
    /// Fills `out` from the log at `offset`.
    ///
    /// # Errors
    ///
    /// [`StageFault`], also when the log ends first.
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), StageFault>;
}

/// What an install's edge log holds for one fresh page with children: one
/// fixed-size record (FIG-6596), which a spool's scratch reserves.
pub const EDGE_RECORD_BYTES: u64 = 53;

/// The edge log of a manifest that holds its candidate in memory.
#[derive(Debug, Default)]
pub struct MemoryEdgeLog(pub Vec<u8>);

impl EdgeLog for MemoryEdgeLog {
    fn append(&mut self, bytes: &[u8]) -> Result<(), StageFault> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), StageFault> {
        let bytes = usize::try_from(offset)
            .ok()
            .and_then(|start| self.0.get(start..start.checked_add(out.len())?))
            .ok_or_else(|| StageFault::Io("the edge log ended early".to_owned()))?;
        out.copy_from_slice(bytes);
        Ok(())
    }
}

/// [`CandidateManifest::fresh_range`] of `node`, held in memory.
///
/// # Errors
///
/// [`StageFault::Io`] when the range runs past the node.
pub fn node_range(node: &EncodedNode, offset: u64, len: usize) -> Result<Vec<u8>, StageFault> {
    usize::try_from(offset)
        .ok()
        .and_then(|start| node.bytes.get(start..start.checked_add(len)?))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| StageFault::Io(format!("page {} has no such range", node.id)))
}

/// One activation's spool.
///
/// Implemented by the parent's page service (layer hp-paging, FIG-6190);
/// the store (hp-publish, FIG-6191) consumes its manifests.
pub trait StageSpool: Send {
    fn lease(&self) -> StageLeaseId;
    /// Seals `page` into the spool after checking its digest and that each
    /// child is `protected` or already staged. Sealing an identical page
    /// twice is one page.
    fn seal(
        &mut self,
        page: EncodedNode,
        protected: &dyn Fn(&NodeId) -> bool,
    ) -> Result<(), StageFault>;
    /// Reads a page this activation staged.
    fn read(&self, page: &NodeId) -> Result<Arc<[u8]>, StageFault>;
    /// Whether `page` is staged here. A staged page's child that is not
    /// staged was a protected committed page when the page was sealed: its
    /// provenance, which decides the authority a reader is granted for it.
    fn contains(&self, page: &NodeId) -> bool;
    /// Keeps the staged pages `roots` reach, and drops every other staged
    /// page: garbage from intermediate seals and dirty eviction.
    fn retain(&mut self, roots: &[NodeId]) -> Result<(), StageFault>;
    /// Walks `root`'s staged DAG and builds its manifest. The spool keeps
    /// the candidate until [`StageSpool::settle`].
    fn candidate(
        &mut self,
        root: NodeId,
        base: Option<NodeId>,
        commit: CommitId,
    ) -> Result<Box<dyn CandidateManifest>, StageFault>;
    /// The publication committed authoritatively: the durable root owns the
    /// candidate's pages, and the spool deletes them and nothing else. An
    /// ambiguous publication never settles: it is resolved against the
    /// execution row first.
    fn settle(&mut self, commit: CommitId) -> Result<(), StageFault>;
    /// The candidate under `commit` will not be published (its commit was
    /// abandoned before it was made): the spool forgets it and keeps every
    /// page it staged, so another candidate can be built.
    fn withdraw(&mut self, commit: CommitId) -> Result<(), StageFault>;
    /// Drops everything the activation staged.
    fn discard(self: Box<Self>);
    fn usage(&self) -> StageUsage;
}
