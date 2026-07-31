use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{
    Clock, PluginError, ProcessRegistry, QueuedWorkDriver, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, WakeDeliveryClaimOutcome, WakeDiscardReason,
    WakeTurnPolicy, process_wake_batch_draft_with_policy,
};

const DELIVERY_BATCH_SIZE: usize = 32;
const POLL_INITIAL: Duration = Duration::from_millis(25);
const POLL_MAX: Duration = Duration::from_secs(1);
const RETRY_INITIAL_MS: u64 = 50;
const RETRY_MAX_MS: u64 = 5 * 60 * 1_000;

fn retry_delay_ms(attempts: u64) -> u64 {
    let exponent = attempts.saturating_sub(1).min(63) as u32;
    RETRY_INITIAL_MS
        .saturating_mul(1_u64 << exponent)
        .min(RETRY_MAX_MS)
}

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
    lifetime: Arc<WakeDeliveryDriverLifetime>,
}

struct WakeDeliveryDriverInner {
    registry: Arc<dyn ProcessRegistry>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    queued_work_driver: Option<QueuedWorkDriver>,
    clock: Arc<dyn Clock>,
    wake_turn_policy: WakeTurnPolicy,
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
        Self::new_with_policy(
            registry,
            session_store_factory,
            queued_work_driver,
            clock,
            WakeTurnPolicy::default(),
        )
    }

    /// Start the driver with a factory-selected wake-to-turn policy.
    pub fn new_with_policy(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work_driver: Option<QueuedWorkDriver>,
        clock: Arc<dyn Clock>,
        wake_turn_policy: WakeTurnPolicy,
    ) -> Self {
        let driver = Self {
            inner: Arc::new(WakeDeliveryDriverInner {
                registry,
                session_store_factory,
                queued_work_driver,
                clock,
                wake_turn_policy,
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
        Self::drive_pending_once_with_policy(
            Arc::clone(&self.inner.registry),
            Arc::clone(&self.inner.session_store_factory),
            self.inner.queued_work_driver.clone(),
            Arc::clone(&self.inner.clock),
            &self.inner.wake_turn_policy,
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
        Self::drive_pending_once_with_policy(
            registry,
            session_store_factory,
            queued_work_driver,
            clock,
            &WakeTurnPolicy::default(),
            limit,
        )
        .await
    }

    pub(crate) async fn drive_pending_once_with_policy(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work_driver: Option<QueuedWorkDriver>,
        clock: Arc<dyn Clock>,
        wake_turn_policy: &WakeTurnPolicy,
        limit: usize,
    ) -> Result<WakeDeliveryDriveReport, PluginError> {
        let mut report = WakeDeliveryDriveReport::default();
        for delivery in registry.claim_pending_wake_deliveries(limit).await? {
            report.inspected += 1;
            let claim_token = delivery.claim_token()?;
            if clock.timestamp_ms() >= delivery.expires_at_ms {
                match registry
                    .discard_wake_delivery(
                        &delivery.delivery_id,
                        claim_token,
                        WakeDiscardReason::Expired,
                    )
                    .await
                {
                    Ok(WakeDeliveryClaimOutcome::Applied) => {
                        tracing::info!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %delivery.wake.target_session_id,
                            reason = "expired",
                            "process wake delivery discarded"
                        );
                        report.discarded_expired += 1;
                    }
                    Ok(WakeDeliveryClaimOutcome::ClaimLost { state }) => {
                        tracing::debug!(
                            delivery_id = %delivery.delivery_id,
                            ?state,
                            "concurrent process wake transition won before expiry discard"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            delivery_id = %delivery.delivery_id,
                            error = %error,
                            "process wake expiry transition failed; delivery deferred"
                        );
                        Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref()).await?;
                        report.retryable_failures += 1;
                    }
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
                            Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref()).await?;
                            report.retryable_failures += 1;
                            continue;
                        }
                    };
                    if was_deleted {
                        match registry
                            .discard_wake_delivery(
                                &delivery.delivery_id,
                                claim_token,
                                WakeDiscardReason::TargetGone,
                            )
                            .await
                        {
                            Ok(WakeDeliveryClaimOutcome::Applied) => {
                                tracing::info!(
                                    delivery_id = %delivery.delivery_id,
                                    target_session_id = %target_session_id,
                                    reason = "target_gone",
                                    "process wake delivery discarded"
                                );
                                report.discarded_target_gone += 1;
                            }
                            Ok(WakeDeliveryClaimOutcome::ClaimLost { state }) => {
                                tracing::debug!(
                                    delivery_id = %delivery.delivery_id,
                                    ?state,
                                    "concurrent process wake transition won before target-gone discard"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    delivery_id = %delivery.delivery_id,
                                    error = %error,
                                    "process wake target-gone transition failed; delivery deferred"
                                );
                                Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref())
                                    .await?;
                                report.retryable_failures += 1;
                            }
                        }
                    } else {
                        tracing::debug!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %target_session_id,
                            "process wake target has never existed; delivery remains pending"
                        );
                        Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref()).await?;
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
                    Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref()).await?;
                    report.retryable_failures += 1;
                    continue;
                }
            };

            match store
                .enqueue_queued_work(process_wake_batch_draft_with_policy(
                    delivery.wake.clone(),
                    wake_turn_policy,
                ))
                .await
            {
                Ok(enqueued) => {
                    tracing::info!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        batch_id = %enqueued.batch_id,
                        source_key = ?enqueued.source_key,
                        delivery_policy = wake_turn_policy.delivery().as_str(),
                        wake_mode = ?wake_turn_policy.mode(),
                        slot_policy = enqueued.slot_policy.as_str(),
                        merge_key = ?enqueued.merge_key,
                        outcome = "enqueued",
                        "process wake enqueued"
                    );
                    match registry
                        .mark_wake_enqueued(&delivery.delivery_id, claim_token)
                        .await
                    {
                        Ok(WakeDeliveryClaimOutcome::Applied) => {
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
                        Ok(WakeDeliveryClaimOutcome::ClaimLost { state }) => {
                            tracing::debug!(
                                delivery_id = %delivery.delivery_id,
                                ?state,
                                "concurrent process wake transition already settled delivery"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                delivery_id = %delivery.delivery_id,
                                error = %error,
                                "process wake terminal mark failed; delivery deferred"
                            );
                            Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref()).await?;
                            report.retryable_failures += 1;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        error = %error,
                        "process wake enqueue failed; delivery remains pending"
                    );
                    Self::defer_retry(registry.as_ref(), &delivery, clock.as_ref()).await?;
                    report.retryable_failures += 1;
                }
            }
        }
        Ok(report)
    }

    async fn defer_retry(
        registry: &dyn ProcessRegistry,
        delivery: &crate::WakeDelivery,
        clock: &dyn Clock,
    ) -> Result<(), PluginError> {
        match registry
            .defer_wake_delivery(
                &delivery.delivery_id,
                delivery.claim_token()?,
                clock
                    .timestamp_ms()
                    .saturating_add(retry_delay_ms(delivery.attempts)),
            )
            .await
        {
            Ok(WakeDeliveryClaimOutcome::Applied | WakeDeliveryClaimOutcome::ClaimLost { .. }) => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn run_loop(inner: Arc<WakeDeliveryDriverInner>, shutdown: CancellationToken) {
        let mut poll = POLL_INITIAL;
        loop {
            let report = match Self::drive_pending_once_with_policy(
                Arc::clone(&inner.registry),
                Arc::clone(&inner.session_store_factory),
                inner.queued_work_driver.clone(),
                Arc::clone(&inner.clock),
                &inner.wake_turn_policy,
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
            let made_progress =
                report.enqueued + report.discarded_expired + report.discarded_target_gone > 0;
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

#[cfg(test)]
mod tests {
    use super::{RETRY_INITIAL_MS, RETRY_MAX_MS, retry_delay_ms};

    #[test]
    fn retry_delay_is_bounded_for_every_attempt_count() {
        assert_eq!(retry_delay_ms(0), RETRY_INITIAL_MS);
        assert_eq!(retry_delay_ms(1), RETRY_INITIAL_MS);
        assert_eq!(retry_delay_ms(2), RETRY_INITIAL_MS * 2);
        assert_eq!(retry_delay_ms(14), RETRY_MAX_MS);
        assert_eq!(retry_delay_ms(64), RETRY_MAX_MS);
        assert_eq!(retry_delay_ms(u64::MAX), RETRY_MAX_MS);
    }
}
