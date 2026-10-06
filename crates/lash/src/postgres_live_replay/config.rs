//! The PostgreSQL live replay store's one typed, validated configuration.
//!
//! Every knob has a documented default and a range; [`validate`] refuses a
//! value outside it before the store connects, so a deployment never runs
//! with a tick, window or pool it did not mean.
//!
//! [`validate`]: PostgresLiveReplayConfig::validate

use std::time::Duration;

/// Where the store keeps its tables, how it batches, what it retains, and
/// how it holds its connections.
///
/// Serialized with durations in milliseconds (`publish_tick_ms`, ...) and
/// every field optional: an absent field takes its default, an unknown one
/// is refused. Hosts expose it as one JSON object (the workbench's
/// `AGENT_WORKBENCH_LIVE_REPLAY_CONFIG`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostgresLiveReplayConfig {
    /// The PostgreSQL schema that holds the store's tables; it also names
    /// the store's notification channel, so stores in different schemas of
    /// one database are independent. Default `lash_live_replay`: a lowercase
    /// identifier of at most 48 characters.
    pub schema: String,
    /// How long a replica gathers publications, across all its sessions,
    /// before it writes them in one transaction with one notification.
    /// Default 5 ms; 0 writes whatever is queued at once. At most 1 s.
    #[serde(rename = "publish_tick_ms", with = "millis")]
    pub publish_tick: Duration,
    /// Publish transactions a replica runs at once. Each locks only its
    /// own sessions' heads, so ticks overlap their round trips. Default 4;
    /// at least 1 and below `pool_max_connections`.
    pub publish_concurrency: usize,
    /// The most events one publish transaction carries; a tick that gathers
    /// more writes them in further transactions. A single publication is
    /// never split. Default 1024, 1 to 65536.
    pub max_batch_events: usize,
    /// Events retained per session; the oldest beyond it are trimmed.
    /// Default 2048, 1 to 1,000,000.
    pub max_events_per_session: usize,
    /// How long an event stays replayable, by database time. A session idle
    /// this long with nothing retained is forgotten. Default 120 s, 1 ms to
    /// 24 h.
    #[serde(rename = "max_age_ms", with = "millis")]
    pub max_age: Duration,
    /// Encoded bytes retained per session; the oldest events beyond it are
    /// trimmed, and a single publication larger than it is refused.
    /// Default 8 MiB, 1 KiB to 1 GiB.
    pub max_bytes_per_session: usize,
    /// How often a replica reclaims expired events and forgotten sessions.
    /// Default 30 s, 100 ms to 1 h.
    #[serde(rename = "cleanup_interval_ms", with = "millis")]
    pub cleanup_interval: Duration,
    /// The most a cleanup run is delayed past its interval, drawn at random
    /// each run so replicas do not clean in step. Default 10 s; at most the
    /// interval.
    #[serde(rename = "cleanup_jitter_ms", with = "millis")]
    pub cleanup_jitter: Duration,
    /// The first wait before the listener reconnects after losing its
    /// connection; each failed attempt doubles it. Default 100 ms, 1 ms to
    /// 60 s.
    #[serde(rename = "listener_backoff_initial_ms", with = "millis")]
    pub listener_backoff_initial: Duration,
    /// The longest wait between listener reconnect attempts. Default 5 s; at
    /// least the initial backoff, at most 10 min.
    #[serde(rename = "listener_backoff_max_ms", with = "millis")]
    pub listener_backoff_max: Duration,
    /// Connections the store's pool may open, the listener's included.
    /// Default 8, 2 to 256.
    pub pool_max_connections: u32,
    /// Connections the pool keeps open while idle. Default 0; at most
    /// `pool_max_connections`.
    pub pool_min_connections: u32,
    /// How long a query waits for a pooled connection. Default 5 s, 100 ms
    /// to 5 min.
    #[serde(rename = "pool_acquire_timeout_ms", with = "millis")]
    pub pool_acquire_timeout: Duration,
    /// How long an idle pooled connection lives. Default 10 min, 1 s to
    /// 24 h.
    #[serde(rename = "pool_idle_timeout_ms", with = "millis")]
    pub pool_idle_timeout: Duration,
}

impl Default for PostgresLiveReplayConfig {
    fn default() -> Self {
        Self {
            schema: "lash_live_replay".to_string(),
            publish_tick: Duration::from_millis(5),
            publish_concurrency: 4,
            max_batch_events: 1024,
            max_events_per_session: 2048,
            max_age: Duration::from_secs(120),
            max_bytes_per_session: 8 * 1024 * 1024,
            cleanup_interval: Duration::from_secs(30),
            cleanup_jitter: Duration::from_secs(10),
            listener_backoff_initial: Duration::from_millis(100),
            listener_backoff_max: Duration::from_secs(5),
            pool_max_connections: 8,
            pool_min_connections: 0,
            pool_acquire_timeout: Duration::from_secs(5),
            pool_idle_timeout: Duration::from_secs(600),
        }
    }
}

/// A configuration value outside its documented range.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("live replay config `{field}` {reason}")]
pub struct PostgresLiveReplayConfigError {
    /// The serialized name of the refused field.
    pub field: &'static str,
    /// What the field must be.
    pub reason: String,
}

const SCHEMA_MAX_LEN: usize = 48;

