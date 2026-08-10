use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::PluginError;

use super::worker_capacity::{
    DefaultWorkerSlotSupplier, ObservedWorkerSlotSupplier, WorkerCapacityMetrics,
    WorkerSlotSupplier as _,
};

const WAKE_RETRY_INITIAL: Duration = Duration::from_millis(25);
const WAKE_RETRY_MAX: Duration = Duration::from_secs(1);
const WAKE_MAX_ATTEMPTS: u32 = 8;

#[cfg(any(test, feature = "testing"))]
pub const QUEUED_WORK_MAX_TRANSIENT_ATTEMPTS: usize = WAKE_MAX_ATTEMPTS as usize;

/// Default maximum number of queued-work wake executions admitted at once.
pub const DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY: usize = 64;

/// Elapsed time after which an unfinished queued-work wake emits warning telemetry.
pub const QUEUED_WORK_SLOW_WAKE_THRESHOLD: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueuedWorkExecutionConcurrency(usize);

impl QueuedWorkExecutionConcurrency {
    fn new(concurrency: usize) -> Result<Self, QueuedWorkExecutionConcurrencyError> {
        if !(1..=Semaphore::MAX_PERMITS).contains(&concurrency) {
            return Err(QueuedWorkExecutionConcurrencyError { concurrency });
        }
        Ok(Self(concurrency))
    }

    fn get(self) -> usize {
        self.0
    }
}

/// Invalid queued-work wake execution concurrency supplied by a host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "queued-work execution concurrency must be between 1 and {max} (inclusive), got {concurrency}",
    max = Semaphore::MAX_PERMITS
)]
pub struct QueuedWorkExecutionConcurrencyError {
    concurrency: usize,
}

#[derive(Clone, Debug)]
pub struct QueuedWorkRunRequest {
    pub session_id: Option<String>,
    pub reason: String,
    pub trace_idle: bool,
}

