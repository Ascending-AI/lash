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
//! `timestamp_ms`/`node_timestamp` are wall-clock (durable records). `sleep`
//! and `sleep_until` replace direct `tokio::time` calls so a fake clock can
//! resolve them without real wall-clock waits.

use std::time::{Duration, Instant};

use async_trait::async_trait;

/// Runtime and embedded-store time source. Cloneable as `Arc<dyn Clock>`;
/// carried on the runtime host configuration.
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

    /// Replaces `tokio::time::sleep`.
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
///     fn node_timestamp(&self) -> lash_core::session_graph::NodeTimestamp { todo!() }
///     fn timestamp_ms(&self) -> u64 { 42 }
/// }
/// ```
pub trait ClockWallTime {
    /// Wall-clock time as a validated nanosecond UTC node timestamp.
    fn node_timestamp(&self) -> NodeTimestamp;

    /// Wall-clock time as nonnegative epoch milliseconds, for durable records.
    fn timestamp_ms(&self) -> u64;
}

impl<T: Clock + ?Sized> ClockWallTime for T {
    fn node_timestamp(&self) -> NodeTimestamp {
        NodeTimestamp::new(self.timestamp_datetime())
            .unwrap_or_else(|error| panic!("clock returned an invalid node timestamp: {error}"))
    }

    fn timestamp_ms(&self) -> u64 {
        self.timestamp_datetime()
            .timestamp_millis()
            .try_into()
            .unwrap_or_default()
    }
}

/// A graph-node instant with exactly one wire spelling: `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`.
///
/// Construction bounds the year to four digits; parsing also refuses offsets,
/// omitted precision and any spelling that differs from the canonical UTC text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeTimestamp(chrono::DateTime<chrono::Utc>);

/// A refused graph-node timestamp, before it can enter durable history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NodeTimestampError {
    #[error("node timestamp must use YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ with a valid UTC instant")]
    Noncanonical,
    #[error("node timestamp year must be between 0000 and 9999")]
    YearOutOfRange,
}

impl NodeTimestamp {
    /// Width in bytes of every serialized timestamp, excluding JSON quotes.
    pub const WIDTH: usize = 30;

    pub fn new(datetime: chrono::DateTime<chrono::Utc>) -> Result<Self, NodeTimestampError> {
        use chrono::Datelike as _;
        if !(0..=9999).contains(&datetime.year()) {
            return Err(NodeTimestampError::YearOutOfRange);
        }
        Ok(Self(datetime))
    }

    /// The same instant in the session metadata's epoch-millisecond domain.
    pub fn timestamp_millis(self) -> i64 {
        self.0.timestamp_millis()
    }
}

impl std::fmt::Display for NodeTimestamp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
    }
}

impl std::str::FromStr for NodeTimestamp {
    type Err = NodeTimestampError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != Self::WIDTH {
            return Err(NodeTimestampError::Noncanonical);
        }
        let datetime = chrono::DateTime::parse_from_rfc3339(text)
            .map_err(|_| NodeTimestampError::Noncanonical)?;
        let timestamp = Self::new(datetime.with_timezone(&chrono::Utc))?;
        if timestamp.to_string() != text {
            return Err(NodeTimestampError::Noncanonical);
        }
        Ok(timestamp)
    }
}

impl serde::Serialize for NodeTimestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for NodeTimestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for NodeTimestamp {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NodeTimestamp".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time",
            "minLength": 30,
            "maxLength": 30,
            "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\\.[0-9]{9}Z$"
        })
    }
}
