//! The artifact-cleanup outbox's due pass a core runs (ADR 0132 §12).
//!
//! One pass per core, on a background task of its own: on its configured
//! interval it claims a bounded page of due `ArtifactCleanup` rows, on every
//! deployment of every store with no leader (ADR 0109 §1.6-§1.7), so a
//! cleanup whose producer died before its immediate attempt is still
//! delivered. Claim tokens fence each claim, so two deployments never deliver
//! one row twice.

use std::sync::Arc;

use lash_core::runtime::obligations::RecoveryInterval;
use lash_core::runtime::obligations::relay::{ObligationRelay, relay_due};

pub(crate) struct CleanupPass {
    shutdown: tokio_util::sync::CancellationToken,
    clock: Arc<dyn lash_core::Clock>,
    cleanup: Arc<tokio::sync::Mutex<()>>,
}

impl CleanupPass {
    /// The pass for a core reading time from `clock`.
    pub(crate) fn new(clock: Arc<dyn lash_core::Clock>) -> Self {
        Self {
            shutdown: tokio_util::sync::CancellationToken::new(),
            clock,
            cleanup: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Run `relay`'s due pass on the configured interval on a task of its own
    /// until the pass is dropped or the core shuts down. Without a runtime to
    /// run it on, nothing starts: such a core has no background work.
    pub(crate) fn start(&self, relay: Arc<dyn ObligationRelay>, pacing: crate::RecoveryPacing) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let clock = Arc::clone(&self.clock);
        let shutdown = self.shutdown.clone();
        let cleanup = Arc::clone(&self.cleanup);
        runtime.spawn(async move {
            let mut interval = RecoveryInterval::new(Arc::clone(&clock), pacing.interval());
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let _pass = tokio::select! {
                    () = shutdown.cancelled() => break,
                    guard = cleanup.lock() => guard,
                };
                if let Err(error) = relay_due(relay.as_ref(), clock.as_ref(), pacing.cleanup_page).await {
                    tracing::warn!(%error, "artifact cleanup pass failed; the next tick retries it");
                }
            }
        });
    }

    /// Wait for an active cleanup pass and hold periodic passes at their
    /// admission boundary until this guard is dropped.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn quiesce_cleanup(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.cleanup).lock_owned().await
    }

    /// Stop the pass's background work.
    pub(crate) fn stop(&self) {
        self.shutdown.cancel();
    }
}

impl Drop for CleanupPass {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// ADR 0132 §12, ADR 0113 §2.5: a cleanup its producer armed and never
    /// delivered (the producer died before its immediate attempt) is
    /// delivered by the core's own background pass, with no host call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_core_delivers_a_cleanup_its_producer_left_armed() {
        let backend = crate::tests::host::sqlite_memory_store_backend().await;
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
}
