//! The parent's page service: demand reads and private staging for one
//! activation on the persistent heap (seams S3 and S5 of
//! `docs/design/persistent-heap-seams.md`, FIG-6190).
//!
//! A worker's heap faults pages in and seals pages out through its host
//! exchange ([`lash_vm_protocol::WorkerMessage::Pages`]); the parent answers
//! each exchange here, in its own store and spool lane, and never checks out
//! another worker to do it. The service holds, per activation, the root
//! lease the store issued (S6), the capability key, the activation epoch
//! and the spool: never a set of visited pages.
//!
//! - A read is authorized by capability alone ([`capability`]): the root's,
//!   or one issued for a child of a page read or sealed earlier. Committed
//!   pages are read in one bounded batch from the store under the lease;
//!   staged pages from the spool. Every page is re-hashed and its envelope
//!   checked before it crosses, and its answer carries a capability for
//!   exactly the children its envelope declares.
//! - A seal is checked the same way and staged in the spool ([`spool`]): its
//!   children must be committed pages a capability authorizes or pages the
//!   activation staged before. Staging publishes nothing.
//! - Every refusal is infrastructure ([`PageRefusal`]), and the first one
//!   ends the activation: every later exchange is refused the same way.
//!
//! [`LocalPages`] is the same port for an embedder with no parent: the
//! kernel's [`PageSource`] over committed pages it holds itself and a spool.

pub mod capability;
pub mod local;
pub mod spool;
#[cfg(test)]
mod spool_laws;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core_store::checkpoint_graph::node::{SEMANTIC_KINDS, direct_logical, frame_direct};
use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, ManifestSummary, StageFault, StageSpool, StageUsage,
};
use lash_core_store::checkpoint_graph::{
    CommitId, DecodeLimits, EncodedNode, GraphRefusal, MAX_ROOT_BYTES, NodeId, NodeKind,
    check_envelope,
};
use lash_durable::DurableError;
use lash_durable::domain::pages::{PageStore, RootLease, StoredPage};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageKind};
use lash_vm_protocol::page::{
    PageAuthority, PageCapability, PageEnvelope, PageRefusal, PageReply, PageRequest, ReadPage,
};

pub use capability::CapabilityKey;
pub use local::{CommittedPages, LocalPages};
pub use spool::{DiskSpool, SpoolRoot};

/// The store reads a page service makes under its root lease: the slice of
/// [`PageStore`] it consumes, so a layer law can stand a fake store behind
/// it.
#[async_trait::async_trait]
pub trait LeasedReads: Send + Sync {
    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError>;
    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError>;
}

#[async_trait::async_trait]
impl<S: PageStore + ?Sized> LeasedReads for S {
    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        PageStore::read_pages(self, lease, pages).await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        PageStore::renew_root_lease(self, lease).await
    }
}

/// The parent's answer to a worker's page exchanges, one activation's
/// worth.
#[async_trait::async_trait]
pub trait PageExchange: Send {
    /// Answers one exchange. A refusal ends the activation as
    /// infrastructure.
    async fn serve(&mut self, request: PageRequest) -> PageReply;
}

/// How much one exchange may carry each way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageLimits {
    /// The most pages a request names ([`lash_vm_protocol::PAGE_BATCH_PAGES`]).
    pub batch_pages: usize,
    /// The most envelope bytes a request seals
    /// ([`lash_vm_protocol::PAGE_BATCH_BYTES`]).
    pub batch_bytes: u64,
    /// The most envelope bytes one read reply carries: the batch bound,
    /// within what one frame holds. A reply that would pass it answers a
    /// prefix, and the worker asks for the rest.
    pub reply_bytes: u64,
    /// The most MessagePack values one read reply is estimated to hold,
    /// within the frame's node bound.
    pub reply_nodes: u64,
}

impl PageLimits {
    /// The batch bounds within `frame`'s decode limits.
    #[must_use]
    pub fn within(frame: lash_vm_protocol::DecodeLimits) -> Self {
        Self {
            batch_pages: lash_vm_protocol::PAGE_BATCH_PAGES,
            batch_bytes: lash_vm_protocol::PAGE_BATCH_BYTES,
            reply_bytes: lash_vm_protocol::PAGE_BATCH_BYTES
                .min(u64::from(frame.max_frame_bytes).saturating_sub(REPLY_HEADROOM)),
            reply_nodes: frame.max_nodes / 2,
        }
    }
}

