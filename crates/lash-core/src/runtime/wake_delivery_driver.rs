use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{
    Clock, PluginError, ProcessRegistry, QueuedWorkDriver, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, WakeDiscardReason, process_wake_batch_draft,
};

const DELIVERY_BATCH_SIZE: usize = 32;
const POLL_INITIAL: Duration = Duration::from_millis(25);
const POLL_MAX: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeDeliveryDriveReport {
    pub inspected: usize,
    pub enqueued: usize,
    pub discarded_expired: usize,
    pub discarded_target_gone: usize,
    pub retryable_failures: usize,
}

#[derive(Clone)]
pub struct WakeDeliveryDriver {
    inner: Arc<WakeDeliveryDriverInner>,
}

struct WakeDeliveryDriverInner {
    registry: Arc<dyn ProcessRegistry>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    queued_work_driver: Option<QueuedWorkDriver>,
    clock: Arc<dyn Clock>,
    notify: Notify,
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl Drop for WakeDeliveryDriverInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.tasks.close();
    }
}

impl WakeDeliveryDriver {
    /// Start the autonomous startup scan and bounded polling loop.
    pub fn new(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work_driver: Option<QueuedWorkDriver>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let driver = Self {
            inner: Arc::new(WakeDeliveryDriverInner {
                registry,
                session_store_factory,
                queued_work_driver,
                clock,
                notify: Notify::new(),
                shutdown: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        };
        let running = driver.clone();
        driver.inner.tasks.spawn(async move {
            running.run_loop().await;
        });
        driver
    }

    /// Wake the autonomous loop after a process append commits an outbox row.
    pub fn nudge(&self) {
        self.inner.notify.notify_one();
    }

    /// Host/runbook lever: synchronously run one bounded delivery scan.
    pub async fn drive_pending(&self) -> Result<WakeDeliveryDriveReport, PluginError> {
        Self::drive_pending_once(
            Arc::clone(&self.inner.registry),
            Arc::clone(&self.inner.session_store_factory),
            self.inner.queued_work_driver.clone(),
            Arc::clone(&self.inner.clock),
            DELIVERY_BATCH_SIZE,
        )
        .await
    }

    /// One bounded, idempotent delivery pass. This is also used as the
    /// post-append nudge path before a long-lived host driver is available.
    pub async fn drive_pending_once(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work_driver: Option<QueuedWorkDriver>,
        clock: Arc<dyn Clock>,
        limit: usize,
    ) -> Result<WakeDeliveryDriveReport, PluginError> {
        let mut report = WakeDeliveryDriveReport::default();
        for delivery in registry.pending_wake_deliveries(limit).await? {
            report.inspected += 1;
            if clock.timestamp_ms() >= delivery.expires_at_ms {
                registry
                    .discard_wake_delivery(&delivery.delivery_id, WakeDiscardReason::Expired)
                    .await?;
                report.discarded_expired += 1;
                continue;
            }

            let target_session_id = delivery.wake.target_session_id.clone();
            let request = SessionStoreCreateRequest {
                session_id: target_session_id.clone(),
                relation: SessionRelation::default(),
                policy: SessionPolicy::default(),
            };
            let store = match session_store_factory.open_existing_store(&request).await {
                Ok(Some(store)) => store,
                Ok(None) => {
                    // Consult the permanent tombstone as explicit diagnostic
                    // evidence; absence is still terminal because delivery is
                    // not authorized to create a target session.
                    let _was_deleted = session_store_factory
                        .session_was_deleted(&target_session_id)
                        .await
                        .map_err(PluginError::Session)?;
                    registry
                        .discard_wake_delivery(&delivery.delivery_id, WakeDiscardReason::TargetGone)
                        .await?;
                    report.discarded_target_gone += 1;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        error = %error,
                        "process wake target lookup failed; delivery remains pending"
                    );
                    report.retryable_failures += 1;
                    continue;
                }
            };

            match store
                .enqueue_queued_work(process_wake_batch_draft(delivery.wake.clone()))
                .await
            {
                Ok(_) => {
                    registry.mark_wake_enqueued(&delivery.delivery_id).await?;
                    if let Some(driver) = queued_work_driver.as_ref() {
                        driver.wake_pending(Some(&target_session_id), "process_wake");
                    }
                    report.enqueued += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        error = %error,
                        "process wake enqueue failed; delivery remains pending"
                    );
                    report.retryable_failures += 1;
                }
            }
        }
        Ok(report)
    }

    async fn run_loop(self) {
        let mut poll = POLL_INITIAL;
        loop {
            let report = match self.drive_pending().await {
                Ok(report) => report,
                Err(error) => {
                    tracing::warn!(error = %error, "process wake delivery scan failed");
                    WakeDeliveryDriveReport {
                        retryable_failures: 1,
                        ..WakeDeliveryDriveReport::default()
                    }
                }
            };
            if report.inspected >= DELIVERY_BATCH_SIZE {
                poll = POLL_INITIAL;
                continue;
            }
            if report.inspected > 0 && report.retryable_failures == 0 {
                poll = POLL_INITIAL;
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                () = self.inner.notify.notified() => poll = POLL_INITIAL,
                () = tokio::time::sleep(poll) => {
                    poll = poll.saturating_mul(2).min(POLL_MAX);
                }
            }
        }
    }
}
