//! Node-lease timings: host configuration, validated once.
//!
//! The defaults are the PostgreSQL measurement's (S2, FIG-5167): a 3 s
//! heartbeat against a 15 s lease, a 10 s self-stop, a 2 s reap sweep, and a
//! claim poll that backs off from 25 ms to a 250 ms ceiling while claims come
//! back empty, so a lost wake hint costs at most a quarter second. Startup
//! and shutdown are bounded at 2 s each (FIG-5238): a node that cannot
//! register and listen in that time fails to start, and one that cannot
//! release its lease in that time leaves it to expire.

use std::time::Duration;

/// Node-lease timings as a host configures them. Plain data; it takes
/// effect only through [`LeaseSettings::validate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseSettings {
    /// How long a heartbeat keeps a node's lease alive.
    pub ttl: Duration,
    /// How often a node renews its lease.
    pub heartbeat_every: Duration,
    /// How long a node keeps serving with no successful renewal before it
    /// stops itself. Shorter than `ttl`, so a partitioned node stops before
    /// anyone may reap it.
    pub self_stop_after: Duration,
    /// How often a node reaps dead nodes.
    pub reap_every: Duration,
    /// The claim poll's ceiling: how long an idle node waits between claims,
    /// and how long an owner waits for mail with no hint. A lost wake costs
    /// at most this.
    pub claim_poll: Duration,
    /// The claim poll's floor: how soon a node claims again after a claim
    /// that took work. Each empty claim doubles the wait up to
    /// `claim_poll`; a wake hint claims at once.
    pub claim_backoff: Duration,
    /// How long one startup attempt may take, from its registration attempt
    /// until its registration and its listener are in place. Its renewal
    /// starts as soon as it is registered, so a slow listener never ages
    /// the lease. Transient registration failures retry with the claim
    /// backoff until the host stops the node.
    pub startup: Duration,
    /// How long a stopping node waits for its lease's release once its
    /// activations have stopped. A release that does not answer in time is
    /// left to the lease's expiry.
    pub shutdown: Duration,
}

impl Default for LeaseSettings {
    fn default() -> Self {
        Self::standard()
    }
}

impl LeaseSettings {
    /// Standard production: TTL 15 s, heartbeat 3 s, self-stop 10 s,
    /// reap 2 s, claim poll 250 ms / backoff 25 ms, startup and shutdown 2 s.
    /// FIG-5167 measured wakes under these timings; it did not establish
    /// universal numerical choices. Startup/shutdown have no workload evidence.
    pub fn standard() -> Self {
        Self {
            ttl: Duration::from_secs(15),
            heartbeat_every: Duration::from_secs(3),
            self_stop_after: Duration::from_secs(10),
            reap_every: Duration::from_secs(2),
            claim_poll: Duration::from_millis(250),
            claim_backoff: Duration::from_millis(25),
            startup: Duration::from_secs(2),
            shutdown: Duration::from_secs(2),
        }
    }
}

/// Refused [`LeaseSettings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LeaseConfigError {
    /// A timing is under one millisecond: leases are stored in milliseconds.
    #[error("lease timing `{field}` must be at least 1ms")]
    BelowResolution {
        /// The field.
        field: &'static str,
    },
    /// The node would renew no more often than it self-stops, so one late
    /// renewal stops it.
    #[error(
        "heartbeat_every ({heartbeat_every:?}) must be shorter than self_stop_after ({self_stop_after:?})"
    )]
    HeartbeatNotBeforeSelfStop {
        /// The renewal interval.
        heartbeat_every: Duration,
        /// The self-stop bound.
        self_stop_after: Duration,
    },
    /// A partitioned node could still be serving when its lease expires and
    /// another node reaps it.
    #[error("self_stop_after ({self_stop_after:?}) must be shorter than ttl ({ttl:?})")]
    SelfStopNotBeforeExpiry {
        /// The self-stop bound.
        self_stop_after: Duration,
        /// The lease lifetime.
        ttl: Duration,
    },
    /// The claim backoff's floor is above its ceiling.
    #[error("claim_backoff ({claim_backoff:?}) must not exceed claim_poll ({claim_poll:?})")]
    BackoffAbovePoll {
        /// The floor.
        claim_backoff: Duration,
        /// The ceiling.
        claim_poll: Duration,
    },
    /// A timing does not fit a stored millisecond count.
    #[error("lease timing `{field}` is too large")]
    TooLarge {
        /// The field.
        field: &'static str,
    },
}

