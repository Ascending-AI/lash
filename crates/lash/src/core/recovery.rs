//! The core's seat in the recovery leader election (ADR 0109 §1.6–§1.7), and
//! the artifact-cleanup outbox's due pass it runs (ADR 0132 §12).
//!
//! One slot per core. The election runs on a background task of its own,
//! which keeps the lease's cadence and resigns when the slot is dropped or the
//! core shuts down. The cleanup pass runs on another: every
//! [`RECOVERY_TICK`] it claims a bounded page of due `ArtifactCleanup` rows
//! when this deployment's duties include due claims, so a cleanup whose
//! producer died before its immediate attempt is still delivered.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lash_core::engine::RecoveryLeaseConfig;
use lash_core::runtime::obligations::relay::{ObligationRelay, RelayPass, relay_due};
use lash_core::runtime::obligations::{RECOVERY_TICK, RecoveryInterval};
use lash_core::runtime::recovery_lease::RecoveryLease;
use lash_core::store::{LeaseName, RecoveryLeaderStore, StoreError};

use crate::support::RuntimeEnvironment;

/// The most due cleanups one pass claims.
const CLEANUP_PAGE: NonZeroUsize = NonZeroUsize::MIN.saturating_add(255);

pub(crate) struct RecoverySlot {
    /// The slot's one holder, for the core's whole life: an election whose
    /// caller went away is still this holder's, so its lease is resigned
    /// with the slot rather than left to its TTL.
    lease: Arc<RecoveryLease>,
    /// Set once the election task is started: exactly one per slot.
    started: std::sync::atomic::AtomicBool,
    /// `true` once the election's first attempt answered.
    first: Arc<tokio::sync::watch::Sender<bool>>,
    shutdown: tokio_util::sync::CancellationToken,
    clock: Arc<dyn lash_core::Clock>,
}

impl RecoverySlot {
    /// The slot for a core over `env`: the lease is named after the engine
    /// authority that owns the effect state, in the storage the backend's
    /// store set holds.
    pub(crate) fn new(env: &RuntimeEnvironment, config: RecoveryLeaseConfig) -> Self {
        let authority = env.core.control.effect_host.backend().binding_identity();
        Self::over(
            env.core.backend().recovery_leader(),
            LeaseName::new(format!("recovery:{authority}")),
            config,
            Arc::clone(&env.core.clock),
            lash_core::operational_metrics::StoreObserver::new(env.core.tracing.metrics().clone()),
        )
    }

    /// The slot competing for lease `name` on `store`.
    fn over(
        store: Arc<dyn RecoveryLeaderStore>,
        name: LeaseName,
        config: RecoveryLeaseConfig,
        clock: Arc<dyn lash_core::Clock>,
        observer: lash_core::operational_metrics::StoreObserver,
    ) -> Self {
        Self {
            lease: Arc::new(RecoveryLease::new(
                store,
                name,
                config.generation_rank,
                config.timings,
                Arc::clone(&clock),
                observer,
            )),
            started: std::sync::atomic::AtomicBool::new(false),
            first: Arc::new(tokio::sync::watch::channel(false).0),
            shutdown: tokio_util::sync::CancellationToken::new(),
            clock,
        }
    }

    /// Start the election once per slot, on a task of its own, never in a
    /// caller: a caller cancelled after the store granted the lease (a pass
    /// aborted by a shutdown) would otherwise drop the only holder that could
    /// renew or resign it, and the row would name a holder nobody runs until
    /// its TTL lapses. The task resigns once the slot is dropped, whatever
    /// its first attempt answered.
    fn spawn_election(&self, runtime: &tokio::runtime::Handle) {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let lease = Arc::clone(&self.lease);
        let first = Arc::clone(&self.first);
        let shutdown = self.shutdown.clone();
        runtime.spawn(async move {
            let standing = lease.step().await;
            first.send_replace(true);
            keep_cadence(lease, standing, shutdown).await;
        });
    }

