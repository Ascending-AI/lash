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
//! - Every exchange runs the authority check first: the lease is renewed
//!   at a third of its ttl even while the worker waits on a tool, a cached
//!   read checks it too, and an activation that waits or outlives its
//!   budget of host work in exchanges refuses `Deadline`.
//! - Every refusal is infrastructure ([`PageRefusal`]). A request-wide
//!   refusal ends the activation: every later exchange is refused the same way.
//!   Page-specific read faults instead occupy their request slots, so a
//!   later sibling cannot poison an earlier subtree before its consumer
//!   surfaces the first fault.
//!
//! `LocalPages` is the same port for an embedder with no parent: the
//! kernel's [`PageSource`] over committed pages it holds itself and a spool.
//! Only laws use it; the `testing` feature exposes it.

pub mod cache;
pub mod capability;
#[cfg(any(test, feature = "testing"))]
pub mod local;
pub mod shared;
pub mod spool;
#[cfg(test)]
mod spool_laws;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core_execution::runtime::Clock;
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
use lash_kernel_doc::heap::{HeapFault, HostResource, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageKind};
use lash_vm_protocol::page::{
    PageAuthority, PageCapability, PageEnvelope, PageRefusal, PageReply, PageRequest, ReadPage,
    SealBatch,
};

pub use capability::CapabilityKey;
#[cfg(any(test, feature = "testing"))]
pub use local::{CommittedPages, LocalPages};
pub use shared::{ParentSeal, SharedPages, SharedSpool, renew_when_due};
pub use spool::{DiskSpool, SpoolRoot};

/// A spool fault as the heap fault the activation's page port answers.
pub(super) fn staging(fault: StageFault) -> HeapFault {
    match fault {
        StageFault::Quota { .. } => HeapFault::Exhausted {
            resource: HostResource::StagingBytes,
        },
        StageFault::Unauthorized { child, .. } => HeapFault::Unauthorized {
            page: PageId(child.0),
        },
        StageFault::Missing(page) => HeapFault::Missing {
            page: PageId(page.0),
        },
        StageFault::HashMismatch(page) => HeapFault::Corrupt {
            page: PageId(page.0),
            reason: "a staged page does not hash to its identity".to_owned(),
        },
        StageFault::Ended => HeapFault::LeaseLost,
        StageFault::Io(error) => HeapFault::Transport(error),
    }
}

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

/// The most envelope bytes of pages a run's start carries warm
/// ([`ActivationPages::warm`]).
pub const WARM_BYTES: u64 = 8 * 1024;

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
/// envelope's and each child capability's, which crosses as one value.
fn page_nodes(children: usize) -> u64 {
    10 + 2 * children as u64
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
    /// The row does not hold it: the publication is retried by its caller,
    /// or the activation discarded.
    NotCommitted,
}

/// One activation's page service: its root lease, capability key, epoch
/// and spool, within a budget of host work.
pub struct ActivationPages<R: ?Sized> {
    store: Arc<R>,
    /// The committed root the activation reads under; none for a fresh run
    /// that reads only what it stages.
    lease: Option<RootLease>,
    /// Leases on the roots the activation's parks published beside
    /// `lease`'s, renewed with it and ended with the activation. A settled
    /// candidate's pages stay named by their stage capabilities, and a
    /// worker names one again without sealing it: once a later park
    /// retires the root that published it, only its lease here keeps it
    /// stored (FIG-6629).
    published: Vec<RootLease>,
    /// Renew the lease once this long has passed since it was last renewed
    /// (a third of its ttl, `D-LEASE`).
    renew_every: Duration,
    /// When the lease was last renewed, on `clock`.
    renewed: Instant,
    /// The runtime's clock: the activation's timing, and the schedule a
    /// held activation renews on.
    clock: Arc<dyn Clock>,
    key: CapabilityKey,
    epoch: u64,
    spool: Box<dyn StageSpool>,
    /// The budget of host work `spent` may reach across every exchange of
    /// the activation; wall time does not charge it.
    span: Duration,
    /// The host work every exchange so far measured on `clock`.
    spent: Duration,
    limits: PageLimits,
    /// The first refusal, which every later exchange repeats.
    refused: Option<PageRefusal>,
    /// The candidate a park built, and the execution root that wraps it,
    /// until its publication resolves.
    pending: Option<(ManifestSummary, NodeId)>,
    /// Whether a candidate of this activation committed: its pages left the
    /// spool for the store, and their stage capabilities read there.
    settled: bool,
    /// Host nodes a sealed page may name without a capability: the
    /// release manifests and owner binding a checkpoint root's header
    /// names, which the publishing commit itself pins or stages.
    references: BTreeSet<NodeId>,
    /// Whether the activation's lease is one its broker took, which ends
    /// with the activation; the host's own lease outlives it.
    owned: bool,
    /// The process's committed pages: a read the cache answers asks the
    /// store for nothing.
    cache: Option<cache::PageCache>,
    /// What the session's activations read, warmed and sealed.
    hints: Option<cache::PageHints>,
    stats: PageStats,
    /// The envelope bytes of the pages the run sealed since its broker
    /// last took them: what its next save appends to its checkpoints
    /// (FIG-6788).
    sealed_bytes: u64,
}