impl PostgresLiveReplayConfig {
    /// Refuse any value outside its documented range.
    pub fn validate(&self) -> Result<(), PostgresLiveReplayConfigError> {
        let refuse = |field: &'static str, reason: String| {
            Err(PostgresLiveReplayConfigError { field, reason })
        };
        let schema_ok = self.schema.len() <= SCHEMA_MAX_LEN
            && self
                .schema
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
            && self
                .schema
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !schema_ok {
            return refuse(
                "schema",
                format!(
                    "must be a lowercase identifier ([a-z_][a-z0-9_]*) of at most {SCHEMA_MAX_LEN} characters"
                ),
            );
        }
        let ranges: [(&'static str, Duration, Duration, Duration); 7] = [
            (
                "publish_tick_ms",
                self.publish_tick,
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            (
                "max_age_ms",
                self.max_age,
                Duration::from_millis(1),
                Duration::from_secs(24 * 3600),
            ),
            (
                "cleanup_interval_ms",
                self.cleanup_interval,
                Duration::from_millis(100),
                Duration::from_secs(3600),
            ),
            (
                "cleanup_jitter_ms",
                self.cleanup_jitter,
                Duration::ZERO,
                self.cleanup_interval,
            ),
            (
                "listener_backoff_initial_ms",
                self.listener_backoff_initial,
                Duration::from_millis(1),
                Duration::from_secs(60),
            ),
            (
                "listener_backoff_max_ms",
                self.listener_backoff_max,
                self.listener_backoff_initial,
                Duration::from_secs(600),
            ),
            (
                "pool_acquire_timeout_ms",
                self.pool_acquire_timeout,
                Duration::from_millis(100),
                Duration::from_secs(300),
            ),
        ];
        for (field, value, low, high) in ranges {
            if value < low || value > high {
                return refuse(
                    field,
                    format!(
                        "must be between {} and {} ms",
                        low.as_millis(),
                        high.as_millis()
                    ),
                );
            }
        }
        if self.pool_idle_timeout < Duration::from_secs(1)
            || self.pool_idle_timeout > Duration::from_secs(24 * 3600)
        {
            return refuse(
                "pool_idle_timeout_ms",
                "must be between 1000 and 86400000 ms".to_string(),
            );
        }
        let counts: [(&'static str, usize, usize, usize); 3] = [
            ("max_batch_events", self.max_batch_events, 1, 65_536),
            (
                "max_events_per_session",
                self.max_events_per_session,
                1,
                1_000_000,
            ),
            (
                "max_bytes_per_session",
                self.max_bytes_per_session,
                1024,
                1024 * 1024 * 1024,
            ),
        ];
        for (field, value, low, high) in counts {
            if !(low..=high).contains(&value) {
                return refuse(field, format!("must be between {low} and {high}"));
            }
        }
        if !(2..=256).contains(&self.pool_max_connections) {
            return refuse("pool_max_connections", "must be between 2 and 256".into());
        }
        if self.publish_concurrency == 0
            || self.publish_concurrency >= self.pool_max_connections as usize
        {
            return refuse(
                "publish_concurrency",
                "must be at least 1 and below pool_max_connections".into(),
            );
        }
        if self.pool_min_connections > self.pool_max_connections {
            return refuse(
                "pool_min_connections",
                "must be at most pool_max_connections".into(),
            );
        }
        Ok(())
    }
}

/// Durations as whole milliseconds.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Out-of-range values are refused, by the name a host wrote them
    /// under (FIG-5101).
    #[test]
    fn out_of_range_values_are_refused() {
        assert_eq!(PostgresLiveReplayConfig::default().validate(), Ok(()));
        let refused = |config: PostgresLiveReplayConfig| {
            config
                .validate()
                .expect_err("an out-of-range value is refused")
                .field
        };
        assert_eq!(
            refused(PostgresLiveReplayConfig {
                schema: "Live-Replay".into(),
                ..Default::default()
            }),
            "schema"
        );
        assert_eq!(
            refused(PostgresLiveReplayConfig {
                publish_tick: Duration::from_secs(2),
                ..Default::default()
            }),
            "publish_tick_ms"
        );
        assert_eq!(
            refused(PostgresLiveReplayConfig {
                max_events_per_session: 0,
                ..Default::default()
            }),
            "max_events_per_session"
        );
        assert_eq!(
            refused(PostgresLiveReplayConfig {
                cleanup_jitter: Duration::from_secs(60),
                ..Default::default()
            }),
            "cleanup_jitter_ms"
        );
        assert_eq!(
            refused(PostgresLiveReplayConfig {
                publish_concurrency: 8,
                ..Default::default()
            }),
            "publish_concurrency"
        );
        let unknown = serde_json::from_str::<PostgresLiveReplayConfig>(r#"{"tick_ms": 5}"#);
        assert!(unknown.is_err(), "an unknown field is refused");
        let parsed: PostgresLiveReplayConfig =
            serde_json::from_str(r#"{"publish_tick_ms": 20, "max_age_ms": 60000}"#)
                .expect("a partial config parses");
        assert_eq!(parsed.publish_tick, Duration::from_millis(20));
        assert_eq!(parsed.max_age, Duration::from_secs(60));
        assert_eq!(
            parsed.max_events_per_session, 2048,
            "absent fields keep their defaults"
        );
    }
}