/// Frame room a read reply leaves for its capabilities and framing.
const REPLY_HEADROOM: u64 = 512 * 1024;

/// The MessagePack values one page of a reply is estimated to hold: its
/// envelope's and each child capability's.
fn page_nodes(children: usize) -> u64 {
    8 + 16 * children as u64
}

/// What a page service did, for measurement and the layer's laws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageStats {
    pub exchanges: u64,
    pub pages_read: u64,
    pub pages_sealed: u64,
    /// Batched store reads under the root lease.
    pub store_reads: u64,
}

/// How an ambiguous publication resolves against the execution row
/// (`D-SPOOL`).
pub enum Resolution {
    /// The row holds the candidate's commit: it committed, and the spool is
    /// settled.
    Committed,
    /// The row still holds the base: publish the exact candidate again
    /// (idempotent by commit identity), or discard the activation.
    Retry(Box<dyn CandidateManifest>),
    /// The row holds another root: the candidate lost, and the activation
    /// is discarded with its spool.
    Superseded,
}

/// One activation's page service: its root lease, capability key, epoch
/// and spool, under one absolute deadline.
pub struct ActivationPages<R: ?Sized> {
    store: Arc<R>,
    lease: RootLease,
    /// Renew the lease once this long has passed since it was last renewed
    /// (a third of its ttl, `D-LEASE`).
    renew_every: Duration,
    renewed: Instant,
    key: CapabilityKey,
    epoch: u64,
    spool: Box<dyn StageSpool>,
    /// Covers every exchange of the activation; progress never extends it.
    deadline: Instant,
    limits: PageLimits,
    /// The first refusal, which every later exchange repeats.
    refused: Option<PageRefusal>,
    /// The candidate a park built, until its publication resolves.
    pending: Option<ManifestSummary>,
    /// Whether a candidate of this activation committed: its pages left the
    /// spool for the store, and their stage capabilities read there.
    settled: bool,
    stats: PageStats,
}

