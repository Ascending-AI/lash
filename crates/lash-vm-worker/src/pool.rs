use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::process::Worker;
use crate::{PoolConfig, PoolError};
use lash_vm_protocol::*;

/// Parent-owned accounting. Carry this same value across substrate re-drives
/// and replacement checkouts. The pool never rewinds it or retries execution.
#[derive(Clone, Debug, Default)]
pub struct ExecutionBudget(Arc<Mutex<BudgetTotals>>);
#[derive(Debug, Default)]
struct BudgetTotals {
    attempts: u32,
    cpu_nanos: u64,
    replacement: bool,
}
impl ExecutionBudget {
    pub fn totals(&self) -> (u32, Duration) {
        let totals = lock(&self.0);
        (totals.attempts, Duration::from_nanos(totals.cpu_nanos))
    }
    pub fn restored(attempts: u32, cpu: Duration) -> Self {
        Self(Arc::new(Mutex::new(BudgetTotals {
            attempts,
            cpu_nanos: nanos(cpu),
            replacement: attempts > 0,
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
    idle: Vec<Worker>,
    workers: usize,
    queued_items: usize,
    queued_bytes: usize,
    next_lease: u64,
    restarts: VecDeque<Instant>,
    failed: bool,
}
struct Pool {
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
        let pool = Self(Arc::new(Pool {
            config,
            state: Mutex::new(State {
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
    /// Reserve the complete encoded input size, including the frame envelope.
    /// Queue admission checks both items and bytes before retaining any input.
    pub fn checkout(
        &self,
        queued_bytes: usize,
        owner_epoch: OwnerEpoch,
        frame_epoch: FrameEpoch,
        budget: ExecutionBudget,
    ) -> Result<Checkout, PoolError> {
        let deadline = Instant::now() + self.0.config.deadlines.checkout;
        let mut state = lock(&self.0.state);
        let mut queued = false;
        loop {
            if queued && Instant::now() >= deadline {
                unqueue(&mut state, queued, queued_bytes);
                return Err(PoolError::CheckoutTimedOut);
            }
            if state.failed {
                unqueue(&mut state, queued, queued_bytes);
                return Err(PoolError::RestartStorm);
            }
            let worker = if let Some(worker) = state.idle.pop() {
                Some(worker)
            } else if state.workers < self.0.config.max_workers {
                state.workers += 1;
                unqueue(&mut state, queued, queued_bytes);
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
            if let Some(worker) = worker {
                unqueue(&mut state, queued, queued_bytes);
                if let Err(error) = budget.admit(&self.0.config) {
                    state.idle.push(worker);
                    self.0.available.notify_all();
                    return Err(error);
                }
                let lease = ExecutionLease(state.next_lease);
                state.next_lease = state
                    .next_lease
                    .checked_add(1)
                    .ok_or_else(|| PoolError::protocol("lease space exhausted"))?;
                let credited_cpu = worker.cpu_nanos;
                return Ok(Checkout {
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
                state.queued_items += 1;
                state.queued_bytes += queued_bytes;
                queued = true;
            }
            let Some(wait) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                unqueue(&mut state, queued, queued_bytes);
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

impl Pool {
    fn spawn_ready(&self) -> Result<Worker, PoolError> {
        self.spawn_ready_until(Instant::now() + self.config.protocol.no_response_watchdog)
    }
    fn spawn_ready_until(&self, deadline: Instant) -> Result<Worker, PoolError> {
        let mut worker = Worker::spawn(&self.config)?;
        let deadline = deadline.min(Instant::now() + self.config.protocol.no_response_watchdog);
        let frame = worker.receive(deadline)?;
        let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
        fence.admit(&frame.header).map_err(PoolError::protocol)?;
        match frame.message {
            WorkerMessage::Ready { build } if build == self.config.entry.build => Ok(worker),
            _ => Err(PoolError::protocol(
                "worker did not acknowledge the exact compiled build",
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
    fn discard(&self, mut worker: Worker) -> u64 {
        let cpu = worker.terminate();
        drop(worker);
        let mut state = lock(&self.state);
        state.workers -= 1;
        self.failed(&mut state);
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

/// Owns one transport lease. Dropping without release kills and reaps the
/// worker. A failed receive fences that lease before replenishing the pool.
pub struct Checkout {
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
}
impl Checkout {
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
                vm_contract: &lashlang::vm_contract_identity(),
                format_version: if kind == VmStateKind::Snapshot {
                    lashlang::LASHLANG_SNAPSHOT_VERSION
                } else {
                    lashlang::VM_CONTINUATION_FORMAT_VERSION
                },
                max_bytes: self.pool.config.protocol.max_vm_state_bytes,
            })
        {
            self.discard();
            return Err(PoolError::protocol(error));
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
        self.owner = Some(start.owner.clone());
        self.started = true;
        self.exchange(
            ParentMessage::Start(Box::new(start)),
            self.pool.config.protocol.no_response_watchdog,
        )
    }
    /// Host effect work may take any time. No worker/CPU deadline runs while
    /// the parent owns the pending request; this call starts a new phase.
    pub fn effect_result(&mut self, result: EffectResponse) -> Result<WorkerMessage, PoolError> {
        if self.pending.map(|p| p.0) != Some(result.id) {
            return Err(PoolError::protocol(
                "result does not answer the current request",
            ));
        }
        self.pending = None;
        self.exchange(
            ParentMessage::EffectResponse(result),
            self.pool.config.protocol.no_response_watchdog,
        )
    }
    /// Awaiting-effect release is an explicit broker extension point. The
    /// current VM can park only at a ProcessBoundary. A broker must implement
    /// a pending-effect checkpoint before nested admission can release here.
    pub fn park(&mut self) -> Result<OpaqueVmState, PoolError> {
        if !matches!(self.pending, Some((_, EffectKind::ProcessBoundary))) {
            return Err(PoolError::PendingEffectParkingRequired);
        }
        self.pending = None;
        match self.exchange(
            ParentMessage::Park,
            self.pool.config.protocol.no_response_watchdog,
        )? {
            WorkerMessage::Suspended { state } => Ok(state),
            _ => {
                let error = PoolError::PendingEffectParkingRequired;
                self.discard();
                Err(error)
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
        self.discard();
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
            Ok(WorkerMessage::ResetDone) => {
                if let Some(worker) = self.worker.take() {
                    let mut state = lock(&self.pool.state);
                    if state.failed {
                        drop(state);
                        self.pool.discard(worker);
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
            header: self.outgoing.next_header(),
            message,
        };
        let worker = self.worker.as_mut().ok_or_else(PoolError::eof)?;
        let bytes = worker
            .codec
            .encode_parent(&frame)
            .map_err(PoolError::from)?;
        if matches!(frame.message, ParentMessage::Start(_)) && bytes.len() > self.reservation {
            return Err(PoolError::QueueFull { bytes: bytes.len() });
        }
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
        }
        result
    }
    fn exchange_inner(
        &mut self,
        message: ParentMessage,
        timeout: Duration,
    ) -> Result<WorkerMessage, PoolError> {
        self.resettable = false;
        self.send(message, timeout)?;
        let mut deadline = Instant::now() + timeout;
        let mut phase = None;
        loop {
            let received = self
                .worker
                .as_mut()
                .ok_or_else(PoolError::eof)?
                .receive(deadline);
            let frame = match received {
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive {
                    ..
                })) if phase.is_some() => return Err(limit(WorkerLimit::Deadline)),
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
                    self.budget
                        .charge(cpu_nanos - self.credited_cpu, &self.pool.config)?;
                    self.credited_cpu = cpu_nanos;
                    self.worker.as_mut().ok_or_else(PoolError::eof)?.cpu_nanos = cpu_nanos;
                    phase = Some(next);
                    deadline = Instant::now()
                        + match next {
                            WorkerPhase::Computing => self.pool.config.deadlines.compute,
                            WorkerPhase::Serializing => self.pool.config.deadlines.serialization,
                            WorkerPhase::Responding => timeout,
                        };
                }
                WorkerMessage::LimitExceeded { limit: exhausted } => return Err(limit(exhausted)),
                WorkerMessage::PayloadTooLarge { limit, size } => {
                    return Err(InfrastructureOutcome::PayloadTooLarge { limit, size }.into());
                }
                WorkerMessage::EffectRequest(request) => {
                    self.bound(
                        request.payload.0.len() as u64,
                        self.pool.config.protocol.max_effect_value_bytes,
                    )?;
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
                                vm_contract: &lashlang::vm_contract_identity(),
                                format_version: if kind == VmStateKind::Snapshot {
                                    lashlang::LASHLANG_SNAPSHOT_VERSION
                                } else {
                                    lashlang::VM_CONTINUATION_FORMAT_VERSION
                                },
                                max_bytes: self.pool.config.protocol.max_vm_state_bytes,
                            })
                            .map_err(PoolError::protocol)?;
                    }
                    self.resettable = !matches!(message, WorkerMessage::GuestError { .. });
                    if !self.resettable {
                        self.discard();
                    }
                    // Do not consult exit status after a fully decoded terminal.
                    return Ok(message);
                }
                WorkerMessage::Cancelled => return Ok(WorkerMessage::Cancelled),
                _ => return Err(PoolError::protocol("unexpected worker response")),
            }
        }
    }
    fn discard(&mut self) {
        if let Some(worker) = self.worker.take() {
            let cpu = self.pool.discard(worker);
            let _ = self
                .budget
                .charge(cpu.saturating_sub(self.credited_cpu), &self.pool.config);
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
        self.discard();
    }
}
