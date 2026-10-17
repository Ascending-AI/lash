//! Opt-in, bounded physical-operation timing. All callers compile out without
//! `perf-witness`; an inactive witness performs no clock reads or allocation.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::AlreadyInstalled;

/// Configured maximum retained records per process/window, not a measurement.
pub const RECORD_LIMIT: usize = 8192;
/// Static producer sites; excess sites share an explicitly named overflow bin.
pub const SITE_LIMIT: usize = 64;
/// Whether this process currently accepts physical queue observations.
pub fn is_recording() -> bool {
    ENABLED.load(Ordering::Acquire)
}

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
    opened: Instant,
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
    /// Nanoseconds since this collector opened, on its monotonic clock.
    pub enqueued_ns: u64,
    pub started_ns: Option<u64>,
    pub finished_ns: u64,
    pub occupancy_at_enqueue: u64,
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
    pub sites: BTreeMap<&'static str, Site>,
    /// Enqueues at excess sites, aggregated into `other_sites`.
    pub overflow_site_operations: u64,
    /// Thread-filtered collectors only filter finalized records. Occupancy
    /// requires a process window, so their occupancy fields are unavailable.
    pub occupancy_available: bool,
}

/// Fixed logarithmic nanosecond bins: zero, then inclusive powers of two,
/// with the final bin ending at u64::MAX. Percentiles are upper bounds,
/// clamped to the observed maximum, never exact-sample percentiles.
#[derive(Clone, Debug, Serialize)]
pub struct Histogram {
    pub n: u64,
    pub sum_ns: u128,
    pub max: u64,
    pub buckets: Vec<u64>,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            n: 0,
            sum_ns: 0,
            max: 0,
            buckets: vec![0; 66],
            p50: 0,
            p95: 0,
            p99: 0,
        }
    }
}
impl Histogram {
    fn add(&mut self, ns: u64) {
        let index = if ns == 0 {
            0
        } else {
            (64 - (ns - 1).leading_zeros()) as usize + 1
        };
        self.buckets[index] += 1;
        self.n += 1;
        self.sum_ns += u128::from(ns);
        self.max = self.max.max(ns);
    }

    fn quantile(&self, percent: u64) -> u64 {
        let rank = (u128::from(self.n) * u128::from(percent)).div_ceil(100);
        if rank == 0 {
            return 0;
        }
        let mut count = 0_u128;
        for (index, n) in self.buckets.iter().enumerate() {
            count += u128::from(*n);
            if count >= rank {
                let upper = if index == 0 {
                    0
                } else {
                    1_u64.checked_shl((index - 1) as u32).unwrap_or(u64::MAX)
                };
                return upper.min(self.max);
            }
        }
        self.max
    }

    fn summarize(&mut self) {
        self.p50 = self.quantile(50);
        self.p95 = self.quantile(95);
        self.p99 = self.quantile(99);
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Site {
    pub enqueued: u64,
    pub started: u64,
    pub finished: u64,
    pub completed: u64,
    pub incomplete: u64,
    pub dropped_records: u64,
    pub waiting: u64,
    pub in_service: u64,
    pub peak_waiting: u64,
    pub peak_in_service: u64,
    pub peak_occupancy: u64,
    pub peak_occupancy_ns: u64,
    pub first_enqueued_ns: Option<u64>,
    pub last_finished_ns: Option<u64>,
    /// Direct enqueue-to-service wait at this site. Child gate waits belong
    /// to their own sites. Includes lower bounds from incomplete timers.
    pub queue_wait: Histogram,
    pub service: Histogram,
}
impl Site {
    fn enqueue(&mut self, at: u64) -> u64 {
        self.enqueued += 1;
        self.waiting += 1;
        self.first_enqueued_ns = Some(self.first_enqueued_ns.map_or(at, |first| first.min(at)));
        self.peak_waiting = self.peak_waiting.max(self.waiting);
        let occupancy = self.waiting + self.in_service;
        if occupancy > self.peak_occupancy {
            self.peak_occupancy = occupancy;
            self.peak_occupancy_ns = at;
        }
        occupancy
    }
    fn start(&mut self) {
        self.started += 1;
        self.waiting -= 1;
        self.in_service += 1;
        self.peak_in_service = self.peak_in_service.max(self.in_service);
    }
}
impl Snapshot {
    fn site_key(&mut self, site: &'static str) -> &'static str {
        if self.sites.contains_key(site) || self.sites.len() < SITE_LIMIT {
            site
        } else {
            self.overflow_site_operations += 1;
            "other_sites"
        }
    }
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
            opened: Instant::now(),
            thread,
            snapshot: Mutex::new(Snapshot {
                records: Vec::with_capacity(RECORD_LIMIT),
                occupancy_available: thread.is_none(),
                ..Snapshot::default()
            }),
        });
        *current = Some(Arc::clone(&data));
        ENABLED.store(true, Ordering::Release);
        Ok(Self(data))
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self
            .0
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for site in snapshot.sites.values_mut() {
            site.queue_wait.summarize();
            site.service.summarize();
        }
        snapshot
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
    aggregate_site: &'static str,
    occupancy_at_enqueue: u64,
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
        let enqueued = Instant::now();
        let (aggregate_site, occupancy_at_enqueue) = if data.thread.is_none() {
            let mut snapshot = data
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let key = snapshot.site_key(site);
            let occupancy = snapshot
                .sites
                .entry(key)
                .or_default()
                .enqueue(nanos(enqueued - data.opened));
            (key, occupancy)
        } else {
            (site, 0)
        };
        Some(Self {
            data,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            parent: context.map(|context| context.operation),
            site,
            enqueued,
            aggregate_site,
            occupancy_at_enqueue,
            started: None,
            nested_wait: Duration::ZERO,
            completed: false,
        })
    }