impl QueuedWorkRunRequest {
    fn new(session_id: Option<String>, reason: impl Into<String>, trace_idle: bool) -> Self {
        Self {
            session_id,
            reason: reason.into(),
            trace_idle,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkRunErrorClass {
    Transient,
    Terminal,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{error}")]
pub struct QueuedWorkRunError {
    pub class: QueuedWorkRunErrorClass,
    pub error: PluginError,
}

impl QueuedWorkRunError {
    pub fn transient(error: PluginError) -> Self {
        Self {
            class: QueuedWorkRunErrorClass::Transient,
            error,
        }
    }

    pub fn terminal(error: PluginError) -> Self {
        Self {
            class: QueuedWorkRunErrorClass::Terminal,
            error,
        }
    }
}

impl From<PluginError> for QueuedWorkRunError {
    fn from(error: PluginError) -> Self {
        if matches!(&error, PluginError::Runtime(error) if error.is_retryable()) {
            Self::transient(error)
        } else {
            Self::terminal(error)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkWakeDisposition {
    Retrying,
    Terminal,
    Exhausted,
}

/// Operational evidence that a best-effort queued-work wake needs retry.
///
/// A wake failure is never an enqueue failure: the input is already durable,
/// and transient failures re-enter the idempotent pending-work claim path up
/// to the driver's bounded retry limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkWakeFailure {
    pub session_id: Option<String>,
    pub reason: String,
    pub attempt: u32,
    pub retry_after_ms: u64,
    pub disposition: QueuedWorkWakeDisposition,
    pub error: String,
}

/// Operational evidence that an admitted queued-work wake remains unfinished.
///
/// This event is observational only. It never cancels or times out the wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkSlowWake {
    pub session_id: Option<String>,
    pub reason: String,
    pub attempt: u32,
    pub threshold_ms: u64,
    pub available_permits: Option<usize>,
    pub admission_limit: Option<usize>,
}

/// Repeating operational evidence that queued work is blocked by a live
/// session execution lease.
///
/// This event is observational only. The inline driver must fully hydrate the
/// runtime before it can distinguish a blocked claim from an idle queue, so
/// one hydration per bounded contention poll is the current floor. The cheap
/// pre-hydration peek deliberately remains a conservative queue predicate; it
/// does not expose lease state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkWakeContended {
    pub session_id: Option<String>,
    pub reason: String,
    pub contended_passes: u32,
    pub contended_ms: u64,
    pub threshold_ms: u64,
    pub available_permits: Option<usize>,
    pub admission_limit: Option<usize>,
}

/// Whether one queued-work pass actually claimed durable work.
///
/// The inline reference driver reports this so a positive conservative peek
/// followed by a live session-lease conflict backs off instead of rehydrating
/// eagerly. External engine submitters may retain the default `Unknown` result;
/// Lash never re-drives engine-owned work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkRunProgress {
    Unknown,
    Claimed,
    Blocked,
}

#[async_trait::async_trait]
pub trait QueuedWorkRunHandle: Send + Sync {
    /// Cheap durable check performed before runtime hydration.
    ///
    /// `Some(true)` admits a run and rechecks afterward until the durable queue
    /// is idle; `Some(false)` skips hydration. `None` preserves single-pass
    /// behavior for external handles without this persistence seam. The
    /// embedded Lash adapter returns `Some` from a real
    /// [`SessionStoreFactory`](crate::SessionStoreFactory) read.
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        Ok(None)
    }

    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError>;

    /// Host-driven single pass: claim and submit ready queued work, optionally
    /// narrowed to one session. The symmetric counterpart to
    /// [`ProcessRunHandle::claim_and_run_pending`](super::ProcessRunHandle::claim_and_run_pending).
    ///
    /// Idempotency is the store scheduler's job, not a same-process memory
    /// guard. Hosts call this on an event (enqueue, process wake, turn
    /// completion) instead of polling.
    async fn claim_and_run_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<(), QueuedWorkRunError> {
        let request =
            QueuedWorkRunRequest::new(session_id.map(str::to_string), reason.to_string(), false);
        self.run_queued_work(request).await
    }

    /// Run one pass and report whether the pass claimed durable work.
    ///
    /// External handles keep the single-pass default. The inline reference
    /// handle overrides this to distinguish progress from lease contention.
    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        self.claim_and_run_pending(session_id, reason).await?;
        Ok(QueuedWorkRunProgress::Unknown)
    }
}

#[derive(Clone)]
pub struct QueuedWorkDriver {
    inner: Arc<QueuedWorkDriverInner>,
    _lifetime: Arc<QueuedWorkDriverLifetime>,
}

struct QueuedWorkDriverInner {
    run_handle: Arc<dyn QueuedWorkRunHandle>,
    shutdown: CancellationToken,
    wake_tasks: TaskTracker,
    scheduler: Arc<QueuedWorkExecutionScheduler>,
    slow_wake_threshold: Duration,
}

struct QueuedWorkDriverLifetime {
    shutdown: CancellationToken,
    wake_tasks: TaskTracker,
}

impl Drop for QueuedWorkDriverLifetime {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.wake_tasks.close();
    }
}

#[derive(Clone, Debug)]
struct QueuedWorkDemand {
    session_id: Option<String>,
    reasons: Vec<String>,
}

impl QueuedWorkDemand {
    fn new(session_id: Option<String>, reason: String) -> Self {
        Self {
            session_id,
            reasons: vec![reason],
        }
    }

    fn merge(&mut self, other: Self) {
        for reason in other.reasons {
            if !self.reasons.contains(&reason) {
                self.reasons.push(reason);
            }
        }
    }

    fn reason(&self) -> String {
        self.reasons.join(",")
    }
}

#[derive(Default)]
struct QueuedWorkExecutionSchedulerState {
    pending: VecDeque<QueuedWorkDemand>,
    scheduled: BTreeSet<Option<String>>,
    rerun: BTreeMap<Option<String>, QueuedWorkDemand>,
    active: usize,
    dispatcher_running: bool,
}

struct QueuedWorkExecutionScheduler {
    slots: Option<Arc<dyn super::WorkerSlotSupplier>>,
    admission_limit: Option<usize>,
    metrics: WorkerCapacityMetrics,
    state: Mutex<QueuedWorkExecutionSchedulerState>,
    changed: Arc<tokio::sync::Notify>,
}

struct QueuedWorkExecutionTaskCompletion {
    session_id: Option<String>,
    completed: tokio::sync::mpsc::UnboundedSender<Option<String>>,
}

impl Drop for QueuedWorkExecutionTaskCompletion {
    fn drop(&mut self) {
        let _ = self.completed.send(self.session_id.clone());
    }
}