    /// Run `relay`'s due pass every [`RECOVERY_TICK`] on a task of its own
    /// until the slot is dropped or the core shuts down. The first pass waits
    /// for the election's first answer, so an uncontested deployment claims
    /// at once. Without a runtime to run it on, nothing starts: such a core
    /// has no background work.
    pub(crate) fn start_cleanup(&self, relay: Arc<dyn ObligationRelay>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.spawn_election(&runtime);
        let lease = Arc::clone(&self.lease);
        let clock = Arc::clone(&self.clock);
        let shutdown = self.shutdown.clone();
        let mut answered = self.first.subscribe();
        runtime.spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => return,
                // The sender lives in the slot, so the wait ends on the
                // answer or with the slot.
                _ = answered.wait_for(|answered| *answered) => {}
            }
            let mut interval = RecoveryInterval::new(Arc::clone(&clock), RECOVERY_TICK);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = interval.tick() => {}
                }
                if let Err(error) =
                    cleanup_pass(&lease, relay.as_ref(), clock.as_ref(), CLEANUP_PAGE).await
                {
                    tracing::warn!(%error, "artifact cleanup pass failed; the next tick retries it");
                }
            }
        });
    }

    /// Give the lease up now, and stop the slot's background work.
    pub(crate) async fn resign(&self) {
        self.shutdown.cancel();
        if self.started.load(std::sync::atomic::Ordering::SeqCst) {
            self.lease.resign().await;
        }
    }
}

/// One cleanup pass: claim and deliver at most `page` due cleanups when
/// `lease`'s duties at `clock`'s now include due claims (ADR 0109 §1.7: the
/// leader alone where claims do not skip each other, every deployment where
/// they do). `None` when this deployment may not claim.
pub(crate) async fn cleanup_pass(
    lease: &RecoveryLease,
    relay: &dyn ObligationRelay,
    clock: &dyn lash_core::Clock,
    page: NonZeroUsize,
) -> Result<Option<RelayPass>, StoreError> {
    if !lease.duties(clock.timestamp_ms()).due_claims {
        return Ok(None);
    }
    relay_due(relay, clock, page).await.map(Some)
}

/// Step `lease` on its own cadence from the first attempt's `standing`
/// until `shutdown` fires, then resign: wait the delay the current standing
/// calls for, then attempt again.
async fn keep_cadence(
    lease: Arc<RecoveryLease>,
    mut standing: lash_core::runtime::recovery_lease::Standing,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(lease.next_delay(standing)) => {}
        }
        standing = lease.step().await;
    }
    lease.resign().await;
}

impl Drop for RecoverySlot {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use lash_core::engine::{RecoveryLeaseConfig, RecoveryLeaseTimings};
    use lash_core::store::{
        HolderId, LeaseAnswer, LeaseClaim, LeaseName, LeaseRow, RecoveryLeaderStore, StoreError,
    };

    use super::*;

    /// A lease row in memory whose first grant answers only once the test
    /// lets it: the window in which a caller can go away after the store
    /// granted the lease.
    struct GatedGrant {
        row: Mutex<Option<LeaseRow>>,
        granted: tokio::sync::Notify,
        answer: tokio::sync::Semaphore,
        resigned: tokio::sync::Notify,
    }

    impl GatedGrant {
        fn new() -> Self {
            Self {
                row: Mutex::new(None),
                granted: tokio::sync::Notify::new(),
                answer: tokio::sync::Semaphore::new(0),
                resigned: tokio::sync::Notify::new(),
            }
        }

        fn holder(&self) -> Option<HolderId> {
            self.row
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(|row| row.holder.clone())
        }

