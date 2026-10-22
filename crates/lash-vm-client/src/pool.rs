/// version_surface = "coexist"
/// version_guard(items(LASH_VM_CLIENT_POOL_DOMAIN_VERSION, measurements))
const LASH_VM_CLIENT_POOL_DOMAIN_VERSION: &str = "lash-vm-client/pool-v1";

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub use crate::budget::ExecutionBudget;
use crate::ipc::Worker;
use crate::measurements::{
    ExecutionClass, ExecutionReceipt, Measurements, PoolMeasurements, SharedMeasurements,
};
use crate::{PoolConfig, PoolError};
use lash_protocol_types::engine_fault::{
    Detail, Exchange, ExecutionLease, FrameEpoch, InfrastructureOutcome, OwnerEpoch,
    ProtocolBreach, SequenceFault,
};
use lash_protocol_types::worker_limit::WorkerLimit;
use lash_vm_protocol::page::{PageCapability, PageExchangeId, PageReply, PageRequest, SealBatch};
use lash_vm_protocol::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    pub workers: usize,
    pub idle: usize,
    pub queued_items: usize,
    pub queued_bytes: usize,
    pub restart_storm: bool,
}
struct State {
    #[cfg(feature = "testing")]
    measured_cpu_nanos: u64,
    idle: Vec<Worker>,
    workers: usize,
    queued_items: usize,
    queued_bytes: usize,
    next_lease: u64,
    restarts: VecDeque<Instant>,
    failed: bool,
}
struct Pool {
    phases: Option<crate::phases::Recorder>,
    epoch: u64,
    measurements: SharedMeasurements,
    config: PoolConfig,
    /// The pool start's frames, encoded once from the parent and shared by
    /// prewarm and replacement workers.
    packages: Vec<Vec<u8>>,
    library: Arc<crate::packages::WorkerLibrary>,
    /// The most workers the pool runs at once, within its memory budget.
    capacity: usize,
    state: Mutex<State>,
    available: Condvar,
}

/// A synchronized pool of OS processes. Calls block only for explicitly
/// bounded durations; async hosts use their owned blocking executor.
#[derive(Clone)]
pub struct WorkerPool(Arc<Pool>);
impl WorkerPool {
    pub(crate) fn prepare_start(&self, start: &mut Start) -> Result<(), PoolError> {
        self.0.library.prepare_start(start)
    }
    pub fn new(config: PoolConfig) -> Result<Self, PoolError> {
        Self::new_observed(config, None)
    }
    pub(crate) fn new_observed(
        config: PoolConfig,
        phases: Option<crate::phases::Recorder>,
    ) -> Result<Self, PoolError> {
        config.validate()?;
        if let Some(cgroup) = &config.cgroup {
            cgroup.probe().map_err(PoolError::cgroup)?;
        }
        let held = config.worker_library().map_err(|error| {
            PoolError::breach(ProtocolBreach::Machine {
                detail: Detail::new(error),
            })
        })?;
        let codec = FrameCodec::new(config.protocol.decode)
            .with_detail_limit(config.tuning.diagnostic_bytes);
        let packages = codec.encode_pool_start(&held.frame())?;
        static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
        let pool = Self(Arc::new(Pool {
            phases,
            packages,
            library: held,
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            measurements: Arc::new(Mutex::new(Measurements::default())),
            capacity: config.worker_capacity(),
            config,
            state: Mutex::new(State {
                #[cfg(feature = "testing")]
                measured_cpu_nanos: 0,
                idle: Vec::new(),
                workers: 0,
                queued_items: 0,
                queued_bytes: 0,
                next_lease: 1,
                restarts: VecDeque::new(),
                failed: false,
            }),
            available: Condvar::new(),
        }));
        // Even a pool with no warm workers validates the binary against its
        // frame at startup. A missing native must never first meet a guest.
        if pool.0.config.min_workers == 0 {
            let mut probe = pool.0.spawn_ready()?;
            probe.terminate();
        }
        for _ in 0..pool.0.config.min_workers {
            let worker = pool.0.spawn_ready()?;
            let mut state = lock(&pool.0.state);
            state.workers += 1;
            state.idle.push(worker);
        }
        Ok(pool)
    }
    /// Bounded diagnostic events, separate from billing and worker deadlines.
    pub fn effect_phases(&self) -> Option<crate::EffectPhases> {
        self.0
            .phases
            .as_ref()
            .map(crate::phases::Recorder::snapshot)
    }
    pub fn config(&self) -> &PoolConfig {
        &self.0.config
    }
    pub fn stats(&self) -> PoolStats {
        let state = lock(&self.0.state);
        PoolStats {
            workers: state.workers,
            idle: state.idle.len(),
            queued_items: state.queued_items,
            queued_bytes: state.queued_bytes,
            restart_storm: state.failed,
        }
    }
    /// Enable bounded receipts before admitting load. Overflow is explicit.
    pub fn enable_execution_receipts(&self, capacity: usize) {
        lock(&self.0.measurements).receipt_capacity = capacity;
    }
    /// Starts an opt-in buffer measurement window at a released-worker park.
    #[cfg(feature = "perf-witness")]
    pub fn clear_buffer_peaks(&self) {
        let mut measurements = lock(&self.0.measurements);
        measurements.counters.peak_buffer_bytes = 0;
        measurements.counters.worker_work.peak_buffer_bytes = 0;
    }
    pub fn measurements(&self) -> PoolMeasurements {
        let state = lock(&self.0.state);
        let measurements = lock(&self.0.measurements);
        PoolMeasurements {
            exporter: LASH_VM_CLIENT_POOL_DOMAIN_VERSION,
            epoch: self.0.epoch,
            workers: state.workers,
            idle: state.idle.len(),
            queued_items: state.queued_items,
            queued_bytes: state.queued_bytes,
            counters: measurements.counters,
            units: crate::measurements::units(),
            executions: measurements.executions.clone(),
        }
    }
    /// Worker CPU charged by this pool, including reset and reaped failures.
    /// This is a measurement counter, independent of execution admission.
    #[cfg(feature = "testing")]
    pub fn measured_cpu(&self) -> Duration {
        Duration::from_nanos(lock(&self.0.state).measured_cpu_nanos)
    }
}

// DHAT traces every bootstrap allocation while staging the helper registry
// and compiling the warm library. Remote measurements reached 9.9 s against
// the standard 5 s silence window. A profiling build declares four windows
// (20 s with standard bounds); serving exchanges still use one window.
const STARTUP_SILENCE_WINDOWS: u32 = if cfg!(feature = "dhat-heap") { 4 } else { 1 };

#[cfg(feature = "dhat-heap")]
const _: () = assert!(
    matches!(env!("CARGO_PKG_VERSION").as_bytes(), b"0.0.0-dev"),
    "dhat-heap is only available in development packages"
);

