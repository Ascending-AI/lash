//! Virtual time for every node of a simulated deployment.
//!
//! One [`SimClock`] drives both the store's durable instant (the store set is
//! built over it: `SqliteStoreSet::memory_with_clock`, and PostgreSQL's
//! `with_clock_for_testing`, which binds the clock's instant into every
//! statement) and every local timer the runtime arms through
//! [`lash_core_ids::clock::Clock`]. Leases therefore expire, deadlines pass and
//! polls fire on virtual time only.

use lash_core_ids::clock::Clock;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIM_EPOCH_MS: u64 = 1_700_000_000_000;
const UNSCHEDULED_STEP_MS: u64 = 5_000;
/// Yields granted to runnable tasks between two clock moves, so a task woken
/// by one move arms its next timer before the clock moves again.
const SETTLE_YIELDS: usize = 32;
type SleepWaiter = tokio::sync::oneshot::Sender<()>;
type SleepersByDeadline = BTreeMap<u64, Vec<SleepWaiter>>;

/// Virtual clock shared by the simulated runtime and its stores.
///
/// Scheduled sleeps fast-forward with virtual time. Task interleaving remains
/// under the Tokio scheduler and is not controlled by this clock.
#[derive(Debug)]
pub struct SimClock {
    logical_ms: AtomicU64,
    monotonic_origin: Instant,
    sleepers: Mutex<SleepersByDeadline>,
    sleep_registered: tokio::sync::Notify,
}

impl SimClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            logical_ms: AtomicU64::new(0),
            monotonic_origin: Instant::now(),
            sleepers: Mutex::new(BTreeMap::new()),
            sleep_registered: tokio::sync::Notify::new(),
        })
    }

    /// Milliseconds of virtual time since the clock started.
    pub fn logical_ms(&self) -> u64 {
        self.logical_ms.load(Ordering::SeqCst)
    }

    /// The wall-clock epoch milliseconds the store records for `logical_ms`.
    pub fn timestamp_ms_at(logical_ms: u64) -> u64 {
        SIM_EPOCH_MS.saturating_add(logical_ms)
    }

    /// Move time to `target_ms`, waking every sleeper due on the way in
    /// deadline order and letting woken tasks run between deadlines.
    pub async fn advance_to(&self, target_ms: u64) {
        loop {
            let current = self.logical_ms();
            if current >= target_ms {
                return;
            }
            let (next, waiters) = {
                let mut sleepers = self.sleepers.lock_recover();
                let next_deadline = sleepers
                    .range((current + 1)..=target_ms)
                    .next()
                    .map(|(deadline, _)| *deadline);
                let next = next_deadline
                    .unwrap_or_else(|| target_ms.min(current.saturating_add(UNSCHEDULED_STEP_MS)));
                self.logical_ms.store(next, Ordering::SeqCst);
                let waiters = next_deadline
                    .and_then(|deadline| sleepers.remove(&deadline))
                    .unwrap_or_default();
                (next, waiters)
            };
            for waiter in waiters {
                let _ = waiter.send(());
            }
            if next < target_ms {
                settle().await;
            }
        }
    }

    pub async fn advance_by(&self, delta_ms: u64) {
        self.advance_to(self.logical_ms().saturating_add(delta_ms))
            .await;
    }

    /// The earliest deadline a live sleeper waits for, if one is armed. A
    /// sleep whose future was dropped (the losing arm of a `select!`) arms
    /// nothing.
    pub fn next_due(&self) -> Option<u64> {
        let current = self.logical_ms();
        let mut sleepers = self.sleepers.lock_recover();
        sleepers.retain(|_, waiters| {
            waiters.retain(|waiter| !waiter.is_closed());
            !waiters.is_empty()
        });
        sleepers
            .range((current + 1)..)
            .next()
            .map(|(deadline, _)| *deadline)
    }

    /// Let runnable tasks arm their timers, then jump to the earliest armed
    /// deadline and wake its sleepers. Answers the new time, or `None` when
    /// no timer is armed (time does not move).
    pub async fn advance_to_next_due(&self) -> Option<u64> {
        settle().await;
        let due = self.next_due()?;
        self.advance_to(due).await;
        settle().await;
        Some(due)
    }

    /// Wait until some task sleeps until exactly `deadline_ms`.
    pub async fn wait_for_sleep(&self, deadline_ms: u64) {
        loop {
            let registered = self.sleep_registered.notified();
            if self.sleepers.lock_recover().contains_key(&deadline_ms) {
                return;
            }
            registered.await;
        }
    }

    pub(crate) async fn wait_until_ms(&self, deadline_ms: u64) {
        let receiver = {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let mut sleepers = self.sleepers.lock_recover();
            if self.logical_ms() >= deadline_ms {
                return;
            }
            sleepers.entry(deadline_ms).or_default().push(sender);
            self.sleep_registered.notify_waiters();
            receiver
        };
        let _ = receiver.await;
    }

    pub(crate) fn deadline_ms(&self, deadline: Instant) -> u64 {
        deadline
            .saturating_duration_since(self.monotonic_origin)
            .as_millis() as u64
    }
}

/// Yield enough times for every task woken so far to reach its next await.
pub async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

#[async_trait::async_trait]
impl Clock for SimClock {
    fn now(&self) -> Instant {
        self.monotonic_origin + Duration::from_millis(self.logical_ms())
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + Duration::from_millis(Self::timestamp_ms_at(self.logical_ms())),
        )
    }

    async fn sleep(&self, duration: Duration) {
        self.wait_until_ms(
            self.logical_ms()
                .saturating_add(duration.as_millis() as u64),
        )
        .await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        self.wait_until_ms(self.deadline_ms(deadline)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `advance_to_next_due` jumps exactly to the earliest live timer and
    /// wakes only its sleepers; a sleep whose future was dropped arms
    /// nothing, so time never jumps to a deadline nobody waits for.
    #[tokio::test]
    async fn time_jumps_to_the_earliest_live_timer_only() {
        let clock = SimClock::new();
        let abandoned = {
            let clock = Arc::clone(&clock);
            tokio::spawn(async move { clock.sleep(Duration::from_millis(500)).await })
        };
        let woken = {
            let clock = Arc::clone(&clock);
            tokio::spawn(async move {
                clock.sleep(Duration::from_millis(2_000)).await;
                clock.logical_ms()
            })
        };
        let later = {
            let clock = Arc::clone(&clock);
            tokio::spawn(async move { clock.sleep(Duration::from_millis(7_000)).await })
        };
        clock.wait_for_sleep(500).await;
        clock.wait_for_sleep(2_000).await;
        clock.wait_for_sleep(7_000).await;
        abandoned.abort();
        let _ = abandoned.await;

        assert_eq!(clock.advance_to_next_due().await, Some(2_000));
        assert_eq!(woken.await.expect("the sleeper ran"), 2_000);
        assert!(!later.is_finished(), "a later timer must not fire early");
        assert_eq!(clock.advance_to_next_due().await, Some(7_000));
        later.await.expect("the later sleeper ran");
        assert_eq!(clock.advance_to_next_due().await, None);
        assert_eq!(clock.logical_ms(), 7_000);
    }
}
