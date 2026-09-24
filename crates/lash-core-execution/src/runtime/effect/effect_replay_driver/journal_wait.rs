//! How the driver waits: on the journal's change notifications, raced against
//! the driver clock's deadline (FIG-3579, FIG-3598).
//!
//! Every place the driver waits for another writer — a claim queued behind a
//! live lease, a discharge or a tool child's intent drain held by the
//! commit-order barrier, a reader waiting for a group's next settlement —
//! arms the row store's [`EffectJournalWake`] for the subject *before* the
//! read that decides to wait, then parks until one of:
//!
//! - the notifier fires: a writer the notifier reaches committed a change;
//! - the clock reaches the deadline the read reported (a lease expiry);
//! - for a journal another process can write unannounced, the bounded
//!   [`CROSS_PROCESS_POLL`] elapses.
//!
//! Nothing else re-reads the journal. A memory backend, where every writer
//! is in this process, has no poll at all.
//!
//! The clock is the injected [`Clock`](crate::Clock), and its sleeps are the
//! timer; a deadline — a lease expiry, a renewal budget — counts as reached
//! only once the clock's face says so. The face is also re-read on a
//! real-time backoff, so a wait neither spins on a frozen clock whose sleeps
//! return at once nor misses a deadline a test clock was advanced past by
//! hand. The renewal cadence is the one wait whose sleep is authoritative on
//! its own, and it too paces itself in real time when a sleep returns with
//! the face unmoved ([`cadence_sleep`]).

use super::*;
use std::pin::Pin;
use tokio::sync::futures::OwnedNotified;

/// How stale a parked driver's view may grow of a journal that writers in
/// another process change without waking it. The one remaining poll: it runs
/// only for [`EffectJournalWriters::Unannounced`] subjects.
const CROSS_PROCESS_POLL: Duration = Duration::from_millis(100);

/// How long a subscription may take before the wait falls back to the poll
/// alone.
const SUBSCRIBE_BUDGET: Duration = Duration::from_secs(1);

/// A wake nothing notifies, re-read on the poll: what a wait parks on when
/// its subscription could not be had.
fn poll_only_wake() -> EffectJournalWake {
    EffectJournalWake {
        notify: Arc::new(tokio::sync::Notify::new()),
        writers: EffectJournalWriters::Unannounced,
    }
}

/// First real-time pause of the watch on the clock's wall face.
const CLOCK_RECHECK_FIRST: Duration = Duration::from_millis(1);

/// Longest real-time pause of the watch on the clock's wall face: how late a
/// queued claim may notice a test clock advanced by hand.
const CLOCK_RECHECK_MAX: Duration = Duration::from_secs(1);

/// A lease expiry on the driver clock's wall face, in epoch milliseconds, as
/// the store stamped it.
#[derive(Clone, Copy, Debug)]
pub(super) struct ClockDeadline(u64);

impl ClockDeadline {
    /// What is left before the clock's wall face reaches this deadline;
    /// `None` once it has.
    fn remaining(self, clock: &dyn crate::Clock) -> Option<Duration> {
        let remaining = Duration::from_millis(self.0.saturating_sub(clock.timestamp_ms()));
        (!remaining.is_zero()).then_some(remaining)
    }

    /// Resolve once the clock's wall face reaches this deadline, and not
    /// before.
    async fn reached(self, clock: &dyn crate::Clock) {
        let Some(remaining) = self.remaining(clock) else {
            return;
        };
        clock_confirms(clock.sleep(remaining), || self.remaining(clock).is_none()).await;
    }
}

/// Resolve once the clock's monotonic face reaches `deadline`, and not
/// before: [`ClockDeadline::reached`] on the face [`Clock::now`] reads.
///
/// [`Clock::now`]: crate::Clock::now
pub(super) async fn monotonic_deadline_reached(clock: &dyn crate::Clock, deadline: Instant) {
    clock_confirms(clock.sleep_until(deadline), || clock.now() >= deadline).await;
}

