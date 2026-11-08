/// version_surface = "coexist"
/// version_guard(items(LASH_VM_CLIENT_POOL_DOMAIN_VERSION, measurements))
const LASH_VM_CLIENT_POOL_DOMAIN_VERSION: &str = "lash-vm-client/pool-v1";

mod documents;

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
    restarts: VecDeque<(Instant, InfrastructureOutcome)>,
    failed: bool,
}
impl State {
    fn restart_storm(&self) -> PoolError {
        PoolError::RestartStorm {
            first: Box::new(
                self.restarts
                    .front()
                    .map(|(_, cause)| cause.clone())
                    .unwrap_or_else(|| PoolError::eof().into_outcome()),
            ),
        }
    }
}
struct Pool {
    phases: Option<crate::phases::Recorder>,
    epoch: u64,
    measurements: SharedMeasurements,
    config: PoolConfig,
    /// The pool start's frames, encoded once from the parent and shared by
    /// prewarm and replacement workers.
    packages: Arc<Vec<Vec<u8>>>,
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
        let packages = held.pool_frames(&codec)?;
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
    fn failed(&self, state: &mut State, cause: &PoolError) {
        let now = Instant::now();
        while state
            .restarts
            .front()
            .is_some_and(|(time, _)| now.duration_since(*time) >= self.config.restart_window)
        {
            state.restarts.pop_front();
        }
        state
            .restarts
            .push_back((now, cause.clone().into_outcome()));
        if state.restarts.len() >= self.config.max_restarts {
            state.failed = true;
        }
    }
    fn discard(&self, mut worker: Worker, failed: bool, cause: Option<&PoolError>) -> u64 {
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
            self.failed(&mut state, cause.unwrap_or(&PoolError::eof()));
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
                Err(error) => {
                    state.workers -= 1;
                    self.failed(&mut state, &error);
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
    /// No task is ready. On the persistent heap, `seal` is the pages that
    /// crossed with the park: the caller admits them and hands their
    /// grants to [`Checkout::grant`]. A checkpoint asks for its root with
    /// [`Checkout::seal`].
    Parked {
        park: EncodedPayload,
        meters: RunMeters,
        seal: Option<SealBatch>,
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
    pool: Arc<Pool>,
    state: CheckoutState,
    budget: ExecutionBudget,
    credited_cpu: u64,
    outgoing: MessageFence,
    incoming: MessageFence,
    reservation: usize,
    printed: Vec<EncodedPayload>,
    /// Position hints and exact count deltas, as the run frames carried them.
    sites: Vec<EncodedPayload>,
    /// The printed bytes the current exchange has handed over.
    printed_bytes: u64,
}

enum CheckoutState {
    Live(Box<Lease>),
    Discarded,
}

struct Lease {
    worker: Worker,
    phase: LeasePhase,
}

enum LeasePhase {
    Fresh,
    Preparing { owner: VmOwner },
    Prepared { owner: VmOwner },
    Run(Box<Run>),
}

struct Run {
    owner: VmOwner,
    document: String,
    class: ExecutionClass,
    /// Once computing is observed, a worker death belongs to this run.
    computed: bool,
    state: RunState,
    document_sources: std::collections::BTreeMap<lash_kernel_value::DocumentId, EncodedPayload>,
    grants: Vec<PageCapability>,
    print_budget: u64,
}

enum RunState {
    Ready(ReadyRun),
    InFlight,
    AwaitHost(HostReadId),
    /// Pages always resume Computing, including a split Start or Deliver.
    AwaitPages {
        id: PageExchangeId,
        resume: Continuation,
    },
}

struct ReadyRun {
    start: StartDispatch,
    deliveries: Vec<Delivery>,
}

enum StartDispatch {
    Queued(Box<Start>),
    Sent,
}

impl StartDispatch {
    fn take(&mut self) -> Option<Box<Start>> {
        match std::mem::replace(self, Self::Sent) {
            Self::Queued(start) => Some(start),
            Self::Sent => None,
        }
    }
}

/// Owned by the exchange, then by its page wait until that exchange resumes.
enum Continuation {
    Run,
    Split(Split),
}

impl CheckoutState {
    fn live(&self) -> Result<&Lease, PoolError> {
        match self {
            Self::Live(lease) => Ok(lease),
            Self::Discarded => Err(PoolError::eof()),
        }
    }
    fn live_mut(&mut self) -> Result<&mut Lease, PoolError> {
        match self {
            Self::Live(lease) => Ok(lease),
            Self::Discarded => Err(PoolError::eof()),
        }
    }
    fn run(&self) -> Result<&Run, PoolError> {
        match &self.live()?.phase {
            LeasePhase::Run(run) => Ok(run),
            _ => Err(PoolError::breach(SequenceFault::NoRun)),
        }
    }
    fn run_mut(&mut self) -> Result<&mut Run, PoolError> {
        match &mut self.live_mut()?.phase {
            LeasePhase::Run(run) => Ok(run),
            _ => Err(PoolError::breach(SequenceFault::NoRun)),
        }
    }
    fn ready(&mut self) -> Result<&mut ReadyRun, PoolError> {
        match &mut self.run_mut()?.state {
            RunState::Ready(ready) => Ok(ready),
            _ => Err(PoolError::breach(SequenceFault::NoRun)),
        }
    }
    fn resume_pages(&mut self, id: PageExchangeId) -> Result<Continuation, PoolError> {
        let run = self
            .run_mut()
            .map_err(|_| PoolError::breach(SequenceFault::WrongPageExchange))?;
        match std::mem::replace(&mut run.state, RunState::InFlight) {
            RunState::AwaitPages { id: wait, resume } if wait == id => Ok(resume),
            state => {
                run.state = state;
                Err(PoolError::breach(SequenceFault::WrongPageExchange))
            }
        }
    }
    fn resettable(&self) -> bool {
        match self {
            Self::Live(lease) => match &lease.phase {
                LeasePhase::Fresh | LeasePhase::Prepared { .. } => true,
                LeasePhase::Run(run) => matches!(run.state, RunState::Ready(_)),
                LeasePhase::Preparing { .. } => false,
            },
            Self::Discarded => false,
        }
    }
    fn begin_exchange(&mut self) {
        if let Ok(run) = self.run_mut() {
            run.state = RunState::InFlight;
        }
    }
    fn safe_point(&mut self) {
        if let Ok(run) = self.run_mut() {
            run.state = RunState::Ready(ReadyRun {
                start: StartDispatch::Sent,
                deliveries: Vec::new(),
            });
        }
    }
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
        self.state
            .live()
            .ok()
            .and_then(|lease| lease.worker.phases.clone())
    }
    fn phase_identity(&mut self) {
        if let Ok(lease) = self.state.live_mut()
            && let Some(scope) = &mut lease.worker.phases
        {
            let (owner, document) = match &lease.phase {
                LeasePhase::Fresh => (None, None),
                LeasePhase::Preparing { owner } | LeasePhase::Prepared { owner } => {
                    (Some(owner.as_str().to_owned()), None)
                }
                LeasePhase::Run(run) => (
                    Some(run.owner.as_str().to_owned()),
                    Some(run.document.clone()),
                ),
            };
            scope.owner = owner;
            scope.document = document;
        }
    }
    fn charge_cpu(&mut self, cpu_nanos: u64) -> Result<(), PoolError> {
        // Charging records the delta even when it refuses the execution.
        // Advance the receipt first so reaping cannot charge it again.
        let delta = cpu_nanos.saturating_sub(std::mem::replace(&mut self.credited_cpu, cpu_nanos));
        #[cfg(feature = "testing")]
        {
            let mut state = lock(&self.pool.state);
            state.measured_cpu_nanos = state.measured_cpu_nanos.saturating_add(delta);
        }
        let result = self.budget.charge(delta, &self.pool.config);
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
        self.state
            .live()?
            .worker
            .pipe
            .try_clone()
            .map_err(PoolError::io)
    }
    pub fn pid(&self) -> Option<u32> {
        self.state.live().ok().map(|lease| lease.worker.pid())
    }
    pub fn budget(&self) -> &ExecutionBudget {
        &self.budget
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
        self.drive(slice, cancel)
    }

    /// The exchange of one slice, its start and its deliveries: in one
    /// frame, or, when they outgrow one, the start and each delivery in an
    /// exchange of its own before the slice.
    fn drive(&mut self, slice: u64, cancel: bool) -> Result<RunStep, PoolError> {
        let timeout = self.pool.config.protocol.no_response_watchdog;
        let ready = self.state.ready()?;
        let start = ready.start.take();
        let deliveries = std::mem::take(&mut ready.deliveries);
        #[cfg(feature = "perf-witness")]
        self.count_warm(start.as_deref());
        let scope = self.phase_scope();
        let encode_start = scope.as_ref().map(|scope| scope.recorder.now());
        let together = self
            .state
            .live()
            .ok()
            .map(|lease| &lease.worker)
            .and_then(|worker| {
                worker
                    .codec
                    .encode_borrowed(&BorrowedParentFrame {
                        header: self.outgoing.next_header_copy(),
                        message: BorrowedParentMessage::Run {
                            start: start.as_deref(),
                            deliveries: &deliveries,
                            slice,
                            cancel,
                            sealed: &self.state.run().ok()?.grants,
                        },
                    })
                    .ok()
                    .filter(|bytes| worker.codec.check_frame(bytes).is_ok())
            });
        if let (Some(scope), Some(start)) = (&scope, encode_start) {
            scope.record("encode_run", start);
        }
        if let Some(bytes) = together {
            self.state.run_mut()?.grants.clear();
            let message = self.exchange_encoded(&bytes, timeout)?;
            return self.finish_exchange(message, Continuation::Run, Exchange::Run);
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
        match start {
            Some(start) => self.exchange_step(
                ParentMessage::Start(start),
                Continuation::Split(split),
                None,
            ),
            None => self.split_next(split),
        }
    }

    fn split_next(&mut self, mut split: Split) -> Result<RunStep, PoolError> {
        if let Some(Delivery { wait, outcome }) = split.deliveries.next() {
            return self.exchange_step(
                ParentMessage::Deliver { wait, outcome },
                Continuation::Split(split),
                None,
            );
        }
        let message = match split.then {
            SplitThen::Run { slice, cancel } => ParentMessage::Run {
                start: None,
                deliveries: Vec::new(),
                slice,
                cancel,
                sealed: std::mem::take(&mut self.state.run_mut()?.grants),
            },
            SplitThen::Seal => ParentMessage::Seal,
        };
        self.exchange_step(message, Continuation::Run, None)
    }

    /// The continuation stays with its page wait, never in a separate slot.
    fn exchange_step(
        &mut self,
        message: ParentMessage,
        continuation: Continuation,
        phase: Option<WorkerPhase>,
    ) -> Result<RunStep, PoolError> {
        let exchange = match message {
            ParentMessage::Start(_) => Exchange::Run,
            ParentMessage::Deliver { .. } => Exchange::Deliver,
            ParentMessage::Pages { .. } => Exchange::Pages,
            ParentMessage::Seal => Exchange::Seal,
            _ => Exchange::Run,
        };
        let timeout = self.pool.config.protocol.no_response_watchdog;
        let message = self.exchange(message, timeout, phase)?;
        self.finish_exchange(message, continuation, exchange)
    }

    fn finish_exchange(
        &mut self,
        message: WorkerMessage,
        continuation: Continuation,
        exchange: Exchange,
    ) -> Result<RunStep, PoolError> {
        if let WorkerMessage::Pages { id, request } = message {
            self.state.run_mut()?.state = RunState::AwaitPages {
                id,
                resume: continuation,
            };
            return Ok(RunStep::Pages { id, request });
        }
        match continuation {
            Continuation::Split(split) => match message {
                WorkerMessage::Started | WorkerMessage::Delivered => self.split_next(split),
                message => Err(self.unexpected(exchange, &message)),
            },
            Continuation::Run => self.step(message),
        }
    }

    /// Answers the machine's pending host read; the slice runs on. The
    /// parent's work on the answer may have taken any time: no worker
    /// deadline ran while the parent owned the read.
    pub fn host_answer(
        &mut self,
        id: HostReadId,
        answer: EncodedPayload,
    ) -> Result<RunStep, PoolError> {
        if !matches!(self.state.run().map(|run| &run.state), Ok(RunState::AwaitHost(wait)) if *wait == id)
        {
            return Err(PoolError::breach(SequenceFault::WrongReadId));
        }
        if let Err(error) = self.effect_bound(answer.0.len()) {
            self.discard_after(&error);
            return Err(error);
        }
        if let Some(scope) = self.phase_scope() {
            scope.point("answer");
        }
        self.exchange_step(
            ParentMessage::HostAnswer { id, answer },
            Continuation::Run,
            None,
        )
    }

    /// Answers the machine's pending page exchange with the page service's
    /// reply; the slice runs on, its statement and native stack intact. No
    /// worker deadline ran while the parent served the pages.
    pub fn page_reply(
        &mut self,
        id: PageExchangeId,
        reply: PageReply,
    ) -> Result<RunStep, PoolError> {
        let resume = self.state.resume_pages(id)?;
        self.exchange_step(
            ParentMessage::Pages { id, reply },
            resume,
            Some(WorkerPhase::Computing),
        )
    }

    fn step(&mut self, message: WorkerMessage) -> Result<RunStep, PoolError> {
        let (step, sites) = match message {
            WorkerMessage::HostRead {
                id,
                kind,
                request,
                sites,
            } => (RunStep::HostRead { id, kind, request }, sites),
            WorkerMessage::Parked {
                park,
                meters,
                seal,
                sites,
            } => (RunStep::Parked { park, meters, seal }, sites),
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
        match &step {
            RunStep::Parked { meters, .. }
            | RunStep::Slice { meters }
            | RunStep::Ended { meters, .. } => self.budget.observe(*meters),
            _ => {}
        }
        #[cfg(feature = "perf-witness")]
        self.count_checkpoint_root(&step);
        self.sites.extend(sites);
        Ok(step)
    }

    /// Counts the `CheckpointRoot` page a seal hands over: it crosses in
    /// the seal's batch.
    #[cfg(feature = "perf-witness")]
    fn count_checkpoint_root(&self, step: &RunStep) {
        let RunStep::Sealed {
            seal: Some(seal),
            candidate,
        } = step
        else {
            return;
        };
        if let Some(root) = seal.pages.iter().find(|page| page.id() == *candidate) {
            let mut measurements = lock(&self.pool.measurements);
            measurements.counters.checkpoint_roots += 1;
            measurements.counters.checkpoint_root_bytes += root.envelope().len() as u64;
        }
    }

    /// Counts the pages `start` carries to warm the worker, as it crosses.
    #[cfg(feature = "perf-witness")]
    fn count_warm(&self, start: Option<&Start>) {
        if let Some(start) = start {
            lock(&self.pool.measurements).counters.warm_pages += start.warm.len() as u64;
        }
    }

    /// Holds `sealed`, the stage capabilities the page service issued for
    /// the seal batch that crossed with the last step, until they cross
    /// with the run's next slice (FIG-6288).
    pub fn grant(&mut self, sealed: Vec<PageCapability>) {
        if let Ok(run) = self.state.run_mut() {
            run.grants = sealed;
        }
    }

    /// Hands the machine one committed outcome, with the run's next slice:
    /// the machine takes it before it runs on, or drops it when its wait
    /// had been withdrawn.
    pub fn deliver(
        &mut self,
        wait: lash_kernel_value::sites::WaitId,
        outcome: EncodedPayload,
    ) -> Result<(), PoolError> {
        self.state.ready()?;
        if let Err(error) = self.effect_bound(outcome.0.len()) {
            self.discard_after(&error);
            return Err(error);
        }
        self.state
            .ready()?
            .deliveries
            .push(Delivery { wait, outcome });
        Ok(())
    }

    /// Asks the worker to seal the run as it stands at this safe point: a
    /// park whose candidate did not cross with it, or a run's start before
    /// its first slice, whose start then crosses first, on its own. The
    /// seal's page exchanges are answered through
    /// [`Checkout::page_reply`] as a slice's are; it ends in
    /// [`RunStep::Sealed`].
    pub fn seal(&mut self) -> Result<RunStep, PoolError> {
        let ready = self.state.ready()?;
        if !ready.deliveries.is_empty() {
            return Err(PoolError::breach(SequenceFault::NoRun));
        }
        let start = ready.start.take();
        #[cfg(feature = "perf-witness")]
        self.count_warm(start.as_deref());
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
        if !matches!(self.state.live()?.phase, LeasePhase::Fresh) {
            return Err(PoolError::breach(SequenceFault::CheckoutAlreadyStarted));
        }
        self.state.live_mut()?.phase = LeasePhase::Preparing {
            owner: owner.clone(),
        };
        self.phase_identity();
        match self.exchange(
            ParentMessage::Prepare {
                owner: owner.clone(),
                request,
            },
            self.pool.config.protocol.no_response_watchdog,
            None,
        )? {
            WorkerMessage::Prepared { response } => {
                self.state.live_mut()?.phase = LeasePhase::Prepared { owner };
                Ok(response)
            }
            message => Err(self.unexpected(Exchange::Prepare, &message)),
        }
    }
    /// Physical stop, followed by bounded hard kill on silence. The broker
    /// retains the durable cancellation and completion winner.
    pub fn cancel(&mut self) -> Result<WorkerMessage, PoolError> {
        let phase = matches!(
            self.state.run().map(|run| &run.state),
            Ok(RunState::AwaitPages { .. })
        )
        .then_some(WorkerPhase::Computing);
        self.state.begin_exchange();
        self.printed_bytes = 0;
        let grace = self.pool.config.deadlines.cancel_grace;
        let response = self
            .send(ParentMessage::Cancel, grace)
            .and_then(|()| self.answer(grace, false, phase));
        let response = self.settle(response);
        self.discard_for(false, None);
        response
    }
    /// Call once the run stands at a safe point the broker stops at: its end,
    /// or a park that outlives this activation.
    pub fn release(mut self) -> Result<(), PoolError> {
        if !self.state.resettable() {
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
                if let CheckoutState::Live(mut lease) =
                    std::mem::replace(&mut self.state, CheckoutState::Discarded)
                {
                    lease.worker.cpu_nanos = cpu_nanos;
                    self.budget.unhold();
                    let mut state = lock(&self.pool.state);
                    if state.failed {
                        drop(state);
                        self.pool.discard(lease.worker, true, None);
                    } else {
                        state.idle.push(lease.worker);
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
        let worker = &mut self.state.live_mut()?.worker;
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
        let worker = &mut self.state.live_mut()?.worker;
        self.outgoing.next_header();
        worker.send_encoded(bytes, kind, timeout)
    }
    fn receive_control(&mut self, timeout: Duration) -> Result<WorkerMessage, PoolError> {
        let frame = self
            .state
            .live_mut()?
            .worker
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
        phase: Option<WorkerPhase>,
    ) -> Result<WorkerMessage, PoolError> {
        self.state.begin_exchange();
        self.printed_bytes = 0;
        let result = self
            .budget
            .wall(&self.pool.config)
            .and_then(|()| self.send(message, timeout))
            .and_then(|()| self.answer(timeout, true, phase));
        self.settle(result)
    }
    /// [`Checkout::exchange`] of a frame already encoded under the next
    /// outgoing header.
    fn exchange_encoded(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<WorkerMessage, PoolError> {
        self.state.begin_exchange();
        self.printed_bytes = 0;
        let result = self
            .budget
            .wall(&self.pool.config)
            .and_then(|()| {
                self.send_encoded(bytes, crate::measurements::IpcTraffic::RUN_KIND, timeout)
            })
            .and_then(|()| self.answer(timeout, true, None));
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
                if self.state.run().is_ok_and(|run| run.computed) =>
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
    fn answer(
        &mut self,
        timeout: Duration,
        walled: bool,
        mut phase: Option<WorkerPhase>,
    ) -> Result<WorkerMessage, PoolError> {
        // A worker answered a page exchange computes on, under a fresh
        // compute deadline: none ran while the parent served its pages. The
        // execution's wall time runs on.
        let wall = walled.then(|| self.budget.wall_deadline(&self.pool.config));
        let bounded = |bound: Duration| {
            let deadline = Instant::now() + bound;
            wall.map_or(deadline, |wall| deadline.min(wall))
        };
        let mut deadline = bounded(self.phase_bound(phase, timeout));
        #[cfg(feature = "testing")]
        let mut phase_started = Instant::now();
        loop {
            let received = self.state.live_mut()?.worker.receive(deadline);
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
                WorkerMessage::Documents { documents } => {
                    if phase != Some(WorkerPhase::Computing) || self.state.run().is_err() {
                        return Err(PoolError::breach(SequenceFault::InvalidPhase));
                    }
                    self.answer_documents(documents, timeout)?;
                    deadline = bounded(self.phase_bound(phase, timeout));
                }
                WorkerMessage::Progress {
                    phase: next,
                    cpu_nanos,
                    meters,
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
                            .accumulate(*work);
                    }
                    if let Some(meters) = meters {
                        self.budget.observe(meters);
                    }
                    self.charge_cpu(cpu_nanos)?;
                    self.state.live_mut()?.worker.cpu_nanos = cpu_nanos;
                    if next == WorkerPhase::Computing {
                        let lease_id = self.lease().0;
                        let lease = self.state.live_mut()?;
                        if let LeasePhase::Run(run) = &mut lease.phase
                            && !run.computed
                        {
                            lock(&self.pool.measurements).execution(ExecutionReceipt {
                                lease: lease_id,
                                class_name: run.class,
                                owner: run.owner.as_str().to_owned(),
                                pid: lease.worker.pid(),
                                process_epoch: lease.worker.process_epoch.clone(),
                            });
                            run.computed = true;
                        }
                    }
                    if next == WorkerPhase::Computing
                        && let Some(scope) = self
                            .state
                            .live_mut()
                            .ok()
                            .map(|lease| &mut lease.worker)
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
                    if self.printed_bytes
                        > self.state.run().map_or(u64::MAX, |run| run.print_budget)
                    {
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
                    self.state.run_mut()?.state = RunState::AwaitHost(id);
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
                    self.state.safe_point();
                    return Ok(message);
                }
                WorkerMessage::Cancelled => {
                    self.discard_for(false, None);
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
        self.discard_for(true, None);
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
        self.discard_for(!own, Some(error));
    }
    fn discard_for(&mut self, failed: bool, cause: Option<&PoolError>) {
        if let CheckoutState::Live(lease) =
            std::mem::replace(&mut self.state, CheckoutState::Discarded)
        {
            if let Some(scope) = &lease.worker.phases {
                scope.point("loss");
            }
            let cpu = self.pool.discard(lease.worker, failed, cause);
            let _ = self.charge_cpu(cpu);
            if !matches!(lease.phase, LeasePhase::Fresh) {
                self.budget.replacement(true);
            }
            self.budget.unhold();
        }
    }
}
impl Drop for Checkout {
    fn drop(&mut self) {
        self.discard_for(false, None);
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
            lash_core_execution::facade_support::turn::sync_phase("worker checkout wait", || {
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
                        self.0.report_deadline(
                            &state,
                            "checkout",
                            checkout_started.elapsed(),
                            &budget,
                        );
                        self.0
                            .unqueue(&mut state, queued, queued_bytes, queue_started);
                        return Err(PoolError::CheckoutTimedOut);
                    }
                    if state.failed {
                        self.0
                            .unqueue(&mut state, queued, queued_bytes, queue_started);
                        return Err(state.restart_storm());
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
                                return Err(state.restart_storm());
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
                                    self.0.failed(&mut state, &error);
                                }
                                self.0.available.notify_all();
                                return Err(if state.failed {
                                    state.restart_storm()
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
                                PoolError::Infrastructure(
                                    InfrastructureOutcome::WorkerLimitExceeded {
                                        limit: WorkerLimit::Cpu { .. }
                                    }
                                )
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
                            pool: self.0.clone(),
                            state: CheckoutState::Live(Box::new(Lease {
                                worker,
                                phase: LeasePhase::Fresh,
                            })),
                            budget,
                            credited_cpu,
                            outgoing: MessageFence::new(lease, owner_epoch, frame_epoch),
                            incoming: MessageFence::new(lease, owner_epoch, frame_epoch),
                            reservation: queued_bytes,
                            printed: Vec::new(),
                            sites: Vec::new(),
                            printed_bytes: 0,
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
                        self.0.report_deadline(
                            &state,
                            "checkout",
                            checkout_started.elapsed(),
                            &budget,
                        );
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
            })
        }
    }
}