        fn answer(&self, claim: &LeaseClaim) -> LeaseAnswer {
            let row = self
                .row
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            LeaseAnswer {
                leader: row.as_ref().is_some_and(|row| row.holder == claim.holder),
                row,
                db_now_ms: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl RecoveryLeaderStore for GatedGrant {
        async fn acquire(&self, claim: &LeaseClaim) -> Result<LeaseAnswer, StoreError> {
            let granted = {
                let mut row = self
                    .row
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let free = row.is_none();
                if free {
                    *row = Some(LeaseRow {
                        holder: claim.holder.clone(),
                        generation_rank: claim.generation_rank,
                        term: 1,
                        elected_at_ms: 0,
                        expires_at_ms: i64::MAX,
                    });
                }
                free
            };
            if granted {
                self.granted.notify_one();
                self.answer
                    .acquire()
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?
                    .forget();
            }
            Ok(self.answer(claim))
        }

        async fn renew(&self, claim: &LeaseClaim, _term: i64) -> Result<LeaseAnswer, StoreError> {
            Ok(self.answer(claim))
        }

        async fn resign(
            &self,
            _name: &LeaseName,
            holder: &HolderId,
            _term: i64,
        ) -> Result<bool, StoreError> {
            let mut row = self
                .row
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let held = row.as_ref().is_some_and(|row| row.holder == *holder);
            if held {
                *row = None;
                self.resigned.notify_one();
            }
            Ok(held)
        }

        fn due_claims_need_leader(&self) -> bool {
            true
        }
    }

    /// FIG-3873 S5: a slot that goes while the store is granting its
    /// deployment the lease must not leave the row to a holder nobody runs.
    /// The election runs on its own task, so the grant's answer still reaches
    /// the slot's holder, which resigns once the slot goes: a deployment that
    /// replaces this one leads at once rather than after the lease's TTL.
    #[tokio::test]
    async fn an_election_granted_as_its_slot_goes_resigns_with_it() {
        let store = Arc::new(GatedGrant::new());
        let slot = RecoverySlot::over(
            Arc::clone(&store) as Arc<dyn RecoveryLeaderStore>,
            LeaseName::new("recovery:cancelled-election"),
            RecoveryLeaseConfig {
                generation_rank: 0,
                timings: RecoveryLeaseTimings {
                    renew_timeout: Duration::from_secs(600),
                    ..RecoveryLeaseTimings::default()
                },
            },
            Arc::new(lash_core::facade_support::SystemClock),
            lash_core::operational_metrics::StoreObserver::default(),
        );
        slot.spawn_election(&tokio::runtime::Handle::current());
        store.granted.notified().await;
        assert!(store.holder().is_some(), "the store granted the lease");
        // The slot goes before the grant's answer arrives, as a core shut
        // down mid-election does.
        store.answer.add_permits(1);
        drop(slot);
        tokio::time::timeout(Duration::from_secs(30), store.resigned.notified())
            .await
            .expect("the slot's holder resigns the lease it was granted");
        assert_eq!(store.holder(), None);
    }

    /// ADR 0132 §12, ADR 0113 §2.5: a cleanup its producer armed and never
    /// delivered (the producer died before its immediate attempt) is
    /// delivered by the core's own background pass, with no host call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_core_delivers_a_cleanup_its_producer_left_armed() {
        let backend = crate::tests::harness::sqlite_memory_store_backend().await;
        let id = backend
            .artifact_cleanup()
            .arm_cleanup(
                &lash_core::ArtifactCleanup::ended(
                    lash_core::ArtifactReferrer::HostPin(lash_core::HostArtifactPin::mint()),
                    Vec::new(),
                    None,
                ),
                backend.clock().timestamp_ms(),
            )
            .await
            .expect("arm the cleanup");
        let ledger = backend.obligation_ledger(lash_core::store::ObligationKind::ArtifactCleanup);
        let core = crate::tests::standard_core_over(backend.clone());
        tokio::time::timeout(Duration::from_secs(30), async {
            while ledger
                .state(&id)
                .await
                .expect("read the cleanup's state")
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the core's cleanup pass delivers the armed cleanup");
        core.shutdown().await.expect("shut the core down");
    }

    /// A delivery the law counts, over the real artifact-cleanup relay.
    struct Counted {
        relay: lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay,
        delivered: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ObligationRelay for Counted {
        fn ledger(&self) -> &dyn lash_core::store::ObligationLedger {
            self.relay.ledger()
        }

        fn policy(&self) -> lash_core::runtime::obligations::relay::RelayPolicy {
            self.relay.policy()
        }

        async fn deliver(
            &self,
            delivery: lash_core::runtime::obligations::relay::ObligationDelivery<'_>,
        ) -> Result<(), lash_core::runtime::obligations::relay::DeliveryFailure> {
            self.delivered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.relay.deliver(delivery).await
        }
    }

    /// ADR 0132 §12: an armed artifact cleanup whose producer died before its
    /// immediate attempt is delivered by the due pass, and exactly one leader
    /// runs it. Of two deployments over one SQLite store (whose claims need
    /// the leader), the follower claims nothing; once the leader crashes
    /// without resigning, the follower takes the lease at its TTL and
    /// delivers the cleanup once.
    #[tokio::test]
    async fn an_armed_cleanup_is_delivered_after_a_crash_by_one_leader() {
        use lash_core::StoreSet as _;

        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("memory store set"),
        );
        let backend = lash_core::Backend::for_testing(stores.clone());
        let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::facade_support::SystemClock);
        let referrer = lash_core::ArtifactReferrer::HostPin(lash_core::HostArtifactPin::mint());
        // The producer armed its cleanup and died before its own attempt.
        let id = backend
            .artifact_cleanup()
            .arm_cleanup(
                &lash_core::ArtifactCleanup::ended(referrer, Vec::new(), None),
                clock.timestamp_ms(),
            )
            .await
            .expect("arm the cleanup");
        let timings = RecoveryLeaseTimings {
            ttl: Duration::from_millis(400),
            trust_margin: Duration::from_millis(100),
            min_tenure: Duration::ZERO,
            ..RecoveryLeaseTimings::default()
        };
        let deployment = || {
            (
                RecoveryLease::new(
                    stores.recovery_leader(),
                    LeaseName::new("recovery:cleanup-law"),
                    0,
                    timings,
                    Arc::clone(&clock),
                    lash_core::operational_metrics::StoreObserver::default(),
                ),
                Counted {
                    relay: lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
                        &backend,
                        lash_core::ProcessEngineRegistry::new(),
                    ),
                    delivered: std::sync::atomic::AtomicUsize::new(0),
                },
            )
        };
        let (leader, leader_relay) = deployment();
        let (follower, follower_relay) = deployment();
        assert!(matches!(
            leader.step().await,
            lash_core::runtime::recovery_lease::Standing::Leader { .. }
        ));
        assert_eq!(
            follower.step().await,
            lash_core::runtime::recovery_lease::Standing::Follower
        );
        let ledger = backend.obligation_ledger(lash_core::store::ObligationKind::ArtifactCleanup);

        assert_eq!(
            cleanup_pass(&follower, &follower_relay, clock.as_ref(), CLEANUP_PAGE)
                .await
                .expect("the follower's pass"),
            None,
            "a follower claims nothing where claims need the leader"
        );
        assert_eq!(
            ledger.state(&id).await.expect("state"),
            Some(lash_core::store::ObligationState::Due)
        );

        // The leader crashes: it never resigns, so its row stands until its
        // TTL lapses and the follower's next attempt takes the lease.
        drop(leader);
        tokio::time::sleep(timings.ttl + Duration::from_millis(100)).await;
        assert!(matches!(
            follower.step().await,
            lash_core::runtime::recovery_lease::Standing::Leader { .. }
        ));
        let pass = cleanup_pass(&follower, &follower_relay, clock.as_ref(), CLEANUP_PAGE)
            .await
            .expect("the new leader's pass")
            .expect("the leader claims");
        assert_eq!((pass.claimed, pass.delivered), (1, 1), "{pass:?}");
        assert_eq!(
            ledger.state(&id).await.expect("state"),
            None,
            "a delivered cleanup's row is deleted"
        );
        let again = cleanup_pass(&follower, &follower_relay, clock.as_ref(), CLEANUP_PAGE)
            .await
            .expect("a later pass")
            .expect("the leader claims");
        assert_eq!(
            again.claimed, 0,
            "a delivered cleanup is never claimed again"
        );
        assert_eq!(
            (
                leader_relay
                    .delivered
                    .load(std::sync::atomic::Ordering::SeqCst),
                follower_relay
                    .delivered
                    .load(std::sync::atomic::Ordering::SeqCst),
            ),
            (0, 1),
            "exactly one leader delivered the cleanup, once"
        );
    }
}
