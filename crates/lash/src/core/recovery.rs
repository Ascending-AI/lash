//! The core's seat in the recovery leader election (ADR 0109 §1.6–§1.7).
//!
//! One slot per core, shared by the core and its session driver: the driver's
//! reconcile tick joins the election on its first pass and asks which duties
//! it runs, and the core resigns at shutdown. The election runs on a
//! background task of its own, which keeps the lease's cadence and resigns
//! when the slot is dropped.

use std::sync::Arc;

use lash_core::engine::RecoveryLeaseConfig;
use lash_core::runtime::recovery_lease::{RecoveryDuties, RecoveryLease};
use lash_core::store::{LeaseName, RecoveryLeaderStore};

use crate::support::RuntimeEnvironment;

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
        let authority = env.core.control.effect_host.turn_control_binding_id();
        Self::over(
            env.core.backend().recovery_leader(),
            LeaseName::new(format!("recovery:{authority}")),
            config,
            Arc::clone(&env.core.clock),
        )
    }

    /// The slot competing for lease `name` on `store`.
    fn over(
        store: Arc<dyn RecoveryLeaderStore>,
        name: LeaseName,
        config: RecoveryLeaseConfig,
        clock: Arc<dyn lash_core::Clock>,
    ) -> Self {
        Self {
            lease: Arc::new(RecoveryLease::new(
                store,
                name,
                config.generation_rank,
                config.timings,
                Arc::clone(&clock),
            )),
            started: std::sync::atomic::AtomicBool::new(false),
            first: Arc::new(tokio::sync::watch::channel(false).0),
            shutdown: tokio_util::sync::CancellationToken::new(),
            clock,
        }
    }

    /// This core's lease: the first use starts its election, and every use
    /// waits for the election's first attempt, so the first tick of an
    /// uncontested deployment already leads.
    ///
    /// The election runs on a task of its own, never in the caller: a caller
    /// cancelled after the store granted the lease (a recovery tick aborted
    /// by a shutdown) would otherwise drop the only holder that could renew
    /// or resign it, and the row would name a holder nobody runs until its
    /// TTL lapses. The task resigns once the slot is dropped, whatever its
    /// first attempt answered.
    pub(crate) async fn lease(&self) -> &Arc<RecoveryLease> {
        if !self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let lease = Arc::clone(&self.lease);
            let first = Arc::clone(&self.first);
            let shutdown = self.shutdown.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        let standing = lease.step().await;
                        first.send_replace(true);
                        keep_cadence(lease, standing, shutdown).await;
                    });
                }
                // No runtime to keep a cadence on: one attempt, inline.
                Err(_) => {
                    lease.step().await;
                    first.send_replace(true);
                }
            }
        }
        let mut answered = self.first.subscribe();
        // The sender lives in the slot, so the wait ends only on the answer.
        let _ = answered.wait_for(|answered| *answered).await;
        &self.lease
    }

    /// The duties this deployment runs now.
    pub(crate) async fn duties(&self) -> RecoveryDuties {
        let lease = self.lease().await;
        lease.duties(self.clock.timestamp_ms())
    }

    /// Give the lease up now, if this core ever joined the election.
    pub(crate) async fn resign(&self) {
        if self.started.load(std::sync::atomic::Ordering::SeqCst) {
            self.lease.resign().await;
        }
    }
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

    /// FIG-3873 S5: a recovery tick cancelled after the store granted its
    /// deployment the lease must not leave the row to a holder nobody runs.
    /// The election's answer still reaches the slot's holder, which resigns
    /// once the slot goes, so a deployment that replaces this one leads at
    /// once rather than after the lease's TTL.
    #[tokio::test]
    async fn an_election_whose_caller_is_cancelled_after_the_grant_resigns_with_its_slot() {
        let store = Arc::new(GatedGrant::new());
        let slot = Arc::new(RecoverySlot::over(
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
        ));
        let tick = tokio::spawn({
            let slot = Arc::clone(&slot);
            async move {
                slot.duties().await;
            }
        });
        store.granted.notified().await;
        assert!(store.holder().is_some(), "the store granted the lease");
        // The tick that asked goes away before the grant's answer arrives,
        // as a recovery pass a shutdown aborts does.
        tick.abort();
        assert!(tick.await.is_err_and(|error| error.is_cancelled()));
        store.answer.add_permits(1);
        drop(slot);
        tokio::time::timeout(Duration::from_secs(30), store.resigned.notified())
            .await
            .expect("the slot's holder resigns the lease it was granted");
        assert_eq!(store.holder(), None);
    }
}
