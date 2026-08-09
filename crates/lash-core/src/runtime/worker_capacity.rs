use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Semaphore;

/// The independent worker lane requesting admission from a slot supplier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSlotKind {
    /// Inline durable-process execution.
    Process,
    /// Inline queued-work wake execution.
    QueuedWork,
}

impl WorkerSlotKind {
    #[cfg(feature = "otel-trace")]
    pub(crate) const fn attribute_value(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::QueuedWork => "queued_work",
        }
    }
}

/// An admission slot whose owned value is released when this guard is dropped.
///
/// Suppliers put their own RAII token inside with [`Self::new`]. Lash holds the
/// guard for exactly the admitted work interval, including across spawned task
/// boundaries, so completion, cancellation, and panic all release the host
/// token through ordinary unwinding.
#[must_use = "dropping the permit immediately releases the worker slot"]
pub struct WorkerSlotPermit {
    _guard: Box<dyn Any + Send + Sync>,
}

impl WorkerSlotPermit {
    /// Wrap a host-owned RAII token as a worker slot permit.
    pub fn new<T>(guard: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            _guard: Box::new(guard),
        }
    }
}

/// Host-defined admission control for Lash's inline worker lanes.
///
/// The non-blocking method lets a dispatcher reserve capacity before it takes
/// work from its intake buffer. A parked run uses the async method to reacquire
/// its slot before resuming. Implementations must return RAII permits: dropping
/// a permit must release exactly one slot.
///
/// [`Self::reserve_slot`] futures must be cancel-safe: Lash may drop one while
/// shutting down a dispatcher. A supplier panic unwinds only that dispatcher;
/// its running latch is reset so a later drive can recover.
#[async_trait::async_trait]
pub trait WorkerSlotSupplier: Send + Sync {
    /// Wait until one slot for `kind` can be reserved.
    async fn reserve_slot(&self, kind: WorkerSlotKind) -> WorkerSlotPermit;

    /// Reserve one slot without waiting, or leave the work queued.
    fn try_reserve_slot(&self, kind: WorkerSlotKind) -> Option<WorkerSlotPermit>;

    /// Current immediately reservable slots, used for bounded intake and OTel.
    fn available_slots(&self, kind: WorkerSlotKind) -> usize;
}

pub(crate) struct DefaultWorkerSlotSupplier {
    process: Arc<Semaphore>,
    queued_work: Arc<Semaphore>,
}

impl DefaultWorkerSlotSupplier {
    pub(crate) fn new(process: usize, queued_work: usize) -> Self {
        Self {
            process: Arc::new(Semaphore::new(process)),
            queued_work: Arc::new(Semaphore::new(queued_work)),
        }
    }

    fn semaphore(&self, kind: WorkerSlotKind) -> &Arc<Semaphore> {
        match kind {
            WorkerSlotKind::Process => &self.process,
            WorkerSlotKind::QueuedWork => &self.queued_work,
        }
    }
}

#[async_trait::async_trait]
impl WorkerSlotSupplier for DefaultWorkerSlotSupplier {
    async fn reserve_slot(&self, kind: WorkerSlotKind) -> WorkerSlotPermit {
        let permit = Arc::clone(self.semaphore(kind))
            .acquire_owned()
            .await
            .expect("fixed worker slot semaphore remains open");
        WorkerSlotPermit::new(permit)
    }

    fn try_reserve_slot(&self, kind: WorkerSlotKind) -> Option<WorkerSlotPermit> {
        Arc::clone(self.semaphore(kind))
            .try_acquire_owned()
            .ok()
            .map(WorkerSlotPermit::new)
    }

    fn available_slots(&self, kind: WorkerSlotKind) -> usize {
        self.semaphore(kind).available_permits()
    }
}

#[derive(Clone)]
pub(crate) struct WorkerCapacityMetrics {
    #[cfg(feature = "otel-trace")]
    worker_id: Arc<str>,
    #[cfg(feature = "otel-trace")]
    inner: lash_trace::otel::WorkerCapacityMetrics,
}

