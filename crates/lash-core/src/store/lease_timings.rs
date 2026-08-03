//! Host-configurable lease timing capability.

use std::time::Duration;

/// How many renew intervals must fit inside one lease TTL.
///
/// The runtime renews every lease it holds on a background cadence; requiring
/// the TTL to cover at least three renew intervals means a healthy owner can
/// miss two consecutive renewals (scheduler stalls, transient store errors)
/// before a peer may treat the lease as expired. This generalizes the previous
/// hardcoded 30s TTL / 10s renew contract.
const MIN_TTL_TO_RENEW_RATIO: u32 = 3;

/// Lease timing capability for the durable single-writer *lease* lanes the
/// runtime renews on a cadence: session execution leases, process leases, and
/// durable effect-replay leases.
///
/// Queued-work and turn-input claims are deliberately *not* governed by this
/// type: they are not leases, carry no TTL, and are never renewed. They pin a
/// session-execution-lease generation so a successor can re-claim them after
/// handoff (ADR 0029); settlement authority remains claim ownership, not time.
///
/// The TTL is the failover-latency vs false-takeover-risk knob: a shorter TTL
/// lets a peer reclaim work from a crashed owner sooner, while a longer TTL
/// tolerates slower renewal under load. The renew interval is how often a live
/// owner extends its leases; the constructor enforces
/// `ttl >= 3 * renew_interval` so a healthy owner always has renewal slack
/// before its lease can expire under it.
///
/// Defaults to 30s TTL with a 10s renew interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseTimings {
    ttl: Duration,
    renew_interval: Duration,
}

/// Rejected [`LeaseTimings`] construction.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LeaseTimingsError {
    #[error("lease ttl must be at least 1ms")]
    TtlTooSmall,
    #[error("lease renew interval must be at least 1ms")]
    RenewIntervalTooSmall,
    #[error(
        "lease ttl ({ttl:?}) must be at least {MIN_TTL_TO_RENEW_RATIO}x the renew interval \
         ({renew_interval:?}) so an owner can miss renewals without losing a live lease"
    )]
    TtlRenewRatioTooSmall {
        ttl: Duration,
        renew_interval: Duration,
    },
}

impl LeaseTimings {
    /// Build lease timings, enforcing `ttl >= 3 * renew_interval` and
    /// millisecond-resolution non-zero values (leases are persisted in epoch
    /// milliseconds).
    pub fn new(ttl: Duration, renew_interval: Duration) -> Result<Self, LeaseTimingsError> {
        if ttl.as_millis() == 0 {
            return Err(LeaseTimingsError::TtlTooSmall);
        }
        if renew_interval.as_millis() == 0 {
            return Err(LeaseTimingsError::RenewIntervalTooSmall);
        }
        if ttl < renew_interval.saturating_mul(MIN_TTL_TO_RENEW_RATIO) {
            return Err(LeaseTimingsError::TtlRenewRatioTooSmall {
                ttl,
                renew_interval,
            });
        }
        Ok(Self {
            ttl,
            renew_interval,
        })
    }

    /// Build lease timings from a TTL alone, deriving the renew interval as
    /// `ttl / 3` (the boundary the invariant allows).
    pub fn from_ttl(ttl: Duration) -> Result<Self, LeaseTimingsError> {
        Self::new(ttl, ttl / MIN_TTL_TO_RENEW_RATIO)
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn renew_interval(&self) -> Duration {
        self.renew_interval
    }

    /// TTL in epoch milliseconds, as passed to store lease claim/renew calls
    /// (session execution, process, and effect-replay leases).
    pub fn ttl_ms(&self) -> u64 {
        duration_to_ms(self.ttl)
    }

    pub fn renew_interval_ms(&self) -> u64 {
        duration_to_ms(self.renew_interval)
    }
}

impl Default for LeaseTimings {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30),
            renew_interval: Duration::from_secs(10),
        }
    }
}

fn duration_to_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_lease_timings_keep_the_contractual_windows() {
        let timings = LeaseTimings::default();
        assert_eq!(timings.ttl_ms(), 30_000);
        assert_eq!(timings.renew_interval_ms(), 10_000);
        assert_eq!(
            timings.ttl_ms(),
            timings.renew_interval_ms() * u64::from(MIN_TTL_TO_RENEW_RATIO)
        );
    }

    #[test]
    fn constructor_enforces_ttl_renew_ratio() {
        assert!(LeaseTimings::new(Duration::from_secs(30), Duration::from_secs(10)).is_ok());
        assert_eq!(
            LeaseTimings::new(Duration::from_secs(29), Duration::from_secs(10)),
            Err(LeaseTimingsError::TtlRenewRatioTooSmall {
                ttl: Duration::from_secs(29),
                renew_interval: Duration::from_secs(10),
            })
        );
        assert_eq!(
            LeaseTimings::new(Duration::ZERO, Duration::from_secs(1)),
            Err(LeaseTimingsError::TtlTooSmall)
        );
        assert_eq!(
            LeaseTimings::new(Duration::from_secs(30), Duration::from_micros(500)),
            Err(LeaseTimingsError::RenewIntervalTooSmall)
        );
    }

    #[test]
    fn from_ttl_derives_the_boundary_renew_interval() {
        let timings = LeaseTimings::from_ttl(Duration::from_millis(60)).expect("valid timings");
        assert_eq!(timings.ttl_ms(), 60);
        assert_eq!(timings.renew_interval_ms(), 20);
        assert_eq!(
            LeaseTimings::from_ttl(Duration::from_millis(2)),
            Err(LeaseTimingsError::RenewIntervalTooSmall)
        );
    }
}
