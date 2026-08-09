use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::TimeZone as _;

const SIM_EPOCH_MS: u64 = 1_700_000_000_000;
const UNSCHEDULED_STEP_MS: u64 = 5_000;
const UNSCHEDULED_STEP_YIELDS: usize = 32;
type SleepWaiter = tokio::sync::oneshot::Sender<()>;
type SleepersByDeadline = BTreeMap<u64, Vec<SleepWaiter>>;

/// Virtual clock shared by the simulated runtime and embedded stores.
///
/// Scheduled sleeps fast-forward with virtual time. Task interleaving remains
/// under the Tokio scheduler and is not controlled by this clock.
#[derive(Debug)]
pub(crate) struct SimClock {
    logical_ms: AtomicU64,
    monotonic_origin: Instant,
    sleepers: Mutex<SleepersByDeadline>,
}

impl SimClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            logical_ms: AtomicU64::new(0),
            monotonic_origin: Instant::now(),
            sleepers: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn logical_ms(&self) -> u64 {
        self.logical_ms.load(Ordering::SeqCst)
    }

    pub(crate) async fn advance_to(&self, target_ms: u64) {
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
                for _ in 0..UNSCHEDULED_STEP_YIELDS {
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    pub(crate) async fn advance_by(&self, delta_ms: u64) {
        self.advance_to(self.logical_ms().saturating_add(delta_ms))
            .await;
    }

    async fn wait_until_ms(&self, deadline_ms: u64) {
        let receiver = {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let mut sleepers = self.sleepers.lock_recover();
            if self.logical_ms() >= deadline_ms {
                return;
            }
            sleepers.entry(deadline_ms).or_default().push(sender);
            receiver
        };
        let _ = receiver.await;
    }
}

#[async_trait::async_trait]
impl lash_core::Clock for SimClock {
    fn now(&self) -> Instant {
        self.monotonic_origin + Duration::from_millis(self.logical_ms())
    }

    fn timestamp_ms(&self) -> u64 {
        SIM_EPOCH_MS.saturating_add(self.logical_ms())
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .timestamp_millis_opt(self.timestamp_ms() as i64)
            .single()
            .expect("sim clock timestamp is representable")
    }

    async fn sleep(&self, duration: Duration) {
        self.wait_until_ms(
            self.logical_ms()
                .saturating_add(duration.as_millis() as u64),
        )
        .await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        let deadline_ms = deadline
            .saturating_duration_since(self.monotonic_origin)
            .as_millis() as u64;
        self.wait_until_ms(deadline_ms).await;
    }
}
