use crate::SessionId;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use super::QueuedWorkExecutionConcurrency;
use crate::runtime::coalescing_scheduler::{
    CoalescingDispatcherGuard, CoalescingSchedulerHandle, CoalescingSchedulerState,
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

pub(crate) struct QueuedWorkExecutionScheduler {
    pub(super) slots: Option<Arc<dyn WorkerSlotSupplier>>,
    pub(super) admission_limit: Option<usize>,
    pub(super) metrics: WorkerCapacityMetrics,
    pub(super) state: Mutex<QueuedWorkExecutionSchedulerState>,
    pub(super) changed: Arc<tokio::sync::Notify>,
}

impl CoalescingSchedulerHandle for QueuedWorkExecutionScheduler {
    type Key = Option<SessionId>;
    type Work = QueuedWorkDemand;
    type Extra = ();

    fn state(&self) -> &Mutex<QueuedWorkExecutionSchedulerState> {
        &self.state
    }

    fn changed(&self) -> &Arc<tokio::sync::Notify> {
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
        self.scheduler.complete(&self.session_id, |session_id| {
            tracing::warn!(
                target: "lash_core::queued_work",
                session_id = session_id.as_ref().map(SessionId::as_str),
                event = "queued_work.scheduler_accounting",
                "queued-work execution completed without an active scheduler entry"
            );
        });
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
            state: Mutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: Arc::new(tokio::sync::Notify::new()),
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
            state: Mutex::new(QueuedWorkExecutionSchedulerState::default()),
            changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(super) fn lock_state(
        &self,
    ) -> std::sync::MutexGuard<'_, QueuedWorkExecutionSchedulerState> {
        self.state.lock_recover()
    }

    pub(super) fn available_permits(&self) -> Option<usize> {
        self.slots
            .as_ref()
            .map(|slots| slots.available_slots(WorkerSlotKind::QueuedWork))
    }
}
