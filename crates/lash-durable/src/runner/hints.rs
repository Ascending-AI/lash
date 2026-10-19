//! A node's wake hints: what its mailbox writers hand it after their
//! commits ([`Hints`], [`WakeSink`]), and the activations it runs, by actor,
//! with what each is doing as its claims see it.
//!
//! An owner with nothing to do stays hot, but its slot is room: a claim may
//! take actors in its place, and asks it to yield, so idle owners never
//! hold readied work off a full node (FIG-6890). A claim that is due while
//! the node has no room at all is owed: the next owner to wait idle, or the
//! next activation to end, rings the claim loop, so no readied actor's wake
//! is lost to a full node.

use super::Wake;
use crate::ids::{ActorKey, NodeId};
use crate::node_wakes::WakeBatch;
use crate::port::{ActorState, MailCommit, Owner, Woken};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use tokio::sync::Notify;

/// Where a store set's producer commits hand what they woke, after the
/// commit: the session work a host writes beside the session's wake (ADR
/// 0132 §12) outside any mailbox transaction, such as an accepted send.
/// Each node this process serves over the store set attaches its [`Hints`]
/// while it serves. A woken actor goes to the node that owns it, hinted in
/// process; any other goes to one serving node, which publishes it to its
/// owner's node or rings a claim as [`Hints::wake`] does. With no node
/// attached the wake is dropped, and the polls find the work.
#[derive(Clone, Default)]
pub struct WakeSink {
    inner: Arc<WakeSinkInner>,
}

#[derive(Default)]
struct WakeSinkInner {
    /// The attached nodes' hints, by attachment.
    nodes: Mutex<Vec<(u64, Hints)>>,
    next: AtomicU64,
}

impl WakeSink {
    /// Hand `hints` what this sink is handed, until the answer drops.
    #[must_use = "the node hears the sink's wakes only while the attachment lives"]
    pub fn attach(&self, hints: &Hints) -> WakeAttachment {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        self.nodes().push((id, hints.clone()));
        WakeAttachment {
            sink: Arc::clone(&self.inner),
            id,
        }
    }

    /// Deliver one actor a committed producer transaction woke.
    pub fn wake(&self, woken: &Woken) {
        let hints = {
            let nodes = self.nodes();
            let owner = woken
                .owner
                .as_ref()
                .and_then(|owner| nodes.iter().find(|(_, hints)| hints.is_mine(owner)));
            owner
                .or_else(|| nodes.first())
                .map(|(_, hints)| hints.clone())
        };
        if let Some(hints) = hints {
            hints.wake(woken);
        }
    }

    /// Plan transaction-local remote hints without notifying a listener or
    /// reserving a claim slot. With no attached runner, route readiness among
    /// the live nodes the transaction read; an owned actor names its owner.
    #[must_use]
    pub fn plan(&self, woken: &Woken, peers: &[NodeId]) -> WakeBatch {
        let nodes = self.nodes();
        let hints = woken
            .owner
            .as_ref()
            .and_then(|owner| nodes.iter().find(|(_, hints)| hints.is_mine(owner)))
            .or_else(|| nodes.first())
            .map(|(_, hints)| hints);
        let mut batch = WakeBatch::default();
        match &woken.owner {
            Some(owner) if hints.is_some_and(|hints| hints.is_mine(owner)) => {}
            Some(owner) => {
                batch
                    .owned
                    .entry(owner.node.clone())
                    .or_default()
                    .insert(woken.actor.clone());
            }
            None if woken.state == ActorState::Ready => {
                let peer = match hints {
                    Some(hints) if hints.inner.room.load(Ordering::Acquire) > 0 => None,
                    Some(hints) => hints.peer_for(&woken.actor),
                    None => peers
                        .iter()
                        .max_by_key(|peer| rendezvous(&woken.actor, peer))
                        .cloned(),
                };
                batch.ready.extend(peer);
            }
            None => {}
        }
        batch
    }

    fn nodes(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Hints)>> {
        self.inner
            .nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for WakeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakeSink")
            .field("nodes", &self.nodes().len())
            .finish()
    }
}

