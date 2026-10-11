//! Opt-in causal clocks and cumulative connection service counters. This is
//! transient evidence; none of it is engine state or a persisted store format.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

static ENABLED: AtomicBool = AtomicBool::new(false);
static CURRENT: Mutex<Option<Snapshot>> = Mutex::new(None);
const LIMIT: usize = 100_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Stamp {
    /// Linux CLOCK_MONOTONIC, common to parent and child on this host.
    pub monotonic_ns: u64,
    pub unix_ns: u64,
}
impl Stamp {
    pub fn now() -> Self {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let unix = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
        Self {
            monotonic_ns: time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64,
            unix_ns: unix.tv_sec as u64 * 1_000_000_000 + unix.tv_nsec as u64,
        }
    }
    pub fn since(self, earlier: Self) -> f64 {
        // A negative phase is an instrument error, never silently clamped.
        (i128::from(self.monotonic_ns) - i128::from(earlier.monotonic_ns)) as f64 / 1_000_000.0
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Input {
    pub accepted: Option<Stamp>,
    pub run: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Run {
    pub admission: Option<Stamp>,
    /// The instant COMMIT begins, retained only if COMMIT succeeds. The
    /// return is an upper bound, not the publication instant: a remote
    /// follower can read the terminal before COMMIT returns to its caller.
    pub terminal: Option<Stamp>,
    pub terminal_commit_returned: Option<Stamp>,
    pub first_activity: Option<Stamp>,
    pub first_visible: Option<Stamp>,
    pub first_delta: Option<Stamp>,
    pub provider_ms: Option<f64>,
    pub model_started: Option<Stamp>,
    pub model_span_ms: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Worker {
    pub pid: u32,
    pub thread: String,
    pub reader: bool,
    pub busy_ns: u64,
    /// Successful BEGIN IMMEDIATEs, including later rollback/refusal.
    pub write_transactions: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub inputs: BTreeMap<String, Input>,
    pub runs: BTreeMap<String, Run>,
    pub workers: BTreeMap<String, Worker>,
    pub dropped_records: u64,
    #[serde(skip)]
    sessions: BTreeMap<String, String>,
}
fn current() -> MutexGuard<'static, Option<Snapshot>> {
    CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn with_recording(f: impl FnOnce(&mut Snapshot)) {
    if ENABLED.load(Ordering::Acquire)
        && let Some(snapshot) = current().as_mut()
    {
        f(snapshot);
    }
}

pub struct Collector;
impl Collector {
    pub fn install() -> Result<Self, super::AlreadyInstalled> {
        let mut state = current();
        if state.is_some() {
            return Err(super::AlreadyInstalled);
        }
        *state = Some(Snapshot::default());
        ENABLED.store(true, Ordering::Release);
        Ok(Self)
    }
    pub fn snapshot(&self) -> Snapshot {
        current().as_ref().cloned().unwrap_or_default()
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Release);
        *current() = None;
    }
}

pub enum CommitMark {
    Accepted {
        input: String,
    },
    Admitted {
        session: String,
        run: String,
        inputs: Vec<String>,
    },
    Terminal {
        run: String,
    },
}
thread_local! {
    static MARKS: RefCell<Option<Vec<CommitMark>>> = const { RefCell::new(None) };
    static TRANSACTIONS: RefCell<u64> = const { RefCell::new(0) };
    static GATE_WAIT: RefCell<Duration> = const { RefCell::new(Duration::ZERO) };
}
/// Mark staging belongs to one physical transaction. A rollback, error or
/// panic drops it without publishing evidence of a commit that never happened.
pub struct Transaction;
impl Transaction {
    pub fn begin() -> Option<Self> {
        if !ENABLED.load(Ordering::Acquire) {
            return None;
        }
        MARKS.with(|marks| *marks.borrow_mut() = Some(Vec::new()));
        Some(Self)
    }
    pub fn committed(self, before: Stamp, returned: Stamp) {
        let marks = MARKS.with(|marks| marks.borrow_mut().take().unwrap_or_default());
        with_recording(|snapshot| {
            for mark in marks {
                if snapshot.inputs.len() + snapshot.runs.len() >= LIMIT {
                    snapshot.dropped_records += 1;
                    continue;
                }
                match mark {
                    CommitMark::Accepted { input } => {
                        snapshot
                            .inputs
                            .entry(input)
                            .or_default()
                            .accepted
                            .get_or_insert(before);
                    }
                    CommitMark::Admitted {
                        session,
                        run,
                        inputs,
                    } => {
                        snapshot.sessions.insert(session, run.clone());
                        snapshot.runs.entry(run.clone()).or_default().admission = Some(before);
                        for input in inputs {
                            snapshot.inputs.entry(input).or_default().run = Some(run.clone());
                        }
                    }
                    CommitMark::Terminal { run } => {
                        let run = snapshot.runs.entry(run).or_default();
                        run.terminal = Some(before);
                        run.terminal_commit_returned = Some(returned);
                    }
                }
            }
        });
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        MARKS.with(|marks| *marks.borrow_mut() = None);
    }
}
pub fn stage(mark: impl FnOnce() -> CommitMark) {
    if ENABLED.load(Ordering::Acquire) {
        MARKS.with(|marks| {
            if let Some(marks) = marks.borrow_mut().as_mut() {
                marks.push(mark());
            }
        });
    }
}

/// Called at the serving node's outward activity publication, before the
/// sink can see it. Delta means an actual assistant/reasoning delta; visible
/// also includes block/tool starts and finishes, and is kept separately.
pub fn activity(run: &str, visible: bool, delta: bool, model_start: bool, model_end: bool) {
    with_recording(|snapshot| {
        let Some(run) = snapshot.runs.get_mut(run) else {
            return;
        };
        let now = Stamp::now();
        run.first_activity.get_or_insert(now);
        if visible {
            run.first_visible.get_or_insert(now);
        }
        if delta {
            run.first_delta.get_or_insert(now);
        }
        if model_start {
            run.model_started = Some(now);
        }
        if model_end && let Some(started) = run.model_started.take() {
            run.model_span_ms += now.since(started);
        }
    });
}
pub fn provider(session: &str, ms: f64) {
    with_recording(|snapshot| {
        if let Some(run) = snapshot.sessions.get(session)
            && let Some(run) = snapshot.runs.get_mut(run)
        {
            *run.provider_ms.get_or_insert(0.0) += ms;
        }
    });
}

/// Time on a connection thread, including catalog work, excluding enqueue
/// and explicit in-process gate waits. SQLite busy waits and COMMIT I/O are
/// service time: they occupy the connection and limit its throughput.
pub struct Busy {
    start: Instant,
    reader: bool,
}
impl Busy {
    pub fn enter(reader: bool) -> Option<Self> {
        if !ENABLED.load(Ordering::Acquire) {
            return None;
        }
        GATE_WAIT.with(|wait| *wait.borrow_mut() = Duration::ZERO);
        TRANSACTIONS.with(|count| *count.borrow_mut() = 0);
        Some(Self {
            start: Instant::now(),
            reader,
        })
    }
}
impl Drop for Busy {
    fn drop(&mut self) {
        let busy = self
            .start
            .elapsed()
            .saturating_sub(GATE_WAIT.with(|wait| *wait.borrow()));
        with_recording(|snapshot| {
            let thread = format!("{:?}", std::thread::current().id());
            let key = format!("{}:{thread}", std::process::id());
            let worker = snapshot.workers.entry(key).or_insert_with(|| Worker {
                pid: std::process::id(),
                thread,
                reader: self.reader,
                ..Worker::default()
            });
            worker.busy_ns += nanos(busy);
            worker.write_transactions += TRANSACTIONS.with(|count| *count.borrow());
        });
    }
}
pub fn gate<T>(f: impl FnOnce() -> T) -> T {
    if !ENABLED.load(Ordering::Acquire) {
        return f();
    }
    let start = Instant::now();
    let value = f();
    GATE_WAIT.with(|wait| *wait.borrow_mut() += start.elapsed());
    value
}
fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

pub fn began_write() {
    if ENABLED.load(Ordering::Acquire) {
        TRANSACTIONS.with(|count| *count.borrow_mut() += 1);
    }
}

/// Cumulative process counters only: cloning a sample must never walk the
/// retained run history. Concurrent samples overlap; case deltas are the
/// authoritative per-send physical cost.
pub fn workers() -> Vec<Worker> {
    current()
        .as_ref()
        .map(|s| s.workers.values().cloned().collect())
        .unwrap_or_default()
}
