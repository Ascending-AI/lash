//! Injected time source.
//!
//! lash is a replayable durable runtime, so time is an effect like any other:
//! durable timestamps must be reproducible under replay, and timeout/backoff
//! logic must be drivable deterministically in tests. Every wall-clock and
//! monotonic read in the runtime path goes through a [`Clock`]; the default
//! [`SystemClock`] reads the real OS clock, and tests inject a controllable
//! one.
//!
//! Boundary: `now()` is monotonic (measurement/elapsed only, never persisted);
//! `timestamp_ms`/`timestamp_rfc3339` are wall-clock (durable records). `sleep`
//! and `sleep_until` replace direct `tokio::time` calls so a fake clock can
//! resolve them without real wall-clock waits.

use std::time::{Duration, Instant};

use async_trait::async_trait;

/// Runtime and embedded-store time source. Cloneable as `Arc<dyn Clock>`;
/// carried on [`RuntimeHostConfig`](super::RuntimeHostConfig).
///
/// SQLite and in-memory persistence read this host-injectable clock because
/// they run in the same clock domain as their host. PostgreSQL lease decisions
/// deliberately remain database-authoritative (`transaction_timestamp()` /
/// `clock_timestamp()`) to protect fencing across skewed hosts; the
/// `postgres_clock_contract` tests pin that boundary.
#[async_trait]
pub trait Clock: ClockWallTime + Send + Sync + std::fmt::Debug {
    /// Monotonic instant for measuring elapsed time. Never persisted.
    fn now(&self) -> Instant;

    /// One wall-clock instant, shared by the derived durable timestamp faces.
    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc>;

    /// Sleep for `duration`. Replaces `tokio::time::sleep`.
    async fn sleep(&self, duration: Duration);

    /// Sleep until `deadline` (a value from [`now`](Clock::now)). Replaces
    /// `tokio::time::sleep_until`.
    async fn sleep_until(&self, deadline: Instant);
}

/// The real OS clock. Native behavior is identical to the direct `std`/`tokio`
/// calls it replaces.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

/// Derived wall-clock faces, implemented uniformly for every [`Clock`].
///
/// The blanket implementation prevents clocks from supplying disagreeing faces.
/// Each face reads the clock once; RFC 3339 retains the datetime precision.
///
/// A clock cannot override the derived faces independently:
///
/// ```compile_fail,E0119
/// use lash_core::{Clock, ClockWallTime};
/// use std::time::{Duration, Instant};
/// #[derive(Debug)]
/// struct DifferentialClock;
/// #[async_trait::async_trait]
/// impl Clock for DifferentialClock {
///     fn now(&self) -> Instant { Instant::now() }
///     fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
///         chrono::DateTime::from_timestamp_millis(1_000).unwrap()
///     }
///     async fn sleep(&self, _: Duration) {}
///     async fn sleep_until(&self, _: Instant) {}
/// }
/// impl ClockWallTime for DifferentialClock {
///     fn timestamp_rfc3339(&self) -> String { "2026-07-26T00:00:00Z".into() }
///     fn timestamp_ms(&self) -> u64 { 42 }
/// }
/// ```
pub trait ClockWallTime {
    /// Wall-clock time as an RFC 3339 string, for durable records.
    fn timestamp_rfc3339(&self) -> String;

    /// Wall-clock time as nonnegative epoch milliseconds, for durable records.
    fn timestamp_ms(&self) -> u64;
}

impl<T: Clock + ?Sized> ClockWallTime for T {
    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_ms(&self) -> u64 {
        self.timestamp_datetime()
            .timestamp_millis()
            .try_into()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Debug, Default)]
    struct CountingClock(AtomicU64);

    #[async_trait]
    impl Clock for CountingClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
            self.0.fetch_add(1, Ordering::SeqCst);
            chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap()
        }
        async fn sleep(&self, _: Duration) {}
        async fn sleep_until(&self, _: Instant) {}
    }

    #[test]
    fn each_derived_face_reads_one_instant() {
        let clock = CountingClock::default();
        assert_eq!(
            clock.timestamp_rfc3339(),
            "2023-11-14T22:13:20.123456789+00:00"
        );
        assert_eq!(clock.0.load(Ordering::SeqCst), 1);
        assert_eq!(clock.timestamp_ms(), 1_700_000_000_123);
        assert_eq!(clock.0.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn system_clock_wall_clock_faces_agree() {
        let before = SystemClock.timestamp_ms();
        let datetime = SystemClock.timestamp_datetime();
        let text = chrono::DateTime::parse_from_rfc3339(&SystemClock.timestamp_rfc3339())
            .expect("clock emits RFC 3339");
        let after = SystemClock.timestamp_ms();
        assert!((before..=after).contains(&(datetime.timestamp_millis() as u64)));
        assert!((before..=after).contains(&(text.timestamp_millis() as u64)));
    }
}
