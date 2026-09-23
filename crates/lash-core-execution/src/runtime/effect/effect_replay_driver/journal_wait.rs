//! How the driver waits: on the journal's change notifications, raced against
//! the driver clock's deadline (FIG-3579).
//!
//! Every place the driver waits for another writer — a claim queued behind a
//! live lease, a discharge held by the commit-order barrier, a reader waiting
//! for a group's next settlement — arms the row store's
//! [`EffectJournalWake`] for the subject *before* the read that decides to
//! wait, then parks until one of:
//!
//! - the notifier fires: a writer the notifier reaches committed a change;
//! - the clock reaches the deadline the read reported (a lease expiry);
//! - for a journal another process can write unannounced, the bounded
//!   [`CROSS_PROCESS_POLL`] elapses.
//!
//! Nothing else re-reads the journal. A memory deployment, where every writer
//! is in this process, has no poll at all.
//!
//! The clock is the injected [`Clock`](crate::Clock), and its sleeps are the
//! timer; a lease expiry counts as reached only once the clock's wall face
//! says so. The face is also re-read on a real-time backoff, so a queued claim
//! neither spins on a frozen clock whose sleeps return at once nor misses an
//! expiry a test clock was advanced past by hand.

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
    ///
    /// The clock's sleep is the timer, and the face is what confirms it. The
    /// face is also watched on a real-time backoff capped at
    /// [`CLOCK_RECHECK_MAX`], because two kinds of test clock break the
    /// sleep's promise: a frozen clock's sleep returns at once with the face
    /// unmoved (sleeping again would be the hot loop), and a hand-advanced
    /// clock's face can pass the deadline while its sleep still runs in real
    /// time. The watch reads the clock only, never the journal.
    async fn reached(self, clock: &dyn crate::Clock) {
        let Some(remaining) = self.remaining(clock) else {
            return;
        };
        tokio::select! {
            () = clock.sleep(remaining) => {}
            () = self.face_reached(clock) => return,
        }
        // The sleep is done: the face is short of the deadline only by a
        // step or a frozen clock, so the watch starts over at its first
        // pause rather than resuming a backoff already grown to a second —
        // an NTP step must not delay a lease takeover by that much.
        self.face_reached(clock).await;
    }

    /// Re-read the clock's wall face on a real-time backoff until it reaches
    /// this deadline.
    async fn face_reached(self, clock: &dyn crate::Clock) {
        let mut pause = CLOCK_RECHECK_FIRST;
        while self.remaining(clock).is_some() {
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(CLOCK_RECHECK_MAX);
        }
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