/// A node's attachment to a [`WakeSink`]: dropping it detaches the node.
pub struct WakeAttachment {
    sink: Arc<WakeSinkInner>,
    id: u64,
}

impl Drop for WakeAttachment {
    fn drop(&mut self) {
        self.sink
            .nodes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}

impl std::fmt::Debug for WakeAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakeAttachment").finish_non_exhaustive()
    }
}

#[derive(Default)]
pub(super) struct HintsInner {
    /// The activations this node runs.
    slots: Mutex<Slots>,
    /// Rings the claim loop.
    pub(super) claim: Notify,
    /// This node's boot, once registered: the owner a local hint serves.
    boot: Mutex<Option<Owner>>,
    /// Whether woken actors owned elsewhere, or readied, are published.
    pub(super) publishing: AtomicBool,
    /// The slots this node offers readied actors until its next claim: its
    /// room ([`Hints::room`]), none while it drains. Each readied actor hinted to
    /// this node's own claim loop takes one.
    room: AtomicUsize,
    /// The other nodes whose listeners this node's last liveness probe saw
    /// live, sorted: where a readied actor goes when this node has no room.
    peers: Mutex<Vec<NodeId>>,
    /// What the next flush publishes.
    pending: Mutex<WakeBatch>,
    /// Who hears of the logs other nodes appended to.
    followers: Mutex<Option<Arc<dyn LogFollowers>>>,
    /// Rings the publisher.
    pub(super) flush: Notify,
}

/// A node's followers of actor event logs: what its listener heard of logs
/// that grew on any node. A latency aid only: a follower still re-reads its
/// log on its own cadence.
pub trait LogFollowers: Send + Sync + 'static {
    /// These actors' logs grew.
    fn appended(&self, actors: &[ActorKey]);

    /// The listener lost hints: any followed log may have grown.
    fn resubscribed(&self);
}

/// A wake's delivery: a mailbox writer on this node hands the runner what
/// its commit woke, after the commit. An actor this boot runs is hinted in
/// process and nothing is published; a readied unowned actor rings one
/// claim loop: this node's while it has room, or else, with
/// [`NodeWakes`](crate::NodeWakes), the one live peer its key hashes to, or
/// else this node's, which owes the claim until room opens; an actor another node
/// owns is published to that node alone. Publishes coalesce until the
/// publisher's next flush.
#[derive(Clone, Default)]
pub struct Hints {
    pub(super) inner: Arc<HintsInner>,
}

impl Hints {
    /// Deliver everything `commit` woke.
    pub fn woke(&self, commit: &MailCommit) {
        for woken in &commit.woken {
            self.wake(woken);
        }
    }

    /// Deliver one woken actor.
    pub fn wake(&self, woken: &Woken) {
        match &woken.owner {
            Some(owner) if self.is_mine(owner) => {
                if !self.hint_running(&woken.actor) {
                    self.inner.claim.notify_one();
                }
            }
            Some(owner) => self.queue(|batch| {
                batch
                    .owned
                    .entry(owner.node.clone())
                    .or_default()
                    .insert(woken.actor.clone());
            }),
            None if woken.state == ActorState::Ready => {
                if self.take_room() {
                    self.inner.claim.notify_one();
                } else if let Some(peer) = self.peer_for(&woken.actor) {
                    self.queue(|batch| {
                        batch.ready.insert(peer);
                    });
                } else {
                    // No room offered and no peer to take it: the claim
                    // loop decides under the slots' lock, and owes the claim
                    // if it finds none.
                    self.inner.claim.notify_one();
                }
            }
            None => {}
        }
    }

    /// Tell every listening node that a commit grew `actor`'s event log.
    /// Called after the commit; nothing is published without node wakes.
    pub fn appended(&self, actor: ActorKey) {
        self.queue(|batch| {
            batch.appended.insert(actor);
        });
    }

    /// Hand what this node's listener hears of appended logs to
    /// `followers`.
    pub fn follow(&self, followers: Arc<dyn LogFollowers>) {
        *self
            .inner
            .followers
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(followers);
    }

