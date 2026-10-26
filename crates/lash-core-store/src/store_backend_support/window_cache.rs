//! The windows a store decoded, shared across its later reads (FIG-5930).
//!
//! A send loads its session's window several times: the engine's turn, the
//! runtime its pass opens, the commit's publication, the resident refresh
//! and the follower's terminal report. Each read used to decode every node
//! from the frame to the leaf, so a persistent session paid O(history) per
//! send, five times over.
//!
//! Graph rows are immutable history ([`super::head_path`], facts 1 to 3): a
//! node's parent, owner, generation and body are written once, and an
//! owner's chain grows only from its head leaf. A window decoded once is
//! therefore the prefix of every later window on the same frame, and a later
//! read needs only the rows past the leaf it holds. The store still reads
//! the session's liveness, its head and the leaf and frame rows on every
//! read, so deletion, revision and the window's anchor are always judged
//! against the current rows; only the node bodies below the cached leaf are
//! reused, and only while the frame row is the one they were read under.
//!
//! The cache keeps one window per session, the newest it read, within a byte
//! budget over the stored bodies it holds; the least recently used window
//! goes first. Records are shared with every graph a read hands out
//! (`SessionGraph` copies on write), so a held window costs no copy. Durable
//! deletion removes the store's reference; held graph/read-model snapshots
//! then own their data until their last reader drops. For a retained session,
//! the last graph reader also removes the cached window. Readers share a
//! lease across window versions; the cached graph itself holds no lease,
//! so the store cannot keep its own readers alive. Reads start a cache
//! access before their database snapshot so an older snapshot cannot put a
//! deleted session back into the cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use lash_kernel_value::identity::SessionId;
use lash_protocol_types::sync::MutexExt;

use crate::session_graph::{SessionGraph, WindowAnchor};
use lash_vocabulary::identity::NodeId;

/// The stored body bytes one store's cached windows may hold together.
pub const WINDOW_CACHE_BUDGET_BYTES: u64 = 32 << 20;

/// The frame row a window is read under, as the store read it now.
#[derive(Clone, Copy, Debug)]
pub struct WindowFrame<'a> {
    pub frame_node_id: &'a str,
    pub generation: u64,
    pub external_parent: Option<&'a str>,
}

impl WindowFrame<'_> {
    fn anchors(&self, anchor: &WindowAnchor) -> bool {
        anchor.frame_node_id.as_str() == self.frame_node_id
            && anchor.generation == self.generation
            && anchor.external_parent.as_deref() == self.external_parent
    }
}

/// What a read can take from the cache for one leaf.
pub enum WindowReuse {
    /// The cached window holds the leaf: no row to read.
    Whole(SessionGraph),
    /// The cached window ends below the leaf: read the rows from
    /// `from_generation` up to the leaf and [`SessionGraph::extend_window`]
    /// `window` with them.
    Extend {
        window: SessionGraph,
        from_generation: u64,
        bytes: u64,
    },
    /// Nothing cached on this frame: read the whole window.
    Miss,
}

struct Entry {
    window: SessionGraph,
    leaf_generation: u64,
    bytes: u64,
    used: u64,
    readers: Weak<WindowReaders>,
}

impl Entry {
    fn reader_graph(&self) -> Option<SessionGraph> {
        let mut graph = self.window.clone();
        graph.set_read_owner(Some(self.readers.upgrade()?));
        Some(graph)
    }
}

/// Shared by the graphs a store read hands out, never by the cached graph.
/// An older live snapshot keeps reuse of the newest window warm; the last
/// snapshot releases that window even when its durable session is retained.
#[derive(Debug)]
pub(crate) struct WindowReaders {
    cache: Weak<WindowCache>,
    session: SessionId,
}