impl<R: LeasedReads + ?Sized> ActivationPages<R> {
    /// The page service of an activation leased on `lease`'s root (none
    /// for one that reads only what it stages), staging into `spool`,
    /// timed on `clock` within `span` of host work in exchanges.
    #[expect(
        clippy::too_many_arguments,
        reason = "one activation's whole authority"
    )]
    pub fn new(
        store: Arc<R>,
        lease: Option<RootLease>,
        renew_every: Duration,
        key: CapabilityKey,
        epoch: u64,
        spool: Box<dyn StageSpool>,
        clock: Arc<dyn Clock>,
        span: Duration,
        limits: PageLimits,
    ) -> Self {
        Self {
            renewed: clock.now(),
            store,
            lease,
            published: Vec::new(),
            renew_every,
            clock,
            key,
            epoch,
            spool,
            span,
            spent: Duration::ZERO,
            limits,
            refused: None,
            pending: None,
            settled: false,
            references: BTreeSet::new(),
            owned: false,
            cache: None,
            hints: None,
            stats: PageStats::default(),
            sealed_bytes: 0,
        }
    }

    /// Answers committed reads from `cache` first, and keeps there what the
    /// store answers.
    #[must_use]
    pub fn caching(mut self, cache: cache::PageCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Notes in `hints` what the activation reads, warms and seals, and
    /// warms what they name.
    #[must_use]
    pub fn hinted(mut self, hints: cache::PageHints) -> Self {
        self.hints = Some(hints);
        self
    }

    /// Lets sealed pages name `references`, host nodes the publishing
    /// commit pins or stages beside the candidate (S4: a checkpoint root's
    /// release manifest and owner binding).
    #[must_use]
    pub fn with_references(mut self, references: impl IntoIterator<Item = NodeId>) -> Self {
        self.references.extend(references);
        self
    }

    /// Reads `pages` the store holds under the activation's lease, for the
    /// parent: the pages a run that parked committed, which the session
    /// its end leaves still reaches.
    ///
    /// # Errors
    ///
    /// As a worker's read of them.
    pub async fn read_committed(
        &mut self,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, PageRefusal> {
        if let Some(refusal) = &self.refused {
            return Err(refusal.clone());
        }
        let at = self.clock.now();
        let answer = match self.authority().await.and_then(|()| self.check_span()) {
            Err(refusal) => Err(refusal),
            Ok(()) => self.store_read(pages).await,
        };
        self.spent += self.clock.now().saturating_duration_since(at);
        match answer {
            Ok(pages) => Ok(pages),
            Err(refusal) => {
                if matches!(
                    refusal,
                    PageRefusal::Unauthorized | PageRefusal::LeaseLost | PageRefusal::Deadline
                ) {
                    self.refused = Some(refusal.clone());
                }
                Err(refusal)
            }
        }
    }

    /// The capability for `page`, which the parent knows the activation
    /// may read: a page under the leased root (the checkpoint root a
    /// resumed worker starts from, a carried session's pages) or one it
    /// staged. Its authority is its provenance.
    #[must_use]
    pub fn issue(&self, page: NodeId) -> PageCapability {
        self.key.issue(page, self.provenance(&page), self.epoch)
    }

    /// The page a worker's capability names, when it is this activation's.
    ///
    /// # Errors
    ///
    /// [`PageRefusal::Unauthorized`].
    pub fn verify(&self, capability: &PageCapability) -> Result<NodeId, PageRefusal> {
        self.authorize(capability)
    }

    /// Admits a seal batch that crossed with a worker's park, seal or end
    /// (FIG-6288) exactly as a seal exchange of the same pages: the same
    /// digest, authority, bound and quota checks, the first refusal ending
    /// the activation. The stage capability of each page, in batch order.
    ///
    /// # Errors
    ///
    /// The refusal a seal exchange of the batch would get.
    pub async fn admit(&mut self, batch: SealBatch) -> Result<Vec<PageCapability>, PageRefusal> {
        if let Some(refusal) = &self.refused {
            return Err(refusal.clone());
        }
        let at = self.clock.now();
        let reply = match self.authority().await.and_then(|()| self.check_span()) {
            Err(refusal) => Err(refusal),
            Ok(()) => self.seal(batch.pages, &batch.children),
        };
        self.spent += self.clock.now().saturating_duration_since(at);
        match reply {
            Ok(PageReply::Sealed { pages }) => Ok(pages),
            Ok(_) => Err(PageRefusal::Corrupt),
            Err(refusal) => {
                self.refused = Some(refusal.clone());
                Err(refusal)
            }
        }
    }

    /// The envelope bytes of the pages the run sealed since the last
    /// take, counted once for each seal that offered them: what the save
    /// that takes them appends to the run's checkpoints (FIG-6788).
    pub fn take_sealed_bytes(&mut self) -> u64 {
        std::mem::take(&mut self.sealed_bytes)
    }

    /// `page`, when this activation staged it and its spool still holds
    /// it: a candidate root a worker named.
    ///
    /// # Errors
    ///
    /// [`PageRefusal::Missing`].
    pub fn staged(&self, page: NodeId) -> Result<NodeId, PageRefusal> {
        if self.spool.contains(&page) {
            Ok(page)
        } else {
            Err(PageRefusal::Missing)
        }
    }

    #[must_use]
    pub fn lease(&self) -> Option<&RootLease> {
        self.lease.as_ref()
    }

    /// Reads the store under `lease` from now on: an activation that read
    /// under none takes one on the root its first park published.
    pub fn lease_on(&mut self, lease: RootLease) {
        self.lease = Some(lease);
        self.renewed = self.clock.now();
        self.owned = true;
    }

    /// Keeps the root a park of this activation published stored under
    /// `lease` until the activation ends: what its candidate committed
    /// stays named by the capabilities the activation issued for it.
    pub fn protect_published(&mut self, lease: RootLease) {
        self.published.push(lease);
    }

    /// This activation, its lease one its broker took.
    #[must_use]
    pub fn owning(mut self) -> Self {
        self.owned = self.lease.is_some();
        self
    }

    /// Ends the activation's reads: the leases its broker took, which the
    /// caller releases; never the host's own.
    pub fn end(&mut self) -> Vec<RootLease> {
        let owned = std::mem::take(&mut self.owned);
        let lease = self.lease.take().filter(|_| owned);
        lease
            .into_iter()
            .chain(std::mem::take(&mut self.published))
            .collect()
    }

    /// The spool, for pages the parent seals itself (a session's
    /// descriptor).
    pub fn spool(&mut self) -> &mut dyn StageSpool {
        self.spool.as_mut()
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
        // An activation with no lease issues root authority nothing reads.
        PageAuthority::Root(self.lease.as_ref().map_or(0, |lease| lease.id.0))
    }

    fn stage_authority(&self) -> PageAuthority {
        PageAuthority::Stage(self.spool.lease().0)
    }

    /// The authority `page` is read and sealed under: the spool's while it
    /// is staged there, the root lease's otherwise.
    fn provenance(&self, page: &NodeId) -> PageAuthority {
        if self.spool.contains(page) {
            self.stage_authority()
        } else {
            self.root_authority()
        }
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

    /// The activation's authority, checked on every exchange independent
    /// of residency: the lease, and each published root's, is renewed once
    /// `renew_every` (a third of its ttl) passed since its last renewal,
    /// and a failed or timed-out renewal refuses like a lost lease. Within `renew_every` of a
    /// confirmed renewal the store's expiry is still at least two thirds
    /// of a ttl away, so a lease that answers locally is live; past it,
    /// the store decides. An activation with no lease answers `Ok`.
    ///
    /// # Errors
    ///
    /// [`PageRefusal::Deadline`] when the renewal could not run within the
    /// remaining span, [`PageRefusal::LeaseLost`] when the store refused
    /// it.
    async fn authority(&mut self) -> Result<(), PageRefusal> {
        let Some(lease) = self.lease.clone() else {
            return Ok(());
        };
        if self.clock.now().saturating_duration_since(self.renewed) < self.renew_every {
            return Ok(());
        }
        let before = self.clock.now();
        let left = self.span.saturating_sub(self.spent);
        let renew = |lease: RootLease| {
            let store = Arc::clone(&self.store);
            async move {
                tokio::time::timeout(left, store.renew_root_lease(&lease))
                    .await
                    .map_err(|_| PageRefusal::Deadline)?
                    .map_err(|_| PageRefusal::LeaseLost)
            }
        };
        let renewed = renew(lease).await?;
        let mut published = Vec::with_capacity(self.published.len());
        for lease in self.published.clone() {
            published.push(renew(lease).await?);
        }
        self.lease = Some(renewed);
        self.published = published;
        self.renewed = before;
        Ok(())
    }

    /// One tick of the held-activation schedule the run loop arms: the
    /// same authority check an exchange runs, recording its refusal as the
    /// activation's first like any exchange does.
    ///
    /// # Errors
    ///
    /// The refusal an exchange at this instant would get.
    pub async fn keep_alive(&mut self) -> Result<(), PageRefusal> {
        if let Some(refusal) = &self.refused {
            return Err(refusal.clone());
        }
        match self.authority().await {
            Ok(()) => Ok(()),
            Err(refusal) => {
                self.refused = Some(refusal.clone());
                Err(refusal)
            }
        }
    }

    /// The clock the activation times on, and the instant its renewal is
    /// due on it: none for an activation with no lease, or one already
    /// refused.
    fn renew_at(&self) -> Option<(Arc<dyn Clock>, Instant)> {
        if self.lease.is_none() || self.refused.is_some() {
            return None;
        }
        Some((Arc::clone(&self.clock), self.renewed + self.renew_every))
    }

    /// Refused when the exchanges of this activation already spent `span`
    /// of host work.
    fn check_span(&self) -> Result<(), PageRefusal> {
        if self.spent >= self.span {
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

    /// Reads `pages`: from the process's cache, and the rest from the store
    /// under the lease, renewing it first when a third of its ttl has
    /// passed.
    async fn store_read(&mut self, pages: &[NodeId]) -> Result<Vec<StoredPage>, PageRefusal> {
        let Some(cache) = self.cache.clone() else {
            return self.fetch(pages).await;
        };
        let mut read = Vec::with_capacity(pages.len());
        let mut missing = Vec::new();
        for id in pages {
            match cache.get(id) {
                Some(bytes) => read.push(StoredPage { id: *id, bytes }),
                None => missing.push(*id),
            }
        }
        if !missing.is_empty() {
            let fetched = self.fetch(&missing).await?;
            cache.keep(
                fetched
                    .iter()
                    .map(|page| (page.id, Arc::clone(&page.bytes))),
            );
            read.extend(fetched);
        }
        Ok(read)
    }

    /// Reads `pages` from the store under the lease the authority guard
    /// keeps live; the cache stays responsible only for immutable bytes.
    async fn fetch(&mut self, pages: &[NodeId]) -> Result<Vec<StoredPage>, PageRefusal> {
        let Some(lease) = self.lease.clone() else {
            return Err(PageRefusal::Unauthorized);
        };
        self.stats.store_reads += 1;
        tokio::time::timeout(
            self.span.saturating_sub(self.spent),
            self.store.read_pages(&lease, pages),
        )
        .await
        .map_err(|_| PageRefusal::Deadline)?
        .map_err(store_refusal)
    }

    async fn read(&mut self, capabilities: Vec<PageCapability>) -> Result<PageReply, PageRefusal> {
        if capabilities.len() > self.limits.batch_pages {
            return Err(self.too_large());
        }
        let wanted: Vec<_> = capabilities
            .iter()
            .map(|capability| {
                (
                    NodeId(capability.page),
                    self.authorize(capability).map(|_| capability.authority),
                )
            })
            .collect();
        let mut bytes = BTreeMap::new();
        let mut committed = BTreeSet::new();
        for (id, authority) in &wanted {
            let Ok(authority) = authority else { continue };
            if bytes.contains_key(id) {
                continue;
            }
            if matches!(authority, PageAuthority::Stage(_)) {
                match self.spool.read(id) {
                    Ok(page) => {
                        bytes.insert(*id, Ok(page));
                        continue;
                    }
                    Err(StageFault::Missing(_)) if self.settled => {}
                    Err(fault) => {
                        bytes.insert(*id, Err(stage_refusal(fault)));
                        continue;
                    }
                }
            }
            committed.insert(*id);
        }
        if !committed.is_empty() {
            let ids: Vec<_> = committed.into_iter().collect();
            let started = self.clock.now();
            match self.store_read(&ids).await {
                Ok(found) => {
                    for page in found {
                        bytes.insert(page.id, Ok(page.bytes));
                    }
                }
                Err(refusal)
                    if matches!(refusal, PageRefusal::LeaseLost | PageRefusal::Deadline) =>
                {
                    self.refused = Some(refusal.clone());
                    for id in ids {
                        bytes.insert(id, Err(refusal.clone()));
                    }
                }
                Err(refusal) if ids.len() == 1 => {
                    bytes.insert(ids[0], Err(refusal));
                }
                Err(_) => {
                    // The store's atomic batch can refuse without returning
                    // its successful rows. Recover ordered per-page answers
                    // on this exceptional path; the production store port
                    // stays atomic and the happy path stays one read.
                    for id in ids {
                        let remaining = self
                            .span
                            .saturating_sub(self.spent)
                            .saturating_sub(self.clock.now().saturating_duration_since(started));
                        if let Some(refusal) = &self.refused {
                            bytes.insert(id, Err(refusal.clone()));
                            continue;
                        }
                        if remaining.is_zero() {
                            self.refused = Some(PageRefusal::Deadline);
                            bytes.insert(id, Err(PageRefusal::Deadline));
                            continue;
                        }
                        let found = tokio::time::timeout(remaining, self.store_read(&[id])).await;
                        let answer = match found {
                            Ok(Ok(found)) => found
                                .into_iter()
                                .find(|page| page.id == id)
                                .map(|page| page.bytes)
                                .ok_or(PageRefusal::Missing),
                            Ok(Err(refusal)) => Err(refusal),
                            Err(_) => Err(PageRefusal::Deadline),
                        };
                        if let Err(refusal @ (PageRefusal::LeaseLost | PageRefusal::Deadline)) =
                            &answer
                        {
                            self.refused = Some(refusal.clone());
                        }
                        bytes.insert(id, answer);
                    }
                }
            }
        }
        let (mut reply_bytes, mut reply_nodes) = (0_u64, 0_u64);
        let mut pages = Vec::with_capacity(wanted.len());
        for (id, authority) in wanted {
            let read = authority.and_then(|_| {
                let page = bytes
                    .get(&id)
                    .ok_or(PageRefusal::Missing)?
                    .as_ref()
                    .map_err(Clone::clone)?;
                self.answer(id, page).map(|read| (read, page.len() as u64))
            });
            match read {
                Ok((read, size)) => {
                    let nodes = page_nodes(read.children.len());
                    if !pages.is_empty()
                        && (reply_bytes + size > self.limits.reply_bytes
                            || reply_nodes + nodes > self.limits.reply_nodes)
                    {
                        break;
                    }
                    reply_bytes += size;
                    reply_nodes += nodes;
                    if let Some(hints) = &self.hints {
                        hints.extend([id]);
                    }
                    self.stats.pages_read += 1;
                    pages.push(Ok(read));
                }
                Err(refusal) => {
                    let nodes = page_nodes(0);
                    if !pages.is_empty() && reply_nodes + nodes > self.limits.reply_nodes {
                        break;
                    }
                    reply_nodes += nodes;
                    pages.push(Err(refusal));
                }
            }
        }
        Ok(PageReply::Read { pages })
    }

    /// `page`, read as `id`, as it crosses: its envelope checked, with a
    /// capability for each child it declares. A child's authority is its
    /// provenance: a staged child reads and seals under the spool's, a
    /// committed child (a committed page's, or one a staged page reuses)
    /// under the root lease that protects it.
    fn answer(&self, id: NodeId, page: &[u8]) -> Result<ReadPage, PageRefusal> {
        let envelope = check_envelope(page, id, &DecodeLimits::within_total(u64::MAX))
            .map_err(|_| PageRefusal::Corrupt)?;
        Ok(ReadPage {
            page: PageEnvelope {
                id: id.0,
                envelope: page.to_vec(),
            },
            children: envelope
                .children
                .iter()
                .map(|child| self.key.issue(child, self.provenance(&child), self.epoch))
                .collect(),
        })
    }

    /// The pages a fresh activation reads first, as a read of them answers:
    /// `roots`, and under them, breadth first, the pages the session's last
    /// activation read, warmed or sealed ([`cache::PageHints`]), within
    /// `budget` envelope bytes. They cross with the run's start, so its
    /// first statements fault none of them in one exchange at a time
    /// (FIG-6321). Only what the roots reach is warmed; what does not fit,
    /// or was not touched last time, the worker reads on demand.
    ///
    /// # Errors
    ///
    /// As a read of the pages.
    pub async fn warm(
        &mut self,
        roots: &[NodeId],
        budget: u64,
    ) -> Result<Vec<ReadPage>, PageRefusal> {
        if let Some(refusal) = &self.refused {
            return Err(refusal.clone());
        }
        let at = self.clock.now();
        let warmed = match self.authority().await.and_then(|()| self.check_span()) {
            Err(refusal) => Err(refusal),
            Ok(()) => self.warm_within(roots, budget).await,
        };
        self.spent += self.clock.now().saturating_duration_since(at);
        match warmed {
            Ok(warm) => Ok(warm),
            Err(refusal) => {
                self.refused = Some(refusal.clone());
                Err(refusal)
            }
        }
    }

    /// The body of [`Self::warm`], run once its authority and span check
    /// pass.
    async fn warm_within(
        &mut self,
        roots: &[NodeId],
        budget: u64,
    ) -> Result<Vec<ReadPage>, PageRefusal> {
        let hinted = self
            .hints
            .as_ref()
            .map(cache::PageHints::take)
            .unwrap_or_default();
        let mut seen: BTreeSet<NodeId> = roots.iter().copied().collect();
        let mut level: Vec<NodeId> = roots.to_vec();
        let mut left = budget;
        let mut warm = Vec::new();
        while !level.is_empty() && left > 0 && warm.len() < self.limits.batch_pages {
            let mut bytes: BTreeMap<NodeId, Arc<[u8]>> = BTreeMap::new();
            let mut committed = Vec::new();
            for id in &level {
                match self.spool.read(id) {
                    Ok(page) => {
                        bytes.insert(*id, page);
                    }
                    Err(StageFault::Missing(_)) => committed.push(*id),
                    Err(fault) => return Err(stage_refusal(fault)),
                }
            }
            if !committed.is_empty() {
                for page in self.store_read(&committed).await? {
                    bytes.insert(page.id, page.bytes);
                }
            }
            let mut next = Vec::new();
            for id in level {
                let page = bytes.get(&id).ok_or(PageRefusal::Missing)?;
                let size = page.len() as u64;
                if size > left || warm.len() == self.limits.batch_pages {
                    continue;
                }
                let envelope = check_envelope(page, id, &DecodeLimits::within_total(u64::MAX))
                    .map_err(|_| PageRefusal::Corrupt)?;
                // A host node under a root (a release, an owner binding) is
                // never a heap read.
                if !SEMANTIC_KINDS.contains(&envelope.kind.0) {
                    continue;
                }
                let read = self.answer(id, page)?;
                left -= size;
                next.extend(
                    envelope
                        .children
                        .iter()
                        .filter(|child| hinted.contains(child) && seen.insert(*child)),
                );
                warm.push(read);
            }
            level = next;
        }
        if let Some(hints) = &self.hints {
            hints.extend(warm.iter().map(|read| NodeId(read.page.id)));
        }
        self.stats.pages_read += warm.len() as u64;
        Ok(warm)
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
        let mut protected = self.references.clone();
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
        if let Some(hints) = &self.hints {
            hints.extend(sealed.iter().map(|capability| NodeId(capability.page)));
        }
        self.stats.pages_sealed += sealed.len() as u64;
        self.sealed_bytes = self.sealed_bytes.saturating_add(bytes);
        Ok(PageReply::Sealed { pages: sealed })
    }

    /// The candidate rooted at `root`, a page the activation staged (the
    /// checkpoint root a worker's `Parked` named, verified, or a page the
    /// parent sealed), built against `base`, the root the owner's row
    /// holds, and published as the owner's root `wrapper` under `commit`:
    /// one publication identity (FIG-6197 F1), which the store checks and
    /// an ambiguous publication resolves by. The spool keeps it until its
    /// publication resolves.
    ///
    /// # Errors
    ///
    /// [`StageFault::Missing`] for a root the activation did not stage,
    /// and the spool's faults.
    pub fn candidate(
        &mut self,
        root: NodeId,
        base: Option<NodeId>,
        wrapper: NodeId,
        commit: CommitId,
    ) -> Result<Box<dyn CandidateManifest>, StageFault> {
        let id = root;
        let manifest = self.spool.candidate(id, base, commit)?;
        self.pending = Some((manifest.summary(), wrapper));
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
    /// ([`PageStore::candidate_state`]): committed only when the row holds
    /// the candidate's execution root under its commit. Worker
    /// disconnection settles nothing: only the row does.
    ///
    /// # Errors
    ///
    /// [`StageFault::Missing`] when no candidate is pending, and the
    /// spool's faults.
    pub fn resolve(&mut self, row: Option<(NodeId, CommitId)>) -> Result<Resolution, StageFault> {
        let (summary, wrapper) = self.pending.ok_or(StageFault::Ended)?;
        match row {
            Some((root, commit)) if root == wrapper && commit == summary.commit => {
                self.settle(commit)?;
                Ok(Resolution::Committed)
            }
            _ => Ok(Resolution::NotCommitted),
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
        let at = self.clock.now();
        let reply = match self.authority().await.and_then(|()| self.check_span()) {
            Err(refusal) => Err(refusal),
            Ok(()) => match request {
                PageRequest::Read { pages } => self.read(pages).await,
                PageRequest::Seal { pages, children } => self.seal(pages, &children),
            },
        };
        self.spent += self.clock.now().saturating_duration_since(at);
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
            GraphRefusal::FenceLost
            | GraphRefusal::LeaseLost
            | GraphRefusal::RootRetired
            | GraphRefusal::RevisionConflict,
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
