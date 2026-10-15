//! The session's pages as its executor reads and stages them (FIG-6193).
//!
//! A session's carried state is pages of the persistent heap. Its cells
//! stage what they seal in one spool their turn shares, which the turn's
//! commit settles; what the store holds they read under one lease on the
//! root the session's committed state stands under, taken the first time
//! a committed page is read and reused by every later read of the turn.
//!
//! The host's own reads (the bindings a prompt lists, the names a cell is
//! lowered against) run synchronously over the heap. A page the store holds
//! and the executor has not read faults the read: the executor fetches
//! every page the read missed, under the lease, and runs it again.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::plugin::SessionPages;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::node::parse_envelope;
use lash_core_store::checkpoint_graph::stage::{StageFault, StageSpool};
use lash_durable::domain::pages::{RootLease, RootLeaseRequest};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageKind, PageSource};
use lash_vm_broker::kernel::next_stage_lease;
use lash_vm_broker::kernel::pages::cache::PageCache;
use lash_vm_broker::kernel::pages::{SharedSpool, SpoolRoot, answer_page, frame_page};

/// How long the session's lease lives between renewals.
pub(crate) const LEASE_TTL: Duration = Duration::from_secs(60);

/// The parent's access to a session's pages: the store it reads them from,
/// the lease it reads under, the turn's spool and the pages it read.
#[derive(Default)]
pub(crate) struct SessionPageAccess {
    /// Where the session's committed state stands, as the turn last said.
    handle: Option<SessionPages>,
    /// The lease the session's reads go under, with when it was last
    /// renewed.
    lease: Option<(RootLease, Instant)>,
    spool: Option<SharedSpool>,
    /// The activations of ended runs whose carries the session staged and
    /// no commit owns yet, oldest first: the committed pages a carry names
    /// stay protected by its run's lease until a capture of the session is
    /// committed, and are read under it while the session holds no root.
    runs: VecDeque<HeldRun>,
    /// The committed pages the process read, shared with its activations'
    /// page services (FIG-6321).
    cache: PageCache,
    /// Lease writes the executor made: acquisitions and renewals.
    lease_writes: u64,
}

/// An ended run's activation the session holds, with the store its lease
/// is ended in.
pub(crate) struct HeldRun {
    pages: lash_vm_broker::kernel::pages::SharedPages,
    store: Arc<dyn lash_durable::domain::pages::PageStore>,
}

impl HeldRun {
    pub(crate) fn new(
        pages: lash_vm_broker::kernel::pages::SharedPages,
        store: Arc<dyn lash_durable::domain::pages::PageStore>,
    ) -> Self {
        Self { pages, store }
    }

    /// Ends the run's activation and its lease.
    pub(crate) async fn end(self) {
        lash_vm_broker::kernel::end_activation(self.store.as_ref(), &self.pages).await;
    }
}

impl std::fmt::Debug for SessionPageAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPageAccess")
            .field("handle", &self.handle)
            .field("leased", &self.lease.is_some())
            .field("spool", &self.spool.is_some())
            .field("cached", &self.cache.usage().0)
            .finish()
    }
}

