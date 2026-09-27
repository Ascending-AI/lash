//! The recovery leader lease, host side (ADR 0109 §1.6–§1.7).
//!
//! One deployment per engine authority runs the leader-only recovery duties:
//! the parks arm, rate-bounded repair scans, drain hand-over, park-feed
//! compaction and opt-in retention; on SQLite also the due-obligation claims.
//! Every other duty runs on every deployment. The lease is load control, never
//! a fence: every duty stays idempotent when two leaders overlap (ADR 0080).
//!
//! [`RecoveryLease::step`] makes one acquire-or-renew attempt against the
//! store; the host repeats it on [`RecoveryLease::next_delay`]'s cadence. The
//! reconcile tick asks [`RecoveryLease::duties`] before each arm: a leader
//! whose last renew is older than its trust window acts as a follower until
//! it renews again, so it stops leading before its row can expire.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Clock;
use crate::store::{HolderId, LeaseClaim, LeaseName, RecoveryLeaderStore};

pub use crate::engine::{RecoveryLeaseConfig, RecoveryLeaseTimings};

/// Where this process stands on the lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// This process leads `term` and trusts it until `trusted_until_ms`
    /// (host clock).
    Leader { term: i64, trusted_until_ms: u64 },
    /// Another process leads, or nobody could be elected.
    Follower,
}

/// Which recovery duties this process may run right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryDuties {
    /// The leader-only duties: parks, repair scans, drain hand-over,
    /// compaction, retention.
    pub leader: bool,
    /// Due-obligation claims: every deployment where the store lets claims
    /// skip each other, the leader only where it does not.
    pub due_claims: bool,
}

impl RecoveryDuties {
    /// Every duty: what a caller that owns its storage alone runs.
    pub const ALL: Self = Self {
        leader: true,
        due_claims: true,
    };
}

/// The host half of the recovery leader lease over one store.
pub struct RecoveryLease {
    store: Arc<dyn RecoveryLeaderStore>,
    claim: LeaseClaim,
    timings: RecoveryLeaseTimings,
    clock: Arc<dyn Clock>,
    standing: Mutex<Standing>,
    /// Serializes attempts, so the background cadence and an inline step
    /// never race each other's term.
    attempt: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for RecoveryLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryLease")
            .field("name", &self.claim.name)
            .field("holder", &self.claim.holder)
            .field("standing", &self.standing())
            .finish_non_exhaustive()
    }
}

impl RecoveryLease {
    /// A fresh holder competing for lease `name` on `store` at build rank
    /// `generation_rank`.
    #[must_use]
    pub fn new(
        store: Arc<dyn RecoveryLeaderStore>,
        name: LeaseName,
        generation_rank: i64,
        timings: RecoveryLeaseTimings,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            claim: LeaseClaim {
                name,
                holder: HolderId::mint(),
                generation_rank,
                ttl_ms: duration_ms(timings.ttl),
                min_tenure_ms: duration_ms(timings.min_tenure),
            },
            timings,
            clock,
            standing: Mutex::new(Standing::Follower),
            attempt: tokio::sync::Mutex::new(()),
        }
    }

    /// This process's holder id.
    #[must_use]
    pub fn holder(&self) -> &HolderId {
        &self.claim.holder
    }

    /// The lease's name.
    #[must_use]
    pub fn name(&self) -> &LeaseName {
        &self.claim.name
    }

    /// The standing the last attempt recorded.
    #[must_use]
    pub fn standing(&self) -> Standing {
        *self
            .standing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set_standing(&self, standing: Standing) {
        *self
            .standing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = standing;
        let (leading, term) = match standing {
            Standing::Leader { term, .. } => (true, u64::try_from(term).unwrap_or_default()),
            Standing::Follower => (false, 0),
        };
        crate::operational_metrics::record_recovery_leadership(
            self.claim.name.as_str(),
            leading,
            term,
        );
    }

    /// Whether this process leads at `now_ms` (host clock) within its trust
    /// window.
    #[must_use]
    pub fn leads(&self, now_ms: u64) -> bool {
        matches!(
            self.standing(),
            Standing::Leader { trusted_until_ms, .. } if now_ms < trusted_until_ms
        )
    }

    /// The duties this process may run at `now_ms`.
    #[must_use]
    pub fn duties(&self, now_ms: u64) -> RecoveryDuties {
        let leader = self.leads(now_ms);
        RecoveryDuties {
            leader,
            due_claims: leader || !self.store.due_claims_need_leader(),
        }
    }

    /// One acquire-or-renew attempt: a leader renews its term, anyone else
    /// tries to acquire. An attempt that fails or outlasts
    /// [`RecoveryLeaseTimings::renew_timeout`] leaves this process a follower.
    pub async fn step(&self) -> Standing {
        let _attempt = self.attempt.lock().await;
        let started_ms = self.clock.timestamp_ms();
        let held = match self.standing() {
            Standing::Leader { term, .. } => Some(term),
            Standing::Follower => None,
        };
        let request = async {
            match held {
                Some(term) => self.store.renew(&self.claim, term).await,
                None => self.store.acquire(&self.claim).await,
            }
        };
        let answer = tokio::time::timeout(self.timings.renew_timeout, request).await;
        let standing = match answer {
            Ok(Ok(answer)) if answer.leader => match answer.row {
                Some(row) => Standing::Leader {
                    term: row.term,
                    trusted_until_ms: started_ms
                        .saturating_add(duration_ms(self.timings.ttl))
                        .saturating_sub(duration_ms(self.timings.trust_margin)),
                },
                None => Standing::Follower,
            },
            Ok(Ok(_)) => Standing::Follower,
            Ok(Err(error)) => {
                tracing::warn!(lease = self.claim.name.as_str(), %error, "recovery lease attempt failed; this process follows until one succeeds");
                Standing::Follower
            }
            Err(_) => {
                tracing::warn!(
                    lease = self.claim.name.as_str(),
                    "recovery lease attempt timed out; this process follows until one succeeds"
                );
                Standing::Follower
            }
        };
        if held.is_some() && standing == Standing::Follower {
            tracing::info!(
                lease = self.claim.name.as_str(),
                "recovery lease lost; leader duties stop"
            );
        }
        self.set_standing(standing);
        standing
    }

    /// Give up the lease if this process holds it, so a follower takes over
    /// at its next attempt instead of after the TTL. Leaves this process a
    /// follower either way.
    pub async fn resign(&self) {
        let _attempt = self.attempt.lock().await;
        if let Standing::Leader { term, .. } = self.standing() {
            let resigned = self
                .store
                .resign(&self.claim.name, &self.claim.holder, term)
                .await;
            if let Err(error) = resigned {
                tracing::warn!(lease = self.claim.name.as_str(), %error, "recovery lease resign failed; it expires after its TTL");
            }
        }
        self.set_standing(Standing::Follower);
    }

    /// The delay before the next attempt after one that left `standing`.
    #[must_use]
    pub fn next_delay(&self, standing: Standing) -> Duration {
        match standing {
            Standing::Leader { .. } => self.timings.renew_every,
            Standing::Follower => {
                let jitter_ms = duration_ms(self.timings.follower_jitter);
                let jitter = if jitter_ms == 0 {
                    0
                } else {
                    u64::try_from(uuid::Uuid::new_v4().as_u128() % u128::from(jitter_ms))
                        .unwrap_or_default()
                };
                self.timings.follower_retry + Duration::from_millis(jitter)
            }
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
