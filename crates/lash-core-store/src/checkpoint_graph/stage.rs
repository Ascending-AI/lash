//! S5: an activation's private staging spool and the candidate manifest it
//! hands a store (FIG-6195, `docs/design/persistent-heap-seams.md`).
//!
//! Declarations only. The spool is node-local temporary storage owned by
//! the parent for one activation: content-addressed, disk-indexed, quota
//! limited, with bounded buffers. It is never a checkpoint, never admits an
//! effect and need not survive the parent. A sealed page may name only
//! committed pages the activation's root lease protects or pages sealed
//! into this spool before it, so the staged graph is acyclic by
//! construction.
//!
//! At a park the spool walks the candidate root's staged DAG only, stopping
//! at protected committed references, drops staged pages the candidate does
//! not reach, and external-sorts the rest into a [`CandidateManifest`]: one
//! stream in global digest order, the store's blob lock order.

use std::sync::Arc;

use super::commit::CommitId;
use super::node::{EncodedNode, NodeId};

/// One activation's stage lease: it scopes the spool and every stage
/// capability; reset or expiry ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StageLeaseId(pub u64);

/// What a spool holds, against its quota.
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
/// reads it once to lock and insert nodes and again to insert edges, and
/// again on a retried transaction.
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
    /// Walks `root`'s staged DAG and builds its manifest; staged pages it
    /// does not reach are dropped. The spool keeps the candidate until
    /// [`StageSpool::settle`].
    fn candidate(
        &mut self,
        root: NodeId,
        base: Option<NodeId>,
        commit: CommitId,
    ) -> Result<Box<dyn CandidateManifest>, StageFault>;
    /// The publication's outcome is authoritative: committed (the durable
    /// root owns the pages) or refused (the activation is discarded). Only
    /// then may the spool delete the candidate. An ambiguous publication
    /// never settles: it is resolved against the execution row first.
    fn settle(&mut self, commit: CommitId) -> Result<(), StageFault>;
    /// Drops everything the activation staged.
    fn discard(self: Box<Self>);
    fn usage(&self) -> StageUsage;
}