impl Pool {
    fn unqueue(&self, state: &mut State, queued: bool, bytes: usize, started: Instant) {
        if queued {
            lock(&self.measurements).counters.queue_delay_ns += nanos(started.elapsed());
            unqueue(state, queued, bytes);
        }
    }
    #[cfg(feature = "testing")]
    fn report_deadline(
        &self,
        state: &State,
        bound: &str,
        elapsed: Duration,
        budget: &ExecutionBudget,
    ) {
        let (attempts, cpu) = budget.totals();
        eprintln!(
            "WORKER_DEADLINE bound={bound} elapsed_ms={:.3} cpu_accounted_ms={:.3} attempts={attempts} checkout_ms={} compute_ms={} serialization_ms={} cumulative_cpu_ms={} workers={} idle={} max_workers={} queued_items={} queued_bytes={}",
            elapsed.as_secs_f64() * 1000.0,
            cpu.as_secs_f64() * 1000.0,
            self.config.deadlines.checkout.as_millis(),
            self.config.deadlines.compute.as_millis(),
            self.config.deadlines.serialization.as_millis(),
            self.config.deadlines.cumulative_cpu.as_millis(),
            state.workers,
            state.idle.len(),
            self.config.max_workers,
            state.queued_items,
            state.queued_bytes,
        );
    }
    fn startup_watchdog(&self) -> Duration {
        self.config
            .protocol
            .no_response_watchdog
            .saturating_mul(STARTUP_SILENCE_WINDOWS)
    }
    fn spawn_ready(&self) -> Result<Worker, PoolError> {
        self.spawn_ready_until(Instant::now() + self.startup_watchdog())
    }
    fn spawn_ready_until(&self, deadline: Instant) -> Result<Worker, PoolError> {
        let phase_start = self.phases.as_ref().map(crate::phases::Recorder::now);
        let started = Instant::now();
        let mut worker = Worker::spawn(&self.config)?;
        worker.measurements = Some(self.measurements.clone());
        worker.phases = self.phases.as_ref().map(|recorder| crate::phases::Scope {
            recorder: recorder.clone(),
            pid: worker.pid(),
            process_epoch: worker.process_epoch.clone(),
            lease: 0,
            owner_epoch: 0,
            frame_epoch: 0,
            owner: None,
            document: None,
            effect: None,
        });
        let deadline = deadline.min(Instant::now() + self.startup_watchdog());
        // Bootstrap silence starts before exec, not at the first socket read.
        let frame = worker
            .receive(deadline)
            .map_err(|error| error.with_silence(started.elapsed()))?;
        let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
        fence.admit(&frame.header).map_err(PoolError::breach)?;
        match frame.message {
            WorkerMessage::Ready {
                protocol_version,
                crate_version,
            } => {
                if let (Some(scope), Some(start)) = (&worker.phases, phase_start) {
                    scope.record("spawn_to_ready", start);
                }
                check_worker_protocol_version(
                    protocol_version,
                    env!("CARGO_PKG_VERSION"),
                    &crate_version,
                )?;
                // Ready admits the protocol only. Installation is a bounded
                // startup exchange, completed before this worker can enter the
                // pool or receive a guest checkout.
                worker.install_packages(&self.packages, deadline)?;
                let mut measurements = lock(&self.measurements);
                if measurements.pending_replacements > 0 {
                    measurements.pending_replacements -= 1;
                    measurements.counters.replacements += 1;
                }
                #[cfg(feature = "perf-witness")]
                lash_core_execution::perf_witness::startup::record_subject(
                    lash_core_execution::perf_witness::startup::Phase::VmReady,
                    worker.pid(),
                );
                Ok(worker)
            }
            message => Err(PoolError::breach(ProtocolBreach::Unexpected {
                exchange: Exchange::Handshake,
                found: message.kind(),
            })),
        }
    }
    fn failed(&self, state: &mut State) {
        let now = Instant::now();
        while state
            .restarts
            .front()
            .is_some_and(|time| now.duration_since(*time) >= self.config.restart_window)
        {
            state.restarts.pop_front();
        }
        state.restarts.push_back(now);
        if state.restarts.len() >= self.config.max_restarts {
            state.failed = true;
        }
    }
    fn discard(&self, mut worker: Worker, failed: bool) -> u64 {
        {
            let mut measurements = lock(&self.measurements);
            measurements.counters.discards += 1;
            measurements.pending_replacements += 1;
        }
        let cpu = worker.terminate();
        drop(worker);
        let mut state = lock(&self.state);
        state.workers -= 1;
        if failed {
            self.failed(&mut state);
        }
        // The just-reaped slot stays reserved while its replacement starts.
        let replace = !state.failed && state.workers < self.config.min_workers;
        if replace {
            state.workers += 1;
        }
        self.available.notify_all();
        drop(state);
        if replace {
            let replacement = self.spawn_ready();
            let mut state = lock(&self.state);
            match replacement {
                Ok(worker) if !state.failed => state.idle.push(worker),
                Ok(mut worker) => {
                    drop(state);
                    worker.terminate();
                    state = lock(&self.state);
                    state.workers -= 1;
                }
                Err(_) => {
                    state.workers -= 1;
                    self.failed(&mut state);
                }
            }
            self.available.notify_all();
        }
        cpu
    }
}
fn unqueue(state: &mut State, queued: bool, bytes: usize) {
    if queued {
        state.queued_items -= 1;
        state.queued_bytes -= bytes;
    }
}
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
pub(crate) fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}
pub(crate) fn limit(limit: WorkerLimit) -> PoolError {
    InfrastructureOutcome::WorkerLimitExceeded { limit }.into()
}

/// Where a run stands after the parent drove it one exchange.
#[derive(Debug)]
pub enum RunStep {
    /// The machine reads its host; [`Checkout::host_answer`] runs it on.
    HostRead {
        id: HostReadId,
        kind: HostReadKind,
        request: EncodedPayload,
    },
    /// The machine's heap reads or seals pages; [`Checkout::page_reply`]
    /// runs it on with the page service's answer.
    Pages {
        id: PageExchangeId,
        request: PageRequest,
    },
    /// No task is ready. On the persistent heap, `candidate` is the root
    /// the park sealed, and `seal` the pages that crossed with the park:
    /// the caller admits them and hands their grants to
    /// [`Checkout::grant`].
    Parked {
        park: EncodedPayload,
        meters: RunMeters,
        seal: Option<SealBatch>,
        candidate: Option<[u8; 32]>,
    },
    /// The slice is spent and a task is still ready.
    Slice { meters: RunMeters },
    /// The run is over, with the pages that crossed with its end.
    Ended {
        end: EncodedPayload,
        meters: RunMeters,
        seal: Option<SealBatch>,
    },
    /// The candidate a seal made: its root, with the pages that crossed
    /// with it.
    Sealed {
        seal: Option<SealBatch>,
        candidate: [u8; 32],
    },
}

