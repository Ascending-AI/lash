use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::PluginError;

const WAKE_RETRY_INITIAL: Duration = Duration::from_millis(25);
const WAKE_RETRY_MAX: Duration = Duration::from_secs(1);
const WAKE_MAX_ATTEMPTS: u32 = 8;

/// Default maximum number of queued-work wake executions admitted at once.
pub const DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY: usize = 64;

/// Elapsed time after which an unfinished queued-work wake emits warning telemetry.
pub const QUEUED_WORK_SLOW_WAKE_THRESHOLD: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueuedWorkExecutionConcurrency(usize);

impl QueuedWorkExecutionConcurrency {
    const DEFAULT: Self = Self(DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY);

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
        Self::terminal(error)
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
    permits: Arc<Semaphore>,
    state: Mutex<QueuedWorkExecutionSchedulerState>,
    changed: tokio::sync::Notify,
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
            self.scheduler
                .state
                .lock()
                .expect("lock queued-work scheduler")
                .dispatcher_running = false;
            self.scheduler.changed.notify_one();
        }
    }
}

impl QueuedWorkExecutionScheduler {
    fn new(concurrency: QueuedWorkExecutionConcurrency) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(concurrency.get())),
            state: Mutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: tokio::sync::Notify::new(),
        }
    }
}

impl QueuedWorkDriver {
    pub fn validate_execution_concurrency(
        concurrency: usize,
    ) -> Result<(), QueuedWorkExecutionConcurrencyError> {
        QueuedWorkExecutionConcurrency::new(concurrency).map(drop)
    }

    pub fn new(run_handle: Arc<dyn QueuedWorkRunHandle>) -> Self {
        Self::with_shutdown_token(run_handle, CancellationToken::new())
    }

    /// Construct a driver with a host-selected admission bound.
    pub fn with_execution_concurrency(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        concurrency: usize,
    ) -> Result<Self, QueuedWorkExecutionConcurrencyError> {
        Ok(Self::from_parts(
            run_handle,
            CancellationToken::new(),
            QueuedWorkExecutionConcurrency::new(concurrency)?,
            QUEUED_WORK_SLOW_WAKE_THRESHOLD,
        ))
    }

    pub fn with_shutdown_token(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
    ) -> Self {
        Self::from_parts(
            run_handle,
            shutdown,
            QueuedWorkExecutionConcurrency::DEFAULT,
            QUEUED_WORK_SLOW_WAKE_THRESHOLD,
        )
    }

