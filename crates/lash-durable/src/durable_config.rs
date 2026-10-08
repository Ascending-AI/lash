//! [`DurableConfig`]: every substrate parameter, typed and validated once
//! (ADR 0132 §3).
//!
//! The shape is I0's (FIG-5194); each field's owning lane tunes its value
//! and never the shape.

use std::time::Duration;

use crate::config::{LeaseConfig, LeaseConfigError, LeaseSettings};

/// How wakes reach other nodes beyond the claim poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notifier {
    /// Only the claim poll and the per-node mail scan: correct on every
    /// store, at the poll's latency.
    PollOnly,
    /// After each commit that woke an actor, the store's node wakes hint the
    /// owner's node. A latency hint only; correctness never depends on it.
    /// The listener also holds the node's liveness lock, so a crashed node
    /// is reaped as soon as its listener ends rather than when its lease
    /// lapses. A store without node wakes (a SQLite memory database) hints
    /// in process only.
    ///
    /// On PostgreSQL, `LISTEN` and the session lock need a session of their
    /// own: connect the store directly or through a session-mode pooler,
    /// never a transaction-mode one.
    AfterCommit,
}

/// How finished round members commit in groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupCommit {
    /// The most members one outcome transaction carries.
    pub max_members: usize,
    /// How long a finished member may wait for others to share its
    /// transaction.
    pub window: Duration,
}

/// The substrate's parameters as a host configures them. Plain data; it
/// takes effect only through [`DurableSettings::validate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableSettings {
    /// L1, then L8: node-lease timings, including the claim poll.
    pub lease: LeaseSettings,
    /// L3, then L8: the most actors one claim takes; a claim never takes
    /// more than the node's free slots either.
    pub claim_batch: usize,
    /// L3: the most actors a node runs at once.
    pub max_active: usize,
    /// L3: how long an owner keeps an idle actor hot before it releases it.
    pub idle_evict: Duration,
    /// L6: consecutive claims without progress before an actor parks with
    /// `ActivationLoop`.
    pub activation_loop_budget: u32,
    /// L4: group commit of finished round members.
    pub group_commit: GroupCommit,
    /// L7: VM fuel spent without an effect after which a quiet point
    /// snapshots anyway, bounding recomputation after a crash.
    pub snapshot_every_fuel: u64,
    /// L6: how many `Until` children one cascade transaction marks.
    pub cascade_batch: usize,
    /// L8: how wakes reach other nodes, and whether a node's crash is seen
    /// through its listener's liveness lock.
    pub notifier: Notifier,
}

impl Default for DurableSettings {
    fn default() -> Self {
        Self::standard()
    }
}

impl DurableSettings {
    /// Standard production: standard leases, claims of 16 / 256 active,
    /// idle eviction 60 s, activation loop budget 8, groups of 64 / 5 ms,
    /// snapshot every 1,000,000 fuel, cascades of 256, after-commit wakes.
    /// Lease timing evidence is documented by `LeaseSettings::standard`;
    /// no workload measurement establishes the other numerical values.
    pub fn standard() -> Self {
        Self {
            lease: LeaseSettings::default(),
            claim_batch: 16,
            max_active: 256,
            idle_evict: Duration::from_secs(60),
            activation_loop_budget: 8,
            group_commit: GroupCommit {
                max_members: 64,
                window: Duration::from_millis(5),
            },
            snapshot_every_fuel: 1_000_000,
            cascade_batch: 256,
            notifier: Notifier::AfterCommit,
        }
    }
}

/// Refused [`DurableSettings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DurableConfigError {
    /// The lease timings are refused.
    #[error(transparent)]
    Lease(#[from] LeaseConfigError),
    /// A count that must be at least one is zero.
    #[error("`{field}` must be at least 1")]
    Zero {
        /// The field.
        field: &'static str,
    },
    /// A duration is under one millisecond: durable time is stored in
    /// milliseconds.
    #[error("`{field}` must be at least 1ms")]
    BelowResolution {
        /// The field.
        field: &'static str,
    },
    /// A claim could take more actors than the node may run.
    #[error("claim_batch ({claim_batch}) must not exceed max_active ({max_active})")]
    ClaimBeyondCapacity {
        /// The claim batch.
        claim_batch: usize,
        /// The node's capacity.
        max_active: usize,
    },
    /// The group-commit window is not shorter than the claim poll, so a
    /// member's outcome could wait longer than a wake.
    #[error("group_commit.window ({window:?}) must be shorter than lease.claim_poll ({poll:?})")]
    GroupWindowNotBeforePoll {
        /// The window.
        window: Duration,
        /// The claim poll.
        poll: Duration,
    },
}