/// Owns one transport lease. Dropping without release kills and reaps the
/// worker. A failed receive fences that lease before replenishing the pool.
pub struct Checkout {
    #[cfg(feature = "testing")]
    admitted_at: Instant,
    #[cfg(feature = "testing")]
    measured_cpu_receipt: std::cell::Cell<u64>,
    pool: Arc<Pool>,
    worker: Option<Worker>,
    budget: ExecutionBudget,
    /// Whether this checkout counts in its execution's held wall time: from
    /// its admission until it lets its worker go.
    holding: bool,
    credited_cpu: u64,
    outgoing: MessageFence,
    incoming: MessageFence,
    reservation: usize,
    /// Whether the lease's one `Prepare` crossed.
    prepared: bool,
    started: bool,
    /// Whether the worker began computing the run: a death from then until
    /// the run releases it is the run's to answer for (FIG-6786).
    computed: bool,
    /// The run's start, until it crosses with the run's first slice.
    unsent_start: Option<Box<Start>>,
    /// Committed outcomes for the run, until they cross with its next
    /// slice.
    deliveries: Vec<Delivery>,
    /// The host read or page exchange the worker waits on.
    pending: Option<Pending>,
    /// The phase a worker waiting on a page exchange answers from: it
    /// asked while computing and computes on.
    paged: Option<WorkerPhase>,
    /// The stage capabilities issued for the last seal batch that crossed
    /// with the run, until they cross back with its next slice.
    grants: Vec<PageCapability>,
    /// The rest of a slice or seal whose start and deliveries outgrew one
    /// frame and cross an exchange at a time.
    split: Option<Split>,
    /// Whether the worker stands at a safe point, where it can be reset.
    resettable: bool,
    owner: Option<VmOwner>,
    /// The document the run executes, as the parent named it at `start`.
    document: Option<String>,
    printed: Vec<EncodedPayload>,
    /// Position hints and exact count deltas, as the run frames carried them.
    sites: Vec<EncodedPayload>,
    /// The run's heap bound, which also bounds what one exchange prints
    /// (FIG-4458).
    print_budget: u64,
    /// The printed bytes the current exchange has handed over.
    printed_bytes: u64,
    execution_class: Option<ExecutionClass>,
    execution_recorded: bool,
}
/// What a worker waits on mid-slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Read(HostReadId),
    Pages(PageExchangeId),
}

/// What is left to cross of a slice or seal sent an exchange at a time.
/// The start and each delivery can open page exchanges of their own (a
/// checkpoint start reads its root); those are the caller's to answer, as
/// a slice's are, before the next exchange crosses.
struct Split {
    deliveries: std::vec::IntoIter<Delivery>,
    then: SplitThen,
}

enum SplitThen {
    Run { slice: u64, cancel: bool },
    Seal,
}

impl Checkout {
    pub(crate) fn phase_scope(&self) -> Option<crate::phases::Scope> {
        self.worker
            .as_ref()
            .and_then(|worker| worker.phases.clone())
    }
    fn phase_identity(&mut self) {
        if let Some(scope) = self
            .worker
            .as_mut()
            .and_then(|worker| worker.phases.as_mut())
        {
            scope.owner = self.owner.as_ref().map(|owner| owner.as_str().to_owned());
            scope.document = self.document.clone();
        }
    }
    fn charge_cpu(&self, cpu_nanos: u64) -> Result<(), PoolError> {
        #[cfg(feature = "testing")]
        {
            let mut state = lock(&self.pool.state);
            // Failed accounting can charge again when reaping. Measure each
            // worker receipt once without changing execution-budget policy.
            let delta = cpu_nanos.saturating_sub(self.measured_cpu_receipt.replace(cpu_nanos));
            state.measured_cpu_nanos = state.measured_cpu_nanos.saturating_add(delta);
        }
        let result = self.budget.charge(
            cpu_nanos.saturating_sub(self.credited_cpu),
            &self.pool.config,
        );
        #[cfg(feature = "testing")]
        if result.is_err() {
            self.pool.report_deadline(
                &lock(&self.pool.state),
                "cumulative_cpu",
                self.admitted_at.elapsed(),
                &self.budget,
            );
        }
        result
    }
    /// What the run printed since this was last called, in order.
    pub fn take_printed(&mut self) -> Vec<EncodedPayload> {
        std::mem::take(&mut self.printed)
    }
    /// Position hints and count deltas since this was last called, as the
    /// run frames carried them, in order ([`crate::wire::Sites`]).
    pub fn take_sites(&mut self) -> Vec<EncodedPayload> {
        std::mem::take(&mut self.sites)
    }
    pub fn lease(&self) -> ExecutionLease {
        self.outgoing.next_header_copy().lease
    }
    /// The owner and frame epochs the lease runs under.
    pub(crate) fn epochs(&self) -> (OwnerEpoch, FrameEpoch) {
        let header = self.outgoing.next_header_copy();
        (header.owner_epoch, header.frame_epoch)
    }
    pub fn interruptor(&self) -> Result<std::os::unix::net::UnixStream, PoolError> {
        self.worker
            .as_ref()
            .ok_or_else(PoolError::eof)?
            .pipe
            .try_clone()
            .map_err(PoolError::io)
    }
    pub fn pid(&self) -> Option<u32> {
        self.worker.as_ref().map(Worker::pid)
    }
    pub fn budget(&self) -> &ExecutionBudget {
        &self.budget
    }