impl WindowReaders {
    /// Hand an acknowledged immutable window to independent readers without
    /// loading the owner's head back from SQL. Only the appended suffix is
    /// encoded for the cache's byte bound; the existing lease and deletion
    /// fence remain the authority to retain it.
    pub(crate) fn acknowledged(&self, graph: &SessionGraph) {
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        let Some(anchor) = graph.anchor() else { return };
        let Ok(index) = graph.try_cache() else { return };
        // A branched resident graph is not the single-chain store window.
        // Readers can hydrate that projection through the ordinary read.
        if index.active_path_indices.len() != graph.nodes.len() {
            return;
        }
        let (read, prefix, mut bytes) = {
            let entries = cache.entries.lock_recover();
            let Some(entry) = entries
                .by_session
                .get(&self.session)
                .filter(|entry| std::ptr::eq(entry.readers.as_ptr(), self))
            else {
                return;
            };
            let prefix = entry.window.nodes.len();
            let shared = entry.window.anchor() == Some(anchor)
                && prefix <= graph.nodes.len()
                && prefix.checked_sub(1).is_some_and(|last| {
                    graph.nodes[last].node_id == entry.window.nodes[last].node_id
                });
            (
                WindowCacheRead {
                    cache: Arc::clone(&cache),
                    invalidation: entries.invalidation,
                },
                if shared { prefix } else { 0 },
                if shared { entry.bytes } else { 0 },
            )
        };
        // Count the whole encoded row, including its identity and parent,
        // conservatively bounding the stored body without a second inventory.
        for node in graph.nodes.iter().skip(prefix) {
            let Ok(encoded) = serde_json::to_vec(node.as_ref()) else {
                return;
            };
            bytes = bytes.saturating_add(encoded.len() as u64);
        }
        let Some(offset) = graph.nodes.len().checked_sub(1) else {
            return;
        };
        let Some(generation) = anchor.generation.checked_add(offset as u64) else {
            return;
        };
        let mut independent = graph.clone();
        independent.detach_owner_prompt_rendering();
        drop(read.remember(&self.session, &independent, generation, bytes));
    }
}

impl Drop for WindowReaders {
    fn drop(&mut self) {
        let Some(cache) = self.cache.upgrade() else {
            return;
        };
        let mut entries = cache.entries.lock_recover();
        // A budget eviction or deletion may already have replaced this
        // reader's entry. Its drop must not evict a newer reader's window.
        if entries
            .by_session
            .get(&self.session)
            .is_some_and(|entry| std::ptr::eq(entry.readers.as_ptr(), self))
        {
            entries.invalidation += 1;
            entries.remove(&self.session);
            if entries.by_session.is_empty() {
                entries.by_session = HashMap::new();
            }
        }
    }
}

#[derive(Default)]
struct Entries {
    by_session: HashMap<SessionId, Entry>,
    bytes: u64,
    clock: u64,
    invalidation: u64,
}

impl Entries {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn remove(&mut self, session: &SessionId) {
        if let Some(entry) = self.by_session.remove(session) {
            self.bytes -= entry.bytes;
        }
    }
}

/// One store's decoded windows, newest per session.
pub struct WindowCache {
    budget_bytes: u64,
    entries: Mutex<Entries>,
}

impl Default for WindowCache {
    fn default() -> Self {
        Self::new(WINDOW_CACHE_BUDGET_BYTES)
    }
}

impl WindowCache {
    #[must_use]
    pub fn new(budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            entries: Mutex::new(Entries::default()),
        }
    }

    /// Begin before opening the database read snapshot. A concurrent close
    /// invalidates this read's permission to retain what it decoded.
    pub fn read(self: &Arc<Self>) -> WindowCacheRead {
        WindowCacheRead {
            cache: Arc::clone(self),
            invalidation: self.entries.lock_recover().invalidation,
        }
    }

    /// Release this session's graph and its shared projections after durable
    /// deletion. Existing snapshots keep their own references until dropped.
    /// The scalar fence prevents reads of older database snapshots from
    /// repopulating the cache, without retaining per-session tombstones.
    pub fn forget(&self, session: &SessionId) {
        let mut entries = self.entries.lock_recover();
        entries.invalidation += 1;
        entries.remove(session);
        if entries.by_session.is_empty() {
            entries.by_session = HashMap::new();
        }
    }

    /// Drop every cached window: a test that rewrote rows behind the
    /// store's back reads them again.
    pub fn forget_all(&self) {
        let mut entries = self.entries.lock_recover();
        *entries = Entries {
            invalidation: entries.invalidation + 1,
            ..Entries::default()
        };
    }
}

/// One database snapshot's access to decoded windows. A deletion may stop
/// this read from remembering its graph, but does not invalidate its result.
pub struct WindowCacheRead {
    cache: Arc<WindowCache>,
    invalidation: u64,
}

