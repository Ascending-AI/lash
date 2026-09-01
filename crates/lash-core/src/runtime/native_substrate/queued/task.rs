#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::task::{Context, Poll};
use std::time::Duration;

use crate::runtime::{WorkerSlotKind, WorkerSlotPermit};

use super::NativeQueuedWorkInner;
use super::scheduler::{
    QueuedWorkDemand, QueuedWorkExecutionDispatcherGuard, QueuedWorkExecutionTaskCompletion,
};
use super::types::{
    QueuedWorkRunError, QueuedWorkRunErrorClass, QueuedWorkRunProgress, QueuedWorkSlowWake,
    QueuedWorkWakeContended, QueuedWorkWakeFailure, QueuedWorkWakeOutcome,
    bounded_multiplicative_jitter,
};

pub(super) struct QueuedWorkTaskDriver {
    pub(super) inner: Arc<NativeQueuedWorkInner>,
}

pub(super) enum QueuedWorkRunAttemptOutcome {
    Idle,
    Complete,
    Progress,
    Contended,
}

enum RetryState {
    Progress {
        pass: u32,
    },
    Transient {
        pass: u32,
        attempt: u32,
        retry_after: Duration,
    },
    Contended {
        pass: u32,
        passes: u32,
        retry_after: Duration,
        started: tokio::time::Instant,
        heartbeat: tokio::time::Instant,
    },
}

impl RetryState {
    fn pass(&self) -> u32 {
        match self {
            Self::Progress { pass }
            | Self::Transient { pass, .. }
            | Self::Contended { pass, .. } => *pass,
        }
    }

    fn into_transient(self, retry_initial: Duration) -> Self {
        match self {
            Self::Transient {
                pass,
                attempt,
                retry_after,
            } => Self::Transient {
                pass,
                attempt,
                retry_after,
            },
            Self::Progress { pass } | Self::Contended { pass, .. } => Self::Transient {
                pass,
                attempt: 1,
                retry_after: retry_initial,
            },
        }
    }

    fn into_contended(
        self,
        now: tokio::time::Instant,
        retry_initial: Duration,
        slow_wake_threshold: Duration,
    ) -> Self {
        match self {
            Self::Contended {
                pass,
                passes,
                retry_after,
                started,
                heartbeat,
            } => Self::Contended {
                pass,
                passes: passes.saturating_add(1),
                retry_after,
                started,
                heartbeat,
            },
            Self::Progress { pass } | Self::Transient { pass, .. } => Self::Contended {
                pass,
                passes: 1,
                retry_after: retry_initial,
                started: now,
                heartbeat: now + slow_wake_threshold,
            },
        }
    }

    fn advance_backoff(&mut self, retry_max: Duration) {
        match self {
            Self::Transient {
                attempt,
                retry_after,
                ..
            } => {
                *retry_after = (*retry_after).saturating_mul(2).min(retry_max);
                *attempt = attempt.saturating_add(1);
            }
            Self::Contended { retry_after, .. } => {
                *retry_after = (*retry_after).saturating_mul(2).min(retry_max);
            }
            Self::Progress { .. } => unreachable!("progress has no retry backoff"),
        }
    }

    fn advance_pass(&mut self) {
        match self {
            Self::Progress { pass }
            | Self::Transient { pass, .. }
            | Self::Contended { pass, .. } => *pass = pass.saturating_add(1),
        }
    }
}

#[cfg(test)]
struct TestDispatchFuture<F> {
    dispatch: tracing::Dispatch,
    future: Pin<Box<F>>,
}

#[cfg(test)]
impl<F: Future> Future for TestDispatchFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        tracing::dispatcher::with_default(&this.dispatch, || this.future.as_mut().poll(cx))
    }
}