    /// Takes the run for the worker: with the run's first slice, it
    /// validates and compiles `start`'s document and builds a machine that
    /// has executed nothing, or one rebuilt from the parked state; what it
    /// refuses, that slice answers (FIG-5921). `document` is the identity
    /// the parent knows the document by, which a parked state must name;
    /// `class` is what the pool's receipts call the execution. A lease
    /// whose `prepare` lowered the run's document starts it with no reset
    /// between. A start asking for more of a bound than the pool's
    /// [`PoolConfig::run_bounds`] admit is refused with
    /// [`RunRefusal::AboveCeiling`] before anything crosses.
    pub fn start(
        &mut self,
        mut start: Start,
        document: &str,
        class: ExecutionClass,
    ) -> Result<(), PoolError> {
        if self.started {
            return Err(PoolError::breach(SequenceFault::CheckoutAlreadyStarted));
        }
        // The host configuration is the ceiling the pool admits, never a
        // clamp: a run asking more is refused, naming both (FIG-6827).
        if let Some(refusal) = start.bounds.past(&self.pool.config.run_bounds) {
            return Err(PoolError::refused(refusal));
        }
        self.pool.library.prepare_start(&mut start)?;
        // The start crosses within what the checkout reserved for it, as
        // if it crossed alone.
        let worker = self.worker.as_ref().ok_or_else(PoolError::eof)?;
        let size = worker
            .codec
            .borrowed_frame_len(&BorrowedParentFrame {
                header: self.outgoing.next_header_copy(),
                message: BorrowedParentMessage::Start(&start),
            })
            .map_err(PoolError::from);
        match size {
            Ok(size) if size <= self.reservation as u64 => {}
            Ok(size) => {
                self.discard();
                return Err(PoolError::QueueFull {
                    bytes: usize::try_from(size).unwrap_or(usize::MAX),
                });
            }
            Err(error) => {
                self.discard();
                return Err(error);
            }
        }
        self.execution_class = Some(class);
        self.execution_recorded = false;
        self.owner = Some(start.owner.clone());
        self.document = Some(document.to_owned());
        self.phase_identity();
        if let Some(scope) = self.phase_scope() {
            scope.point("document_start");
        }
        self.print_budget = start.bounds.memory;
        self.started = true;
        self.computed = false;
        self.unsent_start = Some(Box::new(start));
        Ok(())
    }

    /// Admits the run a checkout kept from its lowering starts next, as a
    /// checkout of its own would be admitted: under the execution's budget,
    /// holding `reservation` bytes for its input.
    pub(crate) fn readmit(&mut self, reservation: usize) -> Result<(), PoolError> {
        self.budget.admit(&self.pool.config)?;
        self.reservation = reservation;
        Ok(())
    }

    /// Runs ready tasks until the machine reads its host, parks, spends
    /// `slice` charge units or ends. With `cancel` the machine observes a
    /// run cancel at its next safe point. The run's start, when it has not
    /// crossed, and the outcomes delivered since the last slice cross with
    /// it, in one exchange (FIG-5921).
    pub fn run(&mut self, slice: u64, cancel: bool) -> Result<RunStep, PoolError> {
        if self.pending.is_some() || !self.started {
            return Err(PoolError::breach(SequenceFault::NoRun));
        }
        self.drive(slice, cancel)
    }

    /// The exchange of one slice, its start and its deliveries: in one
    /// frame, or, when they outgrow one, the start and each delivery in an
    /// exchange of its own before the slice.
    fn drive(&mut self, slice: u64, cancel: bool) -> Result<RunStep, PoolError> {
        let timeout = self.pool.config.protocol.no_response_watchdog;
        let start = self.unsent_start.take();
        let deliveries = std::mem::take(&mut self.deliveries);
        let scope = self.phase_scope();
        let encode_start = scope.as_ref().map(|scope| scope.recorder.now());
        let together = self.worker.as_ref().and_then(|worker| {
            worker
                .codec
                .encode_borrowed(&BorrowedParentFrame {
                    header: self.outgoing.next_header_copy(),
                    message: BorrowedParentMessage::Run {
                        start: start.as_deref(),
                        deliveries: &deliveries,
                        slice,
                        cancel,
                        sealed: &self.grants,
                    },
                })
                .ok()
                .filter(|bytes| worker.codec.check_frame(bytes).is_ok())
        });
        if let (Some(scope), Some(start)) = (&scope, encode_start) {
            scope.record("encode_run", start);
        }
        if let Some(bytes) = together {
            self.grants.clear();
            let message = self.exchange_encoded(&bytes, timeout)?;
            return self.step(message);
        }
        self.split_from(
            start,
            Split {
                deliveries: deliveries.into_iter(),
                then: SplitThen::Run { slice, cancel },
            },
        )
    }

    /// Crosses `start`, when it has not crossed, then the rest of `split`.
    fn split_from(
        &mut self,
        start: Option<Box<Start>>,
        split: Split,
    ) -> Result<RunStep, PoolError> {
        let exchange = match split.then {
            SplitThen::Run { .. } => Exchange::Run,
            SplitThen::Seal => Exchange::Seal,
        };
        self.split = Some(split);
        match start {
            Some(start) => {
                let message = self.exchange(
                    ParentMessage::Start(start),
                    self.pool.config.protocol.no_response_watchdog,
                )?;
                self.split_step(exchange, message)
            }
            None => self.split_next(),
        }
    }

    /// The worker's answer to one exchange of a split: a page exchange is
    /// handed to the caller, an acknowledgement sends the next exchange.
    fn split_step(
        &mut self,
        exchange: Exchange,
        message: WorkerMessage,
    ) -> Result<RunStep, PoolError> {
        match message {
            WorkerMessage::Pages { id, request } => Ok(RunStep::Pages { id, request }),
            WorkerMessage::Started | WorkerMessage::Delivered => self.split_next(),
            message => Err(self.unexpected(exchange, &message)),
        }
    }

    fn split_next(&mut self) -> Result<RunStep, PoolError> {
        let timeout = self.pool.config.protocol.no_response_watchdog;
        let Some(split) = self.split.as_mut() else {
            return Err(PoolError::breach(SequenceFault::NoRun));
        };
        if let Some(Delivery { wait, outcome }) = split.deliveries.next() {
            let message = self.exchange(ParentMessage::Deliver { wait, outcome }, timeout)?;
            return self.split_step(Exchange::Deliver, message);
        }
        let then = self.split.take().map(|split| split.then);
        let message = match then {
            Some(SplitThen::Run { slice, cancel }) => {
                let sealed = std::mem::take(&mut self.grants);
                self.exchange(
                    ParentMessage::Run {
                        start: None,
                        deliveries: Vec::new(),
                        slice,
                        cancel,
                        sealed,
                    },
                    timeout,
                )?
            }
            Some(SplitThen::Seal) | None => self.exchange(ParentMessage::Seal, timeout)?,
        };
        self.step(message)
    }

    /// Answers the machine's pending host read; the slice runs on. The
    /// parent's work on the answer may have taken any time: no worker
    /// deadline ran while the parent owned the read.
    pub fn host_answer(
        &mut self,
        id: HostReadId,
        answer: EncodedPayload,
    ) -> Result<RunStep, PoolError> {
        if self.pending != Some(Pending::Read(id)) {
            return Err(PoolError::breach(SequenceFault::WrongReadId));
        }
        if let Err(error) = self.effect_bound(answer.0.len()) {
            self.discard_after(&error);
            return Err(error);
        }
        if let Some(scope) = self.phase_scope() {
            scope.point("answer");
        }
        self.pending = None;
        let message = self.exchange(
            ParentMessage::HostAnswer { id, answer },
            self.pool.config.protocol.no_response_watchdog,
        )?;
        self.step(message)
    }