impl<R: LeasedReads + ?Sized> ActivationPages<R> {
    /// The page service of an activation leased on `lease`'s root, staging
    /// into `spool`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one activation's whole authority"
    )]
    pub fn new(
        store: Arc<R>,
        lease: RootLease,
        renew_every: Duration,
        key: CapabilityKey,
        epoch: u64,
        spool: Box<dyn StageSpool>,
        deadline: Instant,
        limits: PageLimits,
    ) -> Self {
        Self {
            store,
            lease,
            renew_every,
            renewed: Instant::now(),
            key,
            epoch,
            spool,
            deadline,
            limits,
            refused: None,
            pending: None,
            settled: false,
            stats: PageStats::default(),
        }
    }

    /// The capability for the leased root: what a worker resuming the
    /// activation starts from.
    #[must_use]
    pub fn root(&self) -> PageCapability {
        self.key
            .issue(self.lease.root, self.root_authority(), self.epoch)
    }

    #[must_use]
    pub fn lease(&self) -> &RootLease {
        &self.lease
    }

    #[must_use]
    pub fn stats(&self) -> PageStats {
        self.stats
    }

    #[must_use]
    pub fn usage(&self) -> StageUsage {
        self.spool.usage()
    }

    /// The refusal that ended the activation, if one did.
    #[must_use]
    pub fn refusal(&self) -> Option<&PageRefusal> {
        self.refused.as_ref()
    }

    fn root_authority(&self) -> PageAuthority {
        PageAuthority::Root(self.lease.id.0)
    }

    fn stage_authority(&self) -> PageAuthority {
        PageAuthority::Stage(self.spool.lease().0)
    }

    /// Checks a capability is this activation's: its MAC, epoch and
    /// authority.
    fn authorize(&self, capability: &PageCapability) -> Result<NodeId, PageRefusal> {
        if capability.epoch != self.epoch
            || (capability.authority != self.root_authority()
                && capability.authority != self.stage_authority())
            || !self.key.verify(capability)
        {
            return Err(PageRefusal::Unauthorized);
        }
        Ok(NodeId(capability.page))
    }

    fn check_deadline(&self) -> Result<(), PageRefusal> {
        if Instant::now() >= self.deadline {
            return Err(PageRefusal::Deadline);
        }
        Ok(())
    }

    fn too_large(&self) -> PageRefusal {
        PageRefusal::BatchTooLarge {
            pages: self.limits.batch_pages as u64,
            bytes: self.limits.batch_bytes,
        }
    }

    /// Reads `pages` from the store under the lease, renewing it first when
    /// a third of its ttl has passed.
    async fn store_read(&mut self, pages: &[NodeId]) -> Result<Vec<StoredPage>, PageRefusal> {
        let deadline = tokio::time::Instant::from_std(self.deadline);
        if self.renewed.elapsed() >= self.renew_every {
            let renewed =
                tokio::time::timeout_at(deadline, self.store.renew_root_lease(&self.lease))
                    .await
                    .map_err(|_| PageRefusal::Deadline)?
                    .map_err(|_| PageRefusal::LeaseLost)?;
            self.lease = renewed;
            self.renewed = Instant::now();
        }
        self.stats.store_reads += 1;
        tokio::time::timeout_at(deadline, self.store.read_pages(&self.lease, pages))
            .await
            .map_err(|_| PageRefusal::Deadline)?
            .map_err(store_refusal)
    }

    async fn read(&mut self, capabilities: Vec<PageCapability>) -> Result<PageReply, PageRefusal> {
        if capabilities.len() > self.limits.batch_pages {
            return Err(self.too_large());
        }
        let mut wanted = Vec::with_capacity(capabilities.len());
        for capability in &capabilities {
            wanted.push((self.authorize(capability)?, capability.authority));
        }
        // Staged pages from the spool; the rest from the store in one batch.
        let mut bytes: BTreeMap<NodeId, Arc<[u8]>> = BTreeMap::new();
        let mut committed = BTreeSet::new();
        for (id, authority) in &wanted {
            if bytes.contains_key(id) {
                continue;
            }
            if matches!(authority, PageAuthority::Stage(_)) {
                match self.spool.read(id) {
                    Ok(page) => {
                        bytes.insert(*id, page);
                        continue;
                    }
                    // A committed candidate's pages read from the store.
                    Err(StageFault::Missing(_)) if self.settled => {}
                    Err(fault) => return Err(stage_refusal(fault)),
                }
            }
            committed.insert(*id);
        }
        if !committed.is_empty() {
            let ids: Vec<NodeId> = committed.into_iter().collect();
            for page in self.store_read(&ids).await? {
                bytes.insert(page.id, page.bytes);
            }
        }
        let limits = DecodeLimits::within_total(u64::MAX);
        let (mut reply_bytes, mut reply_nodes) = (0_u64, 0_u64);
        let mut pages = Vec::with_capacity(wanted.len());
        for (id, authority) in wanted {
            let page = bytes.get(&id).ok_or(PageRefusal::Missing)?;
            let envelope = check_envelope(page, id, &limits).map_err(|_| PageRefusal::Corrupt)?;
            let nodes = page_nodes(envelope.children.len());
            // A reply answers a prefix that fits one frame; at least one
            // page always fits, since a page is at most 64 KiB.
            if !pages.is_empty()
                && (reply_bytes + page.len() as u64 > self.limits.reply_bytes
                    || reply_nodes + nodes > self.limits.reply_nodes)
            {
                break;
            }
            reply_bytes += page.len() as u64;
            reply_nodes += nodes;
            pages.push(ReadPage {
                page: PageEnvelope {
                    id: id.0,
                    envelope: page.to_vec(),
                },
                // A child inherits its page's authority: a staged page's
                // committed children read through the spool's fallback to
                // the store, whose lease protects them.
                children: envelope
                    .children
                    .iter()
                    .map(|child| self.key.issue(child, authority, self.epoch))
                    .collect(),
            });
        }
        self.stats.pages_read += pages.len() as u64;
        Ok(PageReply::Read { pages })
    }

    fn seal(
        &mut self,
        pages: Vec<PageEnvelope>,
        children: &[PageCapability],
    ) -> Result<PageReply, PageRefusal> {
        let bytes: u64 = pages.iter().map(|page| page.envelope.len() as u64).sum();
        if pages.len() > self.limits.batch_pages || bytes > self.limits.batch_bytes {
            return Err(self.too_large());
        }
        // The committed pages a capability authorizes. A stage capability
        // protects a child only once its candidate committed; until then
        // the child must be staged.
        let mut protected = BTreeSet::new();
        for capability in children {
            let id = self.authorize(capability)?;
            if capability.authority == self.root_authority() || self.settled {
                protected.insert(id);
            }
        }
        let mut sealed = Vec::with_capacity(pages.len());
        for page in pages {
            let id = NodeId(page.id);
            if NodeId::of_envelope(&page.envelope) != id {
                return Err(PageRefusal::Corrupt);
            }
            self.spool
                .seal(
                    EncodedNode {
                        id,
                        bytes: page.envelope.into(),
                    },
                    &|child| protected.contains(child),
                )
                .map_err(stage_refusal)?;
            sealed.push(self.key.issue(id, self.stage_authority(), self.epoch));
        }
        self.stats.pages_sealed += sealed.len() as u64;
        Ok(PageReply::Sealed { pages: sealed })
    }

    /// The candidate a park sealed: `root`, the stage capability the
    /// worker's `Parked` carried, built against `base` and published under
    /// `commit`. The spool keeps it until its publication resolves.
    ///
    /// # Errors
    ///
    /// [`StageFault::Unauthorized`] for a capability that is not this
    /// activation's stage capability, and the spool's faults.
    pub fn candidate(
        &mut self,
        root: &PageCapability,
        base: Option<NodeId>,
        commit: CommitId,
    ) -> Result<Box<dyn CandidateManifest>, StageFault> {
        let id = NodeId(root.page);
        if root.authority != self.stage_authority() || self.authorize(root).is_err() {
            return Err(StageFault::Unauthorized {
                page: id,
                child: id,
            });
        }
        let manifest = self.spool.candidate(id, base, commit)?;
        self.pending = Some(manifest.summary());
        Ok(manifest)
    }

    /// Settles the pending candidate on an authoritative commit: its pages
    /// are the store's now, and the spool deletes them.
    ///
    /// # Errors
    ///
    /// The spool's faults.
    pub fn settle(&mut self, commit: CommitId) -> Result<(), StageFault> {
        self.spool.settle(commit)?;
        self.pending = None;
        self.settled = true;
        Ok(())
    }

    /// Resolves an ambiguous publication of the pending candidate against
    /// `row`, the execution's current root and commit as the store reads it
    /// ([`PageStore::candidate_state`]). Worker disconnection settles
    /// nothing: only the row does.
    ///
    /// # Errors
    ///
    /// [`StageFault::Missing`] when no candidate is pending, and the
    /// spool's faults.
    pub fn resolve(&mut self, row: Option<(NodeId, CommitId)>) -> Result<Resolution, StageFault> {
        let summary = self.pending.ok_or(StageFault::Missing(self.lease.root))?;
        match row {
            Some((_, commit)) if commit == summary.commit => {
                self.settle(commit)?;
                Ok(Resolution::Committed)
            }
            Some((root, _)) if Some(root) == summary.base => Ok(Resolution::Retry(
                self.spool
                    .candidate(summary.root, summary.base, summary.commit)?,
            )),
            None if summary.base.is_none() => Ok(Resolution::Retry(self.spool.candidate(
                summary.root,
                summary.base,
                summary.commit,
            )?)),
            _ => Ok(Resolution::Superseded),
        }
    }

    /// Ends the activation's staging: on an authoritative refusal, a lost
    /// lease or a reset. The committed root stands.
    pub fn discard(self) {
        self.spool.discard();
    }
}

