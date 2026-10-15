//! The committed pages a parent process read, kept by content address
//! (FIG-6321).
//!
//! A committed page is immutable and named by its digest, so the bytes a
//! session's reads or an activation's page service fetched once answer
//! every later read of the same page in the process, whichever turn or
//! activation asks: a turn reads from the store only the pages that changed
//! since the last one read them. What a read is allowed to ask for is
//! decided where it is asked (a capability, a lease); the cache only spares
//! the store the fetch. The least recently used pages go first once the
//! cache holds its byte budget.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use lash_core_store::checkpoint_graph::NodeId;
use lash_durable::ActorKey;

/// The bytes a process keeps of the committed pages it read, by default.
pub const PAGE_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// The most sessions a process keeps hints for.
const HINTED_SESSIONS: usize = 1024;
/// The most pages a session's hints name: more than a warm start's budget
/// carries, so a long activation's reads cannot grow them, and a process's
/// hints stay within 8 MiB of page ids.
const HINTED_PAGES: usize = 256;

/// A process's committed pages, shared by every reader that holds a clone.
#[derive(Clone)]
pub struct PageCache(Arc<Mutex<Pages>>);

struct Pages {
    budget: u64,
    bytes: u64,
    tick: u64,
    pages: HashMap<NodeId, (Arc<[u8]>, u64)>,
    order: BTreeMap<u64, NodeId>,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new(PAGE_CACHE_BYTES)
    }
}

impl std::fmt::Debug for PageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pages = self.lock();
        f.debug_struct("PageCache")
            .field("pages", &pages.pages.len())
            .field("bytes", &pages.bytes)
            .finish()
    }
}

impl PageCache {
    /// A cache that keeps at most `budget` bytes of pages.
    #[must_use]
    pub fn new(budget: u64) -> Self {
        Self(Arc::new(Mutex::new(Pages {
            budget,
            bytes: 0,
            tick: 0,
            pages: HashMap::new(),
            order: BTreeMap::new(),
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pages> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes of `page`, when a reader fetched them.
    #[must_use]
    pub fn get(&self, page: &NodeId) -> Option<Arc<[u8]>> {
        let mut pages = self.lock();
        pages.tick += 1;
        let tick = pages.tick;
        let (bytes, used) = pages.pages.get_mut(page)?;
        let (bytes, last) = (Arc::clone(bytes), std::mem::replace(used, tick));
        pages.order.remove(&last);
        pages.order.insert(tick, *page);
        Some(bytes)
    }

    /// Whether the cache holds `page`, without touching its recency.
    #[must_use]
    pub fn contains(&self, page: &NodeId) -> bool {
        self.lock().pages.contains_key(page)
    }

    /// Keeps `pages`, committed pages a reader fetched from the store.
    pub fn keep(&self, pages: impl IntoIterator<Item = (NodeId, Arc<[u8]>)>) {
        let mut cache = self.lock();
        for (id, bytes) in pages {
            let size = bytes.len() as u64;
            if size > cache.budget || cache.pages.contains_key(&id) {
                continue;
            }
            while cache.bytes + size > cache.budget {
                let Some((_, oldest)) = cache.order.pop_first() else {
                    break;
                };
                if let Some((evicted, _)) = cache.pages.remove(&oldest) {
                    cache.bytes -= evicted.len() as u64;
                }
            }
            cache.tick += 1;
            let tick = cache.tick;
            cache.order.insert(tick, id);
            cache.bytes += size;
            cache.pages.insert(id, (bytes, tick));
        }
    }

    /// How many pages and bytes the cache holds.
    #[must_use]
    pub fn usage(&self) -> (usize, u64) {
        let pages = self.lock();
        (pages.pages.len(), pages.bytes)
    }
}

/// The pages one session's last activation read, warmed or sealed: what
/// its next activation's start carries warm, where its roots still reach
/// them ([`super::ActivationPages::warm`]). A turn reads much what the last
/// one read, or the new version the last one sealed.
#[derive(Clone, Default)]
pub struct PageHints(Arc<Mutex<BTreeSet<NodeId>>>);

impl std::fmt::Debug for PageHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PageHints")
            .field(&self.lock().len())
            .finish()
    }
}

impl PageHints {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<NodeId>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The hints so far, leaving none.
    #[must_use]
    pub fn take(&self) -> BTreeSet<NodeId> {
        std::mem::take(&mut *self.lock())
    }

    /// Notes `pages` as touched.
    pub fn extend(&self, pages: impl IntoIterator<Item = NodeId>) {
        let mut hints = self.lock();
        for page in pages {
            if hints.len() >= HINTED_PAGES {
                break;
            }
            hints.insert(page);
        }
    }
}

/// Each session's [`PageHints`], by its actor: a session's turns are
/// separate activations, often of separate session objects, in one process.
#[derive(Clone, Default)]
pub struct HintBook(Arc<Mutex<BTreeMap<ActorKey, PageHints>>>);

impl std::fmt::Debug for HintBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HintBook").field(&self.lock().len()).finish()
    }
}

impl HintBook {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ActorKey, PageHints>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The hints of `actor`'s session. A book that holds as many sessions
    /// as it may forgets one; its next activation warms only its roots.
    #[must_use]
    pub fn of(&self, actor: &ActorKey) -> PageHints {
        let mut book = self.lock();
        if !book.contains_key(actor) && book.len() >= HINTED_SESSIONS {
            book.pop_first();
        }
        book.entry(actor.clone()).or_default().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(n: u8, len: usize) -> (NodeId, Arc<[u8]>) {
        (NodeId([n; 32]), Arc::from(vec![n; len]))
    }

    /// The cache never holds more than its budget, and what it drops is
    /// what was used least recently.
    #[test]
    fn the_cache_keeps_its_byte_budget_and_evicts_the_least_recently_used() {
        let cache = PageCache::new(300);
        cache.keep([page(1, 100), page(2, 100), page(3, 100)]);
        assert!(cache.get(&NodeId([1; 32])).is_some());
        cache.keep([page(4, 100)]);
        assert_eq!(cache.usage(), (3, 300));
        assert!(cache.contains(&NodeId([1; 32])));
        assert!(!cache.contains(&NodeId([2; 32])));
        cache.keep([page(5, 400)]);
        assert!(!cache.contains(&NodeId([5; 32])));
        assert_eq!(cache.usage(), (3, 300));
    }
}
