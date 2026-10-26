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

/// The node's page retirement (`D-RET`): a maintenance task runs the
/// store's bounded retirement steps, and a node whose store's backlog
/// passes `backlog_limit` claims no new actors until it falls back under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetirementSettings {
    /// How long the task waits between sweeps once it caught up.
    pub every: Duration,
    /// The most nodes one step (one transaction) deletes; at most
    /// [`RetirementSettings::MAX_BATCH_NODES`].
    pub batch_nodes: u64,
    /// The most steps one sweep runs; at most
    /// [`RetirementSettings::MAX_STEPS_PER_SWEEP`].
    pub steps_per_sweep: u32,
    /// The retirement holds above which the node takes no new work and
    /// sweeps without waiting; at most
    /// [`RetirementSettings::MAX_BACKLOG_LIMIT`].
    pub backlog_limit: u64,
}

impl RetirementSettings {
    /// The hard maximum of `batch_nodes`: one step's transaction stays
    /// short beside publication.
    pub const MAX_BATCH_NODES: u64 = 4096;
    /// The hard maximum of `steps_per_sweep`.
    pub const MAX_STEPS_PER_SWEEP: u32 = 256;
    /// The hard maximum of `backlog_limit`.
    pub const MAX_BACKLOG_LIMIT: u64 = 1_000_000;

    /// Standard production: a sweep every second of at most 16 steps of
    /// 256 nodes each, and backpressure above 10,000 holds. No workload
    /// measurement establishes these values.
    pub fn standard() -> Self {
        Self {
            every: Duration::from_secs(1),
            batch_nodes: 256,
            steps_per_sweep: 16,
            backlog_limit: 10_000,
        }
    }
}

/// Overridable bounds for resident session and process owners.
/// The standard values are generous starting points: one hour idle and 10,000 owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerRetentionSettings {
    /// Release an owner after this much idle time; zero releases immediately.
    pub idle: Duration,
    /// The most owners resident on this node, including busy owners.
    pub max_resident_owners: std::num::NonZeroUsize,
}

impl Default for OwnerRetentionSettings {
    fn default() -> Self {
        Self::standard()
    }
}

impl OwnerRetentionSettings {
    /// Generous starting points: one hour idle and at most 10,000 resident owners.
    pub fn standard() -> Self {
        Self {
            idle: Duration::from_secs(3600),
            max_resident_owners: const { std::num::NonZeroUsize::new(10_000).unwrap() },
        }
    }

    pub fn validate(self) -> Result<Self, DurableConfigError> {
        if self.idle.as_millis() > i64::MAX as u128 {
            return Err(DurableConfigError::AboveMaximum {
                field: "owner_retention.idle_evict_ms",
                maximum: i64::MAX as u64,
            });
        }
        if !self.idle.is_zero() && self.idle.as_millis() == 0 {
            return Err(DurableConfigError::BelowResolution {
                field: "owner_retention.idle",
            });
        }
        Ok(self)
    }
}

/// The substrate's parameters as a host configures them. Plain data; it
/// takes effect only through [`DurableSettings::validate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableSettings {
    /// Deployment admission for graph reads of durable owner state.
    pub graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy,
    /// L1, then L8: node-lease timings, including the claim poll.
    pub lease: LeaseSettings,
    /// L3, then L8: the most actors one claim takes; a claim never takes
    /// more than the node's free slots either.
    pub claim_batch: usize,
    /// L3: the most actors a node runs at once.
    pub max_active: usize,
    /// Resident-owner policy; defaults to one hour idle and 10,000 owners.
    pub owner_retention: OwnerRetentionSettings,
    /// One submitted message or queued command; defaults to 64 MiB.
    /// A generous starting point hosts can override to fit their commit budget.
    pub max_ingress_bytes: std::num::NonZeroU64,
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
    /// The node's page retirement and its backpressure.
    pub retirement: RetirementSettings,
}

impl Default for DurableSettings {
    fn default() -> Self {
        Self::standard()
    }
}

impl DurableSettings {
    /// Standard production: standard leases, claims of 16 / 256 active,
    /// one hour idle / 10,000 resident owners, 64 MiB ingress,
    /// activation loop budget 8, groups of 64 / 5 ms,
    /// snapshot every 1,000,000 fuel, cascades of 256, after-commit wakes,
    /// standard retirement ([`RetirementSettings::standard`]).
    /// Lease timing evidence is documented by `LeaseSettings::standard`;
    /// no workload measurement establishes the other numerical values.
    pub fn standard() -> Self {
        Self {
            graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy::standard(),
            lease: LeaseSettings::default(),
            claim_batch: 16,
            max_active: 256,
            owner_retention: OwnerRetentionSettings::standard(),
            max_ingress_bytes: const { std::num::NonZeroU64::new(64 * 1024 * 1024).unwrap() },
            activation_loop_budget: 8,
            group_commit: GroupCommit {
                max_members: 64,
                window: Duration::from_millis(5),
            },
            snapshot_every_fuel: 1_000_000,
            cascade_batch: 256,
            notifier: Notifier::AfterCommit,
            retirement: RetirementSettings::standard(),
        }
    }
}