/// Resolve once `reached` holds, timed by the clock's own `sleep`.
///
/// The clock's sleep is the timer, and its face is what confirms it. The
/// face is also watched on a real-time backoff capped at
/// [`CLOCK_RECHECK_MAX`], because two kinds of test clock break the sleep's
/// promise: a frozen clock's sleep returns at once with the face unmoved
/// (sleeping again would be the hot loop), and a hand-advanced clock's face
/// can pass the deadline while its sleep still runs in real time. The watch
/// reads the clock only, never the journal.
async fn clock_confirms(sleep: impl Future<Output = ()>, reached: impl Fn() -> bool) {
    if reached() {
        return;
    }
    tokio::select! {
        () = sleep => {}
        () = face_watch(&reached) => return,
    }
    // The sleep is done: the face is short of the deadline only by a step or
    // a frozen clock, so the watch starts over at its first pause rather than
    // resuming a backoff already grown to a second — an NTP step must not
    // delay a lease takeover by that much.
    face_watch(&reached).await;
}

/// Re-read a clock face on a real-time backoff until `reached` holds.
async fn face_watch(reached: &impl Fn() -> bool) {
    let mut pause = CLOCK_RECHECK_FIRST;
    while !reached() {
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(CLOCK_RECHECK_MAX);
    }
}

/// One renewal-cadence sleep of `wait` on the driver clock.
///
/// Here the clock's sleep is authoritative on its own: a sleep that returns
/// is a renewal due, whatever the face says, so a test clock that gates its
/// sleeps paces renewals exactly. A sleep that returns with the monotonic
/// face not moved at all is a frozen clock's, though, and renewing at once
/// would turn the cadence into a hot loop of store writes. The cadence then
/// also waits for the face to move, or until `wait` of real time has passed
/// since the sleep began — the pace a moving clock would have kept.
pub(super) async fn cadence_sleep(clock: &dyn crate::Clock, wait: Duration) {
    let slept_from = clock.now();
    let paced_until = tokio::time::Instant::now() + wait;
    clock.sleep(wait).await;
    let face_moved = || clock.now() != slept_from;
    tokio::select! {
        () = tokio::time::sleep_until(paced_until) => {}
        () = face_watch(&face_moved) => {}
    }
}

/// A driver's subscription to one journal subject.
pub(super) struct JournalWatch {
    wake: EffectJournalWake,
}

/// A [`JournalWatch`] listening from before the read it guards.
pub(super) struct ArmedWatch {
    notified: Pin<Box<OwnedNotified>>,
    writers: EffectJournalWriters,
}

impl JournalWatch {
    /// Start listening. Call before the read whose answer decides to park:
    /// `notify_waiters` wakes listeners, not later arrivals, so a change
    /// committed between that read and the park is caught only by a
    /// listener enabled before the read.
    pub(super) fn arm(&self) -> ArmedWatch {
        let mut notified = Box::pin(Arc::clone(&self.wake.notify).notified_owned());
        notified.as_mut().enable();
        ArmedWatch {
            notified,
            writers: self.wake.writers,
        }
    }
}

impl ArmedWatch {
    /// Park until the subject may have changed, or the clock reaches
    /// `deadline`, and say which.
    pub(super) async fn park(
        self,
        clock: &dyn crate::Clock,
        deadline: Option<ClockDeadline>,
    ) -> ParkEnd {
        let Self { notified, writers } = self;
        let cross_process = async {
            match writers {
                EffectJournalWriters::Unannounced => tokio::time::sleep(CROSS_PROCESS_POLL).await,
                EffectJournalWriters::Announced => std::future::pending().await,
            }
        };
        let clock_deadline = async {
            match deadline {
                Some(deadline) => deadline.reached(clock).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = notified => ParkEnd::Woken,
            () = cross_process => ParkEnd::Woken,
            () = clock_deadline => ParkEnd::DeadlineReached,
        }
    }
}

/// Why a park ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParkEnd {
    /// A notification, or the cross-process poll: re-read the journal.
    Woken,
    /// The clock reached the deadline.
    DeadlineReached,
}

