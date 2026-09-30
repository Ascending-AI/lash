use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::ipc::Worker;
use crate::measurements::{
    ExecutionClass, ExecutionReceipt, Measurements, PoolMeasurements, SharedMeasurements,
};
use crate::{PoolConfig, PoolError};
use lash_vm_protocol::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// Parent-owned accounting. Carry this same value across substrate re-drives
/// and replacement checkouts. The pool never rewinds it or retries execution.
#[derive(Clone, Debug, Default)]
pub struct ExecutionBudget(Arc<Mutex<BudgetTotals>>);
#[derive(Debug, Default)]
struct BudgetTotals {
    attempts: u32,
    cpu_nanos: u64,
    replacement: bool,
    unknown_cpu_attempts: u32,
}
impl ExecutionBudget {
    pub fn totals(&self) -> (u32, Duration) {
        let totals = lock(&self.0);
        (totals.attempts, Duration::from_nanos(totals.cpu_nanos))
    }
    pub fn recovery_totals(
        &self,
    ) -> lash_core_execution::store::worker_recovery::WorkerRecoveryTotals {
        let totals = lock(&self.0);
        lash_core_execution::store::worker_recovery::WorkerRecoveryTotals {
            attempts: totals.attempts,
            cpu_nanos: totals.cpu_nanos,
            replacement: totals.replacement,
            unknown_cpu_attempts: totals.unknown_cpu_attempts,
        }
    }
    pub fn from_recovery(
        totals: lash_core_execution::store::worker_recovery::WorkerRecoveryTotals,
    ) -> Self {
        Self(Arc::new(Mutex::new(BudgetTotals {
            attempts: totals.attempts,
            cpu_nanos: totals.cpu_nanos,
            replacement: totals.replacement,
            unknown_cpu_attempts: totals.unknown_cpu_attempts,
        })))
    }
    pub fn restored(attempts: u32, cpu: Duration) -> Self {
        Self(Arc::new(Mutex::new(BudgetTotals {
            attempts,
            cpu_nanos: nanos(cpu),
            replacement: attempts > 0,
            unknown_cpu_attempts: 0,
        })))
    }
    fn admit(&self, config: &PoolConfig) -> Result<(), PoolError> {
        let mut totals = lock(&self.0);
        if totals.replacement && totals.attempts >= config.deadlines.max_attempts {
            return Err(PoolError::RetryLimitExceeded);
        }
        if totals.cpu_nanos >= nanos(config.deadlines.cumulative_cpu) {
            return Err(limit(WorkerLimit::Deadline));
        }
        if totals.attempts == 0 || totals.replacement {
            totals.attempts += 1;
        }
        totals.replacement = false;
        Ok(())
    }
    fn charge(&self, delta: u64, config: &PoolConfig) -> Result<(), PoolError> {
        let mut totals = lock(&self.0);
        totals.cpu_nanos = totals.cpu_nanos.saturating_add(delta);
        if totals.cpu_nanos >= nanos(config.deadlines.cumulative_cpu) {
            return Err(limit(WorkerLimit::Deadline));
        }
        Ok(())
    }
}

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
    epoch: u64,
    measurements: SharedMeasurements,
    config: PoolConfig,
    state: Mutex<State>,
    available: Condvar,
}

