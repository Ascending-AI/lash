use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::runtime::process_wake_batch_draft_with_delivery_policy;
use crate::{
    Clock, PluginError, ProcessRegistry, QueuedWorkSubstrate, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, SessionWorkTarget, StoreError,
    WakeDeliveryClaimOutcome, WakeDiscardReason, WorkCadencePolicy,
};

fn retry_delay_ms(attempts: u64, work_cadence: &WorkCadencePolicy) -> u64 {
    let exponent = attempts.saturating_sub(1).min(63) as u32;
    let initial_ms = work_cadence
        .delivery_retry_initial
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let max_ms = work_cadence
        .delivery_retry_max
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    initial_ms.saturating_mul(1_u64 << exponent).min(max_ms)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeDeliveryDriveReport {
    pub inspected: usize,
    pub enqueued: usize,
    pub discarded_expired: usize,
    pub discarded_target_gone: usize,
    pub discarded_sequence_rewound: usize,
    pub floor_absorbed: usize,
    pub retryable_failures: usize,
}

#[derive(Clone, Copy, Debug)]
enum WakeDeliverySettlement {
    Discard(WakeDiscardReason),
    Enqueued,
    Retry,
}

#[derive(Clone, Copy, Debug)]
struct SequenceRewindDiscardLog<'a> {
    session_id: &'a str,
    process_id: &'a str,
    sequence: u64,
    allocation_floor: u64,
}

#[derive(Clone)]
pub struct WakeDeliveryDriver {
    inner: Arc<WakeDeliveryDriverInner>,
    lifetime: Arc<WakeDeliveryDriverLifetime>,
}

