//! The recovery leader lease, host side (ADR 0109 §1.6–§1.7).
//!
//! One deployment per engine authority runs the leader-only recovery duties:
//! the parks arm, rate-bounded repair scans, drain hand-over, park-feed
//! compaction and opt-in retention. Every other duty, the due-obligation
//! claims among them, runs on every deployment of every store. The lease is
//! load control, never a fence: every duty stays idempotent when two leaders
//! overlap (ADR 0109 §1.6).
//!
//! [`RecoveryLease::step`] makes one acquire-or-renew attempt against the
//! store; the host repeats it on [`RecoveryLease::next_delay`]'s cadence. A
//! leader whose last renew is older than its trust window
//! ([`RecoveryLease::leads`]) acts as a follower until it renews again, so it
//! stops leading before its row can expire.

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

/// The host half of the recovery leader lease over one store.
pub struct RecoveryLease {
    store: Arc<dyn RecoveryLeaderStore>,
    claim: LeaseClaim,
    timings: RecoveryLeaseTimings,
    clock: Arc<dyn Clock>,
    observer: crate::operational_metrics::StoreObserver,
    standing: Mutex<Standing>,
    failed_attempts: Mutex<u64>,
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
        observer: crate::operational_metrics::StoreObserver,
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
            observer,
            standing: Mutex::new(Standing::Follower),
            failed_attempts: Mutex::new(0),
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
        self.observer
            .recovery_leadership(self.claim.name.as_str(), leading, term);
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
        if matches!(&answer, Ok(Ok(_))) {
            let mut count = self
                .failed_attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *count > 0 {
                tracing::info!(
                    event = "recovery_lease.recovered",
                    lease = self.claim.name.as_str(),
                    failure_count = *count,
                    "recovery lease store attempts recovered"
                );
                *count = 0;
            }
        }
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
                self.attempt_failed("store", Some(error.runtime_code().as_str()), &error);
                Standing::Follower
            }
            Err(_) => {
                self.attempt_failed("timeout", None, &"recovery lease attempt timed out");
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

    fn attempt_failed(
        &self,
        error_type: &str,
        error_code: Option<&str>,
        error: &dyn std::fmt::Display,
    ) {
        let mut count = self
            .failed_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count = count.saturating_add(1);
        if *count == 1 {
            tracing::warn!(event = "recovery_lease.degraded", lease = self.claim.name.as_str(),
                error_type, error_code, %error, failure_count = *count,
                "recovery lease attempt failed; this process follows until one succeeds");
        }
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

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use crate::store::{LeaseAnswer, StoreError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Faults(AtomicUsize);
    #[async_trait::async_trait]
    impl RecoveryLeaderStore for Faults {
        async fn acquire(&self, _: &LeaseClaim) -> Result<LeaseAnswer, StoreError> {
            match self.0.fetch_add(1, Ordering::SeqCst) {
                0 | 1 => Err(StoreError::Contended),
                2 => std::future::pending().await,
                _ => Ok(LeaseAnswer {
                    leader: false,
                    row: None,
                    db_now_ms: 0,
                }),
            }
        }
        async fn renew(&self, claim: &LeaseClaim, _: i64) -> Result<LeaseAnswer, StoreError> {
            self.acquire(claim).await
        }
        async fn resign(&self, _: &LeaseName, _: &HolderId, _: i64) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    /// FIG-5531: errors and timeouts are one degraded episode; a successful
    /// follower read recovers it without claiming leadership.
    #[tokio::test(start_paused = true)]
    async fn repeated_acquisition_failures_report_transitions_and_count() {
        let faults = Arc::new(Faults(AtomicUsize::new(0)));
        let lease = RecoveryLease::new(
            faults.clone(),
            LeaseName::new("diagnostic-law"),
            1,
            RecoveryLeaseTimings::default(),
            Arc::new(crate::SystemClock),
            Default::default(),
        );
        let (_, capture) = crate::testing::trace_capture::capturing(|| async {
            for _ in 0..4 {
                assert_eq!(lease.step().await, Standing::Follower);
            }
        })
        .await;
        assert_eq!(capture.exactly_one("recovery_lease.degraded").level, "WARN");
        assert_eq!(
            capture
                .exactly_one("recovery_lease.recovered")
                .field("failure_count"),
            "3"
        );
        faults.0.store(0, Ordering::SeqCst);
        let (_, next) =
            crate::testing::trace_capture::capturing(|| async { lease.step().await }).await;
        next.exactly_one("recovery_lease.degraded");
    }
}