/// A synchronized pool of OS processes. Calls block only for explicitly
/// bounded durations; async hosts use their owned blocking executor.
#[derive(Clone)]
pub struct WorkerPool(Arc<Pool>);
impl WorkerPool {
    pub fn new(config: PoolConfig) -> Result<Self, PoolError> {
        config.validate()?;
        static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
        let pool = Self(Arc::new(Pool {
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            measurements: Arc::new(Mutex::new(Measurements::default())),
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
        for _ in 0..pool.0.config.min_workers {
            let worker = pool.0.spawn_ready()?;
            let mut state = lock(&pool.0.state);
            state.workers += 1;
            state.idle.push(worker);
        }
        Ok(pool)
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
    pub fn measurements(&self) -> PoolMeasurements {
        let state = lock(&self.0.state);
        let measurements = lock(&self.0.measurements);
        PoolMeasurements {
            exporter: "lash-vm-client/pool-v1",
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
    fn spawn_ready(&self) -> Result<Worker, PoolError> {
        self.spawn_ready_until(Instant::now() + self.config.protocol.no_response_watchdog)
    }
    fn spawn_ready_until(&self, deadline: Instant) -> Result<Worker, PoolError> {
        let mut worker = Worker::spawn(&self.config)?;
        worker.measurements = Some(self.measurements.clone());
        let deadline = deadline.min(Instant::now() + self.config.protocol.no_response_watchdog);
        let frame = worker.receive(deadline)?;
        let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
        fence.admit(&frame.header).map_err(PoolError::protocol)?;
        match frame.message {
            WorkerMessage::Ready {
                protocol_version,
                crate_version,
            } => {
                check_worker_protocol_version(
                    protocol_version,
                    env!("CARGO_PKG_VERSION"),
                    &crate_version,
                )?;
                let mut measurements = lock(&self.measurements);
                if measurements.pending_replacements > 0 {
                    measurements.pending_replacements -= 1;
                    measurements.counters.replacements += 1;
                }
                Ok(worker)
            }
            _ => Err(PoolError::protocol(
                "worker did not send its protocol handshake",
            )),
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
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}
fn limit(limit: WorkerLimit) -> PoolError {
    InfrastructureOutcome::WorkerLimitExceeded { limit }.into()
}

/// How a worker answered [`Checkout::park`].
#[derive(Debug)]
pub enum ParkOutcome {
    /// The run parked; the state resumes it.
    Parked(OpaqueVmState),
    /// The run could not be captured where it stands. The request is the
    /// worker's [`EffectKind::ParkDeclined`]; answering it with
    /// [`EffectOutcome::Unit`] runs on, and the run issues its request again.
    Declined(EffectRequest),
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
    credited_cpu: u64,
    outgoing: MessageFence,
    incoming: MessageFence,
    reservation: usize,
    started: bool,
    pending: Option<(EffectRequestId, EffectKind)>,
    resettable: bool,
    owner: Option<VmOwner>,
    observations: Vec<EncodedPayload>,
    /// The run's heap budget, which bounds the observations one step hands
    /// its parent (FIG-4458). `None` is unbounded.
    observation_budget: Option<u64>,
    /// The observation bytes the current step has handed over.
    observed_bytes: u64,
    execution_class: Option<ExecutionClass>,
    execution_recorded: bool,
}
impl Checkout {
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
    pub fn take_observations(&mut self) -> Vec<EncodedPayload> {
        std::mem::take(&mut self.observations)
    }
    pub fn lease(&self) -> ExecutionLease {
        self.outgoing.next_header_copy().lease
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
    pub fn start(&mut self, mut start: Start) -> Result<WorkerMessage, PoolError> {
        if self.started {
            return Err(PoolError::protocol("checkout already started"));
        }
        if let ProgramSource::Source { text, .. } = &start.program {
            self.bound(
                text.len() as u64,
                self.pool.config.protocol.max_source_bytes,
            )?;
        }
        let state = match &start.state {
            StartState::Snapshot(state) => Some((state, VmStateKind::Snapshot)),
            StartState::Continuation(state) => Some((state, VmStateKind::Continuation)),
            StartState::Fresh => None,
        };
        if let Some((state, kind)) = state
            && let Err(error) = state.check(&StateExpectation {
                kind,
                owner: &start.owner,
                reads: &lashlang::vm_contract_reads(),
                max_bytes: self.pool.config.protocol.max_vm_state_bytes,
            })
        {
            self.discard();
            return Err(PoolError::Infrastructure(error.into()));
        }
        // The host configuration is the ceiling, never a worker's assertion.
        let cap = |asked: Option<u64>, max: Option<u64>| match (asked, max) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (None, b) => b,
            (a, None) => a,
        };
        start.limits.instruction_budget = cap(
            start.limits.instruction_budget,
            self.pool.config.vm_limits.instruction_budget,
        );
        start.limits.memory_limit_bytes = cap(
            start.limits.memory_limit_bytes,
            self.pool.config.vm_limits.memory_limit_bytes,
        );
        start.limits.max_frame_depth = start
            .limits
            .max_frame_depth
            .min(self.pool.config.vm_limits.max_frame_depth);
        self.execution_class = Some(match &start.program {
            ProgramSource::Artifact {
                entry: ProgramEntry::Process { .. },
                ..
            } => ExecutionClass::Process,
            _ => ExecutionClass::Cell,
        });
        self.owner = Some(start.owner.clone());
        self.observation_budget = start.limits.memory_limit_bytes;
        self.started = true;
        self.exchange(
            ParentMessage::Start(Box::new(start)),
            self.pool.config.protocol.no_response_watchdog,
        )
    }
    /// Performs pure compiler/state work in the worker, under this checkout.
    pub fn prepare(
        &mut self,
        owner: VmOwner,
        request: EncodedPayload,
    ) -> Result<EncodedPayload, PoolError> {
        if self.started {
            return Err(PoolError::protocol("checkout already started"));
        }
        self.owner = Some(owner.clone());
        self.started = true;
        match self.exchange(
            ParentMessage::Prepare { owner, request },
            self.pool.config.protocol.no_response_watchdog,
        )? {
            WorkerMessage::Prepared { response } => Ok(response),
            _ => Err(PoolError::protocol("pure worker work returned no response")),
        }
    }
    /// Host effect work may take any time. No worker/CPU deadline runs while
    /// the parent owns the pending request; this call starts a new phase.
    pub fn effect_result(&mut self, result: EffectResponse) -> Result<WorkerMessage, PoolError> {
        if self.pending.map(|p| p.0) != Some(result.id) {
            return Err(PoolError::protocol(
                "result does not answer the current request",
            ));
        }
        if let EffectOutcome::Value(value) | EffectOutcome::Failed(value) = &result.outcome
            && value.0.len() as u64 > self.pool.config.protocol.max_effect_value_bytes
        {
            let error = limit(WorkerLimit::EffectValue {
                size: value.0.len() as u64,
                bound: self.pool.config.protocol.max_effect_value_bytes,
            });
            self.discard();
            return Err(error);
        }
        self.pending = None;
        self.exchange(
            ParentMessage::EffectResponse(result),
            self.pool.config.protocol.no_response_watchdog,
        )
    }
    /// Parks the run on its pending request instead of answering it: a
    /// process boundary, or an effect the run can issue again
    /// ([`EffectKind::parkable`]), such as one whose operation needs a worker
    /// of its own (FIG-4159). A parked run's state resumes it with `Start`,
    /// and a run parked on an effect issues that effect's request again. The
    /// run may decline when it cannot be captured where it stands: the worker
    /// then asks [`EffectKind::ParkDeclined`], and once that is answered the
    /// run issues its request again on this checkout.
    pub fn park(&mut self) -> Result<ParkOutcome, PoolError> {
        if !self.pending.is_some_and(|(_, kind)| kind.parkable()) {
            return Err(PoolError::protocol("park answers no parkable request"));
        }
        self.pending = None;
        match self.exchange(
            ParentMessage::Park,
            self.pool.config.protocol.no_response_watchdog,
        )? {
            WorkerMessage::Suspended { state } => Ok(ParkOutcome::Parked(state)),
            WorkerMessage::EffectRequest(request) if request.kind == EffectKind::ParkDeclined => {
                Ok(ParkOutcome::Declined(request))
            }
            _ => {
                self.discard();
                Err(PoolError::protocol(
                    "the worker answered a park with neither its state nor a declined park",
                ))
            }
        }
    }
    /// Cooperative physical stop, followed by bounded hard kill on silence.
    /// The broker retains the journaled cancellation and completion winner.
    pub fn cancel(&mut self) -> Result<WorkerMessage, PoolError> {
        let response = self.exchange(
            ParentMessage::Cancel,
            self.pool.config.deadlines.cancel_grace,
        );
        self.discard_for(false);
        response
    }
    /// Call after clean completion or a broker-approved abandonment/park.
    /// The broker must settle any admitted operation before abandoning it.
    pub fn release(mut self) -> Result<(), PoolError> {
        if self.started && !self.resettable {
            self.discard();
            return Err(PoolError::protocol("failed worker cannot reset"));
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
                    return Err(PoolError::protocol("reset CPU accounting regressed"));
                }
                if let Err(error) = self.charge_cpu(cpu_nanos) {
                    self.discard();
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
            Ok(_) => {
                self.discard();
                Err(PoolError::protocol("reset did not return ResetDone"))
            }
            Err(error) => {
                self.discard();
                Err(error)
            }
        }
    }
    fn bound(&self, size: u64, bound: u64) -> Result<(), PoolError> {
        if size > bound {
            return Err(InfrastructureOutcome::PayloadTooLarge { limit: bound, size }.into());
        }
        Ok(())
    }
    fn send(&mut self, message: ParentMessage, timeout: Duration) -> Result<(), PoolError> {
        let frame = ParentFrame {
            header: self.outgoing.next_header_copy(),
            message,
        };
        let worker = self.worker.as_mut().ok_or_else(PoolError::eof)?;
        let bytes = worker
            .codec
            .encode_parent(&frame)
            .map_err(PoolError::from)?;
        if matches!(
            frame.message,
            ParentMessage::Start(_) | ParentMessage::Prepare { .. }
        ) && bytes.len() > self.reservation
        {
            return Err(PoolError::QueueFull { bytes: bytes.len() });
        }
        self.outgoing.next_header();
        worker.send(&frame, timeout)
    }
    fn receive_control(&mut self, timeout: Duration) -> Result<WorkerMessage, PoolError> {
        let frame = self
            .worker
            .as_mut()
            .ok_or_else(PoolError::eof)?
            .receive(Instant::now() + timeout)?;
        self.incoming
            .admit(&frame.header)
            .map_err(PoolError::protocol)?;
        Ok(frame.message)
    }
    fn exchange(
        &mut self,
        message: ParentMessage,
        timeout: Duration,
    ) -> Result<WorkerMessage, PoolError> {
        let result = self.exchange_inner(message, timeout);
        if result.is_err() {
            self.discard();
            #[cfg(feature = "testing")]
            if matches!(
                result,
                Err(PoolError::Infrastructure(
                    InfrastructureOutcome::WorkerLimitExceeded {
                        limit: WorkerLimit::Deadline
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
    fn exchange_inner(
        &mut self,
        message: ParentMessage,
        timeout: Duration,
    ) -> Result<WorkerMessage, PoolError> {
        self.resettable = false;
        self.observed_bytes = 0;
        self.send(message, timeout)?;
        let mut deadline = Instant::now() + timeout;
        let mut phase = None;
        #[cfg(feature = "testing")]
        let mut phase_started = Instant::now();
        loop {
            let received = self
                .worker
                .as_mut()
                .ok_or_else(PoolError::eof)?
                .receive(deadline);
            let frame = match received {
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive {
                    ..
                })) if phase.is_some() => {
                    #[cfg(feature = "testing")]
                    self.pool.report_deadline(
                        &lock(&self.pool.state),
                        match phase {
                            Some(WorkerPhase::Computing) => "compute",
                            Some(WorkerPhase::Serializing) => "serialization",
                            _ => "responding",
                        },
                        phase_started.elapsed(),
                        &self.budget,
                    );
                    return Err(limit(WorkerLimit::Deadline));
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
                .map_err(PoolError::protocol)?;
            match frame.message {
                WorkerMessage::Progress {
                    phase: next,
                    cpu_nanos,
                } => {
                    let allowed = matches!(
                        (phase, next),
                        (None, WorkerPhase::Computing)
                            | (Some(WorkerPhase::Computing), WorkerPhase::Serializing)
                            | (Some(WorkerPhase::Serializing), WorkerPhase::Responding)
                    );
                    if !allowed || cpu_nanos < self.credited_cpu {
                        return Err(PoolError::protocol(
                            "invalid worker phase or CPU accounting",
                        ));
                    }
                    self.charge_cpu(cpu_nanos)?;
                    self.credited_cpu = cpu_nanos;
                    self.worker.as_mut().ok_or_else(PoolError::eof)?.cpu_nanos = cpu_nanos;
                    if !self.execution_recorded && next == WorkerPhase::Computing {
                        if let Some(class_name) = self.execution_class {
                            let worker = self.worker.as_ref().ok_or_else(PoolError::eof)?;
                            let owner = self
                                .owner
                                .as_ref()
                                .ok_or_else(|| PoolError::protocol("execution has no owner"))?;
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
                    phase = Some(next);
                    #[cfg(feature = "testing")]
                    {
                        phase_started = Instant::now();
                    }
                    deadline = Instant::now()
                        + match next {
                            WorkerPhase::Computing => self.pool.config.deadlines.compute,
                            WorkerPhase::Serializing => self.pool.config.deadlines.serialization,
                            WorkerPhase::Responding => timeout,
                        };
                }
                WorkerMessage::Refused { outcome } => return Err(outcome.into()),
                WorkerMessage::Observations { payload } => {
                    // Each chunk crossed in one frame; the step's stream is
                    // held to the run's heap budget, never to a transport
                    // bound (FIG-4458).
                    self.observed_bytes =
                        self.observed_bytes.saturating_add(payload.0.len() as u64);
                    if self
                        .observation_budget
                        .is_some_and(|budget| self.observed_bytes > budget)
                    {
                        return Err(limit(WorkerLimit::Observations));
                    }
                    self.observations.push(payload);
                }
                WorkerMessage::LimitExceeded { limit: exhausted } => return Err(limit(exhausted)),
                WorkerMessage::EffectRequest(request) => {
                    if request.payload.0.len() as u64
                        > self.pool.config.protocol.max_effect_value_bytes
                    {
                        return Err(limit(WorkerLimit::EffectValue {
                            size: request.payload.0.len() as u64,
                            bound: self.pool.config.protocol.max_effect_value_bytes,
                        }));
                    }
                    self.pending = Some((request.id, request.kind));
                    self.resettable = true;
                    return Ok(WorkerMessage::EffectRequest(request));
                }
                message @ (WorkerMessage::Complete { .. }
                | WorkerMessage::Suspended { .. }
                | WorkerMessage::GuestError { .. }) => {
                    let state = match &message {
                        WorkerMessage::Complete { state, .. } => {
                            Some((state, VmStateKind::Snapshot))
                        }
                        WorkerMessage::Suspended { state } => {
                            Some((state, VmStateKind::Continuation))
                        }
                        WorkerMessage::GuestError { state, .. } => {
                            state.as_ref().map(|state| (state, VmStateKind::Snapshot))
                        }
                        _ => None,
                    };
                    if let Some((state, kind)) = state {
                        state
                            .check(&StateExpectation {
                                kind,
                                owner: self
                                    .owner
                                    .as_ref()
                                    .ok_or_else(|| PoolError::protocol("response has no owner"))?,
                                reads: &lashlang::vm_contract_reads(),
                                max_bytes: self.pool.config.protocol.max_vm_state_bytes,
                            })
                            .map_err(|refusal| PoolError::Infrastructure(refusal.into()))?;
                    }
                    self.resettable = !matches!(message, WorkerMessage::GuestError { .. });
                    if !self.resettable {
                        self.discard_for(false);
                        lock(&self.budget.0).replacement = false;
                    }
                    // Do not consult exit status after a fully decoded terminal.
                    return Ok(message);
                }
                WorkerMessage::Prepared { response } => {
                    self.resettable = true;
                    return Ok(WorkerMessage::Prepared { response });
                }
                WorkerMessage::Cancelled => {
                    self.discard_for(false);
                    lock(&self.budget.0).replacement = false;
                    return Ok(WorkerMessage::Cancelled);
                }
                _ => return Err(PoolError::protocol("unexpected worker response")),
            }
        }
    }
    fn discard(&mut self) {
        self.discard_for(true);
    }
    fn discard_for(&mut self, failed: bool) {
        if let Some(worker) = self.worker.take() {
            let cpu = self.pool.discard(worker, failed);
            let _ = self.charge_cpu(cpu);
            self.credited_cpu = cpu;
            if self.started {
                lock(&self.budget.0).replacement = true;
            }
        }
        self.pending = None;
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
                } else if state.workers < self.0.config.max_workers {
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
                            self.0.failed(&mut state);
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
                                limit: WorkerLimit::Deadline
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
                        .ok_or_else(|| PoolError::protocol("lease space exhausted"))?;
                    {
                        let mut measurements = lock(&self.0.measurements);
                        measurements.counters.checkouts += 1;
                        if worker.used {
                            measurements.counters.reuses += 1;
                        }
                    }
                    worker.used = true;
                    let credited_cpu = worker.cpu_nanos;
                    return Ok(Checkout {
                        #[cfg(feature = "testing")]
                        admitted_at: Instant::now(),
                        #[cfg(feature = "testing")]
                        measured_cpu_receipt: std::cell::Cell::new(credited_cpu),
                        pool: self.0.clone(),
                        worker: Some(worker),
                        budget,
                        credited_cpu,
                        outgoing: MessageFence::new(lease, owner_epoch, frame_epoch),
                        incoming: MessageFence::new(lease, owner_epoch, frame_epoch),
                        reservation: queued_bytes,
                        started: false,
                        pending: None,
                        resettable: false,
                        owner: None,
                        observations: Vec::new(),
                        observation_budget: None,
                        observed_bytes: 0,
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