impl Default for WorkerCapacityMetrics {
    fn default() -> Self {
        Self {
            #[cfg(feature = "otel-trace")]
            worker_id: Arc::from(uuid::Uuid::new_v4().to_string()),
            #[cfg(feature = "otel-trace")]
            inner: lash_trace::otel::WorkerCapacityMetrics::from_global_provider(),
        }
    }
}

impl WorkerCapacityMetrics {
    pub(crate) fn slots(&self, kind: WorkerSlotKind, in_use: usize, available: usize) {
        #[cfg(feature = "otel-trace")]
        self.inner
            .record_slots(&self.worker_id, kind.attribute_value(), in_use, available);
        #[cfg(not(feature = "otel-trace"))]
        let _ = (kind, in_use, available);
    }

    pub(crate) fn intake_depth(&self, kind: WorkerSlotKind, depth: usize) {
        #[cfg(feature = "otel-trace")]
        self.inner
            .record_intake_depth(&self.worker_id, kind.attribute_value(), depth);
        #[cfg(not(feature = "otel-trace"))]
        let _ = (kind, depth);
    }
}

pub(crate) struct ObservedWorkerSlotSupplier {
    inner: Arc<dyn WorkerSlotSupplier>,
    process_in_use: Arc<AtomicUsize>,
    queued_work_in_use: Arc<AtomicUsize>,
    metrics: WorkerCapacityMetrics,
}

impl ObservedWorkerSlotSupplier {
    pub(crate) fn new(
        inner: Arc<dyn WorkerSlotSupplier>,
        metrics: WorkerCapacityMetrics,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            process_in_use: Arc::new(AtomicUsize::new(0)),
            queued_work_in_use: Arc::new(AtomicUsize::new(0)),
            metrics,
        })
    }

    fn in_use(&self, kind: WorkerSlotKind) -> &Arc<AtomicUsize> {
        match kind {
            WorkerSlotKind::Process => &self.process_in_use,
            WorkerSlotKind::QueuedWork => &self.queued_work_in_use,
        }
    }

    fn observe(&self, kind: WorkerSlotKind, permit: WorkerSlotPermit) -> WorkerSlotPermit {
        let in_use_counter = self.in_use(kind);
        let in_use = in_use_counter.fetch_add(1, Ordering::AcqRel) + 1;
        self.metrics
            .slots(kind, in_use, self.inner.available_slots(kind));
        WorkerSlotPermit::new(ObservedWorkerSlotPermit {
            permit: Some(permit),
            supplier: Arc::clone(&self.inner),
            in_use: Arc::clone(in_use_counter),
            metrics: self.metrics.clone(),
            kind,
        })
    }
}

#[async_trait::async_trait]
impl WorkerSlotSupplier for ObservedWorkerSlotSupplier {
    async fn reserve_slot(&self, kind: WorkerSlotKind) -> WorkerSlotPermit {
        let permit = self.inner.reserve_slot(kind).await;
        self.observe(kind, permit)
    }

    fn try_reserve_slot(&self, kind: WorkerSlotKind) -> Option<WorkerSlotPermit> {
        self.inner
            .try_reserve_slot(kind)
            .map(|permit| self.observe(kind, permit))
    }

    fn available_slots(&self, kind: WorkerSlotKind) -> usize {
        self.inner.available_slots(kind)
    }
}

struct ObservedWorkerSlotPermit {
    permit: Option<WorkerSlotPermit>,
    supplier: Arc<dyn WorkerSlotSupplier>,
    in_use: Arc<AtomicUsize>,
    metrics: WorkerCapacityMetrics,
    kind: WorkerSlotKind,
}

impl Drop for ObservedWorkerSlotPermit {
    fn drop(&mut self) {
        drop(self.permit.take());
        let in_use = self.in_use.fetch_sub(1, Ordering::AcqRel) - 1;
        self.metrics
            .slots(self.kind, in_use, self.supplier.available_slots(self.kind));
    }
}