impl WindowCacheRead {
    /// What a read of `session`'s window under `frame`, ending at `leaf` at
    /// `leaf_generation`, can reuse.
    pub fn reuse(
        &self,
        session: &SessionId,
        frame: WindowFrame<'_>,
        leaf: &NodeId,
        leaf_generation: u64,
    ) -> Result<WindowReuse, crate::StoreError> {
        let mut entries = self.cache.entries.lock_recover();
        let used = entries.tick();
        let Some(entry) = entries.by_session.get_mut(session) else {
            return Ok(WindowReuse::Miss);
        };
        if !entry
            .window
            .anchor()
            .is_some_and(|anchor| frame.anchors(anchor))
            || leaf_generation < frame.generation
        {
            return Ok(WindowReuse::Miss);
        }
        entry.used = used;
        let Some(window) = entry.reader_graph() else {
            return Ok(WindowReuse::Miss);
        };
        let (cached_generation, bytes) = (entry.leaf_generation, entry.bytes);
        drop(entries);
        if leaf_generation > cached_generation {
            return Ok(WindowReuse::Extend {
                window,
                from_generation: cached_generation + 1,
                bytes,
            });
        }
        if leaf_generation == cached_generation {
            return Ok(if window.leaf_node_id.as_ref() == Some(leaf) {
                WindowReuse::Whole(window)
            } else {
                WindowReuse::Miss
            });
        }
        let prefix = usize::try_from(leaf_generation - frame.generation)
            .ok()
            .and_then(|offset| window.window_prefix(offset + 1, leaf));
        match prefix {
            Some(prefix) => prefix.map(|mut prefix| {
                prefix.set_read_owner(window.read_owner().clone());
                WindowReuse::Whole(prefix)
            }),
            None => Ok(WindowReuse::Miss),
        }
    }

    /// Keep `window`, read for `session` up to `leaf_generation` from
    /// `bytes` of stored bodies, unless the cache holds a longer window on
    /// the same frame or one on a later frame. Returns the graph with the
    /// reader lease that keeps reuse warm until its last snapshot drops.
    pub fn remember(
        &self,
        session: &SessionId,
        window: &SessionGraph,
        leaf_generation: u64,
        bytes: u64,
    ) -> SessionGraph {
        let mut entries = self.cache.entries.lock_recover();
        if entries.invalidation != self.invalidation {
            drop(entries);
            return self.attach(session, window.clone());
        }
        if let Some(held) = entries.by_session.get(session) {
            let newer = match (window.anchor(), held.window.anchor()) {
                (None, _) => false,
                (Some(_), None) => true,
                (Some(anchor), Some(held_anchor)) => {
                    if held_anchor.frame_node_id == anchor.frame_node_id {
                        leaf_generation > held.leaf_generation
                    } else {
                        anchor.generation > held_anchor.generation
                    }
                }
            };
            if !newer {
                drop(entries);
                return self.attach(session, window.clone());
            }
        }
        let readers = entries
            .by_session
            .get(session)
            .map(|entry| entry.readers.clone())
            .unwrap_or_default();
        entries.remove(session);
        if bytes > self.cache.budget_bytes {
            drop(entries);
            return self.attach(session, window.clone());
        }
        while entries.bytes + bytes > self.cache.budget_bytes {
            let Some(oldest) = entries
                .by_session
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(session, _)| session.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
        let mut cached = window.clone();
        cached.set_read_owner(None);
        let used = entries.tick();
        entries.bytes += bytes;
        entries.by_session.insert(
            session.clone(),
            Entry {
                window: cached,
                leaf_generation,
                bytes,
                used,
                readers,
            },
        );
        drop(entries);
        self.attach(session, window.clone())
    }

    /// Attach every returned graph to its session's reader lease, including
    /// the empty head an open session holds before its first turn. The same
    /// lease keeps later anchored windows warm until the last reader drops.
    /// A read invalidated by deletion owns its result without retaining it.
    pub fn attach(&self, session: &SessionId, mut graph: SessionGraph) -> SessionGraph {
        let mut entries = self.cache.entries.lock_recover();
        let retain = entries.invalidation == self.invalidation;
        let readers = retain
            .then(|| {
                entries
                    .by_session
                    .get(session)
                    .and_then(|entry| entry.readers.upgrade())
                    .or_else(|| {
                        graph
                            .read_owner()
                            .as_ref()
                            .filter(|owner| {
                                owner.session == *session
                                    && owner.cache.ptr_eq(&Arc::downgrade(&self.cache))
                            })
                            .cloned()
                    })
            })
            .flatten()
            .unwrap_or_else(|| {
                Arc::new(WindowReaders {
                    cache: Arc::downgrade(&self.cache),
                    session: session.clone(),
                })
            });
        if retain {
            let used = entries.tick();
            let entry = entries
                .by_session
                .entry(session.clone())
                .or_insert_with(|| Entry {
                    window: SessionGraph::default(),
                    leaf_generation: 0,
                    bytes: 0,
                    used,
                    readers: Weak::new(),
                });
            entry.readers = Arc::downgrade(&readers);
        }
        // Replacing a graph's previous lease can drop its last reader, whose
        // destructor takes this lock. Ownership is changed only after unlock.
        drop(entries);
        graph.set_read_owner(Some(readers));
        graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-6208: opening the empty head owns the same lease as its first
    /// committed window, even after the turn's own reader drops at idle.
    #[test]
    fn an_empty_reader_keeps_the_first_anchored_window_warm_until_it_drops() {
        let mut state = crate::RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(16),
            crate::NoProgressBudget::bounded(12),
        ));
        state.ensure_agent_frame_initialized();
        let leaf = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("frame leaf");
        let graph = SessionGraph::from_window(
            state
                .session_graph
                .nodes
                .iter()
                .map(|node| (**node).clone())
                .collect(),
            leaf.clone(),
            WindowAnchor {
                frame_node_id: crate::FrameNodeId::new(leaf.clone()).expect("frame"),
                generation: 0,
                external_parent: None,
                previous_frame_node_id: None,
            },
        )
        .expect("window");
        let cache = Arc::new(WindowCache::new(1024));
        let empty = cache
            .read()
            .remember(&state.session_id, &SessionGraph::default(), 0, 0);
        assert!(
            empty.read_owner().is_some(),
            "an empty reader owns the session lease"
        );
        assert_eq!(cache.entries.lock_recover().bytes, 0);
        let anchored = cache.read().remember(&state.session_id, &graph, 0, 512);
        drop(anchored);
        assert_eq!(
            cache.entries.lock_recover().bytes,
            512,
            "the open empty reader keeps the first window warm"
        );
        // A read of the earlier empty head cannot replace the newest window.
        let earlier = cache
            .read()
            .remember(&state.session_id, &SessionGraph::default(), 0, 0);
        assert_eq!(cache.entries.lock_recover().bytes, 512);
        drop(earlier);
        drop(empty);
        assert!(
            cache.entries.lock_recover().by_session.is_empty(),
            "last-reader drop releases the window"
        );
    }