    /// Answers the machine's pending page exchange with the page service's
    /// reply; the slice runs on, its statement and native stack intact. No
    /// worker deadline ran while the parent served the pages.
    pub fn page_reply(
        &mut self,
        id: PageExchangeId,
        reply: PageReply,
    ) -> Result<RunStep, PoolError> {
        if self.pending != Some(Pending::Pages(id)) {
            return Err(PoolError::breach(SequenceFault::WrongPageExchange));
        }
        self.pending = None;
        let message = self.exchange(
            ParentMessage::Pages { id, reply },
            self.pool.config.protocol.no_response_watchdog,
        )?;
        if self.split.is_some() {
            return self.split_step(Exchange::Pages, message);
        }
        self.step(message)
    }

    fn step(&mut self, message: WorkerMessage) -> Result<RunStep, PoolError> {
        let (step, sites) = match message {
            WorkerMessage::HostRead {
                id,
                kind,
                request,
                sites,
            } => (RunStep::HostRead { id, kind, request }, sites),
            WorkerMessage::Pages { id, request } => (RunStep::Pages { id, request }, None),
            WorkerMessage::Parked {
                park,
                meters,
                seal,
                candidate,
                sites,
            } => (
                RunStep::Parked {
                    park,
                    meters,
                    seal,
                    candidate,
                },
                sites,
            ),
            WorkerMessage::Sealed { seal, candidate } => {
                (RunStep::Sealed { seal, candidate }, None)
            }
            WorkerMessage::Slice { meters, sites } => (RunStep::Slice { meters }, sites),
            WorkerMessage::Ended {
                end,
                meters,
                seal,
                sites,
            } => (RunStep::Ended { end, meters, seal }, sites),
            message => return Err(self.unexpected(Exchange::Run, &message)),
        };
        self.sites.extend(sites);
        Ok(step)
    }

    /// Holds `sealed`, the stage capabilities the page service issued for
    /// the seal batch that crossed with the last step, until they cross
    /// with the run's next slice (FIG-6288).
    pub fn grant(&mut self, sealed: Vec<PageCapability>) {
        self.grants = sealed;
    }

    /// Hands the machine one committed outcome, with the run's next slice:
    /// the machine takes it before it runs on, or drops it when its wait
    /// had been withdrawn.
    pub fn deliver(
        &mut self,
        wait: lash_vocabulary::sites::WaitId,
        outcome: EncodedPayload,
    ) -> Result<(), PoolError> {
        if self.pending.is_some() || !self.started {
            return Err(PoolError::breach(SequenceFault::NoRun));
        }
        if let Err(error) = self.effect_bound(outcome.0.len()) {
            self.discard_after(&error);
            return Err(error);
        }
        self.deliveries.push(Delivery { wait, outcome });
        Ok(())
    }

    /// Asks the worker to seal the run as it stands at this safe point: a
    /// park whose candidate did not cross with it, or a run's start before
    /// its first slice, whose start then crosses first, on its own. The
    /// seal's page exchanges are answered through
    /// [`Checkout::page_reply`] as a slice's are; it ends in
    /// [`RunStep::Sealed`].
    pub fn seal(&mut self) -> Result<RunStep, PoolError> {
        if self.pending.is_some() || !self.started || !self.deliveries.is_empty() {
            return Err(PoolError::breach(SequenceFault::NoRun));
        }
        let start = self.unsent_start.take();
        self.split_from(
            start,
            Split {
                deliveries: Vec::new().into_iter(),
                then: SplitThen::Seal,
            },
        )
    }

    fn effect_bound(&self, size: usize) -> Result<(), PoolError> {
        let bound = self.pool.config.protocol.max_effect_value_bytes;
        if size as u64 > bound {
            return Err(limit(WorkerLimit::EffectValue {
                size: size as u64,
                bound,
            }));
        }
        Ok(())
    }