struct QueuedWorkExecutionDispatcherGuard {
    scheduler: Arc<QueuedWorkExecutionScheduler>,
    armed: bool,
}

impl QueuedWorkExecutionDispatcherGuard {
    fn new(scheduler: Arc<QueuedWorkExecutionScheduler>) -> Self {
        Self {
            scheduler,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for QueuedWorkExecutionDispatcherGuard {
    fn drop(&mut self) {
        if self.armed {
            self.scheduler.lock_state().dispatcher_running = false;
            self.scheduler.changed.notify_one();
        }
    }
}

impl QueuedWorkExecutionScheduler {
    fn unbounded() -> Self {
        Self {
            slots: None,
            admission_limit: None,
            metrics: WorkerCapacityMetrics::default(),
            state: Mutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn inline(concurrency: QueuedWorkExecutionConcurrency) -> Self {
        let supplier = Arc::new(DefaultWorkerSlotSupplier::new(
            super::DEFAULT_PROCESS_EXECUTION_CONCURRENCY,
            concurrency.get(),
        ));
        Self::with_supplier(supplier, Some(concurrency.get()))
    }

    fn with_supplier(
        supplier: Arc<dyn super::WorkerSlotSupplier>,
        admission_limit: Option<usize>,
    ) -> Self {
        let metrics = WorkerCapacityMetrics::default();
        let slots = ObservedWorkerSlotSupplier::new(supplier, metrics.clone());
        metrics.slots(
            super::WorkerSlotKind::QueuedWork,
            0,
            slots.available_slots(super::WorkerSlotKind::QueuedWork),
        );
        metrics.intake_depth(super::WorkerSlotKind::QueuedWork, 0);
        Self {
            slots: Some(slots),
            admission_limit,
            metrics,
            state: Mutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, QueuedWorkExecutionSchedulerState> {
        self.state.lock_recover()
    }

    fn available_permits(&self) -> Option<usize> {
        self.slots
            .as_ref()
            .map(|slots| slots.available_slots(super::WorkerSlotKind::QueuedWork))
    }
}

impl QueuedWorkDriver {
    pub fn validate_execution_concurrency(
        concurrency: usize,
    ) -> Result<(), QueuedWorkExecutionConcurrencyError> {
        QueuedWorkExecutionConcurrency::new(concurrency).map(drop)
    }

    pub fn new(run_handle: Arc<dyn QueuedWorkRunHandle>) -> Self {
        Self::from_parts(
            run_handle,
            CancellationToken::new(),
            None,
            QUEUED_WORK_SLOW_WAKE_THRESHOLD,
        )
    }

    /// Construct an inline reference-substrate driver with a host-selected
    /// admission bound.
    ///
    /// Engine-backed submitters should use [`Self::new`]: their substrate owns
    /// backpressure and Lash only coalesces same-session notifications.
    pub fn with_execution_concurrency(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        concurrency: usize,
    ) -> Result<Self, QueuedWorkExecutionConcurrencyError> {
        Ok(Self::from_parts(
            run_handle,
            CancellationToken::new(),
            Some(QueuedWorkExecutionConcurrency::new(concurrency)?),
            QUEUED_WORK_SLOW_WAKE_THRESHOLD,
        ))
    }

    /// Construct an inline reference-substrate driver admitted by `supplier`.
    #[doc(hidden)]
    pub fn with_worker_slot_supplier(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        supplier: Arc<dyn super::WorkerSlotSupplier>,
    ) -> Self {
        Self::from_parts_with_supplier(
            run_handle,
            CancellationToken::new(),
            None,
            Some(supplier),
            QUEUED_WORK_SLOW_WAKE_THRESHOLD,
        )
    }

    pub fn with_shutdown_token(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
    ) -> Self {
        Self::from_parts(run_handle, shutdown, None, QUEUED_WORK_SLOW_WAKE_THRESHOLD)
    }

    fn from_parts(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
        concurrency: Option<QueuedWorkExecutionConcurrency>,
        slow_wake_threshold: Duration,
    ) -> Self {
        Self::from_parts_with_supplier(run_handle, shutdown, concurrency, None, slow_wake_threshold)
    }

    fn from_parts_with_supplier(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
        concurrency: Option<QueuedWorkExecutionConcurrency>,
        supplier: Option<Arc<dyn super::WorkerSlotSupplier>>,
        slow_wake_threshold: Duration,
    ) -> Self {
        let shutdown = shutdown.child_token();
        let wake_tasks = TaskTracker::new();
        Self {
            inner: Arc::new(QueuedWorkDriverInner {
                run_handle,
                shutdown: shutdown.clone(),
                wake_tasks: wake_tasks.clone(),
                scheduler: Arc::new(match (supplier, concurrency) {
                    (Some(supplier), _) => {
                        QueuedWorkExecutionScheduler::with_supplier(supplier, None)
                    }
                    (None, Some(concurrency)) => QueuedWorkExecutionScheduler::inline(concurrency),
                    (None, None) => QueuedWorkExecutionScheduler::unbounded(),
                }),
                slow_wake_threshold,
            }),
            _lifetime: Arc::new(QueuedWorkDriverLifetime {
                shutdown,
                wake_tasks,
            }),
        }
    }

    pub async fn claim_and_run_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<(), PluginError> {
        if let Err(err) = self
            .inner
            .run_handle
            .claim_and_run_pending(session_id, reason)
            .await
        {
            tracing::warn!("queued work drive ({reason}) failed: {err}");
            return Err(err.error);
        }
        Ok(())
    }

    /// Notify the driver that durable queued work may be claimable.
    ///
    /// Demand is contentless and coalesced per session. At most one rerun bit is
    /// retained while a session is already queued or executing, and a shared
    /// semaphore bounds admitted executions across sessions. Callers therefore
    /// return their durable acceptance receipt without creating one task per
    /// signal.
    pub fn notify_pending_work(&self, session_id: Option<&str>, reason: &str) {
        let session_id = session_id.map(str::to_string);
        let reason = reason.to_string();
        let should_start_dispatcher = {
            let mut state = self.inner.scheduler.lock_state();
            let demand = QueuedWorkDemand::new(session_id.clone(), reason);
            if state.scheduled.insert(session_id.clone()) {
                state.pending.push_back(demand);
            } else {
                state
                    .rerun
                    .entry(session_id)
                    .and_modify(|rerun| rerun.merge(demand.clone()))
                    .or_insert(demand);
            }
            if state.dispatcher_running {
                false
            } else {
                state.dispatcher_running = true;
                true
            }
        };
        {
            let state = self.inner.scheduler.lock_state();
            self.inner.scheduler.metrics.intake_depth(
                super::WorkerSlotKind::QueuedWork,
                state.pending.len() + state.rerun.len(),
            );
        }
        self.inner.scheduler.changed.notify_one();
        if should_start_dispatcher {
            let driver = QueuedWorkTaskDriver {
                inner: Arc::clone(&self.inner),
            };
            self.inner
                .wake_tasks
                .spawn(async move { driver.run_dispatcher().await });
        }
    }
}

struct QueuedWorkTaskDriver {
    inner: Arc<QueuedWorkDriverInner>,
}

enum QueuedWorkRunAttemptOutcome {
    Idle,
    Complete,
    Progress,
    Contended,
}

impl QueuedWorkTaskDriver {
    async fn run_dispatcher(&self) {
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
                            super::process_worker::scope_queued_work_execution_permit(
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
                        super::WorkerSlotKind::QueuedWork,
                        state.pending.len() + state.rerun.len(),
                    );
                }
                () = self.inner.scheduler.changed.notified() => {}
            }
        }
    }

    async fn next_execution(&self) -> Option<(QueuedWorkDemand, Option<super::WorkerSlotPermit>)> {
        if self.inner.scheduler.lock_state().pending.is_empty() {
            return None;
        }
        let permit = match self.inner.scheduler.slots.as_ref() {
            Some(slots) => {
                let reserve = slots.reserve_slot(super::WorkerSlotKind::QueuedWork);
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
            super::WorkerSlotKind::QueuedWork,
            state.pending.len() + state.rerun.len(),
        );
        Some((demand, permit))
    }

    async fn run_demand(&self, mut demand: QueuedWorkDemand) {
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
        let backoff = super::process_worker::release_process_execution_permit_while(
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    struct BurstRunHandle {
        hydrations: AtomicUsize,
        pending: Mutex<VecDeque<usize>>,
        observed: Mutex<Vec<usize>>,
        reasons: Mutex<Vec<String>>,
        completed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for BurstRunHandle {
        async fn peek_claimable_queued_work(
            &self,
            _session_id: Option<&str>,
        ) -> Result<Option<bool>, QueuedWorkRunError> {
            Ok(Some(!self.pending.lock_recover().is_empty()))
        }

        async fn run_queued_work(
            &self,
            request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.hydrations.fetch_add(1, Ordering::SeqCst);
            self.reasons.lock_recover().push(request.reason);
            let drained = {
                let mut pending = self.pending.lock_recover();
                let limit = pending.len().min(3);
                pending.drain(..limit).collect::<Vec<_>>()
            };
            self.observed.lock_recover().extend(drained);
            self.completed.notify_one();
            Ok(())
        }

        async fn claim_and_run_pending_with_progress(
            &self,
            session_id: Option<&str>,
            reason: &str,
        ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
            self.claim_and_run_pending(session_id, reason).await?;
            Ok(QueuedWorkRunProgress::Claimed)
        }
    }

    #[tokio::test]
    async fn burst_for_one_session_drains_ordered_batches_with_coalesced_hydrations() {
        const SIGNALS: usize = 8;
        let handle = Arc::new(BurstRunHandle {
            hydrations: AtomicUsize::new(0),
            pending: Mutex::new((0..SIGNALS).collect()),
            observed: Mutex::new(Vec::new()),
            reasons: Mutex::new(Vec::new()),
            completed: tokio::sync::Notify::new(),
        });
        let driver = QueuedWorkDriver::new(handle.clone());
        for index in 0..SIGNALS {
            let reason = if index % 2 == 0 {
                "queued_turn_input"
            } else {
                "process_wake"
            };
            driver.notify_pending_work(Some("session-burst"), reason);
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let completed = handle.completed.notified();
                if handle.observed.lock_recover().len() == SIGNALS {
                    break;
                }
                completed.await;
            }
        })
        .await
        .expect("all queued work drains in bounded batches");
        assert_eq!(handle.hydrations.load(Ordering::SeqCst), 3);
        assert_eq!(
            *handle.observed.lock_recover(),
            (0..SIGNALS).collect::<Vec<_>>()
        );
        assert_eq!(
            *handle.reasons.lock_recover(),
            vec![
                "queued_turn_input,process_wake",
                "queued_turn_input,process_wake",
                "queued_turn_input,process_wake",
            ]
        );
    }

    struct AdmissionRunHandle {
        active: AtomicUsize,
        max_active: AtomicUsize,
        entered: AtomicUsize,
        completed: AtomicUsize,
        changed: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for AdmissionRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_waiters();
            self.release
                .acquire()
                .await
                .expect("release remains open")
                .forget();
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.completed.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_waiters();
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_slot_supplier_releases_permits_and_preserves_admission_bound() {
        const SIGNALS: usize = 8;
        const CONCURRENCY: usize = 2;
        let handle = Arc::new(AdmissionRunHandle {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            entered: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let driver = QueuedWorkDriver::with_execution_concurrency(handle.clone(), CONCURRENCY)
            .expect("valid concurrency");
        for index in 0..SIGNALS {
            driver.notify_pending_work(Some(&format!("session-{index}")), "queued_turn_input");
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let changed = handle.changed.notified();
                if handle.entered.load(Ordering::SeqCst) == CONCURRENCY {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("the configured number of executions is admitted");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(handle.entered.load(Ordering::SeqCst), CONCURRENCY);
        assert_eq!(handle.max_active.load(Ordering::SeqCst), CONCURRENCY);

        handle.release.add_permits(SIGNALS);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let changed = handle.changed.notified();
                if handle.completed.load(Ordering::SeqCst) == SIGNALS {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("all retained demand executes after permits are released");
        assert_eq!(handle.max_active.load(Ordering::SeqCst), CONCURRENCY);
    }

    #[tokio::test]
    async fn external_engine_submitters_do_not_inherit_the_inline_admission_bound() {
        const SIGNALS: usize = 8;
        let handle = Arc::new(AdmissionRunHandle {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            entered: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let driver = QueuedWorkDriver::new(handle.clone());
        for index in 0..SIGNALS {
            driver.notify_pending_work(Some(&format!("engine-session-{index}")), "engine_submit");
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let changed = handle.changed.notified();
                if handle.entered.load(Ordering::SeqCst) == SIGNALS {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("the engine substrate admits every submitted session");
        assert_eq!(handle.max_active.load(Ordering::SeqCst), SIGNALS);
        handle.release.add_permits(SIGNALS);
    }

    struct ParkAwareRunHandle {
        first_parked: tokio::sync::Notify,
        second_entered: tokio::sync::Notify,
        resume_first: tokio::sync::Semaphore,
        completed: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for ParkAwareRunHandle {
        async fn run_queued_work(
            &self,
            request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            match request.session_id.as_deref() {
                Some("session-parked") => {
                    self.first_parked.notify_one();
                    super::super::process_worker::release_process_execution_permit_while(async {
                        self.resume_first
                            .acquire()
                            .await
                            .expect("resume semaphore remains open")
                            .forget();
                    })
                    .await;
                }
                Some("session-runnable") => self.second_entered.notify_one(),
                session => panic!("unexpected session: {session:?}"),
            }
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn inline_admission_slot_is_released_while_a_turn_is_parked() {
        let handle = Arc::new(ParkAwareRunHandle {
            first_parked: tokio::sync::Notify::new(),
            second_entered: tokio::sync::Notify::new(),
            resume_first: tokio::sync::Semaphore::new(0),
            completed: AtomicUsize::new(0),
        });
        let driver = QueuedWorkDriver::with_execution_concurrency(handle.clone(), 1)
            .expect("valid concurrency");
        driver.notify_pending_work(Some("session-parked"), "queued_turn_input");
        handle.first_parked.notified().await;

        driver.notify_pending_work(Some("session-runnable"), "queued_turn_input");
        tokio::time::timeout(Duration::from_secs(1), handle.second_entered.notified())
            .await
            .expect("a parked inline turn releases its queued-work slot");
        handle.resume_first.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.completed.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the parked turn reacquires its slot and completes");
    }

    struct RerunRunHandle {
        pending: Mutex<VecDeque<usize>>,
        observed: Mutex<Vec<usize>>,
        reasons: Mutex<Vec<String>>,
        runs: AtomicUsize,
        first_entered: tokio::sync::Notify,
        release_first: tokio::sync::Semaphore,
        completed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for RerunRunHandle {
        async fn peek_claimable_queued_work(
            &self,
            _session_id: Option<&str>,
        ) -> Result<Option<bool>, QueuedWorkRunError> {
            Ok(Some(!self.pending.lock_recover().is_empty()))
        }

        async fn run_queued_work(
            &self,
            request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            let run = self.runs.fetch_add(1, Ordering::SeqCst);
            self.reasons.lock_recover().push(request.reason);
            let drained = self.pending.lock_recover().drain(..).collect::<Vec<_>>();
            self.observed.lock_recover().extend(drained);
            if run == 0 {
                self.first_entered.notify_one();
                self.release_first
                    .acquire()
                    .await
                    .expect("release remains open")
                    .forget();
            }
            self.completed.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn signal_during_an_inflight_run_schedules_exactly_one_rerun() {
        let handle = Arc::new(RerunRunHandle {
            pending: Mutex::new(VecDeque::from([0])),
            observed: Mutex::new(Vec::new()),
            reasons: Mutex::new(Vec::new()),
            runs: AtomicUsize::new(0),
            first_entered: tokio::sync::Notify::new(),
            release_first: tokio::sync::Semaphore::new(0),
            completed: tokio::sync::Notify::new(),
        });
        let driver = QueuedWorkDriver::new(handle.clone());
        driver.notify_pending_work(Some("session-rerun"), "first");
        handle.first_entered.notified().await;

        handle.pending.lock_recover().push_back(1);
        driver.notify_pending_work(Some("session-rerun"), "second");
        driver.notify_pending_work(Some("session-rerun"), "second");
        handle.release_first.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.runs.load(Ordering::SeqCst) < 2 {
                handle.completed.notified().await;
            }
        })
        .await
        .expect("the in-flight signal causes one rerun");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert_eq!(handle.runs.load(Ordering::SeqCst), 2);
        assert_eq!(*handle.observed.lock_recover(), vec![0, 1]);
        assert_eq!(
            *handle.reasons.lock_recover(),
            vec!["first", "second"],
            "the in-flight signal is attributed to the rerun it schedules"
        );
    }

    struct EmptyPeekRunHandle {
        peeks: AtomicUsize,
        hydrations: AtomicUsize,
        peeked: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for EmptyPeekRunHandle {
        async fn peek_claimable_queued_work(
            &self,
            _session_id: Option<&str>,
        ) -> Result<Option<bool>, QueuedWorkRunError> {
            self.peeks.fetch_add(1, Ordering::SeqCst);
            self.peeked.notify_one();
            Ok(Some(false))
        }

        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.hydrations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn empty_claimable_peek_skips_hydration() {
        let handle = Arc::new(EmptyPeekRunHandle {
            peeks: AtomicUsize::new(0),
            hydrations: AtomicUsize::new(0),
            peeked: tokio::sync::Notify::new(),
        });
        let driver = QueuedWorkDriver::new(handle.clone());
        driver.notify_pending_work(Some("session-empty"), "queued_turn_input");
        handle.peeked.notified().await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert_eq!(handle.peeks.load(Ordering::SeqCst), 1);
        assert_eq!(handle.hydrations.load(Ordering::SeqCst), 0);
    }

    struct CreateOnlyFactory {
        inner: crate::InMemorySessionStoreFactory,
    }

    #[async_trait::async_trait]
    impl crate::AttachmentRootSet for CreateOnlyFactory {
        async fn live_attachment_refs(
            &self,
            cutoff: u64,
        ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
            crate::AttachmentRootSet::live_attachment_refs(&self.inner, cutoff).await
        }

        async fn has_live_attachment_ref(
            &self,
            id: &crate::AttachmentId,
            cutoff: u64,
        ) -> Result<bool, crate::StoreError> {
            crate::AttachmentRootSet::has_live_attachment_ref(&self.inner, id, cutoff).await
        }
    }

    #[async_trait::async_trait]
    impl crate::SessionStoreFactory for CreateOnlyFactory {
        async fn create_store(
            &self,
            request: &crate::SessionStoreCreateRequest,
        ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
            self.inner.create_store(request).await
        }

        async fn delete_session(&self, session_id: &str) -> Result<(), String> {
            self.inner.delete_session(session_id).await
        }
    }

    #[tokio::test]
    async fn create_only_factory_treats_claimability_as_unknown_and_runs() {
        let factory = CreateOnlyFactory {
            inner: crate::InMemorySessionStoreFactory::new(),
        };
        let request = crate::SessionStoreCreateRequest {
            session_id: "create-only-factory".to_string(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        };

        assert_eq!(
            crate::SessionStoreFactory::has_claimable_queued_work(&factory, &request, 0)
                .await
                .expect("the conservative default succeeds"),
            None,
            "a factory that cannot inspect an existing store must preserve unknown claimability"
        );
    }

    struct PublicProbeRunHandle {
        peeks: AtomicUsize,
        hydrations: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for PublicProbeRunHandle {
        async fn peek_claimable_queued_work(
            &self,
            _session_id: Option<&str>,
        ) -> Result<Option<bool>, QueuedWorkRunError> {
            self.peeks.fetch_add(1, Ordering::SeqCst);
            Ok(Some(true))
        }

        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.hydrations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn public_single_pass_handle_never_eagerly_rehydrates_a_positive_peek() {
        let handle = Arc::new(PublicProbeRunHandle {
            peeks: AtomicUsize::new(0),
            hydrations: AtomicUsize::new(0),
        });
        let driver = QueuedWorkDriver::new(handle.clone());

        driver.notify_pending_work(Some("session-public-probe"), "queued_turn_input");
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(handle.hydrations.load(Ordering::SeqCst), 1);
        assert_eq!(handle.peeks.load(Ordering::SeqCst), 1);
    }

    struct ContendedRunHandle {
        peeks: AtomicUsize,
        hydrations: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for ContendedRunHandle {
        async fn peek_claimable_queued_work(
            &self,
            _session_id: Option<&str>,
        ) -> Result<Option<bool>, QueuedWorkRunError> {
            self.peeks.fetch_add(1, Ordering::SeqCst);
            Ok(Some(true))
        }

        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.hydrations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn claim_and_run_pending_with_progress(
            &self,
            session_id: Option<&str>,
            reason: &str,
        ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
            self.claim_and_run_pending(session_id, reason).await?;
            Ok(QueuedWorkRunProgress::Blocked)
        }
    }

    #[tokio::test]
    async fn one_notification_during_live_lease_contention_has_bounded_hydrations() {
        let handle = Arc::new(ContendedRunHandle {
            peeks: AtomicUsize::new(0),
            hydrations: AtomicUsize::new(0),
        });
        let driver = QueuedWorkDriver::with_execution_concurrency(handle.clone(), 1)
            .expect("valid concurrency");

        driver.notify_pending_work(Some("session-contended"), "queued_turn_input");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let hydrations = handle.hydrations.load(Ordering::SeqCst);
        assert!(
            (3..=5).contains(&hydrations),
            "one contended notification must back off, got {hydrations} hydrations"
        );
        assert_eq!(handle.peeks.load(Ordering::SeqCst), hydrations);
    }

    mod final_regressions;
    mod slow_wake_telemetry;

    struct FailOnceRunHandle {
        attempts: Arc<AtomicUsize>,
        accepted: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for FailOnceRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(QueuedWorkRunError::transient(PluginError::Session(
                    "transient wake failure".to_string(),
                )));
            }
            self.accepted.notify_one();
            Ok(())
        }
    }

    #[tokio::test]
    async fn best_effort_wake_reenters_pending_claim_without_an_external_event() {
        let handle = Arc::new(FailOnceRunHandle {
            attempts: Arc::new(AtomicUsize::new(0)),
            accepted: tokio::sync::Notify::new(),
        });
        let accepted = handle.accepted.notified();
        let driver = QueuedWorkDriver::new(handle.clone());

        driver.notify_pending_work(Some("session-1"), "queued_turn_input");

        tokio::time::timeout(Duration::from_secs(1), accepted)
            .await
            .expect("the failed wake must retry without another enqueue");
        assert_eq!(handle.attempts.load(Ordering::SeqCst), 2);
    }

    struct AlwaysFailRunHandle {
        attempts: Arc<AtomicUsize>,
        class: QueuedWorkRunErrorClass,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for AlwaysFailRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let error = PluginError::Session("persistent wake failure".to_string());
            Err(match self.class {
                QueuedWorkRunErrorClass::Transient => QueuedWorkRunError::transient(error),
                QueuedWorkRunErrorClass::Terminal => QueuedWorkRunError::terminal(error),
            })
        }
    }

    #[tokio::test]
    async fn terminal_wake_error_stops_after_one_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let driver = QueuedWorkDriver::new(Arc::new(AlwaysFailRunHandle {
            attempts: Arc::clone(&attempts),
            class: QueuedWorkRunErrorClass::Terminal,
        }));

        driver.notify_pending_work(Some("session-terminal"), "queued_turn_input");
        tokio::time::timeout(Duration::from_secs(1), async {
            while attempts.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal wake attempted");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_wake_error_stops_at_the_attempt_limit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let driver = QueuedWorkDriver::new(Arc::new(AlwaysFailRunHandle {
            attempts: Arc::clone(&attempts),
            class: QueuedWorkRunErrorClass::Transient,
        }));

        driver.notify_pending_work(Some("session-exhausted"), "queued_turn_input");
        tokio::time::timeout(Duration::from_secs(5), async {
            while attempts.load(Ordering::SeqCst) < WAKE_MAX_ATTEMPTS as usize {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("transient wake reaches the attempt limit");

        assert_eq!(attempts.load(Ordering::SeqCst), WAKE_MAX_ATTEMPTS as usize);
    }

    struct BlockingRunHandle {
        entered: tokio::sync::Notify,
        dropped: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for BlockingRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.entered.notify_one();
            struct DropProbe(Arc<AtomicBool>);
            impl Drop for DropProbe {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _probe = DropProbe(Arc::clone(&self.dropped));
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn dropping_the_driver_cancels_an_inflight_wake() {
        let dropped = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(BlockingRunHandle {
            entered: tokio::sync::Notify::new(),
            dropped: Arc::clone(&dropped),
        });
        let entered = handle.entered.notified();
        let driver = QueuedWorkDriver::new(handle.clone());
        driver.notify_pending_work(Some("session-shutdown"), "queued_turn_input");
        entered.await;

        drop(driver);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("driver drop must cancel and drop the in-flight wake");
    }
}