    pub(super) fn followers(&self) -> Option<Arc<dyn LogFollowers>> {
        self.inner
            .followers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn is_mine(&self, owner: &Owner) -> bool {
        self.inner
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            == Some(owner)
    }

    /// Take one of the slots this node offers readied actors, if any is
    /// left.
    /// Serve local hints as `owner`, this node's registered boot.
    pub(super) fn serve_as(&self, owner: Owner) {
        *self
            .inner
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(owner);
    }

    pub(super) fn take_room(&self) -> bool {
        self.inner
            .room
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
                room.checked_sub(1)
            })
            .is_ok()
    }

    pub(super) fn offer_room(&self, room: usize) {
        self.inner.room.store(room, Ordering::Release);
    }

    /// The live peer `actor` hashes to: the highest rendezvous score, so a
    /// peer joining or leaving moves only the actors that hash to it.
    pub(super) fn peer_for(&self, actor: &ActorKey) -> Option<NodeId> {
        self.inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .max_by_key(|peer| rendezvous(actor, peer))
            .cloned()
    }

    pub(super) fn see_peers(&self, peers: Vec<NodeId>) {
        *self
            .inner
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = peers;
    }

    pub(super) fn hint_running(&self, actor: &ActorKey) -> bool {
        self.slots()
            .by_actor
            .get(actor)
            .map(|slot| slot.hint.notify_one())
            .is_some()
    }

    pub(super) fn hint_all_running(&self) {
        for slot in self.slots().by_actor.values() {
            slot.hint.notify_one();
        }
    }

    fn queue(&self, add: impl FnOnce(&mut WakeBatch)) {
        if !self.inner.publishing.load(Ordering::Acquire) {
            return;
        }
        add(&mut self
            .inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner));
        self.inner.flush.notify_one();
    }

    pub(super) fn take_pending(&self) -> WakeBatch {
        std::mem::take(
            &mut *self
                .inner
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    pub(super) fn start(&self, actor: ActorKey) -> Arc<Notify> {
        let hint = Arc::new(Notify::new());
        let mut slots = self.slots();
        slots.claims += 1;
        let order = slots.claims;
        slots.by_actor.insert(
            actor,
            Slot {
                hint: Arc::clone(&hint),
                order,
                idle: None,
                yielding: false,
            },
        );
        hint
    }

    /// The activation of `actor` ended: its slot is free, and a claim owed
    /// room is rung.
    pub(super) fn end(&self, actor: &ActorKey) {
        let mut slots = self.slots();
        slots.by_actor.remove(actor);
        if slots.owed {
            self.inner.claim.notify_one();
        }
    }

    pub(super) fn running(&self, actor: &ActorKey) -> bool {
        self.slots().by_actor.contains_key(actor)
    }

    /// The actors a claim may take on this node of `max_active`: its
    /// capacity less the owners it keeps that have work. An idle owner's
    /// slot is room; [`Hints::make_room`] asks it to yield.
    pub(super) fn room(&self, max_active: usize) -> usize {
        self.slots().room(max_active)
    }

    /// [`Hints::room`] for a claim that is due now. With none, the claim
    /// is owed: the next owner to wait idle, or the next activation to end,
    /// rings the claim loop. Read and recorded under one lock, so an owner
    /// that goes idle meanwhile sees the debt.
    pub(super) fn claim_room(&self, max_active: usize) -> usize {
        let mut slots = self.slots();
        let room = slots.room(max_active);
        slots.owed = room == 0;
        room
    }

    /// A claim took all the room it had and may have left readied actors
    /// behind: the next room that opens claims again.
    pub(super) fn owe_claim(&self) {
        self.slots().owed = true;
    }

    /// Ask owners to yield their slots until the node keeps at most
    /// `max_active`: idle owners first, the longest idle first, then the
    /// owners with work in claim order. A yielding owner finishes what it is
    /// doing and releases its actor at its next idle pass.
    pub(super) fn make_room(&self, max_active: usize) {
        let mut slots = self.slots();
        let kept = slots.by_actor.values().filter(|slot| !slot.yielding);
        let excess = kept.count().saturating_sub(max_active);
        if excess == 0 {
            return;
        }
        let mut kept: Vec<&mut Slot> = slots
            .by_actor
            .values_mut()
            .filter(|slot| !slot.yielding)
            .collect();
        kept.sort_by_key(|slot| (slot.idle.is_none(), slot.idle, slot.order));
        for slot in kept.into_iter().take(excess) {
            slot.yielding = true;
            slot.hint.notify_one();
        }
    }

    /// The owner of `actor` waits idle, with nothing to do since `since`:
    /// its slot is room, and a claim owed room is rung. `false` when the
    /// node already asked it to yield.
    pub(super) fn enter_idle(&self, actor: &ActorKey, since: Instant) -> bool {
        let mut slots = self.slots();
        let owed = slots.owed;
        let Some(slot) = slots.by_actor.get_mut(actor) else {
            return true;
        };
        if slot.yielding {
            return false;
        }
        slot.idle = Some(since);
        if owed {
            self.inner.claim.notify_one();
        }
        true
    }

    /// The owner of `actor` stops waiting idle: whether the node asked it
    /// to yield.
    pub(super) fn leave_idle(&self, actor: &ActorKey) -> bool {
        self.slots().by_actor.get_mut(actor).is_some_and(|slot| {
            slot.idle = None;
            slot.yielding
        })
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.inner
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The activations a node runs, by actor, as its hints and its claims see
/// them.
#[derive(Default)]
struct Slots {
    by_actor: HashMap<ActorKey, Slot>,
    /// How many activations this node started: each slot's place in claim
    /// order.
    claims: u64,
    /// A claim is owed room: one was due while the node had none, or took
    /// all it had.
    owed: bool,
}

impl Slots {
    fn room(&self, max_active: usize) -> usize {
        let busy = self
            .by_actor
            .values()
            .filter(|slot| !slot.yielding && slot.idle.is_none())
            .count();
        max_active.saturating_sub(busy)
    }
}

/// One running activation.
struct Slot {
    hint: Arc<Notify>,
    /// Its place in claim order.
    order: u64,
    /// Since when its owner has had nothing to do, while it waits idle.
    idle: Option<Instant>,
    /// The node asked it for its slot: it releases its actor at its next
    /// idle pass.
    yielding: bool,
}

/// An owner's idle wait, as its slot records it: dropping it mid-wait ends
/// the wait too.
pub(super) struct IdleWait<'a> {
    hints: &'a Hints,
    actor: &'a ActorKey,
    left: bool,
}

impl<'a> IdleWait<'a> {
    /// Enter `actor`'s idle wait, since `since`; `None` when the node
    /// already asked it to yield.
    pub(super) fn enter(hints: &'a Hints, actor: &'a ActorKey, since: Instant) -> Option<Self> {
        hints.enter_idle(actor, since).then_some(Self {
            hints,
            actor,
            left: false,
        })
    }

    /// End the wait that `woke` ended: [`Wake::Yield`] when the node asked
    /// for the slot meanwhile.
    pub(super) fn leave(mut self, woke: Wake) -> Wake {
        self.left = true;
        if self.hints.leave_idle(self.actor) {
            Wake::Yield
        } else {
            woke
        }
    }
}

impl Drop for IdleWait<'_> {
    fn drop(&mut self) {
        if !self.left {
            self.hints.leave_idle(self.actor);
        }
    }
}

/// `actor`'s rendezvous score on `peer`: FNV-1a over both, then mixed so
/// names that differ in one byte score far apart. Stable across builds and
/// processes, so every producer that sees one live set picks one peer.
fn rendezvous(actor: &ActorKey, peer: &NodeId) -> u64 {
    let hash = actor
        .as_str()
        .bytes()
        .chain([0xff])
        .chain(peer.as_str().bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    // The 64-bit finalizer of MurmurHash3.
    let hash = (hash ^ (hash >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
    let hash = (hash ^ (hash >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

/// Ends an activation's hint registration however its task ends: returned,
/// panicked or aborted.
pub(super) struct Running {
    pub(super) hints: Hints,
    pub(super) actor: ActorKey,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.hints.end(&self.actor);
    }
}