#[async_trait::async_trait]
impl<R: LeasedReads + ?Sized> PageExchange for ActivationPages<R> {
    async fn serve(&mut self, request: PageRequest) -> PageReply {
        self.stats.exchanges += 1;
        if let Some(refusal) = &self.refused {
            return PageReply::Refused(refusal.clone());
        }
        let reply = match self.check_deadline() {
            Err(refusal) => Err(refusal),
            Ok(()) => match request {
                PageRequest::Read { pages } => self.read(pages).await,
                PageRequest::Seal { pages, children } => self.seal(pages, &children),
            },
        };
        reply.unwrap_or_else(|refusal| {
            self.refused = Some(refusal.clone());
            PageReply::Refused(refusal)
        })
    }
}

/// A store failure as the page exchange states it.
fn store_refusal(error: DurableError) -> PageRefusal {
    match error {
        DurableError::OwnershipLost(_)
        | DurableError::Graph(
            GraphRefusal::FenceLost | GraphRefusal::RootRetired | GraphRefusal::RevisionConflict,
        ) => PageRefusal::LeaseLost,
        DurableError::Graph(GraphRefusal::MissingNode(_)) => PageRefusal::Missing,
        DurableError::Graph(GraphRefusal::HashMismatch(_) | GraphRefusal::ContentConflict(_)) => {
            PageRefusal::Corrupt
        }
        _ => PageRefusal::StoreUnavailable,
    }
}