struct WakeDeliveryDriverInner {
    registry: Arc<dyn ProcessRegistry>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    queued_work: std::sync::Weak<dyn QueuedWorkSubstrate>,
    clock: Arc<dyn Clock>,
    delivery_policy: crate::DeliveryPolicy,
    work_cadence: WorkCadencePolicy,
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
        queued_work: Arc<dyn QueuedWorkSubstrate>,
        clock: Arc<dyn Clock>,
        delivery_policy: crate::DeliveryPolicy,
    ) -> Self {
        Self::with_work_cadence(
            registry,
            session_store_factory,
            queued_work,
            clock,
            delivery_policy,
            WorkCadencePolicy::default(),
        )
    }

    pub(crate) fn with_work_cadence(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work: Arc<dyn QueuedWorkSubstrate>,
        clock: Arc<dyn Clock>,
        delivery_policy: crate::DeliveryPolicy,
        work_cadence: WorkCadencePolicy,
    ) -> Self {
        let driver = Self {
            inner: Arc::new(WakeDeliveryDriverInner {
                registry,
                session_store_factory,
                queued_work: Arc::downgrade(&queued_work),
                clock,
                delivery_policy,
                work_cadence,
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
        let Some(queued_work) = self.inner.queued_work.upgrade() else {
            return Ok(WakeDeliveryDriveReport::default());
        };
        Self::drive_pending_once_with_delivery_policy_and_work_cadence(
            Arc::clone(&self.inner.registry),
            Arc::clone(&self.inner.session_store_factory),
            queued_work,
            Arc::clone(&self.inner.clock),
            self.inner.delivery_policy,
            self.inner.work_cadence.delivery_batch.get(),
            &self.inner.work_cadence,
        )
        .await
    }

    /// One bounded, idempotent delivery pass. This is also used as the
    /// post-append nudge path before a long-lived host driver is available.
    pub async fn drive_pending_once(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work: Arc<dyn QueuedWorkSubstrate>,
        clock: Arc<dyn Clock>,
        limit: usize,
    ) -> Result<WakeDeliveryDriveReport, PluginError> {
        Self::drive_pending_once_with_delivery_policy(
            registry,
            session_store_factory,
            queued_work,
            clock,
            crate::DeliveryPolicy::EarliestSafeBoundary,
            limit,
        )
        .await
    }

    /// One bounded delivery pass using the host-selected wake boundary.
    pub async fn drive_pending_once_with_delivery_policy(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work: Arc<dyn QueuedWorkSubstrate>,
        clock: Arc<dyn Clock>,
        delivery_policy: crate::DeliveryPolicy,
        limit: usize,
    ) -> Result<WakeDeliveryDriveReport, PluginError> {
        let work_cadence = WorkCadencePolicy::default();
        Box::pin(
            Self::drive_pending_once_with_delivery_policy_and_work_cadence(
                registry,
                session_store_factory,
                queued_work,
                clock,
                delivery_policy,
                limit,
                &work_cadence,
            ),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_pending_once_with_delivery_policy_and_work_cadence(
        registry: Arc<dyn ProcessRegistry>,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        queued_work: Arc<dyn QueuedWorkSubstrate>,
        clock: Arc<dyn Clock>,
        delivery_policy: crate::DeliveryPolicy,
        limit: usize,
        work_cadence: &WorkCadencePolicy,
    ) -> Result<WakeDeliveryDriveReport, PluginError> {
        let mut report = WakeDeliveryDriveReport::default();
        for delivery in registry.claim_pending_wake_deliveries(limit).await? {
            report.inspected += 1;
            let claim_token = delivery.claim_token()?;
            if clock.timestamp_ms() >= delivery.expires_at_ms {
                Self::settle(
                    registry.as_ref(),
                    &delivery,
                    claim_token,
                    clock.as_ref(),
                    WakeDeliverySettlement::Discard(WakeDiscardReason::Expired),
                    None,
                    work_cadence,
                    &mut report,
                )
                .await?;
                continue;
            }

            let target_session_id = delivery.wake.target_session_id.clone();
            let request = SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: target_session_id.clone(),
                relation: SessionRelation::default(),
                policy: SessionPolicy::new(crate::TurnBudget::Unbounded),
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
                            Self::settle(
                                registry.as_ref(),
                                &delivery,
                                claim_token,
                                clock.as_ref(),
                                WakeDeliverySettlement::Retry,
                                None,
                                work_cadence,
                                &mut report,
                            )
                            .await?;
                            continue;
                        }
                    };
                    if was_deleted {
                        Self::settle(
                            registry.as_ref(),
                            &delivery,
                            claim_token,
                            clock.as_ref(),
                            WakeDeliverySettlement::Discard(WakeDiscardReason::TargetGone),
                            None,
                            work_cadence,
                            &mut report,
                        )
                        .await?;
                    } else {
                        tracing::debug!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %target_session_id,
                            "process wake target has never existed; delivery remains pending"
                        );
                        Self::settle(
                            registry.as_ref(),
                            &delivery,
                            claim_token,
                            clock.as_ref(),
                            WakeDeliverySettlement::Retry,
                            None,
                            work_cadence,
                            &mut report,
                        )
                        .await?;
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
                    Self::settle(
                        registry.as_ref(),
                        &delivery,
                        claim_token,
                        clock.as_ref(),
                        WakeDeliverySettlement::Retry,
                        None,
                        work_cadence,
                        &mut report,
                    )
                    .await?;
                    continue;
                }
            };

            match store
                .enqueue_queued_work_with_outcome(process_wake_batch_draft_with_delivery_policy(
                    delivery.wake.clone(),
                    delivery_policy,
                ))
                .await
            {
                Ok(enqueue_outcome) => {
                    let enqueued = enqueue_outcome.batch();
                    // Dispatch is best-effort and strictly post-commit. Do it
                    // before settling the outbox claim so Applied, ClaimLost,
                    // and terminal-mark failures all re-arm the durable row.
                    queued_work.notify_session_work(
                        SessionWorkTarget::Session(target_session_id.clone()),
                        "process_wake",
                    );
                    if enqueue_outcome.process_wake_was_absorbed() {
                        tracing::info!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %target_session_id,
                            batch_id = %enqueued.batch_id,
                            source_key = ?enqueued.source_key,
                            outcome = "floor_absorbed",
                            "process wake delivery absorbed by receiver idempotency"
                        );
                        report.floor_absorbed += 1;
                    } else {
                        tracing::info!(
                            delivery_id = %delivery.delivery_id,
                            target_session_id = %target_session_id,
                            batch_id = %enqueued.batch_id,
                            source_key = ?enqueued.source_key,
                            delivery_policy = enqueued.delivery_policy.as_str(),
                            work_kind = enqueued.kind.as_str(),
                            authority = ?enqueued.authority,
                            merge_key = ?enqueued.merge_key,
                            outcome = "enqueued",
                            "process wake enqueued"
                        );
                    }
                    Self::settle(
                        registry.as_ref(),
                        &delivery,
                        claim_token,
                        clock.as_ref(),
                        WakeDeliverySettlement::Enqueued,
                        None,
                        work_cadence,
                        &mut report,
                    )
                    .await?;
                }
                Err(StoreError::ProcessWakeSequenceRewound {
                    session_id,
                    process_id,
                    sequence,
                    allocation_floor,
                }) => {
                    let rewind_log = SequenceRewindDiscardLog {
                        session_id: &session_id,
                        process_id: &process_id,
                        sequence,
                        allocation_floor,
                    };
                    Self::settle(
                        registry.as_ref(),
                        &delivery,
                        claim_token,
                        clock.as_ref(),
                        WakeDeliverySettlement::Discard(WakeDiscardReason::SequenceRewound),
                        Some(rewind_log),
                        work_cadence,
                        &mut report,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        delivery_id = %delivery.delivery_id,
                        target_session_id = %target_session_id,
                        error = %error,
                        "process wake enqueue failed; delivery remains pending"
                    );
                    Self::settle(
                        registry.as_ref(),
                        &delivery,
                        claim_token,
                        clock.as_ref(),
                        WakeDeliverySettlement::Retry,
                        None,
                        work_cadence,
                        &mut report,
                    )
                    .await?;
                }
            }
        }
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle(
        registry: &dyn ProcessRegistry,
        delivery: &crate::WakeDelivery,
        claim_token: &str,
        clock: &dyn Clock,
        settlement: WakeDeliverySettlement,
        rewind_log: Option<SequenceRewindDiscardLog<'_>>,
        work_cadence: &WorkCadencePolicy,
        report: &mut WakeDeliveryDriveReport,
    ) -> Result<(), PluginError> {
        match settlement {
            WakeDeliverySettlement::Discard(reason) => {
                match registry
                    .discard_wake_delivery(&delivery.delivery_id, claim_token, reason)
                    .await
                {
                    Ok(WakeDeliveryClaimOutcome::Applied) => {
                        if let Some(log) = rewind_log {
                            tracing::info!(
                                delivery_id = %delivery.delivery_id,
                                target_session_id = %log.session_id,
                                process_id = %log.process_id,
                                sequence = log.sequence,
                                allocation_floor = log.allocation_floor,
                                reason = "sequence_rewound",
                                "process wake delivery discarded"
                            );
                        } else {
                            tracing::info!(
                                delivery_id = %delivery.delivery_id,
                                target_session_id = %delivery.wake.target_session_id,
                                reason = reason.as_str(),
                                "process wake delivery discarded"
                            );
                        }
                        Self::record_discard_counter(report, reason);
                    }
                    Ok(WakeDeliveryClaimOutcome::ClaimLost { state }) => {
                        tracing::debug!(
                            delivery_id = %delivery.delivery_id,
                            ?state,
                            reason = reason.as_str(),
                            "concurrent process wake transition won before discard"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            delivery_id = %delivery.delivery_id,
                            reason = reason.as_str(),
                            error = %error,
                            "process wake discard transition failed; delivery deferred"
                        );
                        Self::defer_retry(registry, delivery, clock, work_cadence, report).await?;
                    }
                }
            }
            WakeDeliverySettlement::Enqueued => {
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
                        Self::defer_retry(registry, delivery, clock, work_cadence, report).await?;
                    }
                }
            }
            WakeDeliverySettlement::Retry => {
                Self::defer_retry(registry, delivery, clock, work_cadence, report).await?;
            }
        }
        Ok(())
    }

    fn record_discard_counter(report: &mut WakeDeliveryDriveReport, reason: WakeDiscardReason) {
        match reason {
            WakeDiscardReason::Expired => report.discarded_expired += 1,
            WakeDiscardReason::TargetGone => report.discarded_target_gone += 1,
            WakeDiscardReason::SequenceRewound => report.discarded_sequence_rewound += 1,
            WakeDiscardReason::Retargeted => {
                unreachable!("the wake delivery driver does not produce retargeted discards")
            }
        }
    }

    async fn defer_retry(
        registry: &dyn ProcessRegistry,
        delivery: &crate::WakeDelivery,
        clock: &dyn Clock,
        work_cadence: &WorkCadencePolicy,
        report: &mut WakeDeliveryDriveReport,
    ) -> Result<(), PluginError> {
        match registry
            .defer_wake_delivery(
                &delivery.delivery_id,
                delivery.claim_token()?,
                clock
                    .timestamp_ms()
                    .saturating_add(retry_delay_ms(delivery.attempts, work_cadence)),
            )
            .await
        {
            Ok(WakeDeliveryClaimOutcome::Applied | WakeDeliveryClaimOutcome::ClaimLost { .. }) => {
                report.retryable_failures += 1;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn run_loop(inner: Arc<WakeDeliveryDriverInner>, shutdown: CancellationToken) {
        let mut poll = inner.work_cadence.poll_initial;
        loop {
            let Some(queued_work) = inner.queued_work.upgrade() else {
                return;
            };
            let report = match Self::drive_pending_once_with_delivery_policy_and_work_cadence(
                Arc::clone(&inner.registry),
                Arc::clone(&inner.session_store_factory),
                queued_work,
                Arc::clone(&inner.clock),
                inner.delivery_policy,
                inner.work_cadence.delivery_batch.get(),
                &inner.work_cadence,
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
                + report.discarded_sequence_rewound
                > 0;
            let delay = if made_progress
                && report.retryable_failures == 0
                && report.inspected >= inner.work_cadence.delivery_batch.get()
            {
                poll = inner.work_cadence.poll_initial;
                Duration::ZERO
            } else {
                poll
            };
            if made_progress && report.retryable_failures == 0 {
                poll = inner.work_cadence.poll_initial;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = inner.notify.notified() => poll = inner.work_cadence.poll_initial,
                () = tokio::time::sleep(delay) => {
                    poll = poll.saturating_mul(2).min(inner.work_cadence.poll_max);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkCadencePolicy, retry_delay_ms};

    #[test]
    fn retry_delay_is_bounded_for_every_attempt_count() {
        let work_cadence = WorkCadencePolicy::default();
        let initial_ms = work_cadence.delivery_retry_initial.as_millis() as u64;
        let max_ms = work_cadence.delivery_retry_max.as_millis() as u64;

        assert_eq!(retry_delay_ms(0, &work_cadence), initial_ms);
        assert_eq!(retry_delay_ms(1, &work_cadence), initial_ms);
        assert_eq!(retry_delay_ms(2, &work_cadence), initial_ms * 2);
        assert_eq!(retry_delay_ms(14, &work_cadence), max_ms);
        assert_eq!(retry_delay_ms(64, &work_cadence), max_ms);
        assert_eq!(retry_delay_ms(u64::MAX, &work_cadence), max_ms);
    }
}