    fn from_parts(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
        concurrency: QueuedWorkExecutionConcurrency,
        slow_wake_threshold: Duration,
    ) -> Self {
        let shutdown = shutdown.child_token();
        let wake_tasks = TaskTracker::new();
        Self {
            inner: Arc::new(QueuedWorkDriverInner {
                run_handle,
                shutdown: shutdown.clone(),
                wake_tasks: wake_tasks.clone(),
                scheduler: Arc::new(QueuedWorkExecutionScheduler::new(concurrency)),
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
            let mut state = self
                .inner
                .scheduler
                .state
                .lock()
                .expect("lock queued-work scheduler");
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
    Recheck,
}

impl QueuedWorkTaskDriver {
    async fn run_dispatcher(&self) {
        let mut dispatcher_guard =
            QueuedWorkExecutionDispatcherGuard::new(Arc::clone(&self.inner.scheduler));
        let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
        loop {
            while let Some((demand, permit)) = self.next_execution() {
                let driver = Self {
                    inner: Arc::clone(&self.inner),
                };
                let completion = QueuedWorkExecutionTaskCompletion {
                    session_id: demand.session_id.clone(),
                    completed: completed_tx.clone(),
                };
                self.inner.wake_tasks.spawn(async move {
                    let _completion = completion;
                    let _permit = permit;
                    driver.run_demand(demand).await;
                });
            }

            {
                let mut state = self
                    .inner
                    .scheduler
                    .state
                    .lock()
                    .expect("lock queued-work scheduler");
                if state.pending.is_empty() && state.active == 0 {
                    state.dispatcher_running = false;
                    dispatcher_guard.disarm();
                    return;
                }
            }

            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                Some(session_id) = completed_rx.recv() => {
                    let mut state = self
                        .inner
                        .scheduler
                        .state
                        .lock()
                        .expect("lock queued-work scheduler");
                    state.active = state
                        .active
                        .checked_sub(1)
                        .expect("a completed queued-work execution was active");
                    if let Some(demand) = state.rerun.remove(&session_id) {
                        state.pending.push_back(demand);
                    } else {
                        state.scheduled.remove(&session_id);
                    }
                }
                () = self.inner.scheduler.changed.notified() => {}
            }
        }
    }

    fn next_execution(&self) -> Option<(QueuedWorkDemand, OwnedSemaphorePermit)> {
        let mut state = self
            .inner
            .scheduler
            .state
            .lock()
            .expect("lock queued-work scheduler");
        let permit = Arc::clone(&self.inner.scheduler.permits)
            .try_acquire_owned()
            .ok()?;
        let mut demand = state.pending.pop_front()?;
        if let Some(coalesced) = state.rerun.remove(&demand.session_id) {
            demand.merge(coalesced);
        }
        state.active += 1;
        Some((demand, permit))
    }

    async fn run_demand(&self, demand: QueuedWorkDemand) {
        let reason = demand.reason();
        let mut attempt = 1_u32;
        let mut retry_after = WAKE_RETRY_INITIAL;
        loop {
            let result = self.run_attempt(&demand, &reason, attempt).await;
            match result {
                None => return,
                Some(Ok(
                    QueuedWorkRunAttemptOutcome::Idle | QueuedWorkRunAttemptOutcome::Complete,
                )) => {
                    return;
                }
                Some(Ok(QueuedWorkRunAttemptOutcome::Recheck)) => {
                    attempt = 1;
                    retry_after = WAKE_RETRY_INITIAL;
                    continue;
                }
                Some(Err(err)) => {
                    let disposition = match err.class {
                        QueuedWorkRunErrorClass::Terminal => QueuedWorkWakeDisposition::Terminal,
                        QueuedWorkRunErrorClass::Transient if attempt >= WAKE_MAX_ATTEMPTS => {
                            QueuedWorkWakeDisposition::Exhausted
                        }
                        QueuedWorkRunErrorClass::Transient => QueuedWorkWakeDisposition::Retrying,
                    };
                    let failure = QueuedWorkWakeFailure {
                        session_id: demand.session_id.clone(),
                        reason: reason.clone(),
                        attempt,
                        retry_after_ms: if matches!(
                            disposition,
                            QueuedWorkWakeDisposition::Retrying
                        ) {
                            retry_after.as_millis() as u64
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
                }
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                () = tokio::time::sleep(retry_after) => {}
            }
            retry_after = retry_after.saturating_mul(2).min(WAKE_RETRY_MAX);
            attempt = attempt.saturating_add(1);
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
            run_handle
                .claim_and_run_pending(session_id.as_deref(), reason)
                .await?;
            Ok(if claimable.is_some() {
                QueuedWorkRunAttemptOutcome::Recheck
            } else {
                QueuedWorkRunAttemptOutcome::Complete
            })
        };
        tokio::pin!(run);
        let slow = tokio::time::sleep(self.inner.slow_wake_threshold);
        tokio::pin!(slow);
        tokio::select! {
            () = self.inner.shutdown.cancelled() => None,
            result = &mut run => Some(result),
            () = &mut slow => {
                let event = QueuedWorkSlowWake {
                    session_id: demand.session_id.clone(),
                    reason: reason.to_string(),
                    attempt,
                    threshold_ms: self.inner.slow_wake_threshold.as_millis() as u64,
                };
                tracing::warn!(
                    target: "lash_core::queued_work",
                    session_id = event.session_id.as_deref(),
                    reason = %event.reason,
                    attempt = event.attempt,
                    threshold_ms = event.threshold_ms,
                    event = "queued_work.wake_slow",
                    "queued-work wake remains unfinished past the slow-wake threshold"
                );
                tokio::select! {
                    () = self.inner.shutdown.cancelled() => None,
                    result = &mut run => Some(result),
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
            Ok(Some(!self.pending.lock().expect("lock pending").is_empty()))
        }

        async fn run_queued_work(
            &self,
            request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.hydrations.fetch_add(1, Ordering::SeqCst);
            self.reasons
                .lock()
                .expect("lock reasons")
                .push(request.reason);
            let drained = {
                let mut pending = self.pending.lock().expect("lock pending");
                let limit = pending.len().min(3);
                pending.drain(..limit).collect::<Vec<_>>()
            };
            self.observed.lock().expect("lock observed").extend(drained);
            self.completed.notify_one();
            Ok(())
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
                if handle.observed.lock().expect("lock observed").len() == SIGNALS {
                    break;
                }
                completed.await;
            }
        })
        .await
        .expect("all queued work drains in bounded batches");
        assert_eq!(handle.hydrations.load(Ordering::SeqCst), 3);
        assert_eq!(
            *handle.observed.lock().expect("lock observed"),
            (0..SIGNALS).collect::<Vec<_>>()
        );
        assert_eq!(
            *handle.reasons.lock().expect("lock reasons"),
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
    async fn admission_bound_limits_concurrent_executions_without_queued_tasks() {
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

    struct RerunRunHandle {
        pending: Mutex<VecDeque<usize>>,
        observed: Mutex<Vec<usize>>,
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
            Ok(Some(!self.pending.lock().expect("lock pending").is_empty()))
        }

        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            let run = self.runs.fetch_add(1, Ordering::SeqCst);
            let drained = self
                .pending
                .lock()
                .expect("lock pending")
                .drain(..)
                .collect::<Vec<_>>();
            self.observed.lock().expect("lock observed").extend(drained);
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
            runs: AtomicUsize::new(0),
            first_entered: tokio::sync::Notify::new(),
            release_first: tokio::sync::Semaphore::new(0),
            completed: tokio::sync::Notify::new(),
        });
        let driver = QueuedWorkDriver::new(handle.clone());
        driver.notify_pending_work(Some("session-rerun"), "first");
        handle.first_entered.notified().await;

        handle.pending.lock().expect("lock pending").push_back(1);
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
        assert_eq!(*handle.observed.lock().expect("lock observed"), vec![0, 1]);
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

    struct HungRunHandle {
        entered: AtomicUsize,
        dropped: Arc<AtomicUsize>,
        changed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueuedWorkRunHandle for HungRunHandle {
        async fn run_queued_work(
            &self,
            _request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            struct DropProbe(Arc<AtomicUsize>);
            impl Drop for DropProbe {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            let _probe = DropProbe(Arc::clone(&self.dropped));
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_waiters();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn hung_executions_are_bounded_warn_when_slow_and_shutdown_cleanly() {
        const CONCURRENCY: usize = 2;
        const SIGNALS: usize = 8;
        let handle = Arc::new(HungRunHandle {
            entered: AtomicUsize::new(0),
            dropped: Arc::new(AtomicUsize::new(0)),
            changed: tokio::sync::Notify::new(),
        });
        let captured_handle = Arc::clone(&handle);
        let ((), capture) = crate::runtime::tests::trace_capture::capturing(|| async move {
            let driver = QueuedWorkDriver::from_parts(
                captured_handle.clone(),
                CancellationToken::new(),
                QueuedWorkExecutionConcurrency::new(CONCURRENCY).expect("valid concurrency"),
                Duration::from_millis(10),
            );
            for index in 0..SIGNALS {
                driver.notify_pending_work(Some(&format!("hung-{index}")), "process_wake");
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let changed = captured_handle.changed.notified();
                    if captured_handle.entered.load(Ordering::SeqCst) == CONCURRENCY {
                        break;
                    }
                    changed.await;
                }
            })
            .await
            .expect("only the bounded executions enter");
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(captured_handle.entered.load(Ordering::SeqCst), CONCURRENCY);
            drop(driver);
            tokio::time::timeout(Duration::from_secs(1), async {
                while captured_handle.dropped.load(Ordering::SeqCst) < CONCURRENCY {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("shutdown drops every admitted hung execution");
        })
        .await;

        let slow = capture.named("queued_work.wake_slow");
        assert_eq!(slow.len(), CONCURRENCY);
        assert!(slow.iter().all(|event| {
            event.level == "WARN"
                && event.target == "lash_core::queued_work"
                && event.field("threshold_ms") == "10"
                && event.field("reason") == "process_wake"
        }));
    }
}
