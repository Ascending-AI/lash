use std::future::Future;

use super::*;
use crate::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};

/// Permit owned by one inline process execution. All clones refer to the same
/// slot so child-turn and inline-effect task boundaries can park the outer run.
///
/// This type assumes one logical thread of execution per process run. Clones
/// may move that one thread across task boundaries, but must not be awaited by
/// concurrent branches: while one branch has released the slot, a second
/// branch would observe no held permit and could resume without reacquiring it.
/// Intra-run parallel execution must replace this shared-slot protocol before
/// it is introduced.
pub(super) struct ProcessExecutionPermit {
    supplier: Arc<dyn WorkerSlotSupplier>,
    kind: WorkerSlotKind,
    held: std::sync::Mutex<Option<WorkerSlotPermit>>,
    reacquire: tokio::sync::Mutex<()>,
    dispatcher_changed: Arc<tokio::sync::Notify>,
    telemetry: ExecutionPermitTelemetry,
}

#[derive(Clone, Copy)]
struct ExecutionPermitTelemetry {
    reacquire_event: &'static str,
    supplier: &'static str,
}

const PROCESS_EXECUTION_PERMIT_TELEMETRY: ExecutionPermitTelemetry = ExecutionPermitTelemetry {
    reacquire_event: "process_execution_permit.reacquire",
    supplier: "process_worker_slot_supplier",
};

const QUEUED_WORK_EXECUTION_PERMIT_TELEMETRY: ExecutionPermitTelemetry = ExecutionPermitTelemetry {
    reacquire_event: "queued_work_execution_permit.reacquire",
    supplier: "queued_work_slot_supplier",
};

impl ProcessExecutionPermit {
    pub(super) fn new(
        supplier: Arc<dyn WorkerSlotSupplier>,
        permit: WorkerSlotPermit,
        dispatcher_changed: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self::new_with_telemetry(
            supplier,
            WorkerSlotKind::Process,
            permit,
            dispatcher_changed,
            PROCESS_EXECUTION_PERMIT_TELEMETRY,
        )
    }

    fn new_with_telemetry(
        supplier: Arc<dyn WorkerSlotSupplier>,
        kind: WorkerSlotKind,
        permit: WorkerSlotPermit,
        dispatcher_changed: Arc<tokio::sync::Notify>,
        telemetry: ExecutionPermitTelemetry,
    ) -> Self {
        Self {
            supplier,
            kind,
            held: std::sync::Mutex::new(Some(permit)),
            reacquire: tokio::sync::Mutex::new(()),
            dispatcher_changed,
            telemetry,
        }
    }

    async fn ensure_acquired(&self) {
        if self.held.lock_recover().is_some() {
            tracing::debug!(
                consulted = "held_permit",
                gate = "fast_path",
                outcome = "already_held",
                event = self.telemetry.reacquire_event,
                "execution permit is already held; the run resumes without waiting"
            );
            return;
        }
        let _reacquire = self.reacquire.lock().await;
        if self.held.lock_recover().is_some() {
            tracing::debug!(
                consulted = "held_permit",
                gate = "reacquire_serialization",
                outcome = "already_held",
                event = self.telemetry.reacquire_event,
                "another branch of this run reacquired the permit while this one waited"
            );
            return;
        }
        let permit = match self.supplier.try_reserve_slot(self.kind) {
            Some(permit) => {
                tracing::debug!(
                    available_permits = self.supplier.available_slots(self.kind),
                    consulted = self.telemetry.supplier,
                    outcome = "immediate",
                    event = self.telemetry.reacquire_event,
                    "reacquired the execution permit without waiting"
                );
                permit
            }
            None => {
                tracing::debug!(
                    available_permits = 0,
                    consulted = self.telemetry.supplier,
                    outcome = "waiting",
                    event = self.telemetry.reacquire_event,
                    "waiting for an execution permit before resuming the run"
                );
                self.supplier.reserve_slot(self.kind).await
            }
        };
        *self.held.lock_recover() = Some(permit);
        tracing::debug!(
            consulted = self.telemetry.supplier,
            outcome = "held",
            event = self.telemetry.reacquire_event,
            "reacquired the execution permit"
        );
    }

    async fn release_while<F: Future>(&self, future: F) -> F::Output {
        let released = self.held.lock_recover().take();
        let Some(released) = released else {
            return future.await;
        };
        drop(released);
        self.dispatcher_changed.notify_one();
        let output = future.await;
        self.ensure_acquired().await;
        output
    }
}

tokio::task_local! {
    pub(super) static PROCESS_EXECUTION_PERMIT: Arc<ProcessExecutionPermit>;
}

pub(crate) async fn scope_process_execution_permit<F: Future>(
    supplier: Arc<dyn WorkerSlotSupplier>,
    permit: WorkerSlotPermit,
    dispatcher_changed: Arc<tokio::sync::Notify>,
    future: F,
) -> F::Output {
    let permit = Arc::new(ProcessExecutionPermit::new(
        supplier,
        permit,
        dispatcher_changed,
    ));
    PROCESS_EXECUTION_PERMIT.scope(permit, future).await
}

pub(crate) async fn scope_queued_work_execution_permit<F: Future>(
    supplier: Arc<dyn WorkerSlotSupplier>,
    permit: WorkerSlotPermit,
    dispatcher_changed: Arc<tokio::sync::Notify>,
    future: F,
) -> F::Output {
    let permit = Arc::new(ProcessExecutionPermit::new_with_telemetry(
        supplier,
        WorkerSlotKind::QueuedWork,
        permit,
        dispatcher_changed,
        QUEUED_WORK_EXECUTION_PERMIT_TELEMETRY,
    ));
    PROCESS_EXECUTION_PERMIT.scope(permit, future).await
}

#[doc(hidden)]
pub async fn release_process_execution_permit_while<F: Future>(future: F) -> F::Output {
    let permit = PROCESS_EXECUTION_PERMIT.try_with(Arc::clone).ok();
    match permit {
        Some(permit) => permit.release_while(future).await,
        None => future.await,
    }
}

pub(crate) async fn ensure_process_execution_permit() {
    if let Ok(permit) = PROCESS_EXECUTION_PERMIT.try_with(Arc::clone) {
        permit.ensure_acquired().await;
    }
}

pub(crate) fn inherit_process_execution_permit<F: Future>(
    future: F,
) -> impl Future<Output = F::Output> {
    let permit = PROCESS_EXECUTION_PERMIT.try_with(Arc::clone).ok();
    async move {
        match permit {
            Some(permit) => PROCESS_EXECUTION_PERMIT.scope(permit, future).await,
            None => future.await,
        }
    }
}
