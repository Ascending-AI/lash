//! The single-dispatcher coalescing protocol shared by the process-execution
//! scheduler (`lash_core_worker::runtime::process_worker`) and the queued-work
//! scheduler (`runtime::native_substrate::queued`).
//!
//! One protocol, one implementation: one entry per key records whether its
//! demand is queued (with at most one coalesced rerun already waiting behind
//! it) or running (retaining at most one rerun demand), and a FIFO queue of
//! the queued keys orders pops. A `dispatcher_running` latch admits exactly
//! one drain task. Hosts parameterise the key, the work item, and a
//! side-scoped `extra` state block; they keep their own dispatch loops because
//! the loops carry different duties (worklist paging versus exit-on-idle).

#[cfg(not(loom))]
use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use lash_core_ids::execution_permit::SharedNotify;
use lash_core_ids::worker_capacity::{WorkerCapacityMetrics, WorkerSlotKind};

/// The protocol's state mutex swaps to loom's instrumented mutex under
/// `--cfg loom` so the single-dispatcher latch's interleavings are
/// model-checked (FIG-1161 seam 3). Public because
/// [`CoalescingSchedulerHandle::state`] exposes it to the host schedulers.
#[cfg(not(loom))]
pub type CoalescingMutex<T> = std::sync::Mutex<T>;
/// `cfg(loom)` twin of [`CoalescingMutex`].
#[cfg(loom)]
pub type CoalescingMutex<T> = loom::sync::Mutex<T>;

/// `lock_recover` for the loom mutex, so the handle's default methods read
/// identically under both cfgs. Crate-visible so the host schedulers
/// (`queued/scheduler.rs`, `process_worker`) can lock the same state.
#[cfg(loom)]
pub(crate) mod loom_ext {
    pub trait LoomMutexExt<T: ?Sized> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T>;
    }

    impl<T: ?Sized> LoomMutexExt<T> for loom::sync::Mutex<T> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T> {
            self.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }
}

#[cfg(loom)]
use loom_ext::LoomMutexExt as _;

/// Side-scoped scheduler state that must move under the same lock as the
/// protocol fields. `()` for hosts with nothing extra.
pub trait CoalescingExtra: Send {
    /// Runs when a dispatcher guard unwinds or disarms, before the latch is
    /// released to waiters.
    fn on_dispatcher_exit(&mut self) {}
}

impl CoalescingExtra for () {}

/// One key's schedule: work waiting for an attempt, or an in-flight attempt.
/// A retained rerun only ever lives inside one of these variants, so a rerun
/// demand cannot exist for a key with nothing queued or running.
enum CoalescingEntry<W> {
    /// `staged` is the demand the next pop hands to a task. `rerun` retains a
    /// demand admitted while this key was already queued but not yet running;
    /// a host either folds it into the popped demand (`take_rerun`) or lets it
    /// carry into the running attempt and re-queue on completion.
    Queued { staged: W, rerun: Option<W> },
    /// The attempt is in flight; `rerun` retains at most one coalesced demand.
    Running { rerun: Option<W> },
}

impl<W> CoalescingEntry<W> {
    fn rerun_slot(&mut self) -> &mut Option<W> {
        match self {
            Self::Queued { rerun, .. } | Self::Running { rerun } => rerun,
        }
    }

    fn depth(&self) -> usize {
        match self {
            Self::Queued { rerun, .. } => 1 + usize::from(rerun.is_some()),
            Self::Running { rerun } => usize::from(rerun.is_some()),
        }
    }
}

/// The protocol state shared by both schedulers. `entries` is the single
/// record of which keys are queued or running and what each retains;
/// `queue` orders the queued keys first-in-first-out; `dispatcher_running` is
/// the single-dispatcher latch, mutated only through
/// [`claim_dispatcher`](Self::claim_dispatcher) and
/// [`release_dispatcher`](Self::release_dispatcher).
pub struct CoalescingSchedulerState<K, W, E = ()> {
    entries: BTreeMap<K, CoalescingEntry<W>>,
    queue: VecDeque<K>,
    dispatcher_running: bool,
    pub extra: E,
}