    fn unexpected(&mut self, exchange: Exchange, message: &WorkerMessage) -> PoolError {
        self.discard();
        PoolError::breach(ProtocolBreach::Unexpected {
            exchange,
            found: message.kind(),
        })
    }
    /// Performs pure compiler/state work in the worker, under this checkout.
    pub fn prepare(
        &mut self,
        owner: VmOwner,
        request: EncodedPayload,
    ) -> Result<EncodedPayload, PoolError> {
        if self.started || self.prepared {
            return Err(PoolError::breach(SequenceFault::CheckoutAlreadyStarted));
        }
        self.owner = Some(owner.clone());
        self.phase_identity();
        self.prepared = true;
        match self.exchange(
            ParentMessage::Prepare { owner, request },
            self.pool.config.protocol.no_response_watchdog,
        )? {
            WorkerMessage::Prepared { response } => Ok(response),
            message => Err(PoolError::breach(ProtocolBreach::Unexpected {
                exchange: Exchange::Prepare,
                found: message.kind(),
            })),
        }
    }
    /// Physical stop, followed by bounded hard kill on silence. The broker
    /// retains the durable cancellation and completion winner.
    pub fn cancel(&mut self) -> Result<WorkerMessage, PoolError> {
        self.resettable = false;
        self.printed_bytes = 0;
        let grace = self.pool.config.deadlines.cancel_grace;
        let response = self
            .send(ParentMessage::Cancel, grace)
            .and_then(|()| self.answer(grace, false));
        let response = self.settle(response);
        self.discard_for(false);
        response
    }
    /// Call once the run stands at a safe point the broker stops at: its end,
    /// or a park that outlives this activation.
    pub fn release(mut self) -> Result<(), PoolError> {
        if (self.started || self.prepared) && !self.resettable {
            self.discard();
            return Err(PoolError::breach(SequenceFault::FailedWorkerReset));
        }
        self.send(
            ParentMessage::Reset,
            self.pool.config.protocol.no_response_watchdog,
        )?;
        let reply = self.receive_control(self.pool.config.protocol.no_response_watchdog);
        match reply {
            Ok(WorkerMessage::ResetDone { cpu_nanos }) => {
                lock(&self.pool.measurements).counters.resets += 1;
                if cpu_nanos < self.credited_cpu {
                    self.discard();
                    return Err(PoolError::breach(SequenceFault::CpuAccountingRegressed));
                }
                if let Err(error) = self.charge_cpu(cpu_nanos) {
                    self.discard_after(&error);
                    return Err(error);
                }
                self.credited_cpu = cpu_nanos;
                if let Some(worker) = &mut self.worker {
                    worker.cpu_nanos = cpu_nanos;
                }
                if let Some(worker) = self.worker.take() {
                    let mut state = lock(&self.pool.state);
                    if state.failed {
                        drop(state);
                        self.pool.discard(worker, true);
                    } else {
                        state.idle.push(worker);
                        self.pool.available.notify_all();
                    }
                }
                Ok(())
            }
            Ok(message) => {
                self.discard();
                Err(PoolError::breach(ProtocolBreach::Unexpected {
                    exchange: Exchange::Reset,
                    found: message.kind(),
                }))
            }
            Err(error) => {
                self.discard();
                Err(error)
            }
        }
    }
    fn send(&mut self, message: ParentMessage, timeout: Duration) -> Result<(), PoolError> {
        let frame = ParentFrame {
            header: self.outgoing.next_header_copy(),
            message,
        };
        let worker = self.worker.as_mut().ok_or_else(PoolError::eof)?;
        let encode_start = worker.phases.as_ref().map(|scope| scope.recorder.now());
        let bytes = worker
            .codec
            .encode_parent(&frame)
            .map_err(PoolError::from)?;
        if let (Some(scope), Some(start)) = (&worker.phases, encode_start) {
            scope.record("encode", start);
        }
        if matches!(
            frame.message,
            ParentMessage::Start(_) | ParentMessage::Prepare { .. }
        ) && bytes.len() > self.reservation
        {
            return Err(PoolError::QueueFull { bytes: bytes.len() });
        }
        self.send_encoded(
            &bytes,
            crate::measurements::IpcTraffic::parent_kind(&frame.message),
            timeout,
        )
    }
    /// Sends a frame encoded under the next outgoing header.
    fn send_encoded(
        &mut self,
        bytes: &[u8],
        kind: usize,
        timeout: Duration,
    ) -> Result<(), PoolError> {
        let worker = self.worker.as_mut().ok_or_else(PoolError::eof)?;
        self.outgoing.next_header();
        worker.send_encoded(bytes, kind, timeout)
    }
    fn receive_control(&mut self, timeout: Duration) -> Result<WorkerMessage, PoolError> {
        let frame = self
            .worker
            .as_mut()
            .ok_or_else(PoolError::eof)?
            .receive(Instant::now() + timeout)?;
        self.incoming
            .admit(&frame.header)
            .map_err(PoolError::breach)?;
        Ok(frame.message)
    }
    fn exchange(
        &mut self,
        message: ParentMessage,
        timeout: Duration,
    ) -> Result<WorkerMessage, PoolError> {
        self.resettable = false;
        self.printed_bytes = 0;
        let result = self
            .budget
            .wall(&self.pool.config)
            .and_then(|()| self.send(message, timeout))
            .and_then(|()| self.answer(timeout, true));
        self.settle(result)
    }
    /// [`Checkout::exchange`] of a frame already encoded under the next
    /// outgoing header.
    fn exchange_encoded(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<WorkerMessage, PoolError> {
        self.resettable = false;
        self.printed_bytes = 0;
        let result = self
            .budget
            .wall(&self.pool.config)
            .and_then(|()| {
                self.send_encoded(bytes, crate::measurements::IpcTraffic::RUN_KIND, timeout)
            })
            .and_then(|()| self.answer(timeout, true));
        self.settle(result)
    }
    /// An exchange's result: a failed one discards the worker. A worker
    /// that died after it began computing the run died running it.
    fn settle(
        &mut self,
        result: Result<WorkerMessage, PoolError>,
    ) -> Result<WorkerMessage, PoolError> {
        let result = match result {
            Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { evidence }))
                if self.started && self.computed =>
            {
                Err(InfrastructureOutcome::RunCrashedWorker { evidence }.into())
            }
            result => result,
        };
        if let Err(error) = &result {
            self.discard_after(error);
            #[cfg(feature = "testing")]
            if matches!(
                result,
                Err(PoolError::Infrastructure(
                    InfrastructureOutcome::WorkerLimitExceeded {
                        limit: WorkerLimit::Cpu { .. } | WorkerLimit::Deadline { .. }
                    }
                ))
            ) {
                eprintln!(
                    "WORKER_DEADLINE_REAPED elapsed_ms={:.3} cpu_accounted_ms={:.3}",
                    self.admitted_at.elapsed().as_secs_f64() * 1000.0,
                    self.budget.totals().1.as_secs_f64() * 1000.0
                );
            }
        }
        result
    }
    /// How long the worker may stay silent in `phase`: its configured
    /// compute or serialization deadline, else the exchange's `timeout`.
    fn phase_bound(&self, phase: Option<WorkerPhase>, timeout: Duration) -> Duration {
        match phase {
            Some(WorkerPhase::Computing) => self.pool.config.deadlines.compute,
            Some(WorkerPhase::Serializing) => self.pool.config.deadlines.serialization,
            Some(WorkerPhase::Responding) | None => timeout,
        }
    }
    /// The worker's answer to the request just sent, its progress and
    /// prints taken on the way. A `walled` answer is also held to the
    /// execution's held wall time.
    fn answer(&mut self, timeout: Duration, walled: bool) -> Result<WorkerMessage, PoolError> {
        // A worker answered a page exchange computes on, under a fresh
        // compute deadline: none ran while the parent served its pages. The
        // execution's wall time runs on.
        let mut phase = self.paged.take();
        let wall = walled.then(|| self.budget.wall_deadline(&self.pool.config));
        let bounded = |bound: Duration| {
            let deadline = Instant::now() + bound;
            wall.map_or(deadline, |wall| deadline.min(wall))
        };
        let mut deadline = bounded(self.phase_bound(phase, timeout));
        #[cfg(feature = "testing")]
        let mut phase_started = Instant::now();
        loop {
            let received = self
                .worker
                .as_mut()
                .ok_or_else(PoolError::eof)?
                .receive(deadline);
            let frame = match received {
                // The execution held its workers past its wall time, the
                // run's own limit (FIG-6786).
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive {
                    ..
                })) if wall.is_some_and(|wall| Instant::now() >= wall) => {
                    return Err(limit(self.pool.config.deadlines.wall_limit()));
                }
                // The run's own computation or serialization outlasted its
                // deadline: the same run meets it again (FIG-6523).
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive {
                    ..
                })) if matches!(
                    phase,
                    Some(WorkerPhase::Computing | WorkerPhase::Serializing)
                ) =>
                {
                    #[cfg(feature = "testing")]
                    self.pool.report_deadline(
                        &lock(&self.pool.state),
                        match phase {
                            Some(WorkerPhase::Computing) => "compute",
                            _ => "serialization",
                        },
                        phase_started.elapsed(),
                        &self.budget,
                    );
                    return Err(limit(crate::config::deadline_limit(
                        self.phase_bound(phase, timeout),
                    )));
                }
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive {
                    ..
                })) => {
                    return Err(InfrastructureOutcome::WorkerUnresponsive {
                        silent_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
                    }
                    .into());
                }
                other => other?,
            };
            self.incoming
                .admit(&frame.header)
                .map_err(PoolError::breach)?;
            match frame.message {
                WorkerMessage::Progress {
                    phase: next,
                    cpu_nanos,
                    work,
                } => {
                    let allowed = matches!(
                        (phase, next),
                        (None, WorkerPhase::Computing)
                            | (Some(WorkerPhase::Computing), WorkerPhase::Serializing)
                            | (Some(WorkerPhase::Serializing), WorkerPhase::Responding)
                    );
                    if !allowed {
                        return Err(PoolError::breach(SequenceFault::InvalidPhase));
                    }
                    if cpu_nanos < self.credited_cpu {
                        return Err(PoolError::breach(SequenceFault::CpuAccountingRegressed));
                    }
                    if let Some(work) = work {
                        lock(&self.pool.measurements)
                            .counters
                            .worker_work
                            .accumulate(work);
                    }
                    self.charge_cpu(cpu_nanos)?;
                    self.credited_cpu = cpu_nanos;
                    self.worker.as_mut().ok_or_else(PoolError::eof)?.cpu_nanos = cpu_nanos;
                    self.computed |= self.started && next == WorkerPhase::Computing;
                    if !self.execution_recorded && next == WorkerPhase::Computing {
                        if let Some(class_name) = self.execution_class {
                            let worker = self.worker.as_ref().ok_or_else(PoolError::eof)?;
                            let owner = self
                                .owner
                                .as_ref()
                                .ok_or_else(|| PoolError::breach(SequenceFault::MissingOwner))?;
                            lock(&self.pool.measurements).execution(ExecutionReceipt {
                                lease: self.lease().0,
                                class_name,
                                owner: owner.as_str().to_owned(),
                                pid: worker.pid(),
                                process_epoch: worker.process_epoch.clone(),
                            });
                        }
                        self.execution_recorded = true;
                    }
                    if next == WorkerPhase::Computing
                        && let Some(scope) = self
                            .worker
                            .as_mut()
                            .and_then(|worker| worker.phases.as_mut())
                    {
                        scope.point(if scope.effect.is_some() {
                            "resume_observed"
                        } else {
                            "compute_observed"
                        });
                        scope.effect = None;
                    }
                    phase = Some(next);
                    #[cfg(feature = "testing")]
                    {
                        phase_started = Instant::now();
                    }
                    deadline = bounded(self.phase_bound(phase, timeout));
                }
                WorkerMessage::Refused { refusal } => {
                    return Err(refusal.into_outcome().into());
                }
                WorkerMessage::Printed { payload } => {
                    // Each chunk crossed in one frame; the exchange's stream
                    // is held to the run's heap bound, never to a transport
                    // bound (FIG-4458).
                    self.printed_bytes = self.printed_bytes.saturating_add(payload.0.len() as u64);
                    if self.printed_bytes > self.print_budget {
                        return Err(limit(WorkerLimit::Observations));
                    }
                    self.printed.push(payload);
                }
                WorkerMessage::LimitExceeded { limit: exhausted } => return Err(limit(exhausted)),
                WorkerMessage::HostRead {
                    id,
                    kind,
                    request,
                    sites,
                } => {
                    if request.0.len() as u64 > self.pool.config.protocol.max_effect_value_bytes {
                        return Err(limit(WorkerLimit::EffectValue {
                            size: request.0.len() as u64,
                            bound: self.pool.config.protocol.max_effect_value_bytes,
                        }));
                    }
                    self.pending = Some(Pending::Read(id));
                    return Ok(WorkerMessage::HostRead {
                        id,
                        kind,
                        request,
                        sites,
                    });
                }
                WorkerMessage::Pages { id, request } => {
                    // A page exchange is asked mid-computation and adds no
                    // phase of its own. The codec held the request to its
                    // batch bound; the page service owns the activation's
                    // deadline.
                    if phase != Some(WorkerPhase::Computing) {
                        return Err(PoolError::breach(SequenceFault::InvalidPhase));
                    }
                    self.pending = Some(Pending::Pages(id));
                    self.paged = phase;
                    return Ok(WorkerMessage::Pages { id, request });
                }
                // Do not consult exit status after a fully decoded step.
                message @ (WorkerMessage::Started
                | WorkerMessage::Parked { .. }
                | WorkerMessage::Slice { .. }
                | WorkerMessage::Ended { .. }
                | WorkerMessage::Delivered
                | WorkerMessage::Sealed { .. }
                | WorkerMessage::Prepared { .. }) => {
                    if let Some(scope) = self.phase_scope() {
                        scope.point(match &message {
                            WorkerMessage::Ended { .. } => "document_end",
                            WorkerMessage::Parked { .. } => "document_park",
                            _ => "step_observed",
                        });
                    }
                    self.resettable = true;
                    return Ok(message);
                }
                WorkerMessage::Cancelled => {
                    self.discard_for(false);
                    self.budget.replacement(false);
                    return Ok(WorkerMessage::Cancelled);
                }
                message @ (WorkerMessage::Ready { .. } | WorkerMessage::ResetDone { .. }) => {
                    return Err(PoolError::breach(ProtocolBreach::Unexpected {
                        exchange: Exchange::Run,
                        found: message.kind(),
                    }));
                }
            }
        }
    }
    fn discard(&mut self) {
        self.discard_for(true);
    }
    /// Discards the worker after `error`: the run's own typed end, a limit
    /// of its own or a refusal of its own inputs, is no failure of the
    /// pool's and spends nothing of its restart window (FIG-6786).
    fn discard_after(&mut self, error: &PoolError) {
        let own = match error {
            PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded { limit }) => {
                !limit.is_host_verdict()
            }
            PoolError::Infrastructure(InfrastructureOutcome::RunRefused { .. }) => true,
            _ => false,
        };
        self.discard_for(!own);
    }
    fn discard_for(&mut self, failed: bool) {
        if let Some(worker) = self.worker.take() {
            if let Some(scope) = &worker.phases {
                scope.point("loss");
            }
            let cpu = self.pool.discard(worker, failed);
            let _ = self.charge_cpu(cpu);
            self.credited_cpu = cpu;
            if self.started || self.prepared {
                self.budget.replacement(true);
            }
        }
        if std::mem::take(&mut self.holding) {
            self.budget.unhold();
        }
        self.pending = None;
        self.split = None;
        self.resettable = false;
    }
}
impl Drop for Checkout {
    fn drop(&mut self) {
        self.discard_for(false);
    }
}

