//! The session's pages as its executor reads and stages them (FIG-6193).
//!
//! A session's carried state is pages of the persistent heap. Its cells
//! stage what they seal in one spool their turn shares, which the turn's
//! commit settles; what the store holds they read under one lease on the
//! root the session's committed state stands under, taken the first time
//! a committed page is read and reused by every later read of the turn.
//!
//! The host's own reads (the bindings a prompt lists, the names a cell is
//! lowered against) run synchronously over the heap, on a blocking thread.
//! A page the store holds and the executor has not read is a demand: the
//! read waits where it stands while the executor fetches the page under
//! the lease, so a read needs only the pages it reads, never a cache that
//! holds them all at once (FIG-6419).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
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
use tokio::sync::{mpsc, oneshot};

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
    /// Pages the executor asked the store, or a held run, for.
    fetches: u64,
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
    #[error("the session stages pages, and its host named no staging directory")]
    NoStaging,
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

    /// Pages fetched so far: each page each store read asked for.
    #[cfg(test)]
    pub(crate) fn fetches(&self) -> u64 {
        self.fetches
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
                fence: lash_durable::domain::pages::LeaseFence::Actor {
                    actor: handle.actor.clone(),
                    epoch: handle.epoch,
                },
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

    /// Runs `read` over the session's pages on a blocking thread, to its
    /// end. A committed page it reads that the cache does not hold is
    /// fetched, under the lease, while the read waits where it stands:
    /// it keeps its place and what it built, so however few pages the
    /// cache holds, it fetches each page it needs once. A page the read
    /// seals is staged in the turn's spool.
    pub(crate) async fn read<T: Send + 'static>(
        &mut self,
        read: impl FnOnce(Box<dyn PageSource>) -> Result<T, HeapFault> + Send + 'static,
    ) -> Result<T, PageAccessError> {
        let (demands, mut asked) = mpsc::channel(1);
        let source = ParentPages {
            spool: self.spool.clone(),
            cache: self.cache.clone(),
            demands,
            fetched: Fetched::new(),
        };
        let reading = tokio::task::spawn_blocking(move || read(Box::new(source)));
        let mut refused: Option<PageAccessError> = None;
        while let Some(Demand { pages, answer }) = asked.recv().await {
            if refused.is_none() {
                match self.fetch(pages).await {
                    Ok(fetched) => {
                        let _ = answer.send(Ok(fetched));
                        continue;
                    }
                    Err(error) => refused = Some(error),
                }
            }
            let reason = refused.as_ref().map(ToString::to_string);
            let _ = answer.send(Err(HeapFault::Transport(reason.unwrap_or_default())));
        }
        let read = match reading.await {
            Ok(read) => read,
            Err(ended) if ended.is_panic() => std::panic::resume_unwind(ended.into_panic()),
            Err(ended) => Err(HeapFault::Transport(ended.to_string())),
        };
        match refused {
            Some(refusal) => Err(refusal),
            None => read.map_err(PageAccessError::Heap),
        }
    }

    /// Fetches `missing` and, with it, the rest of every payload it
    /// reaches: the host reads a text or bytes value whole, so a payload
    /// index's pages come in with it rather than one demand each. Answers
    /// every page it fetched, and each such page the cache already held,
    /// keeping what it fetched in the cache.
    async fn fetch(&mut self, mut missing: Vec<NodeId>) -> Result<Fetched, PageAccessError> {
        let mut fetched = Fetched::new();
        while !missing.is_empty() {
            missing.sort();
            missing.dedup();
            let batch = missing.split_off(missing.len().saturating_sub(FETCH_BATCH));
            let pages = self.fetch_batch(&batch).await?;
            self.cache.keep(pages.iter().cloned());
            fetched.extend(pages.iter().cloned());
            for (id, bytes) in &pages {
                for child in payload_children(*id, bytes) {
                    if fetched.contains_key(&child)
                        || self
                            .spool
                            .as_ref()
                            .is_some_and(|spool| spool.contains(&child))
                    {
                        continue;
                    }
                    match self.cache.get(&child) {
                        Some(bytes) => {
                            fetched.insert(child, bytes);
                        }
                        None => missing.push(child),
                    }
                }
            }
        }
        Ok(fetched)
    }

    async fn fetch_batch(
        &mut self,
        missing: &[NodeId],
    ) -> Result<Vec<(NodeId, Arc<[u8]>)>, PageAccessError> {
        self.fetches += missing.len() as u64;
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

/// The bytes of committed pages a demand answered, by page.
type Fetched = HashMap<NodeId, Arc<[u8]>>;

/// Committed pages a host read waits on, with where to answer them: every
/// page fetched for them, and the rest of each payload they reach.
struct Demand {
    pages: Vec<NodeId>,
    answer: oneshot::Sender<Result<Fetched, HeapFault>>,
}

/// A host read's pages: the turn's spool, then what was fetched for the
/// read and not read yet, then the cache; any other page the read waits
/// on while the executor fetches it.
struct ParentPages {
    spool: Option<SharedSpool>,
    cache: PageCache,
    demands: mpsc::Sender<Demand>,
    /// Pages a demand answered that the read has not read yet: the rest
    /// of a payload it reads.
    fetched: Fetched,
}

/// The fault of a read whose executor no longer waits on it.
fn abandoned() -> HeapFault {
    HeapFault::Transport("the session's read was abandoned".to_owned())
}

impl ParentPages {
    /// The bytes of `id` if the read holds them without a fetch.
    fn held(&mut self, id: &NodeId) -> Result<Option<Arc<[u8]>>, HeapFault> {
        match &self.spool {
            Some(spool) if spool.contains(id) => spool
                .read(id)
                .map(Some)
                .map_err(|_| HeapFault::Missing { page: PageId(id.0) }),
            _ => Ok(self.fetched.remove(id).or_else(|| self.cache.get(id))),
        }
    }

    /// Fetches `pages`, waiting while the executor does.
    fn demand(&mut self, pages: Vec<NodeId>) -> Result<(), HeapFault> {
        let (answer, answered) = oneshot::channel();
        self.demands
            .blocking_send(Demand { pages, answer })
            .map_err(|_| abandoned())?;
        let fetched = answered.blocking_recv().map_err(|_| abandoned())??;
        self.fetched.extend(fetched);
        Ok(())
    }
}

impl PageSource for ParentPages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        let mut held = Vec::with_capacity(pages.len());
        let mut missing = Vec::new();
        for page in pages {
            let id = NodeId(page.id.0);
            let bytes = self.held(&id)?;
            if bytes.is_none() {
                missing.push(id);
            }
            held.push(bytes);
        }
        if !missing.is_empty() {
            self.demand(missing.clone())?;
        }
        let answers = pages
            .iter()
            .zip(held)
            .map(|(page, bytes)| {
                let bytes = match bytes {
                    Some(bytes) => bytes,
                    None => self
                        .fetched
                        .get(&NodeId(page.id.0))
                        .cloned()
                        .ok_or(HeapFault::Missing { page: page.id })?,
                };
                answer_page(*page, &bytes, |_| Ok(PageGrant::NONE))
            })
            .collect();
        for id in &missing {
            self.fetched.remove(id);
        }
        answers
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        if self.demands.is_closed() {
            return Err(abandoned());
        }
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
