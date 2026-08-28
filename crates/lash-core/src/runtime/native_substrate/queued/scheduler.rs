use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use super::QueuedWorkExecutionConcurrency;
use crate::runtime::worker_capacity::{
    DefaultWorkerSlotSupplier, ObservedWorkerSlotSupplier, WorkerCapacityMetrics,
};
use crate::runtime::{DEFAULT_PROCESS_EXECUTION_CONCURRENCY, WorkerSlotKind, WorkerSlotSupplier};

#[derive(Clone, Debug)]
pub(super) struct QueuedWorkDemand {
    pub(super) session_id: Option<String>,
    pub(super) reasons: Vec<String>,
}

impl QueuedWorkDemand {
    pub(super) fn new(session_id: Option<String>, reason: String) -> Self {
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

#[derive(Default)]
pub(super) struct QueuedWorkExecutionSchedulerState {
    pub(super) pending: VecDeque<QueuedWorkDemand>,
    pub(super) scheduled: BTreeSet<Option<String>>,
    pub(super) rerun: BTreeMap<Option<String>, QueuedWorkDemand>,
    pub(super) active: usize,
    pub(super) dispatcher_running: bool,
}

pub(crate) struct QueuedWorkExecutionScheduler {
    pub(super) slots: Option<Arc<dyn WorkerSlotSupplier>>,
    pub(super) admission_limit: Option<usize>,
    pub(super) metrics: WorkerCapacityMetrics,
    pub(super) state: Mutex<QueuedWorkExecutionSchedulerState>,
    pub(super) changed: Arc<tokio::sync::Notify>,
}

pub(super) struct QueuedWorkExecutionTaskCompletion {
    pub(super) session_id: Option<String>,
    pub(super) completed: tokio::sync::mpsc::UnboundedSender<Option<String>>,
}

impl Drop for QueuedWorkExecutionTaskCompletion {
    fn drop(&mut self) {
        let _ = self.completed.send(self.session_id.clone());
    }
}

pub(super) struct QueuedWorkExecutionDispatcherGuard {
    scheduler: Arc<QueuedWorkExecutionScheduler>,
    armed: bool,
}

impl QueuedWorkExecutionDispatcherGuard {
    pub(super) fn new(scheduler: Arc<QueuedWorkExecutionScheduler>) -> Self {
        Self {
            scheduler,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
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
