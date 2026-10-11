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
//! (`SessionGraph` copies on write), so a held window costs no copy.

use std::collections::HashMap;
use std::sync::Mutex;

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

use crate::NodeId;
use crate::session_graph::{SessionGraph, WindowAnchor};

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
}

#[derive(Default)]
struct Entries {
    by_session: HashMap<SessionId, Entry>,
    bytes: u64,
    clock: u64,
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

    /// What a read of `session`'s window under `frame`, ending at `leaf` at
    /// `leaf_generation`, can reuse.
    pub fn reuse(
        &self,
        session: &SessionId,
        frame: WindowFrame<'_>,
        leaf: &NodeId,
        leaf_generation: u64,
    ) -> Result<WindowReuse, crate::StoreError> {
        let mut entries = self.entries.lock_recover();
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
        let (window, cached_generation, bytes) =
            (entry.window.clone(), entry.leaf_generation, entry.bytes);
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
            Some(prefix) => prefix.map(WindowReuse::Whole),
            None => Ok(WindowReuse::Miss),
        }
    }

    /// Keep `window`, read for `session` up to `leaf_generation` from
    /// `bytes` of stored bodies, unless the cache holds a longer window on
    /// the same frame or one on a later frame.
    pub fn remember(
        &self,
        session: &SessionId,
        window: &SessionGraph,
        leaf_generation: u64,
        bytes: u64,
    ) {
        let Some(anchor) = window.anchor() else {
            return;
        };
        let mut entries = self.entries.lock_recover();
        if let Some(held) = entries.by_session.get(session)
            && let Some(held_anchor) = held.window.anchor()
        {
            let newer = if held_anchor.frame_node_id == anchor.frame_node_id {
                leaf_generation > held.leaf_generation
            } else {
                anchor.generation > held_anchor.generation
            };
            if !newer {
                return;
            }
        }
        entries.remove(session);
        if bytes > self.budget_bytes {
            return;
        }
        while entries.bytes + bytes > self.budget_bytes {
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
        let used = entries.tick();
        entries.bytes += bytes;
        entries.by_session.insert(
            session.clone(),
            Entry {
                window: window.clone(),
                leaf_generation,
                bytes,
                used,
            },
        );
    }

    /// Drop every cached window: a test that rewrote rows behind the
    /// store's back reads them again.
    pub fn forget_all(&self) {
        *self.entries.lock_recover() = Entries::default();
    }
}
