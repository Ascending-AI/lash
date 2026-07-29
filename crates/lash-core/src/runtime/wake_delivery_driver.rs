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
    pub evidence_cleaned: usize,
}

#[derive(Clone)]
pub struct WakeDeliveryDriver {
    inner: Arc<WakeDeliveryDriverInner>,
    lifetime: Arc<WakeDeliveryDriverLifetime>,
}

struct WakeDeliveryDriverInner {
    registry: Arc<dyn ProcessRegistry>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    queued_work_driver: Option<QueuedWorkDriver>,
    clock: Arc<dyn Clock>,
    notify: Notify,
}

struct WakeDeliveryDriverLifetime {
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl Drop for WakeDeliveryDriverLifetime {
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
            }),
            lifetime: Arc::new(WakeDeliveryDriverLifetime {
                shutdown: CancellationToken::new(),
                tasks: TaskTracker::new(),
            }),
        };
        let inner = Arc::clone(&driver.inner);
        let shutdown = driver.lifetime.shutdown.clone();
        driver.lifetime.tasks.spawn(async move {
            Self::run_loop(inner, shutdown).await;
        });
        driver
    }

    /// Wake the autonomous loop after a process append commits an outbox row.
    pub fn nudge(&self) {
        self.inner.notify.notify_one();
    }

    /// Stop the autonomous loop and wait until it has released its store
    /// handles. The runtime calls this during teardown.
    pub async fn shutdown(&self) {
        self.lifetime.shutdown.cancel();
        self.lifetime.tasks.close();
        self.lifetime.tasks.wait().await;
    }

    /// Request shutdown without waiting. Used by synchronous runtime teardown.
    pub fn request_shutdown(&self) {
        self.lifetime.shutdown.cancel();
        self.lifetime.tasks.close();
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
                match registry
                    .discard_wake_delivery(&delivery.delivery_id, WakeDiscardReason::Expired)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %delivery.wake.target_session_id,
                            reason = "expired",
                            "process wake delivery discarded"
                        );
                        report.discarded_expired += 1;
                    }
                    Err(PluginError::WakeDeliveryNotPending { state, .. }) => {
                        tracing::debug!(
                            delivery_id = %delivery.delivery_id,
                            ?state,
                            "concurrent process wake transition won before expiry discard"
                        );
                    }
                    Err(error) => return Err(error),
                }
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
                    let was_deleted = match session_store_factory
                        .session_was_deleted(&target_session_id)
                        .await
                    {
                        Ok(was_deleted) => was_deleted,
                        Err(error) => {
                            tracing::warn!(
                                delivery_id = %delivery.delivery_id,
                                target_session_id = %target_session_id,
                                error = %error,
                                "process wake target tombstone lookup failed; delivery remains pending"
                            );
                            report.retryable_failures += 1;
                            continue;
                        }
                    };
                    if was_deleted {
                        match registry
                            .discard_wake_delivery(
                                &delivery.delivery_id,
                                WakeDiscardReason::TargetGone,
                            )
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(
                                    delivery_id = %delivery.delivery_id,
                                    target_session_id = %target_session_id,
                                    reason = "target_gone",
                                    "process wake delivery discarded"
                                );
                                report.discarded_target_gone += 1;
                            }
                            Err(PluginError::WakeDeliveryNotPending { state, .. }) => {
                                tracing::debug!(
                                    delivery_id = %delivery.delivery_id,
                                    ?state,
                                    "concurrent process wake transition won before target-gone discard"
                                );
                            }
                            Err(error) => return Err(error),
                        }
                    } else {
                        tracing::debug!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %target_session_id,
                            "process wake target has never existed; delivery remains pending"
                        );
                        report.retryable_failures += 1;
                    }
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
                Ok(enqueued) => {
                    tracing::info!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        batch_id = %enqueued.batch_id,
                        source_key = ?enqueued.source_key,
                        "process wake enqueued"
                    );
                    match registry.mark_wake_enqueued(&delivery.delivery_id).await {
                        Ok(()) => {
                            tracing::info!(
                                delivery_id = %delivery.delivery_id,
                                state = "enqueued",
                                "process wake delivery marked terminal"
                            );
                            if let Some(driver) = queued_work_driver.as_ref() {
                                driver.wake_pending(Some(&target_session_id), "process_wake");
                            }
                            report.enqueued += 1;
                        }
                        Err(PluginError::WakeDeliveryNotPending {
                            state: crate::WakeDeliveryState::Enqueued,
                            ..
                        }) => {
                            tracing::debug!(
                                delivery_id = %delivery.delivery_id,
                                "concurrent process wake driver already marked delivery enqueued"
                            );
                        }
                        Err(PluginError::WakeDeliveryNotPending {
                            state: crate::WakeDeliveryState::Discarded,
                            ..
                        }) => {
                            let converged = store
                                .compensate_queued_work_batch(
                                    &target_session_id,
                                    &enqueued.batch_id,
                                )
                                .await
                                .map_err(|error| PluginError::Session(error.to_string()))?;
                            if !converged {
                                return Err(PluginError::Session(format!(
                                    "discarded wake delivery `{}` could not remove queued batch `{}`",
                                    delivery.delivery_id, enqueued.batch_id
                                )));
                            }
                            tracing::info!(
                                delivery_id = %delivery.delivery_id,
                                batch_id = %enqueued.batch_id,
                                "compensated process wake enqueue after terminal discard"
                            );
                        }
                        Err(error) => return Err(error),
                    }
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
        for delivery in registry.wake_evidence_cleanup_deliveries(limit).await? {
            let target_session_id = delivery.wake.target_session_id.clone();
            let request = SessionStoreCreateRequest {
                session_id: target_session_id.clone(),
                relation: SessionRelation::default(),
                policy: SessionPolicy::default(),
            };
            match session_store_factory.open_existing_store(&request).await {
                Ok(Some(store)) => {
                    store
                        .prune_consumed_wake_source_keys(
                            &target_session_id,
                            &[delivery.source_key()],
                        )
                        .await
                        .map_err(|error| PluginError::Session(error.to_string()))?;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        error = %error,
                        "process wake evidence cleanup target lookup failed"
                    );
                    report.retryable_failures += 1;
                    continue;
                }
            }
            registry
                .mark_wake_evidence_cleaned(&delivery.delivery_id)
                .await?;
            tracing::info!(
                delivery_id = %delivery.delivery_id,
                target_session_id = %target_session_id,
                source_key = %delivery.source_key(),
                "process wake consumed evidence reconciled after terminal delivery"
            );
            report.evidence_cleaned += 1;
        }
        Ok(report)
    }

    async fn run_loop(inner: Arc<WakeDeliveryDriverInner>, shutdown: CancellationToken) {
        let mut poll = POLL_INITIAL;
        loop {
            let report = match Self::drive_pending_once(
                Arc::clone(&inner.registry),
                Arc::clone(&inner.session_store_factory),
                inner.queued_work_driver.clone(),
                Arc::clone(&inner.clock),
                DELIVERY_BATCH_SIZE,
            )
            .await
            {
                Ok(report) => report,
                Err(error) => {
                    tracing::warn!(error = %error, "process wake delivery scan failed");
                    WakeDeliveryDriveReport {
                        retryable_failures: 1,
                        ..WakeDeliveryDriveReport::default()
                    }
                }
            };
            let made_progress = report.enqueued
                + report.discarded_expired
                + report.discarded_target_gone
                + report.evidence_cleaned
                > 0;
            let delay = if made_progress
                && report.retryable_failures == 0
                && report.inspected >= DELIVERY_BATCH_SIZE
            {
                poll = POLL_INITIAL;
                Duration::ZERO
            } else {
                poll
            };
            if made_progress && report.retryable_failures == 0 {
                poll = POLL_INITIAL;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = inner.notify.notified() => poll = POLL_INITIAL,
                () = tokio::time::sleep(delay) => {
                    poll = poll.saturating_mul(2).min(POLL_MAX);
                }
            }
        }
    }
}