/// A claim queued behind another owner's live lease.
///
/// The first busy answer only subscribes and re-claims at once, so the common
/// uncontended claim never touches the notifier table. Every later busy
/// answer parks on the row, racing the lease's expiry on the clock — unless
/// the clock already reached that expiry once and the store still called the
/// lease live (a store whose lease clock runs behind the driver's, as a
/// PostgreSQL server's may), in which case only the row's own notification
/// or the cross-process poll can move it.
#[derive(Default)]
pub(super) struct ClaimQueue {
    watch: Option<JournalWatch>,
    expired_on_clock: Option<u64>,
}

impl ClaimQueue {
    /// Listen before the next claim read, once subscribed.
    pub(super) fn arm(&self) -> Option<ArmedWatch> {
        self.watch.as_ref().map(JournalWatch::arm)
    }
}

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// Subscribe to `subject` on this driver's journal.
    ///
    /// A subscription is an optimization, never a precondition: one the
    /// store refuses, or does not grant within [`SUBSCRIBE_BUDGET`] (a
    /// PostgreSQL `LISTEN` connection still reconnecting, a saturated pool),
    /// degrades to a poll-only watch rather than failing the wait. The wait
    /// then re-reads on the cross-process poll and is never left parked on a
    /// wake nobody will send.
    pub(super) async fn watch_journal(&self, subject: EffectJournalSubject<'_>) -> JournalWatch {
        let wake = match tokio::time::timeout(
            SUBSCRIBE_BUDGET,
            self.row_store.journal_wake(subject),
        )
        .await
        {
            Ok(Ok(wake)) => wake,
            Ok(Err(error)) => {
                tracing::warn!(?subject, %error, "journal wake refused; waiting on the poll alone");
                poll_only_wake()
            }
            Err(_) => {
                tracing::warn!(
                    ?subject,
                    budget_ms = SUBSCRIBE_BUDGET.as_millis() as u64,
                    "journal wake not granted in time; waiting on the poll alone"
                );
                poll_only_wake()
            }
        };
        JournalWatch { wake }
    }

    /// Wait at the §5 barrier until no committed sibling below `commit_seq`
    /// in `group_key` still owes its drain.
    ///
    /// The barrier is lifted by a sibling's drain, never by time, so the wait
    /// parks on the group's wake with no clock deadline. It subscribes on the
    /// first blocked answer and re-reads at once, so a drain the barrier
    /// never held touches no notifier; after that each read listens from
    /// before it. Once the answer is unblocked it stays so: the commit
    /// positions below `commit_seq` were fixed when it was allocated.
    pub(super) async fn await_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        let mut watch = None;
        loop {
            let armed = watch.as_ref().map(JournalWatch::arm);
            if !self.row_store.drain_blocked(group_key, commit_seq).await? {
                return Ok(());
            }
            match armed {
                Some(armed) => {
                    armed.park(&*self.clock, None).await;
                }
                None => {
                    watch = Some(
                        self.watch_journal(EffectJournalSubject::Group { group_key })
                            .await,
                    );
                }
            }
        }
    }

    /// Wait for the claim of `replay_key` under `scope` to become claimable
    /// after the store reported it busy until `retry_at_ms`. `armed` is the
    /// listener enabled before that claim read, if the queue had one.
    pub(super) async fn queue_claim(
        &self,
        queue: &mut ClaimQueue,
        armed: Option<ArmedWatch>,
        scope: &ExecutionScope,
        replay_key: &str,
        retry_at_ms: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Some(armed) = armed else {
            let journal_identity = scope
                .journal_identity()
                .map_err(RuntimeEffectControllerError::from)?;
            queue.watch = Some(
                self.watch_journal(EffectJournalSubject::Row {
                    scope_id: journal_identity.key(),
                    replay_key,
                })
                .await,
            );
            return Ok(());
        };
        let deadline = (!queue
            .expired_on_clock
            .is_some_and(|expired| retry_at_ms <= expired))
        .then_some(ClockDeadline(retry_at_ms));
        if armed.park(&*self.clock, deadline).await == ParkEnd::DeadlineReached {
            queue.expired_on_clock = Some(retry_at_ms);
        }
        Ok(())
    }
}
