//! The single-dispatcher coalescing protocol shared by the process-execution
//! scheduler (`lash_core_worker::runtime::process_worker`) and the queued-work
//! scheduler (`runtime::native_substrate::queued`).
//!
//! One protocol, one implementation: a `pending` queue, a per-key `scheduled`
//! set, at most one retained `rerun` per key, `active` execution accounting,
//! and a `dispatcher_running` latch that admits exactly one drain task. Hosts
//! parameterise the key, the work item, a side-scoped `extra` state block, and
//! the underflow report; they keep their own dispatch loops because the loops
//! carry different duties (worklist paging versus exit-on-idle).

use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use lash_core_ids::worker_capacity::{WorkerCapacityMetrics, WorkerSlotKind};

/// Side-scoped scheduler state that must move under the same lock as the
/// protocol fields. `()` for hosts with nothing extra.
pub trait CoalescingExtra: Send {
    /// Runs when a dispatcher guard unwinds or disarms, before the latch is
    /// released to waiters.
    fn on_dispatcher_exit(&mut self) {}
}

impl CoalescingExtra for () {}

/// The protocol state shared by both schedulers. `pending` + `scheduled` +
/// `rerun` coalesce repeated admissions per key; `active` counts in-flight
/// executions; `dispatcher_running` is the single-dispatcher latch, mutated
/// only through [`claim_dispatcher`](Self::claim_dispatcher) and
/// [`release_dispatcher`](Self::release_dispatcher).
pub struct CoalescingSchedulerState<K, W, E = ()> {
    pub pending: VecDeque<W>,
    pub scheduled: BTreeSet<K>,
    pub rerun: BTreeMap<K, W>,
    pub active: usize,
    dispatcher_running: bool,
    pub extra: E,
}

impl<K: Ord, W, E: Default> Default for CoalescingSchedulerState<K, W, E> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            scheduled: BTreeSet::new(),
            rerun: BTreeMap::new(),
            active: 0,
            dispatcher_running: false,
            extra: E::default(),
        }
    }
}

impl<K: Ord + Clone, W, E: CoalescingExtra> CoalescingSchedulerState<K, W, E> {
    /// Queue `work` under `key`, or coalesce it onto the in-flight attempt's
    /// retained rerun. Returns true when the key was newly scheduled.
    pub fn admit(&mut self, key: K, work: W, merge: impl FnOnce(&mut W, W)) -> bool {
        if self.scheduled.insert(key.clone()) {
            self.pending.push_back(work);
            return true;
        }
        match self.rerun.entry(key) {
            std::collections::btree_map::Entry::Occupied(mut slot) => merge(slot.get_mut(), work),
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(work);
            }
        }
        false
    }

    /// Pop the next pending item and count it active.
    pub fn pop_next(&mut self) -> Option<W> {
        let work = self.pending.pop_front()?;
        self.active += 1;
        Some(work)
    }

    /// Claim the dispatcher latch: true exactly when this call starts the
    /// (single) dispatcher.
    pub fn claim_dispatcher(&mut self) -> bool {
        if self.dispatcher_running {
            false
        } else {
            self.dispatcher_running = true;
            true
        }
    }

    /// Whether the dispatcher latch is held.
    pub fn dispatcher_running(&self) -> bool {
        self.dispatcher_running
    }

    /// Release the dispatcher latch and give the host's extra state its exit
    /// hook. Every dispatcher stop — unwind guard or deliberate park — goes
    /// through here so the latch and the host rewind cannot drift apart.
    pub fn release_dispatcher(&mut self) {
        self.dispatcher_running = false;
        self.extra.on_dispatcher_exit();
    }

    /// No queued work and nothing executing.
    pub fn queue_idle(&self) -> bool {
        self.pending.is_empty() && self.active == 0
    }
}

/// What a scheduler exposes to the shared protocol pieces: the locked state,
/// the change notifier, and the capacity metrics a completion must update.
pub trait CoalescingSchedulerHandle: Send + Sync + 'static {
    type Key: Ord + Clone;
    type Work;
    type Extra: CoalescingExtra;

    fn state(&self) -> &Mutex<CoalescingSchedulerState<Self::Key, Self::Work, Self::Extra>>;
    fn changed(&self) -> &Arc<tokio::sync::Notify>;
    fn slot_kind(&self) -> WorkerSlotKind;
    fn metrics(&self) -> &WorkerCapacityMetrics;

    /// Record one finished execution: release the key for a retained rerun,
    /// else unschedule it. An underflowing completion is an accounting bug —
    /// report it and clamp, never panic (this runs in a detached task's Drop).
    fn complete(&self, key: &Self::Key, on_underflow: impl FnOnce(&Self::Key)) {
        let mut state = self.state().lock_recover();
        if state.active == 0 {
            on_underflow(key);
        } else {
            state.active -= 1;
        }
        if let Some(work) = state.rerun.remove(key) {
            state.pending.push_back(work);
        } else {
            state.scheduled.remove(key);
        }
        self.metrics()
            .intake_depth(self.slot_kind(), state.pending.len() + state.rerun.len());
        drop(state);
        self.changed().notify_one();
    }

    /// Park the dispatcher: release the latch, run the host's exit hook, then
    /// notify so a later pass can claim a replacement. `Drop` cannot await,
    /// so the unwind guard and every deliberate stop share this one non-async
    /// method.
    fn park_dispatcher(&self) {
        self.state().lock_recover().release_dispatcher();
        self.changed().notify_one();
    }
}

/// Clears the single-dispatcher latch if the dispatcher task unwinds or ends
/// without disarming. Without this guard one panic permanently leaves queued
/// work with no task allowed to drain it.
pub struct CoalescingDispatcherGuard<S: CoalescingSchedulerHandle> {
    scheduler: Arc<S>,
    armed: bool,
}

impl<S: CoalescingSchedulerHandle> CoalescingDispatcherGuard<S> {
    pub fn new(scheduler: Arc<S>) -> Self {
        Self {
            scheduler,
            armed: true,
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<S: CoalescingSchedulerHandle> Drop for CoalescingDispatcherGuard<S> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.scheduler.park_dispatcher();
    }
}