/// Runtime-only worker checkout.
///
/// The VM service checks a worker out for each execution; a host only starts
/// and prewarms the pool. The trait is not part of the lash facade, and its
/// impl is hidden from docs because it is support plumbing, not host surface
/// (ADR 0051).
pub mod runtime_ops {
    use super::*;

    pub trait WorkerPoolRuntimeOps {
        /// Reserve the complete encoded input size, including the frame envelope.
        /// Queue admission checks both items and bytes before retaining any input.
        fn checkout(
            &self,
            queued_bytes: usize,
            owner_epoch: OwnerEpoch,
            frame_epoch: FrameEpoch,
            budget: ExecutionBudget,
        ) -> Result<Checkout, PoolError>;
    }

    #[doc(hidden)]
    impl WorkerPoolRuntimeOps for WorkerPool {
        fn checkout(
            &self,
            queued_bytes: usize,
            owner_epoch: OwnerEpoch,
            frame_epoch: FrameEpoch,
            budget: ExecutionBudget,
        ) -> Result<Checkout, PoolError> {
            #[cfg(feature = "testing")]
            let checkout_started = Instant::now();
            let phase_start = self.0.phases.as_ref().map(crate::phases::Recorder::now);
            let queue_started = Instant::now();
            let deadline = Instant::now() + self.0.config.deadlines.checkout;
            let mut state = lock(&self.0.state);
            let mut queued = false;
            loop {
                if queued && Instant::now() >= deadline {
                    #[cfg(feature = "testing")]
                    self.0
                        .report_deadline(&state, "checkout", checkout_started.elapsed(), &budget);
                    self.0
                        .unqueue(&mut state, queued, queued_bytes, queue_started);
                    return Err(PoolError::CheckoutTimedOut);
                }
                if state.failed {
                    self.0
                        .unqueue(&mut state, queued, queued_bytes, queue_started);
                    return Err(PoolError::RestartStorm);
                }
                let worker = if let Some(worker) = state.idle.pop() {
                    Some(worker)
                } else if state.workers < self.0.capacity {
                    state.workers += 1;
                    self.0
                        .unqueue(&mut state, queued, queued_bytes, queue_started);
                    queued = false;
                    drop(state);
                    let spawned = self.0.spawn_ready_until(deadline);
                    state = lock(&self.0.state);
                    match spawned {
                        Ok(worker) if !state.failed => Some(worker),
                        Ok(mut worker) => {
                            drop(state);
                            worker.terminate();
                            state = lock(&self.0.state);
                            state.workers -= 1;
                            return Err(PoolError::RestartStorm);
                        }
                        Err(error) => {
                            #[cfg(feature = "testing")]
                            if matches!(
                                error,
                                PoolError::Infrastructure(
                                    InfrastructureOutcome::WorkerUnresponsive { .. }
                                )
                            ) {
                                self.0.report_deadline(
                                    &state,
                                    "checkout",
                                    checkout_started.elapsed(),
                                    &budget,
                                );
                            }
                            state.workers -= 1;
                            if !matches!(&error, PoolError::Infrastructure(outcome) if outcome.deployment_fault().is_some())
                            {
                                self.0.failed(&mut state);
                            }
                            self.0.available.notify_all();
                            return Err(if state.failed {
                                PoolError::RestartStorm
                            } else {
                                error
                            });
                        }
                    }
                } else {
                    None
                };
                if let Some(mut worker) = worker {
                    self.0
                        .unqueue(&mut state, queued, queued_bytes, queue_started);
                    if let Err(error) = budget.admit(&self.0.config) {
                        #[cfg(feature = "testing")]
                        if matches!(
                            error,
                            PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
                                limit: WorkerLimit::Cpu { .. }
                            })
                        ) {
                            self.0.report_deadline(
                                &state,
                                "cumulative_cpu",
                                Duration::ZERO,
                                &budget,
                            );
                        }
                        state.idle.push(worker);
                        self.0.available.notify_all();
                        return Err(error);
                    }
                    let lease = ExecutionLease(state.next_lease);
                    state.next_lease = state
                        .next_lease
                        .checked_add(1)
                        .ok_or_else(|| PoolError::breach(SequenceFault::LeaseSpaceExhausted))?;
                    {
                        let mut measurements = lock(&self.0.measurements);
                        measurements.counters.checkouts += 1;
                        if worker.used {
                            measurements.counters.reuses += 1;
                        }
                    }
                    if let Some(scope) = &mut worker.phases {
                        scope.lease = lease.0;
                        scope.owner_epoch = owner_epoch.0;
                        scope.frame_epoch = frame_epoch.0;
                        scope.owner = None;
                        scope.document = None;
                        scope.effect = None;
                        if let Some(start) = phase_start {
                            scope.record("checkout", start);
                        }
                    }
                    worker.used = true;
                    budget.hold();
                    let credited_cpu = worker.cpu_nanos;
                    return Ok(Checkout {
                        #[cfg(feature = "testing")]
                        admitted_at: Instant::now(),
                        #[cfg(feature = "testing")]
                        measured_cpu_receipt: std::cell::Cell::new(credited_cpu),
                        pool: self.0.clone(),
                        worker: Some(worker),
                        budget,
                        holding: true,
                        credited_cpu,
                        outgoing: MessageFence::new(lease, owner_epoch, frame_epoch),
                        incoming: MessageFence::new(lease, owner_epoch, frame_epoch),
                        reservation: queued_bytes,
                        prepared: false,
                        started: false,
                        computed: false,
                        unsent_start: None,
                        deliveries: Vec::new(),
                        paged: None,
                        grants: Vec::new(),
                        pending: None,
                        split: None,
                        // Nothing has crossed: the worker stands where its
                        // last reset left it.
                        resettable: true,
                        owner: None,
                        document: None,
                        printed: Vec::new(),
                        sites: Vec::new(),
                        print_budget: u64::MAX,
                        printed_bytes: 0,
                        execution_class: None,
                        execution_recorded: false,
                    });
                }
                if !queued {
                    if state.queued_items >= self.0.config.max_queue_items
                        || queued_bytes
                            > self
                                .0
                                .config
                                .max_queue_bytes
                                .saturating_sub(state.queued_bytes)
                    {
                        return Err(PoolError::QueueFull {
                            bytes: queued_bytes,
                        });
                    }
                    lock(&self.0.measurements).counters.queue_waits += 1;
                    state.queued_items += 1;
                    state.queued_bytes += queued_bytes;
                    queued = true;
                }
                let Some(wait) = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                else {
                    #[cfg(feature = "testing")]
                    self.0
                        .report_deadline(&state, "checkout", checkout_started.elapsed(), &budget);
                    self.0
                        .unqueue(&mut state, queued, queued_bytes, queue_started);
                    return Err(PoolError::CheckoutTimedOut);
                };
                let (next, _) = self
                    .0
                    .available
                    .wait_timeout(state, wait)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = next;
            }
        }
    }
}
