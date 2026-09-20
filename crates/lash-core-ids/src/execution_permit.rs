use std::future::Future;
use std::sync::Arc;

#[cfg(not(loom))]
use lash_sansio::sync::MutexExt;

use crate::worker_capacity::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};

/// Under `--cfg loom` the permit's locks and notifier swap to loom-instrumented
/// equivalents so the release/reacquire interleavings are model-checked
/// (FIG-1161). A normal build keeps the production types.
#[cfg(not(loom))]
type StdMutex<T> = std::sync::Mutex<T>;
#[cfg(loom)]
type StdMutex<T> = loom::sync::Mutex<T>;

/// The serializer behind [`ProcessExecutionPermit::ensure_acquired`]'s slow
/// path. Normally `tokio::sync::Mutex` (async-aware); under loom a loom mutex
/// provides the same exclusion.
#[cfg(not(loom))]
type ReacquireMutex = tokio::sync::Mutex<()>;
#[cfg(loom)]
type ReacquireMutex = loom::sync::Mutex<()>;

/// The dispatcher-changed notifier shared with the schedulers' `changed`
/// handle. Under loom it is the crate-local `loom_notify` shim so
/// `notify_one`/`notified` interleavings land inside the model.
#[cfg(not(loom))]
pub type SharedNotify = tokio::sync::Notify;
/// `cfg(loom)` twin of [`SharedNotify`]; public because the type crosses the
/// `lash-core`/`lash-core-worker` boundary in scheduler signatures.
#[cfg(loom)]
pub type SharedNotify = crate::loom_notify::Notify;

/// `lock_recover` for the loom mutex. Keeping the method name lets the code
/// below read identically under both cfgs; `lash_sansio`'s `MutexExt` still
/// covers any real `std::sync::Mutex` in scope.
#[cfg(loom)]
mod loom_ext {
    pub(crate) trait LoomMutexExt<T: ?Sized> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T>;
    }

    impl<T: ?Sized> LoomMutexExt<T> for loom::sync::Mutex<T> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T> {
            self.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }
}

#[cfg(loom)]
use loom_ext::LoomMutexExt as _;

/// Permit owned by one native process execution. All clones refer to the same
/// slot so child-turn and native-effect task boundaries can park the outer run.
///
/// This type assumes one logical thread of execution per process run. Clones
/// may move that one thread across task boundaries, but must not be awaited by
/// concurrent branches: while one branch has released the slot, a second
/// branch would observe no held permit and could resume without reacquiring it.
/// Intra-run parallel execution must replace this shared-slot protocol before
/// it is introduced.
pub struct ProcessExecutionPermit {
    supplier: Arc<dyn WorkerSlotSupplier>,
    kind: WorkerSlotKind,
    held: StdMutex<Option<WorkerSlotPermit>>,
    reacquire: ReacquireMutex,
    dispatcher_changed: Arc<SharedNotify>,
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
    pub fn new(
        supplier: Arc<dyn WorkerSlotSupplier>,
        permit: WorkerSlotPermit,
        dispatcher_changed: Arc<SharedNotify>,
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
        dispatcher_changed: Arc<SharedNotify>,
        telemetry: ExecutionPermitTelemetry,
    ) -> Self {
        Self {
            supplier,
            kind,
            held: StdMutex::new(Some(permit)),
            reacquire: ReacquireMutex::new(()),
            dispatcher_changed,
            telemetry,
        }
    }

    /// Serialize the slow-path reacquisition. A `tokio` mutex normally; under
    /// loom a loom mutex provides the same exclusion inside the model.
    #[cfg(not(loom))]
    async fn reacquire_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reacquire.lock().await
    }

    /// `cfg(loom)` twin of [`Self::reacquire_lock`].
    #[cfg(loom)]
    async fn reacquire_lock(&self) -> loom::sync::MutexGuard<'_, ()> {
        self.reacquire.lock_recover()
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
        let _reacquire = self.reacquire_lock().await;
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
    pub static PROCESS_EXECUTION_PERMIT: Arc<ProcessExecutionPermit>;
}

pub async fn scope_process_execution_permit<F: Future>(
    supplier: Arc<dyn WorkerSlotSupplier>,
    permit: WorkerSlotPermit,
    dispatcher_changed: Arc<SharedNotify>,
    future: F,
) -> F::Output {
    let permit = Arc::new(ProcessExecutionPermit::new(
        supplier,
        permit,
        dispatcher_changed,
    ));
    PROCESS_EXECUTION_PERMIT.scope(permit, future).await
}

pub async fn scope_queued_work_execution_permit<F: Future>(
    supplier: Arc<dyn WorkerSlotSupplier>,
    permit: WorkerSlotPermit,
    dispatcher_changed: Arc<SharedNotify>,
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

pub async fn release_process_execution_permit_while<F: Future>(future: F) -> F::Output {
    let permit = PROCESS_EXECUTION_PERMIT.try_with(Arc::clone).ok();
    match permit {
        Some(permit) => permit.release_while(future).await,
        None => future.await,
    }
}

pub async fn ensure_process_execution_permit() {
    if let Ok(permit) = PROCESS_EXECUTION_PERMIT.try_with(Arc::clone) {
        permit.ensure_acquired().await;
    }
}

pub fn inherit_process_execution_permit<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let permit = PROCESS_EXECUTION_PERMIT.try_with(Arc::clone).ok();
    async move {
        match permit {
            Some(permit) => PROCESS_EXECUTION_PERMIT.scope(permit, future).await,
            None => future.await,
        }
    }
}