/// A read that needs a page the store holds: the store is unreachable, or
/// refused the lease or the read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PageAccessError {
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error("the session's pages are not readable: {0}")]
    Store(#[from] lash_durable::DurableError),
    #[error("the session holds pages in the store, and no store was named")]
    NoStore,
    #[error("the run's pages are not readable: {0}")]
    Run(lash_vm_protocol::page::PageRefusal),
    #[error(transparent)]
    Staging(#[from] StageFault),
}

impl SessionPageAccess {
    /// Takes `pages` as where the session's committed state stands now. A
    /// lease already taken stays: every page the session reads stays
    /// owned by some root of it for the lease's life.
    pub(crate) fn note(&mut self, pages: Option<&SessionPages>) {
        if let Some(pages) = pages {
            self.handle = Some(pages.clone());
        }
    }

    /// Lease writes so far: one per acquisition or renewal.
    #[cfg(test)]
    pub(crate) fn lease_writes(&self) -> u64 {
        self.lease_writes
    }

    /// The turn's spool, opened in `spools` on first use.
    pub(crate) fn spool(&mut self, spools: &SpoolRoot) -> Result<SharedSpool, StageFault> {
        if let Some(spool) = &self.spool {
            return Ok(spool.clone());
        }
        let spool = SharedSpool::new(spools.spool(next_stage_lease())?);
        self.spool = Some(spool.clone());
        Ok(spool)
    }

    /// The turn's spool, when a cell opened one.
    pub(crate) fn staged(&self) -> Option<&SharedSpool> {
        self.spool.as_ref()
    }

    /// Holds `run`, whose carry the session staged, until a committed
    /// capture owns the carry.
    pub(crate) fn hold(&mut self, run: HeldRun) {
        self.runs.push_back(run);
    }

    /// How many ended runs the session holds.
    pub(crate) fn held(&self) -> usize {
        self.runs.len()
    }

    /// Ends the `count` oldest held runs: a committed capture owns their
    /// carries now.
    pub(crate) async fn release_runs(&mut self, count: usize) {
        for _ in 0..count.min(self.runs.len()) {
            if let Some(run) = self.runs.pop_front() {
                run.end().await;
            }
        }
    }

    /// Ends the turn's staging: what it staged and did not publish goes.
    pub(crate) fn end_turn(&mut self) {
        self.spool = None;
    }

    /// The live lease the session's reads go under, taken or renewed as
    /// needed; none when the session holds nothing in the store.
    pub(crate) async fn lease(&mut self) -> Result<Option<RootLease>, PageAccessError> {
        let Some(handle) = self.handle.clone() else {
            return Ok(None);
        };
        if let Some((lease, renewed)) = &self.lease {
            if renewed.elapsed() < LEASE_TTL / 3 {
                return Ok(Some(lease.clone()));
            }
            self.lease_writes += 1;
            if let Ok(renewed) = handle.store.renew_root_lease(lease).await {
                self.lease = Some((renewed.clone(), Instant::now()));
                return Ok(Some(renewed));
            }
        }
        self.lease_writes += 1;
        let lease = handle
            .store
            .acquire_root_lease(RootLeaseRequest {
                owner: handle.owner.clone(),
                expected: handle.root,
                actor: handle.actor.clone(),
                epoch: handle.epoch,
                ttl_millis: i64::try_from(LEASE_TTL.as_millis()).unwrap_or(i64::MAX),
            })
            .await?;
        self.lease = Some((lease.clone(), Instant::now()));
        Ok(Some(lease))
    }

    /// Ends the session's lease; the next read takes another.
    pub(crate) async fn release(&mut self) {
        if let (Some((lease, _)), Some(handle)) = (self.lease.take(), &self.handle) {
            let _ = handle.store.release_root_lease(lease.id).await;
        }
    }

    /// Keeps `pages`, the bytes of committed pages the host already holds
    /// (a restored descriptor), as read.
    pub(crate) fn preload(&mut self, pages: impl IntoIterator<Item = (NodeId, Arc<[u8]>)>) {
        self.cache.keep(pages);
    }

    /// The actor whose committed state the session's pages are.
    pub(crate) fn actor(&self) -> Option<&lash_durable::ActorKey> {
        self.handle.as_ref().map(|handle| &handle.actor)
    }

    /// Reads committed pages through `cache`, the process's.
    pub(crate) fn caching(&mut self, cache: PageCache) {
        self.cache = cache;
    }

    /// Runs `read` over the session's pages until it reads no page it
    /// does not hold: each pass fetches, under the lease, every committed
    /// page the last one missed. A page the read seals is staged in the
    /// turn's spool.
    pub(crate) async fn read<T>(
        &mut self,
        mut read: impl FnMut(Box<dyn PageSource>) -> Result<T, HeapFault>,
    ) -> Result<T, PageAccessError> {
        loop {
            let missing = Arc::new(Mutex::new(Vec::new()));
            let result = read(Box::new(ParentPages {
                spool: self.spool.clone(),
                cache: self.cache.clone(),
                missing: Arc::clone(&missing),
            }));
            let missing = std::mem::take(&mut *lock(&missing));
            match result {
                Err(HeapFault::Missing { .. }) if !missing.is_empty() => {
                    self.fetch(missing).await?;
                }
                other => return other.map_err(PageAccessError::Heap),
            }
        }
    }

    /// Fetches `missing` and, with it, the rest of every payload it
    /// reaches: the host reads a text or bytes value whole, so a payload
    /// index's pages come in with it rather than one read pass each.
    async fn fetch(&mut self, mut missing: Vec<NodeId>) -> Result<(), PageAccessError> {
        while !missing.is_empty() {
            missing.sort();
            missing.dedup();
            let batch = missing.split_off(missing.len().saturating_sub(FETCH_BATCH));
            let pages = self.fetch_batch(&batch).await?;
            for (id, bytes) in &pages {
                missing.extend(payload_children(*id, bytes).into_iter().filter(|child| {
                    !self.cache.contains(child)
                        && !self
                            .spool
                            .as_ref()
                            .is_some_and(|spool| spool.contains(child))
                }));
            }
            self.cache.keep(pages);
        }
        Ok(())
    }

    async fn fetch_batch(
        &mut self,
        missing: &[NodeId],
    ) -> Result<Vec<(NodeId, Arc<[u8]>)>, PageAccessError> {
        if self.handle.is_none()
            && let Some(run) = self.runs.back()
        {
            let pages = run
                .pages
                .lock()
                .await
                .read_committed(missing)
                .await
                .map_err(PageAccessError::Run)?;
            return Ok(pages
                .into_iter()
                .map(|page| (page.id, page.bytes))
                .collect());
        }
        let lease = self.lease().await?.ok_or(PageAccessError::NoStore)?;
        let store = Arc::clone(&self.handle.as_ref().ok_or(PageAccessError::NoStore)?.store);
        let pages = store.read_pages(&lease, missing).await?;
        Ok(pages
            .into_iter()
            .map(|page| (page.id, page.bytes))
            .collect())
    }
}

/// The most pages one store read asks for.
const FETCH_BATCH: usize = 512;

/// The children of `bytes` when it frames a payload index; a page that
/// does not frame is left for the read that answers it to refuse.
fn payload_children(id: NodeId, bytes: &[u8]) -> Vec<NodeId> {
    match parse_envelope(bytes, Some(id)) {
        Ok(envelope) if envelope.kind.0 == PageKind::PAYLOAD_INDEX.0 => {
            envelope.children.iter().collect()
        }
        _ => Vec::new(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One pass of a host read: the turn's spool, then what the executor read
/// from the store; any other page is recorded as missing.
struct ParentPages {
    spool: Option<SharedSpool>,
    cache: PageCache,
    missing: Arc<Mutex<Vec<NodeId>>>,
}

impl PageSource for ParentPages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        let mut answers = Vec::with_capacity(pages.len());
        let mut missing = lock(&self.missing);
        for page in pages {
            let id = NodeId(page.id.0);
            let bytes = match &self.spool {
                Some(spool) if spool.contains(&id) => spool
                    .read(&id)
                    .map_err(|_| HeapFault::Missing { page: page.id })?,
                _ => match self.cache.get(&id) {
                    Some(bytes) => bytes,
                    None => {
                        missing.push(id);
                        continue;
                    }
                },
            };
            answers.push(answer_page(*page, &bytes, |_| Ok(PageGrant::NONE))?);
        }
        if let Some(first) = missing.first() {
            return Err(HeapFault::Missing {
                page: PageId(first.0),
            });
        }
        Ok(answers)
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        let node = frame_page(&page)?;
        let id = PageId(node.id.0);
        let Some(spool) = &mut self.spool else {
            return Err(HeapFault::Malformed(
                "the session sealed a page with no turn spool to stage it in".into(),
            ));
        };
        spool
            .seal(node, &|_| true)
            .map_err(|fault| HeapFault::Malformed(format!("the turn's spool: {fault}")))?;
        Ok(PageRef {
            id,
            grant: PageGrant::NONE,
        })
    }
}
