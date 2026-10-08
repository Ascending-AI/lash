//! Node wakes (L8, FIG-5178): cross-node wake hints sent after commit, and
//! fast crash detection. Neither is ever a fence or a correctness input.
//!
//! - **Wakes.** A node coalesces what its commits woke into one
//!   [`WakeBatch`] per flush and publishes it after those commits. A
//!   readied unowned actor rings one claim loop: the writer's own node's
//!   while it has a free slot, in process, or else the one live node the
//!   actor's key hashes to. Mail for an owned actor rings its owner's node
//!   only, and a writer on the owner's own node hints in process and
//!   publishes nothing. A lost or late hint costs latency only: the claim
//!   poll and each owner's mail scan find the work.
//! - **Liveness.** A node's [`NodeWakeFeed`] holds its boot's liveness lock
//!   for as long as the feed's session lives. A watcher that saw a boot's
//!   lock held, and later sees it free while the boot is still registered,
//!   reaps that boot at once instead of waiting for its lease to lapse. The
//!   epoch stays the only fence: the reap bumps it, exactly as a lease reap
//!   does, and a boot that is merely slow is fenced, never trusted.
//!
//! A dialect without a notification channel (SQLite: one node per database)
//! has no `NodeWakes`; its runner relies on in-process hints and the polls.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::DurableError;
use crate::ids::{ActorKey, NodeId};
use crate::port::{NodeLease, Owner, Reaped};

/// One flush of a node's wake hints, coalesced per channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeBatch {
    /// Nodes picked to claim readied unowned actors: each one's claim loop
    /// is rung once.
    pub ready: BTreeSet<NodeId>,
    /// Owned actors that took mail, under the node that owns each.
    pub owned: BTreeMap<NodeId, BTreeSet<ActorKey>>,
}

impl WakeBatch {
    /// Whether the batch rings nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.owned.is_empty()
    }
}

/// What a node's listener heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeWakeEvent {
    /// This node was picked to claim some readied unowned actor: claim
    /// now, unless it runs as many actors as it may.
    Ready,
    /// These actors, owned by this node, took mail.
    Owned(Vec<ActorKey>),
    /// The listener lost its session and has a new one: every hint sent in
    /// between is lost and every liveness observation is stale. Rescan the
    /// claimable actors and every hot actor's mailbox, and see each boot's
    /// lock afresh.
    Resubscribed,
}

/// One registered boot's liveness lock, as a probe saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootLiveness {
    /// The boot.
    pub boot: Owner,
    /// Whether some session holds its lock.
    pub held: bool,
}

/// A dialect's node wakes. Every method is a latency or liveness aid; the
/// epoch fences and the polls stay correct without any of them.
#[async_trait::async_trait]
pub trait NodeWakes: Send + Sync + 'static {
    /// Send `batch` to the nodes it names. Called only after the commits
    /// that woke its actors, never inside a writing transaction.
    async fn publish(&self, batch: &WakeBatch) -> Result<(), DurableError>;

    /// Open `lease`'s listener: subscribe to the node's own channel, and
    /// take the boot's liveness lock on the listener's session. Returns
    /// once both are in place, so a scan that follows misses no hint sent
    /// after it.
    async fn listen(&self, lease: &NodeLease) -> Result<Box<dyn NodeWakeFeed>, DurableError>;

    /// Every registered boot's liveness lock, held or free, now.
    async fn liveness(&self) -> Result<Vec<BootLiveness>, DurableError>;

    /// Reap `boot` when its liveness lock is free, in one transaction:
    /// delete its node row and release its actors with their epochs bumped,
    /// as a lease reap does. Nothing when the lock is held again, or when
    /// the reaper's own lock is not held (its listener has no live session,
    /// so its view of the others is stale). Refused with
    /// [`DurableError::NodeLeaseLost`] when `reaper` holds no live lease.
    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError>;
}

/// A node's listener: its wake events in arrival order, and the liveness
/// lock it holds while it lives. Dropping it ends the listener's session,
/// which releases the lock.
#[async_trait::async_trait]
pub trait NodeWakeFeed: Send {
    /// The next wake event. Cancel-safe: a dropped call loses nothing.
    async fn next(&mut self) -> NodeWakeEvent;

    /// How many times the listener has lost its session. A liveness
    /// observation made under an earlier session is stale.
    fn session(&self) -> u64;
}
