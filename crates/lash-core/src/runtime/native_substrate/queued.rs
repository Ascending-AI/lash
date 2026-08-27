use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::PluginError;
use crate::runtime::{WorkerSlotKind, WorkerSlotSupplier};

use super::{QueuedWorkSubstrate, SessionDrainOutcome, SessionWorkTarget};

mod scheduler;
mod task;
mod types;

#[cfg(test)]
mod tests;

use scheduler::*;
use task::*;
pub use types::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QueuedWorkExecutionConcurrency(usize);

impl QueuedWorkExecutionConcurrency {
    pub(crate) fn new(concurrency: usize) -> Result<Self, QueuedWorkExecutionConcurrencyError> {
        if !(1..=Semaphore::MAX_PERMITS).contains(&concurrency) {
            return Err(QueuedWorkExecutionConcurrencyError { concurrency });
        }
        Ok(Self(concurrency))
    }

    pub(crate) fn get(self) -> usize {
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

#[derive(Clone)]
pub struct NativeQueuedWork {
    inner: Arc<NativeQueuedWorkInner>,
    _lifetime: Arc<NativeQueuedWorkLifetime>,
}

pub(crate) struct NativeQueuedWorkInner {
    pub(super) run_handle: Arc<dyn QueuedWorkRunHandle>,
    pub(super) shutdown: CancellationToken,
    pub(super) wake_tasks: TaskTracker,
    pub(super) scheduler: Arc<QueuedWorkExecutionScheduler>,
    pub(super) slow_wake_threshold: Duration,
}

pub(crate) struct NativeQueuedWorkLifetime {
    pub(super) shutdown: CancellationToken,
    pub(super) wake_tasks: TaskTracker,
}

impl Drop for NativeQueuedWorkLifetime {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.wake_tasks.close();
    }
}

impl NativeQueuedWork {
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
        supplier: Arc<dyn WorkerSlotSupplier>,
    ) -> Self {
        Self::from_parts_with_supplier(
            run_handle,
            CancellationToken::new(),
            None,
            Some(supplier),
            QUEUED_WORK_SLOW_WAKE_THRESHOLD,
        )
    }

    pub(crate) fn from_parts(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
        concurrency: Option<QueuedWorkExecutionConcurrency>,
        slow_wake_threshold: Duration,
    ) -> Self {
        Self::from_parts_with_supplier(run_handle, shutdown, concurrency, None, slow_wake_threshold)
    }

    pub(crate) fn from_parts_with_supplier(
        run_handle: Arc<dyn QueuedWorkRunHandle>,
        shutdown: CancellationToken,
        concurrency: Option<QueuedWorkExecutionConcurrency>,
        supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        slow_wake_threshold: Duration,
    ) -> Self {
        let shutdown = shutdown.child_token();
        let wake_tasks = TaskTracker::new();
        Self {
            inner: Arc::new(NativeQueuedWorkInner {
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
            _lifetime: Arc::new(NativeQueuedWorkLifetime {
                shutdown,
                wake_tasks,
            }),
        }
    }

    pub(crate) async fn claim_and_run_pending(
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
    pub(crate) fn notify_pending_work(&self, session_id: Option<&str>, reason: &str) {
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
                WorkerSlotKind::QueuedWork,
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

#[async_trait::async_trait]
impl QueuedWorkSubstrate for NativeQueuedWork {
    fn notify_session_work(&self, target: SessionWorkTarget, reason: &str) {
        self.notify_pending_work(target.as_session_id(), reason);
    }

    async fn drain_session_work(
        &self,
        target: SessionWorkTarget,
        reason: &str,
    ) -> Result<SessionDrainOutcome, PluginError> {
        self.claim_and_run_pending(target.as_session_id(), reason)
            .await?;
        Ok(SessionDrainOutcome::Ran)
    }
}
