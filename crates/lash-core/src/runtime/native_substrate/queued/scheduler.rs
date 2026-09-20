use crate::SessionId;
#[cfg(loom)]
use crate::runtime::coalescing_scheduler::loom_ext::LoomMutexExt as _;
#[cfg(not(loom))]
use lash_sansio::sync::MutexExt;
use std::sync::Arc;

use lash_core_ids::execution_permit::SharedNotify;

use super::QueuedWorkExecutionConcurrency;
use crate::runtime::coalescing_scheduler::{
    CoalescingDispatcherGuard, CoalescingMutex, CoalescingSchedulerHandle, CoalescingSchedulerState,
};
use crate::runtime::worker_capacity::{
    DefaultWorkerSlotSupplier, ObservedWorkerSlotSupplier, WorkerCapacityMetrics,
};
use crate::runtime::{DEFAULT_PROCESS_EXECUTION_CONCURRENCY, WorkerSlotKind, WorkerSlotSupplier};

#[derive(Clone, Debug)]
pub(crate) struct QueuedWorkDemand {
    pub(super) session_id: Option<SessionId>,
    pub(super) reasons: Vec<String>,
}

impl QueuedWorkDemand {
    pub(super) fn new(session_id: Option<SessionId>, reason: String) -> Self {
        Self {
            session_id,
            reasons: vec![reason],
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        for reason in other.reasons {
            if !self.reasons.contains(&reason) {
                self.reasons.push(reason);
            }
        }
    }

    pub(super) fn reason(&self) -> String {
        self.reasons.join(",")
    }
}

/// The shared coalescing protocol state; queued work needs no side state.
pub(super) type QueuedWorkExecutionSchedulerState =
    CoalescingSchedulerState<Option<SessionId>, QueuedWorkDemand>;

/// The guard [`QueuedWorkExecutionScheduler::lock_state`] hands out. Under
/// `--cfg loom` the state mutex is loom's, so the guard is too (FIG-1161).
#[cfg(not(loom))]
pub(super) type QueuedSchedulerStateGuard<'a> =
    std::sync::MutexGuard<'a, QueuedWorkExecutionSchedulerState>;
/// `cfg(loom)` twin of [`QueuedSchedulerStateGuard`].
#[cfg(loom)]
pub(super) type QueuedSchedulerStateGuard<'a> =
    loom::sync::MutexGuard<'a, QueuedWorkExecutionSchedulerState>;

pub(crate) struct QueuedWorkExecutionScheduler {
    pub(super) slots: Option<Arc<dyn WorkerSlotSupplier>>,
    pub(super) admission_limit: Option<usize>,
    pub(super) metrics: WorkerCapacityMetrics,
    pub(super) state: CoalescingMutex<QueuedWorkExecutionSchedulerState>,
    pub(super) changed: Arc<SharedNotify>,
}

impl CoalescingSchedulerHandle for QueuedWorkExecutionScheduler {
    type Key = Option<SessionId>;
    type Work = QueuedWorkDemand;
    type Extra = ();

    fn state(&self) -> &CoalescingMutex<QueuedWorkExecutionSchedulerState> {
        &self.state
    }

    fn changed(&self) -> &Arc<SharedNotify> {
        &self.changed
    }

    fn slot_kind(&self) -> WorkerSlotKind {
        WorkerSlotKind::QueuedWork
    }

    fn metrics(&self) -> &WorkerCapacityMetrics {
        &self.metrics
    }
}

pub(super) struct QueuedWorkExecutionTaskCompletion {
    pub(super) session_id: Option<SessionId>,
    pub(super) scheduler: Arc<QueuedWorkExecutionScheduler>,
}

impl Drop for QueuedWorkExecutionTaskCompletion {
    fn drop(&mut self) {
        // A completion with no running entry cannot be produced by the
        // protocol: a task exists only for a popped (running) demand.
        self.scheduler.complete(&self.session_id);
    }
}

pub(super) type QueuedWorkExecutionDispatcherGuard =
    CoalescingDispatcherGuard<QueuedWorkExecutionScheduler>;

impl QueuedWorkExecutionScheduler {
    pub(super) fn unbounded() -> Self {
        Self {
            slots: None,
            admission_limit: None,
            metrics: WorkerCapacityMetrics::default(),
            state: CoalescingMutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: Arc::new(SharedNotify::new()),
        }
    }

    pub(super) fn native(concurrency: QueuedWorkExecutionConcurrency) -> Self {
        let supplier = Arc::new(DefaultWorkerSlotSupplier::new(
            DEFAULT_PROCESS_EXECUTION_CONCURRENCY,
            concurrency.get(),
        ));
        Self::with_supplier(supplier, Some(concurrency.get()))
    }

    pub(super) fn with_supplier(
        supplier: Arc<dyn WorkerSlotSupplier>,
        admission_limit: Option<usize>,
    ) -> Self {
        let metrics = WorkerCapacityMetrics::default();
        let slots = ObservedWorkerSlotSupplier::new(supplier, metrics.clone());
        metrics.slots(
            WorkerSlotKind::QueuedWork,
            0,
            slots.available_slots(WorkerSlotKind::QueuedWork),
        );
        metrics.intake_depth(WorkerSlotKind::QueuedWork, 0);
        Self {
            slots: Some(slots),
            admission_limit,
            metrics,
            state: CoalescingMutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: Arc::new(SharedNotify::new()),
        }
    }

    pub(super) fn lock_state(&self) -> QueuedSchedulerStateGuard<'_> {
        self.state.lock_recover()
    }

    pub(super) fn available_permits(&self) -> Option<usize> {
        self.slots
            .as_ref()
            .map(|slots| slots.available_slots(WorkerSlotKind::QueuedWork))
    }
}
