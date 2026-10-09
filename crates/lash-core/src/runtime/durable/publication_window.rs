//! The window between a durable commit and its publication to a replay
//! store, and the one rule a feed closes it by (FIG-5605, FIG-5626).
//!
//! A commit is durable before its observation is published, so a reader
//! that compares its cursor with the durable head inside that window finds
//! the head ahead of everything the replay holds. The node that commits
//! holds a mark for as long as the publication is its to make
//! ([`PublicationMarks::hold`]). A reader on the node takes the head's
//! window right after it read the head and before it reads the replay
//! ([`PublicationMarks::window`]), and when the replay does not bridge to
//! that head it asks the window to close ([`PublicationWindow::closed`]):
//!
//! - a mark was held: the reader waits for every such mark to drop and
//!   judges again, from a new read of the head. A head that moved on in the
//!   meantime has a window of its own, so the wait for one commit never
//!   stands for the next;
//! - no mark was held: another node made the commit, or this node's
//!   publication was already attempted before the replay was read, and the
//!   replay is all there will be. The reader falls back: a session feed
//!   answers its typed gap, a process feed reconciles from the durable log.
//!
//! A fixed head whose publication failed is judged at most twice: its mark
//! is gone the second time.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt as _;

/// The commits a node is still publishing, per subject: each by the
/// revision it stands on.
pub struct PublicationMarks<S, R> {
    held: Mutex<HashMap<S, Vec<R>>>,
    dropped: tokio::sync::Notify,
}

impl<S, R> Default for PublicationMarks<S, R> {
    fn default() -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            dropped: tokio::sync::Notify::new(),
        }
    }
}

impl<S: Clone + Eq + Hash, R: Copy + Ord> PublicationMarks<S, R> {
    /// Mark a commit of `subject` over `base` as this node's to publish,
    /// until the returned mark is dropped: once its publication was
    /// attempted, or the commit was given up. A commit is marked before a
    /// reader that waits on it can learn of it.
    pub fn hold(self: &Arc<Self>, subject: &S, base: R) -> PublicationMark<S, R> {
        self.held
            .lock_recover()
            .entry(subject.clone())
            .or_default()
            .push(base);
        PublicationMark {
            marks: Arc::clone(self),
            subject: subject.clone(),
            base,
        }
    }

    /// The publication window of `subject`'s durable `head`, which the
    /// caller read just now and has not yet compared with the replay.
    pub fn window(self: &Arc<Self>, subject: &S, head: R) -> PublicationWindow<S, R> {
        PublicationWindow {
            open: self.publishing(subject, head),
            marks: Arc::clone(self),
            subject: subject.clone(),
            head,
        }
    }

    /// Whether this node is publishing a commit that could have moved
    /// `subject` to `head`: one over a revision before it. A commit over
    /// `head` itself has not landed, so a reader inside the pass that will
    /// make it never waits on that pass.
    fn publishing(&self, subject: &S, head: R) -> bool {
        self.held
            .lock_recover()
            .get(subject)
            .is_some_and(|bases| bases.iter().any(|base| *base < head))
    }
}

/// One commit its node is publishing; dropping it wakes every reader held
/// on it.
pub struct PublicationMark<S: Eq + Hash, R: PartialEq> {
    marks: Arc<PublicationMarks<S, R>>,
    subject: S,
    base: R,
}

impl<S: Eq + Hash, R: PartialEq> Drop for PublicationMark<S, R> {
    fn drop(&mut self) {
        let mut held = self.marks.held.lock_recover();
        if let Some(bases) = held.get_mut(&self.subject) {
            if let Some(index) = bases.iter().position(|base| *base == self.base) {
                bases.swap_remove(index);
            }
            if bases.is_empty() {
                held.remove(&self.subject);
            }
        }
        drop(held);
        self.marks.dropped.notify_waiters();
    }
}

/// Whether a node was publishing a commit behind one head of one subject
/// when a reader read that head.
pub struct PublicationWindow<S, R> {
    marks: Arc<PublicationMarks<S, R>>,
    subject: S,
    head: R,
    open: bool,
}

impl<S: Clone + Eq + Hash, R: Copy + Ord> PublicationWindow<S, R> {
    /// Close the window of a head the replay does not bridge to. `true`
    /// once every publication that was in flight behind the head was
    /// attempted, whatever its result: the reader judges again from a new
    /// read of the head. `false`, at once, when none was in flight: the
    /// replay the reader holds is final for this head.
    pub async fn closed(self) -> bool {
        if !self.open {
            return false;
        }
        loop {
            let dropped = self.marks.dropped.notified();
            tokio::pin!(dropped);
            dropped.as_mut().enable();
            if !self.marks.publishing(&self.subject, self.head) {
                return true;
            }
            dropped.await;
        }
    }
}
