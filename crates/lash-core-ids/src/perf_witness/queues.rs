//! Opt-in, bounded physical-operation timing. All callers compile out with
//! `perf-witness`; an inactive witness performs no clock reads or allocation.
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::AlreadyInstalled;

/// Configured maximum retained records per process/window, not a measurement.
pub const RECORD_LIMIT: usize = 8192;
static ENABLED: AtomicBool = AtomicBool::new(false);
static CURRENT: LazyLock<Mutex<Option<Arc<Recording>>>> = LazyLock::new(|| Mutex::new(None));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Copy)]
struct WorkerContext {
    operation: u64,
    window: u64,
}
struct Recording {
    window: u64,
    thread: Option<std::thread::ThreadId>,
    snapshot: Mutex<Snapshot>,
}

thread_local! {
    static WORKER: Cell<(Option<WorkerContext>, Duration)> = const { Cell::new((None, Duration::ZERO)) };
}

/// Durations are single physical-operation samples in nanoseconds. Gate records
/// are children of worker records and overlap them; never sum parent and child.
#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub operation_id: u64,
    pub parent_operation_id: Option<u64>,
    pub site: &'static str,
    pub queue_wait_ns: u64,
    pub service_ns: u64,
    pub elapsed_ns: u64,
    /// False includes a cancelled admission or a panicking worker.
    pub completed: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub records: Vec<Record>,
    pub dropped_records: u64,
}

/// Exclusive process-scoped window; timers retain their original window so
/// accepted work that outlives this guard cannot contaminate the next one.
pub struct Collector(Arc<Recording>);
impl Collector {
    pub fn install() -> Result<Self, AlreadyInstalled> {
        Self::install_scoped(None)
    }

    /// Restrict retained records to one execution thread, for an isolated
    /// connection-worker diagnostic beside unrelated work in the same process.
    pub fn install_for_thread(thread: std::thread::ThreadId) -> Result<Self, AlreadyInstalled> {
        Self::install_scoped(Some(thread))
    }

    fn install_scoped(thread: Option<std::thread::ThreadId>) -> Result<Self, AlreadyInstalled> {
        let mut current = CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_some() {
            return Err(AlreadyInstalled);
        }
        let data = Arc::new(Recording {
            window: NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed),
            thread,
            snapshot: Mutex::new(Snapshot {
                records: Vec::with_capacity(RECORD_LIMIT),
                dropped_records: 0,
            }),
        });
        *current = Some(Arc::clone(&data));
        ENABLED.store(true, Ordering::Release);
        Ok(Self(data))
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Release);
        CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// Enqueue-to-service timer; dropping it also retains partial waits.
pub struct Timer {
    data: Arc<Recording>,
    id: u64,
    parent: Option<u64>,
    site: &'static str,
    enqueued: Instant,
    started: Option<Instant>,
    nested_wait: Duration,
    completed: bool,
}
impl Timer {
    pub fn enqueue(site: &'static str) -> Option<Self> {
        if !ENABLED.load(Ordering::Acquire) {
            return None;
        }
        let data = CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let context = WORKER.with(|worker| worker.get().0);
        // A gate belongs to its accepted worker's window. If that window
        // closed, suppress its child rather than assigning it to a new one.
        if context.is_some_and(|context| context.window != data.window) {
            return None;
        }
        Some(Self {
            data,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            parent: context.map(|context| context.operation),
            site,
            enqueued: Instant::now(),
            started: None,
            nested_wait: Duration::ZERO,
            completed: false,
        })
    }

    pub fn start_service(&mut self) {
        self.started = Some(Instant::now());
    }

    /// Enter only on the SQLite connection thread. Children report gate waits
    /// to this scope so a parent's service excludes those waits exactly once.
    pub fn worker_scope(&mut self) -> WorkerScope<'_> {
        let previous = WORKER.with(|worker| {
            worker.replace((
                Some(WorkerContext {
                    operation: self.id,
                    window: self.data.window,
                }),
                Duration::ZERO,
            ))
        });
        WorkerScope {
            previous,
            timer: self,
        }
    }

    pub fn complete(&mut self) {
        self.completed = true;
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if self
            .data
            .thread
            .is_some_and(|thread| thread != std::thread::current().id())
        {
            return;
        }
        let end = Instant::now();
        let elapsed = end.duration_since(self.enqueued);
        let queued = self
            .started
            .map_or(elapsed, |start| start.duration_since(self.enqueued));
        if self.parent.is_some() {
            WORKER.with(|worker| {
                let (id, waits) = worker.get();
                if id.map(|context| context.operation) == self.parent {
                    worker.set((id, waits + queued));
                }
            });
        }
        let wait = (queued + self.nested_wait).min(elapsed);
        let record = Record {
            operation_id: self.id,
            parent_operation_id: self.parent,
            site: self.site,
            queue_wait_ns: nanos(wait),
            service_ns: nanos(elapsed - wait),
            elapsed_ns: nanos(elapsed),
            completed: self.completed,
        };
        let mut data = self
            .data
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if data.records.len() < RECORD_LIMIT {
            data.records.push(record);
        } else {
            data.dropped_records = data.dropped_records.saturating_add(1);
        }
    }
}

pub struct WorkerScope<'a> {
    previous: (Option<WorkerContext>, Duration),
    timer: &'a mut Timer,
}
impl Drop for WorkerScope<'_> {
    fn drop(&mut self) {
        self.timer.nested_wait = WORKER.with(|worker| worker.replace(self.previous).1);
    }
}
fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    static INSTALL_GUARD: Mutex<()> = Mutex::new(());

    // FIG-5663's bounded-record rule includes incomplete admissions and
    // declares overflow, instead of growing indefinitely or hiding samples.
    #[test]
    fn queue_receipts_bound_records_and_retain_incomplete_admission() {
        let _guard = INSTALL_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let collector = Collector::install().expect("install queue collector");
        drop(Timer::enqueue("cancelled_admission"));
        for _ in 0..RECORD_LIMIT {
            drop(Timer::enqueue("overflow"));
        }
        let snapshot = collector.snapshot();
        assert_eq!(snapshot.records.len(), RECORD_LIMIT);
        assert_eq!(snapshot.dropped_records, 1);
        let cancelled = &snapshot.records[0];
        assert!(!cancelled.completed);
        assert_eq!(cancelled.service_ns, 0);
        assert_eq!(cancelled.queue_wait_ns, cancelled.elapsed_ns);
    }

    // A receipt declares one window: accepted work from a closed window
    // must never publish a gate child into the next window.
    #[test]
    fn old_worker_gates_cannot_contaminate_a_later_window() {
        let _guard = INSTALL_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let first = Collector::install().expect("first window");
        let mut worker = Timer::enqueue("sqlite.worker").expect("accepted worker");
        drop(first);
        let second = Collector::install().expect("second window");
        worker.start_service();
        {
            let _scope = worker.worker_scope();
            if let Some(mut gate) = Timer::enqueue("sqlite.write_gate") {
                gate.start_service();
                gate.complete();
            }
        }
        worker.complete();
        drop(worker);
        assert!(
            second.snapshot().records.is_empty(),
            "old work entered a later receipt window"
        );
    }
}