    /// FIG-6168: a reader of the pre-close database snapshot may finish
    /// decoding after close; it must not resurrect the closed cache entry.
    #[test]
    fn a_read_started_before_close_cannot_retain_its_decoded_window() {
        let mut state = crate::RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(16),
            crate::NoProgressBudget::bounded(12),
        ));
        state.ensure_agent_frame_initialized();
        let leaf = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("frame leaf");
        let graph = SessionGraph::from_window(
            state
                .session_graph
                .nodes
                .iter()
                .map(|node| (**node).clone())
                .collect(),
            leaf.clone(),
            WindowAnchor {
                frame_node_id: crate::FrameNodeId::new(leaf.clone()).expect("frame"),
                generation: 0,
                external_parent: None,
                previous_frame_node_id: None,
            },
        )
        .expect("window");
        let cache = Arc::new(WindowCache::new(1024));
        let read = cache.read();
        let old_reader = read.remember(&state.session_id, &graph, 0, 512);
        assert_eq!(cache.entries.lock_recover().bytes, 512);
        cache.forget(&state.session_id);
        read.remember(&state.session_id, &graph, 0, 512);
        assert_eq!(
            cache.entries.lock_recover().bytes,
            0,
            "the old database snapshot cannot repopulate a closed cache"
        );
        assert!(cache.entries.lock_recover().by_session.is_empty());
        // Invalidation stores no session tombstone: an unrelated live
        // session (or a cache-only eviction) can load and retain afresh.
        let fresh = cache.read();
        let fresh_reader = fresh.remember(&state.session_id, &graph, 0, 512);
        assert_eq!(cache.entries.lock_recover().bytes, 512);
        drop(old_reader);
        assert_eq!(
            cache.entries.lock_recover().bytes,
            512,
            "a pre-close reader cannot evict the new reader's window"
        );
        cache.forget_all();
        fresh.remember(&state.session_id, &graph, 0, 512);
        assert_eq!(cache.entries.lock_recover().bytes, 0);
        drop(fresh_reader);
    }
}