/// Refused [`DurableSettings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DurableConfigError {
    #[error("max_ingress_bytes ({ingress}) exceeds commit byte budget ({commit})")]
    IngressAboveCommitBudget { ingress: u64, commit: u64 },
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
    /// A value is above its hard maximum.
    #[error("`{field}` must be at most {maximum}")]
    AboveMaximum {
        /// The field.
        field: &'static str,
        /// Its hard maximum.
        maximum: u64,
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
        self.owner_retention.validate()?;
        for (field, value) in [("retirement.every", self.retirement.every)] {
            if value.as_millis() == 0 {
                return Err(DurableConfigError::BelowResolution { field });
            }
        }
        let retirement = self.retirement;
        for (field, value, maximum) in [
            (
                "retirement.batch_nodes",
                retirement.batch_nodes,
                RetirementSettings::MAX_BATCH_NODES,
            ),
            (
                "retirement.steps_per_sweep",
                u64::from(retirement.steps_per_sweep),
                u64::from(RetirementSettings::MAX_STEPS_PER_SWEEP),
            ),
            (
                "retirement.backlog_limit",
                retirement.backlog_limit,
                RetirementSettings::MAX_BACKLOG_LIMIT,
            ),
        ] {
            if value == 0 {
                return Err(DurableConfigError::Zero { field });
            }
            if value > maximum {
                return Err(DurableConfigError::AboveMaximum { field, maximum });
            }
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

    /// D-RET limits: the node's retirement defaults to the documented
    /// standard values, in every preset; each limit validates at its hard
    /// maximum and is refused one above it by its typed `AboveMaximum`
    /// naming the field and the maximum, at zero by `Zero`, and a wait
    /// under 1 ms by `BelowResolution`.
    #[test]
    fn retirement_limits_default_validate_at_their_maxima_and_refuse_above() {
        let standard = RetirementSettings {
            every: Duration::from_secs(1),
            batch_nodes: 256,
            steps_per_sweep: 16,
            backlog_limit: 10_000,
        };
        let base = DurableSettings::default();
        assert_eq!(base.retirement, standard);
        assert_eq!(RetirementSettings::standard(), standard);
        assert_eq!(DurableSettings::development().retirement, standard);
        let with = |retirement: RetirementSettings| {
            DurableSettings { retirement, ..base }
                .validate()
                .map(|config| config.settings().retirement)
        };
        let at_maxima = RetirementSettings {
            every: Duration::from_millis(1),
            batch_nodes: RetirementSettings::MAX_BATCH_NODES,
            steps_per_sweep: RetirementSettings::MAX_STEPS_PER_SWEEP,
            backlog_limit: RetirementSettings::MAX_BACKLOG_LIMIT,
        };
        assert_eq!(with(at_maxima), Ok(at_maxima));
        for (field, above, maximum) in [
            (
                "retirement.batch_nodes",
                RetirementSettings {
                    batch_nodes: RetirementSettings::MAX_BATCH_NODES + 1,
                    ..standard
                },
                RetirementSettings::MAX_BATCH_NODES,
            ),
            (
                "retirement.steps_per_sweep",
                RetirementSettings {
                    steps_per_sweep: RetirementSettings::MAX_STEPS_PER_SWEEP + 1,
                    ..standard
                },
                u64::from(RetirementSettings::MAX_STEPS_PER_SWEEP),
            ),
            (
                "retirement.backlog_limit",
                RetirementSettings {
                    backlog_limit: RetirementSettings::MAX_BACKLOG_LIMIT + 1,
                    ..standard
                },
                RetirementSettings::MAX_BACKLOG_LIMIT,
            ),
        ] {
            assert_eq!(
                with(above),
                Err(DurableConfigError::AboveMaximum { field, maximum })
            );
        }
        for (field, zero) in [
            (
                "retirement.batch_nodes",
                RetirementSettings {
                    batch_nodes: 0,
                    ..standard
                },
            ),
            (
                "retirement.steps_per_sweep",
                RetirementSettings {
                    steps_per_sweep: 0,
                    ..standard
                },
            ),
            (
                "retirement.backlog_limit",
                RetirementSettings {
                    backlog_limit: 0,
                    ..standard
                },
            ),
        ] {
            assert_eq!(with(zero), Err(DurableConfigError::Zero { field }));
        }
        assert_eq!(
            with(RetirementSettings {
                every: Duration::from_micros(999),
                ..standard
            }),
            Err(DurableConfigError::BelowResolution {
                field: "retirement.every"
            })
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