impl<K: Ord, W, E: Default> Default for CoalescingSchedulerState<K, W, E> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            queue: VecDeque::new(),
            dispatcher_running: false,
            extra: E::default(),
        }
    }
}

impl<K: Ord + Clone, W, E: CoalescingExtra> CoalescingSchedulerState<K, W, E> {
    /// Queue `work` under `key`, or coalesce it onto the key's retained rerun
    /// when the key is already scheduled. Returns true when the key was newly
    /// scheduled.
    pub fn admit(&mut self, key: K, work: W, merge: impl FnOnce(&mut W, W)) -> bool {
        match self.entries.entry(key.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(CoalescingEntry::Queued {
                    staged: work,
                    rerun: None,
                });
                self.queue.push_back(key);
                true
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let rerun = slot.get_mut().rerun_slot();
                match rerun {
                    Some(retained) => merge(retained, work),
                    None => *rerun = Some(work),
                }
                false
            }
        }
    }

    /// Pop the next queued key and mark its attempt running.
    pub fn pop_next(&mut self) -> Option<W> {
        loop {
            let key = self.queue.pop_front()?;
            match self.entries.remove(&key) {
                Some(CoalescingEntry::Queued { staged, rerun }) => {
                    self.entries.insert(key, CoalescingEntry::Running { rerun });
                    return Some(staged);
                }
                Some(entry @ CoalescingEntry::Running { .. }) => {
                    self.entries.insert(key, entry);
                }
                None => {}
            }
        }
    }

    /// Drain the key's retained rerun demand, if any.
    pub fn take_rerun(&mut self, key: &K) -> Option<W> {
        self.entries
            .get_mut(key)
            .and_then(|entry| entry.rerun_slot().take())
    }

    /// Record one finished execution for `key`: a retained rerun re-enters the
    /// queue in FIFO order with the key still scheduled; otherwise the key
    /// leaves the schedule. A completion for a key with no running attempt is
    /// unrepresentable — this runs in a detached task's Drop, so it degrades
    /// to a no-op rather than panicking.
    pub fn complete(&mut self, key: &K) {
        if !matches!(self.entries.get(key), Some(CoalescingEntry::Running { .. })) {
            return;
        }
        if let Some(CoalescingEntry::Running { rerun: Some(work) }) = self.entries.remove(key) {
            self.entries.insert(
                key.clone(),
                CoalescingEntry::Queued {
                    staged: work,
                    rerun: None,
                },
            );
            self.queue.push_back(key.clone());
        }
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

    /// Any key with a queued demand.
    pub fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// How many attempts are in flight — derived, not tracked.
    pub fn running_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, CoalescingEntry::Running { .. }))
            .count()
    }

    /// Queued demands plus retained reruns: the intake-depth accounting.
    pub fn intake_depth(&self) -> usize {
        self.entries.values().map(CoalescingEntry::depth).sum()
    }

    /// No queued and no running entries: the dispatcher exit condition.
    pub fn queue_idle(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What a scheduler exposes to the shared protocol pieces: the locked state,
/// the change notifier, and the capacity metrics a completion must update.
pub trait CoalescingSchedulerHandle: Send + Sync + 'static {
    type Key: Ord + Clone;
    type Work;
    type Extra: CoalescingExtra;

    fn state(
        &self,
    ) -> &CoalescingMutex<CoalescingSchedulerState<Self::Key, Self::Work, Self::Extra>>;
    fn changed(&self) -> &Arc<SharedNotify>;
    fn slot_kind(&self) -> WorkerSlotKind;
    fn metrics(&self) -> &WorkerCapacityMetrics;

    /// Record one finished execution: release the key for a retained rerun,
    /// else unschedule it.
    fn complete(&self, key: &Self::Key) {
        let mut state = self.state().lock_recover();
        state.complete(key);
        self.metrics()
            .intake_depth(self.slot_kind(), state.intake_depth());
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

#[cfg(test)]
mod tests {
    use super::*;

    type State = CoalescingSchedulerState<u32, String>;

    fn merge(retained: &mut String, incoming: String) {
        retained.push_str(&incoming);
    }

    #[test]
    fn pops_are_first_in_first_out() {
        let mut state = State::default();
        assert!(state.admit(1, "a".to_string(), merge));
        assert!(state.admit(2, "b".to_string(), merge));
        assert!(state.admit(3, "c".to_string(), merge));

        assert_eq!(state.pop_next().as_deref(), Some("a"));
        assert_eq!(state.pop_next().as_deref(), Some("b"));
        assert_eq!(state.pop_next().as_deref(), Some("c"));
        assert_eq!(state.pop_next(), None);
    }

    #[test]
    fn an_admit_between_pop_and_completion_is_retained_then_requeued_in_order() {
        let mut state = State::default();
        assert!(state.admit(1, "first".to_string(), merge));
        assert!(state.admit(2, "other".to_string(), merge));
        assert_eq!(state.pop_next().as_deref(), Some("first"));

        // Key 1 is running: a fresh demand is retained on its entry, not lost
        // and not re-queued ahead of the still-queued key 2.
        assert!(!state.admit(1, "again".to_string(), merge));
        assert_eq!(state.running_count(), 1);
        assert_eq!(state.intake_depth(), 2);
        assert!(!state.queue_idle());

        state.complete(&1);
        assert_eq!(state.pop_next().as_deref(), Some("other"));
        assert_eq!(state.pop_next().as_deref(), Some("again"));
        assert_eq!(state.pop_next(), None);
    }

    #[test]
    fn an_admit_while_queued_is_retained_not_folded_into_the_staged_demand() {
        let mut state = State::default();
        assert!(state.admit(1, "staged".to_string(), merge));
        assert!(!state.admit(1, "retained".to_string(), merge));

        // The staged demand pops unchanged; the retained rerun stays attached
        // to the now-running attempt for the host to fold or re-queue.
        assert_eq!(state.pop_next().as_deref(), Some("staged"));
        assert_eq!(state.take_rerun(&1).as_deref(), Some("retained"));

        state.complete(&1);
        assert!(state.queue_idle());
    }

    #[test]
    fn completing_an_unknown_key_is_a_noop() {
        let mut state = State::default();
        state.complete(&7);
        assert!(state.queue_idle());

        assert!(state.admit(1, "a".to_string(), merge));
        state.complete(&7);
        assert_eq!(state.pop_next().as_deref(), Some("a"));

        // A completion for a queued-but-never-run key leaves its demand alone.
        assert!(state.admit(2, "b".to_string(), merge));
        state.complete(&2);
        assert_eq!(state.pop_next().as_deref(), Some("b"));
    }

    #[test]
    fn queue_idle_is_no_queued_and_no_running_entries() {
        let mut state = State::default();
        assert!(state.queue_idle());

        state.admit(1, "a".to_string(), merge);
        assert!(!state.queue_idle(), "a queued entry keeps it busy");

        state.pop_next();
        assert!(!state.queue_idle(), "a running entry keeps it busy");

        state.complete(&1);
        assert!(
            state.queue_idle(),
            "no queued and no running entries is idle"
        );
    }
}

/// Loom model checks for the single-dispatcher latch (FIG-1161 seam 3). The
/// scheduler handle below is a minimal host: the protocol's `state`,
/// `changed`, and the real `CoalescingSchedulerHandle::complete`/
/// `park_dispatcher` code run under loom-instrumented primitives; the host's
/// own dispatch loop is modeled inline because the production loops carry
/// worklist paging duties outside this seam.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::atomic::{AtomicUsize, Ordering};

    struct LoomScheduler {
        state: CoalescingMutex<CoalescingSchedulerState<u32, u32>>,
        changed: Arc<SharedNotify>,
        metrics: WorkerCapacityMetrics,
    }

    impl LoomScheduler {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: CoalescingMutex::new(CoalescingSchedulerState::default()),
                changed: Arc::new(SharedNotify::new()),
                metrics: WorkerCapacityMetrics::default(),
            })
        }

        /// The `notify_pending_work` critical section: admit the demand and
        /// claim the latch under the same lock, then notify outside it so a
        /// parking dispatcher or a successor claimant observes the change.
        fn notify(&self, key: u32) -> bool {
            let claimed = {
                let mut state = self.state.lock_recover();
                state.admit(key, key, |_, _| ());
                state.claim_dispatcher()
            };
            self.changed.notify_one();
            claimed
        }
    }

    impl CoalescingSchedulerHandle for LoomScheduler {
        type Key = u32;
        type Work = u32;
        type Extra = ();

        fn state(&self) -> &CoalescingMutex<CoalescingSchedulerState<u32, u32>> {
            &self.state
        }

        fn changed(&self) -> &Arc<SharedNotify> {
            &self.changed
        }

        fn slot_kind(&self) -> WorkerSlotKind {
            WorkerSlotKind::QueuedWork
        }

        fn metrics(&self) -> &WorkerCapacityMetrics {
            &self.metrics
        }
    }

    /// Concurrent notifiers race the latch: exactly one claim succeeds, so
    /// exactly one dispatcher task would be spawned.
    #[test]
    fn concurrent_notifies_claim_exactly_one_dispatcher() {
        loom::model(|| {
            let scheduler = LoomScheduler::new();
            let claims = Arc::new(AtomicUsize::new(0));

            let a = loom::thread::spawn({
                let scheduler = Arc::clone(&scheduler);
                let claims = Arc::clone(&claims);
                move || {
                    if scheduler.notify(1) {
                        claims.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            let b = loom::thread::spawn({
                let scheduler = Arc::clone(&scheduler);
                let claims = Arc::clone(&claims);
                move || {
                    if scheduler.notify(2) {
                        claims.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            if scheduler.notify(3) {
                claims.fetch_add(1, Ordering::SeqCst);
            }
            a.join().expect("notifier thread panicked");
            b.join().expect("notifier thread panicked");

            assert_eq!(claims.load(Ordering::SeqCst), 1);
            assert!(scheduler.state.lock_recover().dispatcher_running());
        });
    }

    /// A demand arriving while the dispatcher drains races the park path:
    /// admit-under-lock versus idle-check-and-release-under-lock are
    /// serialized, so the demand is either drained by the running dispatcher
    /// or claims a successor. Queued work must never be left with no
    /// dispatcher.
    #[test]
    fn notify_racing_dispatcher_park_never_strands_queued_work() {
        loom::model(|| {
            let scheduler = LoomScheduler::new();
            assert!(scheduler.notify(0), "the first notify owns the latch");

            let notifier = loom::thread::spawn({
                let scheduler = Arc::clone(&scheduler);
                move || {
                    scheduler.notify(1);
                }
            });

            // The dispatcher loop's tail: pop and complete until the state is
            // idle under the lock, then release the latch — the same sequence
            // `run_dispatcher` and `park_dispatcher` run.
            let dispatcher = loom::thread::spawn({
                let scheduler = Arc::clone(&scheduler);
                move || loop {
                    enum Step {
                        Park,
                        Pop(Option<u32>),
                    }
                    let step = {
                        let mut state = scheduler.state.lock_recover();
                        if state.queue_idle() {
                            state.release_dispatcher();
                            Step::Park
                        } else {
                            Step::Pop(state.pop_next())
                        }
                    };
                    match step {
                        Step::Park => {
                            scheduler.changed.notify_one();
                            break;
                        }
                        Step::Pop(Some(key)) => scheduler.complete(&key),
                        // Non-idle with nothing poppable means a running key is
                        // awaiting completion; yield rather than spin.
                        Step::Pop(None) => loom::thread::yield_now(),
                    }
                }
            });
            notifier.join().expect("notifier thread panicked");
            dispatcher.join().expect("dispatcher thread panicked");

            let state = scheduler.state.lock_recover();
            assert!(
                state.queue_idle() || state.dispatcher_running(),
                "queued demand must never be stranded without a dispatcher"
            );
        });
    }
}