/// Loom model checks for the release/reacquire race in `ensure_acquired`
/// (FIG-1161 seam 1). `LoomSupplier` is a single-slot pool on loom
/// primitives: a dropped `SlotGuard` returns the slot through a loom condvar,
/// so the blocking `reserve_slot` path interleaves like the real supplier's
/// wait.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::{Condvar, Mutex as LoomMutex};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SlotPool {
        free: LoomMutex<usize>,
        freed: Condvar,
        reservations: AtomicUsize,
    }

    struct SlotGuard(Arc<SlotPool>);

    impl Drop for SlotGuard {
        fn drop(&mut self) {
            *self.0.free.lock_recover() += 1;
            self.0.freed.notify_one();
        }
    }

    struct LoomSupplier(Arc<SlotPool>);

    impl LoomSupplier {
        fn new(slots: usize) -> Self {
            Self(Arc::new(SlotPool {
                free: LoomMutex::new(slots),
                freed: Condvar::new(),
                reservations: AtomicUsize::new(0),
            }))
        }

        fn reservations(&self) -> usize {
            self.0.reservations.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl WorkerSlotSupplier for LoomSupplier {
        async fn reserve_slot(&self, _kind: WorkerSlotKind) -> WorkerSlotPermit {
            let mut free = self.0.free.lock_recover();
            while *free == 0 {
                free = self
                    .0
                    .freed
                    .wait(free)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            *free -= 1;
            self.0.reservations.fetch_add(1, Ordering::SeqCst);
            WorkerSlotPermit::new(SlotGuard(Arc::clone(&self.0)))
        }

        fn try_reserve_slot(&self, _kind: WorkerSlotKind) -> Option<WorkerSlotPermit> {
            let mut free = self.0.free.lock_recover();
            if *free == 0 {
                return None;
            }
            *free -= 1;
            self.0.reservations.fetch_add(1, Ordering::SeqCst);
            Some(WorkerSlotPermit::new(SlotGuard(Arc::clone(&self.0))))
        }

        fn available_slots(&self, _kind: WorkerSlotKind) -> usize {
            *self.0.free.lock_recover()
        }
    }

    fn loom_permit(supplier: &Arc<LoomSupplier>) -> Arc<ProcessExecutionPermit> {
        Arc::new(ProcessExecutionPermit::new(
            Arc::clone(supplier) as Arc<dyn WorkerSlotSupplier>,
            WorkerSlotPermit::new(()),
            Arc::new(SharedNotify::new()),
        ))
    }

    /// Two `ensure_acquired` racers on a released permit: the `reacquire`
    /// serializer must make exactly one of them reserve a fresh slot; the
    /// loser must observe the winner's install on the second `held` check.
    #[test]
    fn ensure_acquired_racers_reserve_exactly_once() {
        loom::model(|| {
            let supplier = Arc::new(LoomSupplier::new(1));
            let permit = loom_permit(&supplier);
            // Enter the released state without touching the supplier.
            permit.held.lock_recover().take();

            let other = Arc::clone(&permit);
            let racer = loom::thread::spawn(move || {
                loom::future::block_on(other.ensure_acquired());
            });
            loom::future::block_on(permit.ensure_acquired());
            racer.join().expect("racer thread panicked");

            assert!(permit.held.lock_recover().is_some());
            assert_eq!(
                supplier.reservations(),
                1,
                "both racers must observe the same acquisition"
            );
        });
    }

    /// `release_while` drops the held slot, wakes the dispatcher, runs the
    /// inner future, then reacquires — racing a second `ensure_acquired`.
    /// Whichever ordering loom explores, the supplier must see the initial
    /// reservation plus exactly one reacquisition, and the permit must end
    /// held.
    #[test]
    fn release_while_racing_ensure_acquired_leaves_one_slot_held() {
        loom::model(|| {
            let supplier = Arc::new(LoomSupplier::new(1));
            let initial = supplier
                .try_reserve_slot(WorkerSlotKind::Process)
                .expect("pool starts with a free slot");
            let permit = Arc::new(ProcessExecutionPermit::new(
                Arc::clone(&supplier) as Arc<dyn WorkerSlotSupplier>,
                initial,
                Arc::new(SharedNotify::new()),
            ));

            let other = Arc::clone(&permit);
            let releaser = loom::thread::spawn(move || {
                loom::future::block_on(other.release_while(std::future::ready(())))
            });
            loom::future::block_on(permit.ensure_acquired());
            releaser.join().expect("releaser thread panicked");

            assert!(permit.held.lock_recover().is_some());
            assert_eq!(
                supplier.reservations(),
                2,
                "initial reservation plus exactly one reacquisition"
            );
            assert_eq!(supplier.available_slots(WorkerSlotKind::Process), 0);
        });
    }
}