/// Validated substrate parameters. The default is
/// [`DurableSettings::default`], which validates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableConfig {
    lease: LeaseConfig,
    settings: DurableSettings,
}

impl Default for DurableConfig {
    #[expect(
        clippy::expect_used,
        reason = "the default settings validate; a law in this module pins it"
    )]
    fn default() -> Self {
        DurableSettings::default()
            .validate()
            .expect("the default durable settings validate")
    }
}

impl DurableSettings {
    /// Development: 2 actors per claim, 8 active actors, groups of 8, and
    /// cascades of 32. All other values are standard. These smaller working
    /// capacities are unmeasured conveniences for a local host.
    pub fn development() -> Self {
        Self {
            lease: LeaseSettings::development(),
            claim_batch: 2,
            max_active: 8,
            group_commit: GroupCommit {
                max_members: 8,
                ..Self::standard().group_commit
            },
            cascade_batch: 32,
            ..Self::standard()
        }
    }

    /// Validate these parameters.
    ///
    /// # Errors
    ///
    /// [`DurableConfigError`] naming the first rule they break.
    pub fn validate(self) -> Result<DurableConfig, DurableConfigError> {
        let lease = self.lease.validate()?;
        for (field, value) in [
            ("claim_batch", self.claim_batch),
            ("max_active", self.max_active),
            ("group_commit.max_members", self.group_commit.max_members),
            ("cascade_batch", self.cascade_batch),
        ] {
            if value == 0 {
                return Err(DurableConfigError::Zero { field });
            }
        }
        if self.activation_loop_budget == 0 {
            return Err(DurableConfigError::Zero {
                field: "activation_loop_budget",
            });
        }
        if self.snapshot_every_fuel == 0 {
            return Err(DurableConfigError::Zero {
                field: "snapshot_every_fuel",
            });
        }
        if self.idle_evict.as_millis() == 0 {
            return Err(DurableConfigError::BelowResolution {
                field: "idle_evict",
            });
        }
        if self.claim_batch > self.max_active {
            return Err(DurableConfigError::ClaimBeyondCapacity {
                claim_batch: self.claim_batch,
                max_active: self.max_active,
            });
        }
        if self.group_commit.window >= self.lease.claim_poll {
            return Err(DurableConfigError::GroupWindowNotBeforePoll {
                window: self.group_commit.window,
                poll: self.lease.claim_poll,
            });
        }
        Ok(DurableConfig {
            lease,
            settings: self,
        })
    }
}

impl DurableConfig {
    /// The validated settings.
    #[must_use]
    pub fn settings(&self) -> DurableSettings {
        self.settings
    }

    /// The validated lease timings.
    #[must_use]
    pub fn lease(&self) -> LeaseConfig {
        self.lease
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_validate() {
        assert_eq!(
            DurableSettings::default().validate().map(|c| c.settings()),
            Ok(DurableSettings::default())
        );
    }

    #[test]
    fn each_refusal_fires() {
        let base = DurableSettings::default();
        let refused = |settings: DurableSettings| settings.validate().unwrap_err();
        assert!(matches!(
            refused(DurableSettings {
                lease: LeaseSettings {
                    claim_poll: Duration::ZERO,
                    ..base.lease
                },
                ..base
            }),
            DurableConfigError::Lease(_)
        ));
        assert_eq!(
            refused(DurableSettings {
                cascade_batch: 0,
                ..base
            }),
            DurableConfigError::Zero {
                field: "cascade_batch"
            }
        );
        assert_eq!(
            refused(DurableSettings {
                activation_loop_budget: 0,
                ..base
            }),
            DurableConfigError::Zero {
                field: "activation_loop_budget"
            }
        );
        assert_eq!(
            refused(DurableSettings {
                idle_evict: Duration::from_micros(10),
                ..base
            }),
            DurableConfigError::BelowResolution {
                field: "idle_evict"
            }
        );
        assert!(matches!(
            refused(DurableSettings {
                claim_batch: 300,
                ..base
            }),
            DurableConfigError::ClaimBeyondCapacity { .. }
        ));
        assert!(matches!(
            refused(DurableSettings {
                group_commit: GroupCommit {
                    max_members: 1,
                    window: Duration::from_secs(1),
                },
                ..base
            }),
            DurableConfigError::GroupWindowNotBeforePoll { .. }
        ));
    }
}