/// A spool failure as the page exchange states it.
fn stage_refusal(fault: StageFault) -> PageRefusal {
    match fault {
        StageFault::Quota { quota } => PageRefusal::StagingExhausted { bytes: quota },
        StageFault::HashMismatch(_) => PageRefusal::Corrupt,
        StageFault::Unauthorized { .. } => PageRefusal::Unauthorized,
        StageFault::Missing(_) => PageRefusal::Missing,
        StageFault::Ended => PageRefusal::LeaseLost,
        StageFault::Io(_) => PageRefusal::StoreUnavailable,
    }
}

/// The envelope bound of a semantic page kind: a checkpoint root is
/// root-bounded, every other kind node-bounded.
#[must_use]
pub fn page_bound(kind: PageKind) -> u64 {
    if kind == PageKind::CHECKPOINT_ROOT {
        MAX_ROOT_BYTES
    } else {
        lash_core_store::checkpoint_graph::MAX_NODE_BYTES
    }
}

/// Frames a page the heap seals: its envelope and identity. A body that is
/// not canonical, or does not fit its kind's bound, is the heap's defect.
///
/// # Errors
///
/// [`HeapFault::Malformed`].
pub fn frame_page(body: &PageBody) -> Result<EncodedNode, HeapFault> {
    frame_direct(NodeKind(body.kind.0), &body.tokens, page_bound(body.kind))
        .map_err(|refusal| HeapFault::Malformed(format!("a sealed page does not frame: {refusal}")))
}

/// Checks a page read under `page` and hands it to the heap: its bytes
/// must hash to its identity, frame a semantic kind within its bound, and
/// carry a canonical body. Each child gets the grant `grant` derives for
/// it, in the envelope's ascending order.
///
/// # Errors
///
/// [`HeapFault::Corrupt`].
pub fn answer_page(
    page: PageRef,
    bytes: &[u8],
    mut grant: impl FnMut(PageId) -> Result<PageGrant, HeapFault>,
) -> Result<PageAnswer, HeapFault> {
    let id = NodeId(page.id.0);
    let corrupt = |reason: String| HeapFault::Corrupt {
        page: page.id,
        reason,
    };
    let envelope = check_envelope(bytes, id, &DecodeLimits::within_total(u64::MAX))
        .map_err(|refusal| corrupt(refusal.to_string()))?;
    if !SEMANTIC_KINDS.contains(&envelope.kind.0) {
        return Err(corrupt(format!(
            "kind {} is not a semantic page",
            envelope.kind.0
        )));
    }
    let kind = PageKind(envelope.kind.0);
    if bytes.len() as u64 > page_bound(kind) {
        return Err(corrupt(format!(
            "{} bytes exceed its kind's bound of {}",
            bytes.len(),
            page_bound(kind)
        )));
    }
    let tokens = direct_logical(&envelope, id).map_err(|refusal| corrupt(refusal.to_string()))?;
    let children = envelope
        .children
        .iter()
        .map(|child| {
            let child = PageId(child.0);
            grant(child).map(|grant| PageRef { id: child, grant })
        })
        .collect::<Result<_, _>>()?;
    Ok(PageAnswer {
        page,
        kind,
        tokens: tokens.into(),
        children,
    })
}