/// Validated node-lease timings. The default is [`LeaseSettings::default`],
/// which validates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeaseConfig {
    settings: LeaseSettings,
}

impl LeaseSettings {
    /// Development uses the standard lease timings: TTL 15 s, heartbeat 3 s,
    /// self-stop 10 s, reap 2 s, poll 250 ms, backoff 25 ms, startup/shutdown
    /// 2 s. FIG-5167 measured wake behavior, not universal timing suitability.
    pub fn development() -> Self {
        Self::standard()
    }

    /// Validate these timings.
    ///
    /// # Errors
    ///
    /// [`LeaseConfigError`] naming the first rule they break.
    pub fn validate(self) -> Result<LeaseConfig, LeaseConfigError> {
        for (field, value) in [
            ("ttl", self.ttl),
            ("heartbeat_every", self.heartbeat_every),
            ("self_stop_after", self.self_stop_after),
            ("reap_every", self.reap_every),
            ("claim_poll", self.claim_poll),
            ("claim_backoff", self.claim_backoff),
            ("startup", self.startup),
            ("shutdown", self.shutdown),
        ] {
            if value.as_millis() == 0 {
                return Err(LeaseConfigError::BelowResolution { field });
            }
            if i64::try_from(value.as_millis()).is_err() {
                return Err(LeaseConfigError::TooLarge { field });
            }
        }
        if self.heartbeat_every >= self.self_stop_after {
            return Err(LeaseConfigError::HeartbeatNotBeforeSelfStop {
                heartbeat_every: self.heartbeat_every,
                self_stop_after: self.self_stop_after,
            });
        }
        if self.self_stop_after >= self.ttl {
            return Err(LeaseConfigError::SelfStopNotBeforeExpiry {
                self_stop_after: self.self_stop_after,
                ttl: self.ttl,
            });
        }
        if self.claim_backoff > self.claim_poll {
            return Err(LeaseConfigError::BackoffAbovePoll {
                claim_backoff: self.claim_backoff,
                claim_poll: self.claim_poll,
            });
        }
        Ok(LeaseConfig { settings: self })
    }
}

impl LeaseConfig {
    /// The validated settings.
    #[must_use]
    pub fn settings(&self) -> LeaseSettings {
        self.settings
    }

    /// The lease lifetime in stored milliseconds.
    #[must_use]
    pub fn ttl_millis(&self) -> i64 {
        // `validate` refused a ttl whose milliseconds overflow i64.
        i64::try_from(self.settings.ttl.as_millis()).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_validate() {
        assert_eq!(
            LeaseSettings::default().validate(),
            Ok(LeaseConfig::default())
        );
    }

    #[test]
    fn a_node_must_self_stop_before_its_lease_can_be_reaped() {
        let settings = LeaseSettings {
            self_stop_after: Duration::from_secs(15),
            ..LeaseSettings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(LeaseConfigError::SelfStopNotBeforeExpiry { .. })
        ));
    }

    #[test]
    fn a_node_must_renew_before_it_self_stops() {
        let settings = LeaseSettings {
            heartbeat_every: Duration::from_secs(10),
            ..LeaseSettings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(LeaseConfigError::HeartbeatNotBeforeSelfStop { .. })
        ));
    }

    #[test]
    fn the_claim_backoff_stays_under_its_ceiling() {
        let settings = LeaseSettings {
            claim_backoff: Duration::from_millis(500),
            ..LeaseSettings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(LeaseConfigError::BackoffAbovePoll { .. })
        ));
    }

    #[test]
    fn sub_millisecond_timings_are_refused() {
        let settings = LeaseSettings {
            claim_poll: Duration::from_micros(500),
            ..LeaseSettings::default()
        };
        assert_eq!(
            settings.validate(),
            Err(LeaseConfigError::BelowResolution {
                field: "claim_poll"
            })
        );
    }
}