    pub fn start_service(&mut self) {
        if self.started.is_some() {
            return;
        }
        self.started = Some(Instant::now());
        if self.data.thread.is_none() {
            self.data
                .snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sites
                .entry(self.aggregate_site)
                .or_default()
                .start();
        }
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
            enqueued_ns: nanos(self.enqueued - self.data.opened),
            started_ns: self.started.map(|at| nanos(at - self.data.opened)),
            finished_ns: nanos(end - self.data.opened),
            occupancy_at_enqueue: self.occupancy_at_enqueue,
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
        let dropped = data.records.len() == RECORD_LIMIT;
        let key = if self.data.thread.is_some() {
            data.site_key(self.site)
        } else {
            self.aggregate_site
        };
        let site = data.sites.entry(key).or_default();
        if self.data.thread.is_some() {
            site.enqueued += 1;
            site.started += u64::from(self.started.is_some());
            site.first_enqueued_ns.get_or_insert(record.enqueued_ns);
        } else if self.started.is_some() {
            site.in_service -= 1;
        } else {
            site.waiting -= 1;
        }
        site.finished += 1;
        site.completed += u64::from(self.completed);
        site.incomplete += u64::from(!self.completed);
        site.dropped_records += u64::from(dropped);
        site.last_finished_ns = Some(
            site.last_finished_ns
                .map_or(record.finished_ns, |last| last.max(record.finished_ns)),
        );
        site.queue_wait.add(nanos(queued));
        site.service.add(record.service_ns);
        if !dropped {
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
        let pending = (0..RECORD_LIMIT)
            .map(|_| Timer::enqueue("overflow"))
            .collect::<Vec<_>>();
        assert_eq!(
            collector.snapshot().sites["overflow"].waiting,
            RECORD_LIMIT as u64
        );
        drop(pending);
        let mut late = Timer::enqueue("overflow").expect("late timer");
        let waiting = collector.snapshot();
        assert_eq!(waiting.sites["overflow"].waiting, 1);
        late.start_service();
        let serving = collector.snapshot();
        assert_eq!(serving.sites["overflow"].in_service, 1);
        late.complete();
        drop(late);
        let snapshot = collector.snapshot();
        assert_eq!(snapshot.records.len(), RECORD_LIMIT);
        assert_eq!(snapshot.dropped_records, 2);
        // QUEUE-OVERFLOW-CONSERVATION: bounded detail must not censor the
        // per-site population used to attribute a saturation window.
        let json = serde_json::to_value(&snapshot).expect("queue receipt");
        assert_eq!(
            json["sites"]["overflow"]["finished"],
            RECORD_LIMIT as u64 + 1
        );
        assert_eq!(
            json["sites"]["overflow"]["queue_wait"]["n"],
            RECORD_LIMIT as u64 + 1
        );
        let site = &snapshot.sites["overflow"];
        assert_eq!((site.completed, site.incomplete), (1, RECORD_LIMIT as u64));
        assert_eq!(
            site.enqueued,
            site.finished + site.waiting + site.in_service
        );
        assert_eq!(site.queue_wait.buckets.iter().sum::<u64>(), site.finished);
        assert_eq!(site.service.n, site.finished);
        assert!(site.queue_wait.p50 <= site.queue_wait.p95);
        assert!(site.queue_wait.p95 <= site.queue_wait.p99);
        assert!(site.queue_wait.p99 <= site.queue_wait.max);
        assert!(site.first_enqueued_ns <= site.last_finished_ns);
        assert_eq!(site.peak_occupancy, RECORD_LIMIT as u64);
        assert_eq!(site.peak_waiting, RECORD_LIMIT as u64);
        // QUEUE-HISTOGRAM: the fixed bins include zero and the largest
        // representable duration; quantiles cannot clip the observed tail.
        let mut known = Histogram::default();
        for ns in [0, 3, 7, 9, u64::MAX] {
            known.add(ns);
        }
        known.summarize();
        assert_eq!(
            (known.n, known.p50, known.p95, known.p99, known.max),
            (5, 8, u64::MAX, u64::MAX, u64::MAX)
        );
        let cancelled = &snapshot.records[0];
        assert!(!cancelled.completed);
        assert_eq!(cancelled.service_ns, 0);
        assert_eq!(cancelled.queue_wait_ns, cancelled.elapsed_ns);
    }

    // QUEUE-RESOURCE-ATTRIBUTION: gate waits belong to the gate, so a worker
    // cannot win the resource ranking merely by including its child waits.
    #[test]
    fn resource_histograms_count_nested_wait_only_at_the_gate_site() {
        let _guard = INSTALL_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let collector = Collector::install().expect("window");
        let mut worker = Timer::enqueue("sqlite.worker").expect("worker");
        worker.start_service();
        {
            let _scope = worker.worker_scope();
            let mut gate = Timer::enqueue("sqlite.write_gate").expect("gate");
            gate.start_service();
            gate.complete();
        }
        worker.complete();
        drop(worker);
        let snapshot = collector.snapshot();
        let worker = snapshot
            .records
            .iter()
            .find(|record| record.site == "sqlite.worker")
            .expect("worker record");
        let gate = snapshot
            .records
            .iter()
            .find(|record| record.site == "sqlite.write_gate")
            .expect("gate record");
        let direct_wait = worker.started_ns.expect("started") - worker.enqueued_ns;
        assert_eq!(
            snapshot.sites["sqlite.worker"].queue_wait.sum_ns,
            u128::from(direct_wait)
        );
        assert_eq!(worker.queue_wait_ns, direct_wait + gate.queue_wait_ns);
        assert_eq!(
            snapshot.sites["sqlite.write_gate"].queue_wait.sum_ns,
            u128::from(gate.queue_wait_ns)
        );
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
