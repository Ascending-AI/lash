use std::sync::Arc;
use std::time::Duration;

use super::QueuedWorkDriverInner;
use super::scheduler::{
    QueuedWorkDemand, QueuedWorkExecutionDispatcherGuard, QueuedWorkExecutionTaskCompletion,
};
use super::types::{
    QueuedWorkRunError, QueuedWorkRunErrorClass, QueuedWorkRunProgress, QueuedWorkSlowWake,
    QueuedWorkWakeContended, QueuedWorkWakeDisposition, QueuedWorkWakeFailure, WAKE_MAX_ATTEMPTS,
    WAKE_RETRY_INITIAL, WAKE_RETRY_MAX,
};

pub(super) struct QueuedWorkTaskDriver {
    pub(super) inner: Arc<QueuedWorkDriverInner>,
}

pub(super) enum QueuedWorkRunAttemptOutcome {
    Idle,
    Complete,
    Progress,
    Contended,
}

impl QueuedWorkTaskDriver {
    pub(super) async fn run_dispatcher(&self) {
        let mut dispatcher_guard =
            QueuedWorkExecutionDispatcherGuard::new(Arc::clone(&self.inner.scheduler));
        let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
        loop {
            while let Some((demand, permit)) = self.next_execution().await {
                let driver = Self {
                    inner: Arc::clone(&self.inner),
                };
                let completion = QueuedWorkExecutionTaskCompletion {
                    session_id: demand.session_id.clone(),
                    completed: completed_tx.clone(),
                };
                let scheduler = Arc::clone(&self.inner.scheduler);
                self.inner.wake_tasks.spawn(async move {
                    let _completion = completion;
                    match (permit, scheduler.slots.as_ref()) {
                        (Some(permit), Some(slots)) => {
                            super::super::process_worker::scope_queued_work_execution_permit(
                                Arc::clone(slots),
                                permit,
                                Arc::clone(&scheduler.changed),
                                driver.run_demand(demand),
                            )
                            .await;
                        }
                        (None, None) => driver.run_demand(demand).await,
                        _ => unreachable!("queued-work admission permit matches scheduler mode"),
                    }
                });
            }

            {
                let mut state = self.inner.scheduler.lock_state();
                if state.pending.is_empty() && state.active == 0 {
                    state.dispatcher_running = false;
                    dispatcher_guard.disarm();
                    return;
                }
            }

            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                Some(session_id) = completed_rx.recv() => {
                    let mut state = self.inner.scheduler.lock_state();
                    if state.active == 0 {
                        tracing::warn!(
                            target: "lash_core::queued_work",
                            session_id = session_id.as_deref(),
                            event = "queued_work.scheduler_accounting",
                            "queued-work execution completed without an active scheduler entry"
                        );
                    } else {
                        state.active -= 1;
                    }
                    if let Some(demand) = state.rerun.remove(&session_id) {
                        state.pending.push_back(demand);
                    } else {
                        state.scheduled.remove(&session_id);
                    }
                    self.inner.scheduler.metrics.intake_depth(
                        super::super::WorkerSlotKind::QueuedWork,
                        state.pending.len() + state.rerun.len(),
                    );
                }
                () = self.inner.scheduler.changed.notified() => {}
            }
        }
    }

    pub(super) async fn next_execution(
        &self,
    ) -> Option<(QueuedWorkDemand, Option<super::super::WorkerSlotPermit>)> {
        if self.inner.scheduler.lock_state().pending.is_empty() {
            return None;
        }
        let permit = match self.inner.scheduler.slots.as_ref() {
            Some(slots) => {
                let reserve = slots.reserve_slot(super::super::WorkerSlotKind::QueuedWork);
                tokio::pin!(reserve);
                Some(tokio::select! {
                    biased;
                    () = self.inner.shutdown.cancelled() => return None,
                    permit = &mut reserve => permit,
                })
            }
            None => None,
        };
        if self.inner.shutdown.is_cancelled() {
            drop(permit);
            return None;
        }
        let mut state = self.inner.scheduler.lock_state();
        let Some(mut demand) = state.pending.pop_front() else {
            drop(permit);
            return None;
        };
        if let Some(coalesced) = state.rerun.remove(&demand.session_id) {
            demand.merge(coalesced);
        }
        state.active += 1;
        self.inner.scheduler.metrics.intake_depth(
            super::super::WorkerSlotKind::QueuedWork,
            state.pending.len() + state.rerun.len(),
        );
        Some((demand, permit))
    }

    pub(super) async fn run_demand(&self, mut demand: QueuedWorkDemand) {
        let mut pass = 1_u32;
        let mut transient_attempt = 1_u32;
        let mut transient_retry_after = WAKE_RETRY_INITIAL;
        let mut contended_passes = 0_u32;
        let mut contended_retry_after = WAKE_RETRY_INITIAL;
        let mut contended_since = None;
        let mut next_contention_heartbeat = None;
        loop {
            let reason = demand.reason();
            let result = self.run_attempt(&demand, &reason, pass).await;
            match result {
                None => return,
                Some(Ok(
                    QueuedWorkRunAttemptOutcome::Idle | QueuedWorkRunAttemptOutcome::Complete,
                )) => {
                    return;
                }
                Some(Ok(QueuedWorkRunAttemptOutcome::Progress)) => {
                    transient_attempt = 1;
                    transient_retry_after = WAKE_RETRY_INITIAL;
                    contended_passes = 0;
                    contended_retry_after = WAKE_RETRY_INITIAL;
                    contended_since = None;
                    next_contention_heartbeat = None;
                    self.merge_rerun(&mut demand);
                    pass = pass.saturating_add(1);
                    continue;
                }
                Some(Ok(QueuedWorkRunAttemptOutcome::Contended)) => {
                    // A blocked verdict is only available after full runtime
                    // hydration. Keep that expensive poll bounded and visible,
                    // while preserving an independent full error-retry budget
                    // for the commit race that commonly follows lease release.
                    transient_attempt = 1;
                    transient_retry_after = WAKE_RETRY_INITIAL;
                    contended_passes = contended_passes.saturating_add(1);
                    let now = tokio::time::Instant::now();
                    let started = *contended_since.get_or_insert(now);
                    let heartbeat = next_contention_heartbeat
                        .get_or_insert(started + self.inner.slow_wake_threshold);
                    if now >= *heartbeat {
                        let event = QueuedWorkWakeContended {
                            session_id: demand.session_id.clone(),
                            reason: reason.clone(),
                            contended_passes,
                            contended_ms: now.duration_since(started).as_millis() as u64,
                            threshold_ms: self.inner.slow_wake_threshold.as_millis() as u64,
                            available_permits: self.inner.scheduler.available_permits(),
                            admission_limit: self.inner.scheduler.admission_limit,
                        };
                        tracing::warn!(
                            target: "lash_core::queued_work",
                            session_id = event.session_id.as_deref(),
                            reason = %event.reason,
                            contended_passes = event.contended_passes,
                            contended_ms = event.contended_ms,
                            threshold_ms = event.threshold_ms,
                            available_permits = ?event.available_permits,
                            admission_limit = ?event.admission_limit,
                            event = "queued_work.wake_contended",
                            "queued-work wake remains blocked by session execution contention"
                        );
                        *heartbeat = now + self.inner.slow_wake_threshold;
                    }
                    if !self.wait_for_retry(contended_retry_after).await {
                        return;
                    }
                    contended_retry_after =
                        contended_retry_after.saturating_mul(2).min(WAKE_RETRY_MAX);
                    self.merge_rerun(&mut demand);
                    pass = pass.saturating_add(1);
                    continue;
                }
                Some(Err(err)) => {
                    contended_passes = 0;
                    contended_retry_after = WAKE_RETRY_INITIAL;
                    contended_since = None;
                    next_contention_heartbeat = None;
                    let disposition = match err.class {
                        QueuedWorkRunErrorClass::Terminal => QueuedWorkWakeDisposition::Terminal,
                        QueuedWorkRunErrorClass::Transient
                            if transient_attempt >= WAKE_MAX_ATTEMPTS =>
                        {
                            QueuedWorkWakeDisposition::Exhausted
                        }
                        QueuedWorkRunErrorClass::Transient => QueuedWorkWakeDisposition::Retrying,
                    };
                    let failure = QueuedWorkWakeFailure {
                        session_id: demand.session_id.clone(),
                        reason: reason.clone(),
                        attempt: transient_attempt,
                        retry_after_ms: if matches!(
                            disposition,
                            QueuedWorkWakeDisposition::Retrying
                        ) {
                            transient_retry_after.as_millis() as u64
                        } else {
                            0
                        },
                        disposition,
                        error: err.to_string(),
                    };
                    match failure.disposition {
                        QueuedWorkWakeDisposition::Retrying => tracing::warn!(
                            target: "lash_core::queued_work",
                            session_id = failure.session_id.as_deref(),
                            reason = %failure.reason,
                            attempt = failure.attempt,
                            retry_after_ms = failure.retry_after_ms,
                            error = %failure.error,
                            event = "queued_work.wake_retry",
                            "queued-work wake failed; retrying the pending-work claim"
                        ),
                        QueuedWorkWakeDisposition::Terminal => {
                            tracing::warn!(
                                target: "lash_core::queued_work",
                                session_id = failure.session_id.as_deref(),
                                reason = %failure.reason,
                                attempt = failure.attempt,
                                error = %failure.error,
                                event = "queued_work.wake_terminal",
                                "queued-work wake stopped after a terminal failure"
                            );
                            return;
                        }
                        QueuedWorkWakeDisposition::Exhausted => {
                            tracing::warn!(
                                target: "lash_core::queued_work",
                                session_id = failure.session_id.as_deref(),
                                reason = %failure.reason,
                                attempt = failure.attempt,
                                error = %failure.error,
                                event = "queued_work.wake_exhausted",
                                "queued-work wake exhausted its retry budget"
                            );
                            return;
                        }
                    }
                    if !self.wait_for_retry(transient_retry_after).await {
                        return;
                    }
                    transient_retry_after =
                        transient_retry_after.saturating_mul(2).min(WAKE_RETRY_MAX);
                    transient_attempt = transient_attempt.saturating_add(1);
                    self.merge_rerun(&mut demand);
                    pass = pass.saturating_add(1);
                }
            }
        }
    }

    async fn wait_for_retry(&self, retry_after: Duration) -> bool {
        let backoff = super::super::process_worker::release_process_execution_permit_while(
            tokio::time::sleep(retry_after),
        );
        tokio::pin!(backoff);
        tokio::select! {
            () = self.inner.shutdown.cancelled() => false,
            () = &mut backoff => true,
        }
    }

    fn merge_rerun(&self, demand: &mut QueuedWorkDemand) {
        if let Some(coalesced) = self
            .inner
            .scheduler
            .lock_state()
            .rerun
            .remove(&demand.session_id)
        {
            demand.merge(coalesced);
        }
    }

    async fn run_attempt(
        &self,
        demand: &QueuedWorkDemand,
        reason: &str,
        attempt: u32,
    ) -> Option<Result<QueuedWorkRunAttemptOutcome, QueuedWorkRunError>> {
        let run_handle = Arc::clone(&self.inner.run_handle);
        let session_id = demand.session_id.clone();
        let run = async move {
            let claimable = run_handle
                .peek_claimable_queued_work(session_id.as_deref())
                .await?;
            if claimable == Some(false) {
                return Ok(QueuedWorkRunAttemptOutcome::Idle);
            }
            // Unknown claimability bounds a successfully completed hydration to
            // this one pass. It does not convert a transiently failed hydration
            // into success: that error still receives the finite retry ladder in
            // `run_demand`, after which the demand idles until a new notification
            // re-arms it.
            let progress = run_handle
                .claim_and_run_pending_with_progress(session_id.as_deref(), reason)
                .await?;
            Ok(match progress {
                QueuedWorkRunProgress::Blocked if claimable == Some(true) => {
                    QueuedWorkRunAttemptOutcome::Contended
                }
                QueuedWorkRunProgress::Claimed if claimable.is_some() => {
                    QueuedWorkRunAttemptOutcome::Progress
                }
                QueuedWorkRunProgress::Unknown
                | QueuedWorkRunProgress::Claimed
                | QueuedWorkRunProgress::Blocked => QueuedWorkRunAttemptOutcome::Complete,
            })
        };
        tokio::pin!(run);
        loop {
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return None,
                result = &mut run => return Some(result),
                () = tokio::time::sleep(self.inner.slow_wake_threshold) => {
                    let event = QueuedWorkSlowWake {
                        session_id: demand.session_id.clone(),
                        reason: reason.to_string(),
                        attempt,
                        threshold_ms: self.inner.slow_wake_threshold.as_millis() as u64,
                        available_permits: self.inner.scheduler.available_permits(),
                        admission_limit: self.inner.scheduler.admission_limit,
                    };
                    tracing::warn!(
                        target: "lash_core::queued_work",
                        session_id = event.session_id.as_deref(),
                        reason = %event.reason,
                        attempt = event.attempt,
                        threshold_ms = event.threshold_ms,
                        available_permits = ?event.available_permits,
                        admission_limit = ?event.admission_limit,
                        event = "queued_work.wake_slow",
                        "queued-work wake remains unfinished past the slow-wake threshold"
                    );
                }
            }
        }
    }
}