#[cfg(test)]
fn with_test_dispatch<F: Future>(dispatch: tracing::Dispatch, future: F) -> TestDispatchFuture<F> {
    TestDispatchFuture {
        dispatch,
        future: Box::pin(future),
    }
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
                #[cfg(test)]
                let test_dispatch = driver.inner.test_dispatch.clone();
                let run = async move {
                    let _completion = completion;
                    match (permit, scheduler.slots.as_ref()) {
                        (Some(permit), Some(slots)) => {
                            crate::runtime::process_worker::scope_queued_work_execution_permit(
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
                };
                #[cfg(test)]
                let run = with_test_dispatch(test_dispatch, run);
                self.inner.wake_tasks.spawn(run);
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
                        WorkerSlotKind::QueuedWork,
                        state.pending.len() + state.rerun.len(),
                    );
                }
                () = self.inner.scheduler.changed.notified() => {}
            }
        }
    }

    pub(super) async fn next_execution(
        &self,
    ) -> Option<(QueuedWorkDemand, Option<WorkerSlotPermit>)> {
        if self.inner.scheduler.lock_state().pending.is_empty() {
            return None;
        }
        let permit = match self.inner.scheduler.slots.as_ref() {
            Some(slots) => {
                let reserve = slots.reserve_slot(WorkerSlotKind::QueuedWork);
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
            WorkerSlotKind::QueuedWork,
            state.pending.len() + state.rerun.len(),
        );
        Some((demand, permit))
    }

    pub(super) async fn run_demand(&self, mut demand: QueuedWorkDemand) {
        let work_cadence = self.inner.work_cadence.clone();
        let mut retry_state = RetryState::Progress { pass: 1 };
        loop {
            let reason = demand.reason();
            let result = self.run_attempt(&demand, &reason, retry_state.pass()).await;
            match result {
                None => return,
                Some(Ok(
                    QueuedWorkRunAttemptOutcome::Idle | QueuedWorkRunAttemptOutcome::Complete,
                )) => {
                    return;
                }
                Some(Ok(QueuedWorkRunAttemptOutcome::Progress)) => {
                    retry_state = RetryState::Progress {
                        pass: retry_state.pass().saturating_add(1),
                    };
                    self.merge_rerun(&mut demand);
                    continue;
                }
                Some(Ok(QueuedWorkRunAttemptOutcome::Contended)) => {
                    // A blocked verdict is only available after full runtime
                    // hydration. Keep that expensive poll bounded and visible,
                    // while preserving an independent full error-retry budget
                    // for the commit race that commonly follows lease release.
                    let now = tokio::time::Instant::now();
                    retry_state = retry_state.into_contended(
                        now,
                        work_cadence.retry_initial,
                        work_cadence.slow_wake_threshold,
                    );
                    let RetryState::Contended {
                        passes,
                        started,
                        heartbeat,
                        ..
                    } = &mut retry_state
                    else {
                        unreachable!("contention creates contended retry state")
                    };
                    if now >= *heartbeat {
                        let event = QueuedWorkWakeContended {
                            session_id: demand.session_id.clone(),
                            reason: reason.clone(),
                            contended_passes: *passes,
                            contended_ms: now.duration_since(*started).as_millis() as u64,
                            threshold_ms: work_cadence.slow_wake_threshold.as_millis() as u64,
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
                        *heartbeat = now + self.inner.work_cadence.slow_wake_threshold;
                    }
                    let retry_after = match &retry_state {
                        RetryState::Contended { retry_after, .. } => *retry_after,
                        _ => unreachable!("contention creates contended retry state"),
                    };
                    if !self
                        .wait_for_retry(bounded_multiplicative_jitter(
                            retry_after,
                            work_cadence.retry_initial,
                            work_cadence.retry_max,
                        ))
                        .await
                    {
                        return;
                    }
                    retry_state.advance_backoff(work_cadence.retry_max);
                    self.merge_rerun(&mut demand);
                    retry_state.advance_pass();
                    continue;
                }
                Some(Err(err)) => {
                    retry_state = retry_state.into_transient(work_cadence.retry_initial);
                    let (transient_attempt, transient_retry_after) = match &retry_state {
                        RetryState::Transient {
                            attempt,
                            retry_after,
                            ..
                        } => (*attempt, *retry_after),
                        _ => unreachable!("an error creates transient retry state"),
                    };
                    let disposition = match err.class {
                        QueuedWorkRunErrorClass::Terminal => QueuedWorkWakeOutcome::Terminal,
                        QueuedWorkRunErrorClass::Transient
                            if transient_attempt >= work_cadence.max_transient_attempts.get() =>
                        {
                            QueuedWorkWakeOutcome::Exhausted
                        }
                        QueuedWorkRunErrorClass::Transient => QueuedWorkWakeOutcome::Retrying,
                    };
                    let retry_after =
                        matches!(disposition, QueuedWorkWakeOutcome::Retrying).then(|| {
                            bounded_multiplicative_jitter(
                                transient_retry_after,
                                work_cadence.retry_initial,
                                work_cadence.retry_max,
                            )
                        });
                    let failure = QueuedWorkWakeFailure {
                        session_id: demand.session_id.clone(),
                        reason: reason.clone(),
                        attempt: transient_attempt,
                        retry_after_ms: retry_after.map_or(0, |delay| delay.as_millis() as u64),
                        disposition,
                        error: err.to_string(),
                    };
                    match failure.disposition {
                        QueuedWorkWakeOutcome::Retrying => {
                            crate::operational_metrics::record_queued_work_wake_retry();
                            tracing::warn!(
                                target: "lash_core::queued_work",
                                session_id = failure.session_id.as_deref(),
                                reason = %failure.reason,
                                attempt = failure.attempt,
                                retry_after_ms = failure.retry_after_ms,
                                error = %failure.error,
                                event = "queued_work.wake_retry",
                                "queued-work wake failed; retrying the pending-work claim"
                            );
                        }
                        QueuedWorkWakeOutcome::Terminal => {
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
                        QueuedWorkWakeOutcome::Exhausted => {
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
                    if !self
                        .wait_for_retry(retry_after.expect("retrying disposition has a delay"))
                        .await
                    {
                        return;
                    }
                    retry_state.advance_backoff(work_cadence.retry_max);
                    self.merge_rerun(&mut demand);
                    retry_state.advance_pass();
                }
            }
        }
    }

    async fn wait_for_retry(&self, retry_after: Duration) -> bool {
        let backoff = crate::runtime::process_worker::release_process_execution_permit_while(
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
                () = tokio::time::sleep(self.inner.work_cadence.slow_wake_threshold) => {
                    let event = QueuedWorkSlowWake {
                        session_id: demand.session_id.clone(),
                        reason: reason.to_string(),
                        attempt,
                        threshold_ms: self
                            .inner
                            .work_cadence
                            .slow_wake_threshold
                            .as_millis() as u64,
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
