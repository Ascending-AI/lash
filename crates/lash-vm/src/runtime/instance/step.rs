//! The owned step/resume interface a worker runs model code through.
//!
//! [`VmInstance::start`](super::VmInstance::start) runs until the program asks
//! its host for something, and returns an owned [`VmStep`]:
//!
//! - [`VmStep::Suspended`] names the request: an ability operation, a cancel
//!   checkpoint, or (in process mode) a segment boundary. The host answers it
//!   with [`VmResume`] through [`VmInstance::resume`](super::VmInstance::resume).
//! - [`VmStep::Parked`] ends the run in a durable continuation: at a segment
//!   boundary, or on an operation the run awaits that its host parked it on
//!   ([`VmResume::Park`] answering a [parkable](VmRequest::parkable) effect,
//!   FIG-4159), so the worker holding it can be released while the host
//!   settles the operation. A run started from that continuation issues the
//!   same operation again, and the host answers it with the outcome it held.
//! - [`VmStep::Complete`] and [`VmStep::GuestError`] end it with its outcome.
//!
//! Nothing that crosses this interface borrows: requests, answers and
//! outcomes are owned values, and the host is never called. The VM's own
//! suspension lives inside the instance, which is what lets a worker process
//! own it while its parent brokers every effect.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use lash_sansio::sync::MutexExt;
use thiserror::Error;

use crate::LashVmExecutionObservation;
use crate::runtime::{
    AbilityOp, AbilityOutcome, CompiledProgram, ContinuationError, ExecutionBounds, ExecutionHost,
    ExecutionHostError, ExecutionMode, ExecutionOutcome, ExecutionScratch, ProfileReport,
    ProjectedBindings, RuntimeError, RuntimeFailure, State, Vm, VmContinuation, VmParkableRun,
    VmRunOutcome,
};

/// What a run starts from.
#[derive(Debug)]
pub enum VmExecutionStart {
    /// The instance's session state: a foreground cell, or a process body's
    /// first segment over its argument globals.
    Session,
    /// A parked continuation: a process body's segment, or a run of either
    /// mode parked on an operation it awaits. Its mode must be the run's.
    Continuation(Box<VmContinuation>),
}

/// How a run executes. Owned configuration only: no host object.
#[derive(Clone)]
pub struct VmRunConfig {
    pub pacing: crate::VmPacing,
    pub mode: ExecutionMode,
    pub bounds: ExecutionBounds,
    /// Descriptors owned by this execution, including worker-local IPC views.
    /// Host descriptors themselves never cross the worker wire.
    pub projected: ProjectedBindings,
    pub observe_execution: bool,
    pub trace_runtime_errors: bool,
    pub profile: bool,
    pub collect_heap_every_allocation: bool,
}

impl std::fmt::Debug for VmRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmRunConfig")
            .field("mode", &self.mode)
            .field("bounds", &self.bounds)
            .field("pacing", &self.pacing)
            .field("projected", &self.projected.names().collect::<Vec<_>>())
            .field("observe_execution", &self.observe_execution)
            .field("trace_runtime_errors", &self.trace_runtime_errors)
            .field("profile", &self.profile)
            .field(
                "collect_heap_every_allocation",
                &self.collect_heap_every_allocation,
            )
            .finish()
    }
}

impl VmRunConfig {
    pub fn new(mode: ExecutionMode, bounds: ExecutionBounds) -> Self {
        Self {
            pacing: crate::VmPacing::standard(),
            mode,
            bounds,
            projected: ProjectedBindings::new(),
            observe_execution: false,
            trace_runtime_errors: false,
            profile: false,
            collect_heap_every_allocation: false,
        }
    }
}

/// What a suspended run asks its host for.
#[derive(Debug)]
pub enum VmRequest {
    /// An ability operation: a resource operation, an await, a print, a
    /// sleep, a finish or a fail.
    Effect(AbilityOp),
    /// The run's executed-instruction count reached cancel checkpoint `n`;
    /// the host answers with its journaled observation of cancellation.
    CancelCheckpoint(u64),
    /// Process mode: an effect completed, and the host may park the run here
    /// as a segment boundary.
    Boundary,
    /// Process mode: the boundary the host asked to park at could not be
    /// captured; the host acknowledges and the run continues.
    ParkDeclined(ContinuationError),
}

impl VmRequest {
    /// Whether [`VmResume::Park`] may answer this request: an effect the run
    /// can stand on again — a resource operation, a resource-operation batch,
    /// a sleep or the await of a process handle (alone or as
    /// the next pending leaf of a tuple, list or record of them, FIG-4275) —
    /// so the host can park the run awaiting it and answer the operation when
    /// a continuation issues it again. A print or a terminal is answered in
    /// place.
    pub fn parkable(&self) -> bool {
        matches!(
            self,
            Self::Effect(
                AbilityOp::ResourceOperation(_)
                    | AbilityOp::ResourceOperationBatch(_)
                    | AbilityOp::Await(_)
                    | AbilityOp::Sleep(_)
            )
        )
    }

    fn kind(&self) -> RequestKind {
        match self {
            Self::Effect(_) => RequestKind::Effect {
                parkable: self.parkable(),
            },
            Self::CancelCheckpoint(_) => RequestKind::CancelCheckpoint,
            Self::Boundary => RequestKind::Boundary,
            Self::ParkDeclined(_) => RequestKind::ParkDeclined,
        }
    }
}

/// Which request is pending, kept to check the resume that answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestKind {
    Effect { parkable: bool },
    CancelCheckpoint,
    Boundary,
    ParkDeclined,
}

impl RequestKind {
    fn name(self) -> &'static str {
        match self {
            Self::Effect { .. } => "effect",
            Self::CancelCheckpoint => "cancel checkpoint",
            Self::Boundary => "boundary",
            Self::ParkDeclined => "park declined",
        }
    }
}

/// The host's answer to the pending request.
#[derive(Debug)]
pub enum VmResume {
    /// A completed effect observed durable cancellation.
    EffectCancelled,
    Effect(Result<AbilityOutcome, ExecutionHostError>),
    CancelCheckpoint {
        cancelled: bool,
    },
    /// Answers a boundary (run on) or a declined park (acknowledged).
    Continue,
    /// Answers a boundary, or a parkable effect the run awaits: park the run
    /// here.
    Park,
}

impl VmResume {
    fn kind(&self) -> &'static str {
        match self {
            Self::Effect(_) => "effect",
            Self::EffectCancelled => "cancelled effect",
            Self::CancelCheckpoint { .. } => "cancel checkpoint",
            Self::Continue => "continue",
            Self::Park => "park",
        }
    }

    fn answers(&self, request: RequestKind) -> bool {
        matches!(
            (request, self),
            (
                RequestKind::Effect { .. },
                Self::Effect(_) | Self::EffectCancelled
            ) | (RequestKind::Effect { parkable: true }, Self::Park)
                | (RequestKind::CancelCheckpoint, Self::CancelCheckpoint { .. })
                | (RequestKind::Boundary, Self::Continue | Self::Park)
                | (RequestKind::ParkDeclined, Self::Continue)
        )
    }
}

#[derive(Debug)]
pub struct VmSuspended {
    pub request: VmRequest,
    /// Execution observations emitted since the previous step, in order.
    pub observations: Vec<LashVmExecutionObservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmParkReason {
    /// The host asked to park at a boundary.
    Boundary,
    /// The host handed the pending operation to a successor; the
    /// continuation re-issues it.
    HandedOver,
    /// The host parked the run on the effect it awaits (FIG-4159); the
    /// continuation re-issues it, and the host answers it with the outcome it
    /// held.
    AwaitingEffect,
}

#[derive(Debug)]
pub struct VmParked {
    pub continuation: Box<VmContinuation>,
    pub reason: VmParkReason,
    pub observations: Vec<LashVmExecutionObservation>,
    pub profile: Option<ProfileReport>,
}

#[derive(Debug)]
pub struct VmComplete {
    pub outcome: ExecutionOutcome,
    pub observations: Vec<LashVmExecutionObservation>,
    pub profile: Option<ProfileReport>,
}

#[derive(Debug)]
pub struct VmGuestError {
    pub failure: RuntimeFailure,
    pub observations: Vec<LashVmExecutionObservation>,
    pub profile: Option<ProfileReport>,
}

#[derive(Debug)]
pub enum VmStep {
    Suspended(VmSuspended),
    Parked(VmParked),
    Complete(VmComplete),
    GuestError(VmGuestError),
}

/// A misuse of the interface; the instance is unchanged by it.
#[derive(Debug, Error)]
pub enum VmStepError {
    #[error("a run is already in flight on this instance")]
    AlreadyRunning,
    #[error("no run is in flight on this instance")]
    NotRunning,
    #[error("a `{found}` resume does not answer the pending `{expected}` request")]
    ResumeMismatch {
        expected: &'static str,
        found: &'static str,
    },
    #[error("a continuation parked in {parked:?} mode cannot resume a {run:?} run")]
    ContinuationModeMismatch {
        parked: ExecutionMode,
        run: ExecutionMode,
    },
    #[error("the continuation does not resume this program: {0}")]
    ContinuationRefused(ContinuationError),
    #[error("the run stopped without asking its host for anything")]
    Stalled,
}

/// The live interrupt of one run: a worker's reader sets it when the parent
/// cancels, and the run's next cooperative probe answers cancelled. It never
/// decides the durable winner, which is the journaled checkpoint observation.
#[derive(Clone, Debug, Default)]
pub struct VmInterrupt(Arc<AtomicBool>);

impl VmInterrupt {
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_interrupted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The run's side of the mailbox: what it asked, what it was answered, and
/// what it emitted since the last step.
#[derive(Default)]
struct Mailbox {
    /// The request the run posted and the next step has not handed out yet.
    posted: Option<VmRequest>,
    /// The request awaiting its answer.
    pending: Option<RequestKind>,
    answer: Option<VmResume>,
    /// The cancellation the host last observed at a checkpoint.
    cancelled: bool,
    /// The host answered the pending effect with [`VmResume::Park`]: the run
    /// is standing on it again, to park.
    parked_on_effect: bool,
    observations: Vec<LashVmExecutionObservation>,
    runtime_failure: Option<RuntimeFailure>,
    profile: Option<ProfileReport>,
    scratch: Option<ExecutionScratch>,
}

/// The host a stepped run executes against. It answers every synchronous
/// question from the owned configuration, and every asynchronous one by
/// posting a request and waiting for the answer the next resume delivers.
struct StepHost {
    config: VmRunConfig,
    interrupt: VmInterrupt,
    mailbox: Mutex<Mailbox>,
}

impl StepHost {
    fn ask(&self, request: VmRequest) -> Answer<'_> {
        let mut mailbox = self.mailbox.lock_recover();
        debug_assert!(
            mailbox.pending.is_none(),
            "one request is pending at a time"
        );
        mailbox.pending = Some(request.kind());
        mailbox.posted = Some(request);
        Answer { host: self }
    }
}

/// Resolves when the next resume answers the posted request.
struct Answer<'a> {
    host: &'a StepHost,
}

impl Future for Answer<'_> {
    type Output = VmResume;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<VmResume> {
        match self.host.mailbox.lock_recover().answer.take() {
            Some(answer) => Poll::Ready(answer),
            None => Poll::Pending,
        }
    }
}

impl ExecutionHost for StepHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match self.ask(VmRequest::Effect(op)).await {
            VmResume::Effect(result) => result,
            VmResume::EffectCancelled => {
                self.mailbox.lock_recover().cancelled = true;
                Err(ExecutionHostError::new("execution cancelled"))
            }
            // `answer` admits a park only for a parkable effect, whose arm
            // stands on the operation again when it is answered this way.
            VmResume::Park => {
                self.mailbox.lock_recover().parked_on_effect = true;
                Ok(AbilityOutcome::HandedOver)
            }
            // `answer` admits only the resume that answers the request.
            _ => Err(ExecutionHostError::new(
                "an effect was answered by another resume",
            )),
        }
    }

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        if let VmResume::CancelCheckpoint { cancelled } =
            self.ask(VmRequest::CancelCheckpoint(checkpoint)).await
        {
            self.mailbox.lock_recover().cancelled |= cancelled;
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.config.mode
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        self.config.projected.clone()
    }

    fn trace_runtime_errors(&self) -> bool {
        self.config.trace_runtime_errors
    }

    fn profile_execution(&self) -> bool {
        self.config.profile
    }

    fn vm_pacing(&self) -> crate::VmPacing {
        self.config.pacing
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        self.config.bounds
    }

    fn is_cancelled(&self) -> bool {
        self.interrupt.is_interrupted() || self.mailbox.lock_recover().cancelled
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.config.collect_heap_every_allocation
    }

    fn take_scratch(&self) -> Option<ExecutionScratch> {
        self.mailbox.lock_recover().scratch.take()
    }

    fn store_scratch(&self, scratch: ExecutionScratch) {
        self.mailbox.lock_recover().scratch = Some(scratch);
    }

    fn observe_runtime_failure(&self, failure: RuntimeFailure) {
        self.mailbox.lock_recover().runtime_failure = Some(failure);
    }

    fn observe_profile(&self, profile: ProfileReport) {
        let mut mailbox = self.mailbox.lock_recover();
        match &mut mailbox.profile {
            Some(existing) => existing.merge(&profile),
            None => mailbox.profile = Some(profile),
        }
    }

    fn observes_lash_vm_execution(&self) -> bool {
        self.config.observe_execution
    }

    fn observe_lash_vm_execution(&self, observation: LashVmExecutionObservation) {
        self.mailbox.lock_recover().observations.push(observation);
    }
}

/// How a run ended, before the mailbox's emissions are attached.
enum RunEnd {
    Finished(ExecutionOutcome),
    Failed(RuntimeFailure),
    Parked(Box<VmContinuation>, VmParkReason),
    Refused(ContinuationError),
}

type RunFuture = Pin<Box<dyn Future<Output = (RunEnd, Option<State>)> + Send>>;

/// One run in flight: the VM future that owns the run's state, and the host
/// it posts to.
pub(super) struct VmExecution {
    host: Arc<StepHost>,
    run: RunFuture,
}

pub(super) enum Polled {
    Suspended(VmSuspended),
    Ended(Ended),
    /// The continuation the run was started from does not resume its
    /// program; the run never began.
    Refused(ContinuationError, Option<State>, Option<ExecutionScratch>),
}

pub(super) struct Ended {
    pub(super) step: VmStep,
    /// The session state a foreground run returns to its instance.
    pub(super) state: Option<State>,
    pub(super) scratch: Option<ExecutionScratch>,
}

impl VmExecution {
    pub(super) fn start(
        program: Arc<CompiledProgram>,
        start: VmExecutionStart,
        config: VmRunConfig,
        state: State,
        scratch: ExecutionScratch,
    ) -> Result<Self, VmStepError> {
        if let VmExecutionStart::Continuation(continuation) = &start
            && continuation.mode != config.mode
        {
            return Err(VmStepError::ContinuationModeMismatch {
                parked: continuation.mode,
                run: config.mode,
            });
        }
        let host = Arc::new(StepHost {
            config,
            interrupt: VmInterrupt::default(),
            mailbox: Mutex::new(Mailbox {
                scratch: Some(scratch),
                ..Mailbox::default()
            }),
        });
        let run: RunFuture = match host.config.mode {
            ExecutionMode::Foreground => {
                Box::pin(run_foreground(program, host.clone(), start, state))
            }
            ExecutionMode::Process => Box::pin(run_process(program, host.clone(), start, state)),
        };
        Ok(Self { host, run })
    }

    pub(super) fn interrupt(&self) -> VmInterrupt {
        self.host.interrupt.clone()
    }

    /// Delivers `resume` if it answers the pending request.
    pub(super) fn answer(&mut self, resume: VmResume) -> Result<(), VmStepError> {
        let mut mailbox = self.host.mailbox.lock_recover();
        let Some(pending) = mailbox.pending else {
            return Err(VmStepError::NotRunning);
        };
        if !resume.answers(pending) {
            return Err(VmStepError::ResumeMismatch {
                expected: pending.name(),
                found: resume.kind(),
            });
        }
        mailbox.pending = None;
        mailbox.answer = Some(resume);
        Ok(())
    }

    /// Runs until the VM posts a request or ends. The VM awaits nothing but
    /// this run's host, so a pending poll always leaves a request behind.
    pub(super) fn poll(&mut self) -> Result<Polled, VmStepError> {
        let mut context = Context::from_waker(Waker::noop());
        match self.run.as_mut().poll(&mut context) {
            Poll::Pending => {
                let mut mailbox = self.host.mailbox.lock_recover();
                let Some(request) = mailbox.posted.take() else {
                    return Err(VmStepError::Stalled);
                };
                Ok(Polled::Suspended(VmSuspended {
                    request,
                    observations: std::mem::take(&mut mailbox.observations),
                }))
            }
            Poll::Ready((end, state)) => {
                let mut mailbox = self.host.mailbox.lock_recover();
                let observations = std::mem::take(&mut mailbox.observations);
                let profile = mailbox.profile.take();
                let scratch = mailbox.scratch.take();
                let step = match end {
                    RunEnd::Finished(outcome) => VmStep::Complete(VmComplete {
                        outcome,
                        observations,
                        profile,
                    }),
                    RunEnd::Failed(failure) => VmStep::GuestError(VmGuestError {
                        failure: mailbox.runtime_failure.take().unwrap_or(failure),
                        observations,
                        profile,
                    }),
                    RunEnd::Parked(continuation, reason) => VmStep::Parked(VmParked {
                        continuation,
                        reason,
                        observations,
                        profile,
                    }),
                    RunEnd::Refused(error) => {
                        return Ok(Polled::Refused(error, state, scratch));
                    }
                };
                Ok(Polled::Ended(Ended {
                    step,
                    state,
                    scratch,
                }))
            }
        }
    }
}

fn failure(error: RuntimeError) -> RuntimeFailure {
    RuntimeFailure { error, span: None }
}

/// A foreground cell: the session state goes in and comes back out. Its host
/// may park it on an operation it awaits (FIG-4159): the run then ends in a
/// continuation that holds the session's runtime globals and heap, and the
/// rest of the session state (its expired-function names) comes back to the
/// instance until a run resumed from that continuation installs them again.
async fn run_foreground(
    program: Arc<CompiledProgram>,
    host: Arc<StepHost>,
    start: VmExecutionStart,
    mut state: State,
) -> (RunEnd, Option<State>) {
    let host = host.as_ref();
    let traced = host.config.trace_runtime_errors;
    let mut scratch = host.take_scratch().unwrap_or_default();
    let vm = match start {
        VmExecutionStart::Session => {
            Vm::from_state_recycling(&program, &mut state, host, &mut scratch)
        }
        VmExecutionStart::Continuation(mut continuation) => {
            let expired = std::mem::take(&mut continuation.expired_functions);
            match Vm::resume_from(*continuation, &program, host) {
                Ok(vm) => {
                    state.restore_expired_functions(expired);
                    Ok(vm)
                }
                Err(error) => {
                    host.store_scratch(scratch);
                    return (RunEnd::Refused(error), Some(state));
                }
            }
        }
    };
    let mut vm = match vm {
        Ok(vm) => vm,
        Err(error) => {
            host.store_scratch(scratch);
            return (RunEnd::Failed(failure(error)), Some(state));
        }
    };
    loop {
        match vm.run_parkable(traced).await {
            VmParkableRun::ParkedOnOperation => match vm.suspend() {
                Ok(mut continuation) => {
                    vm.flush_profile(host);
                    host.store_scratch(scratch);
                    // The whole session travels in the continuation: the
                    // instance keeps nothing of a parked run.
                    continuation.expired_functions = state.take_expired_functions();
                    return (
                        RunEnd::Parked(Box::new(continuation), VmParkReason::AwaitingEffect),
                        None,
                    );
                }
                // The run cannot be captured where it stands: it runs on, and
                // issues the operation again for the host to answer.
                Err(error) => {
                    host.ask(VmRequest::ParkDeclined(error)).await;
                }
            },
            VmParkableRun::Ended(result) => {
                if traced && let Err(failure) = &result {
                    host.observe_runtime_failure(failure.clone());
                }
                vm.flush_profile(host);
                let installed = vm
                    .recycle_into_state_parts(&mut scratch)
                    .and_then(|(globals, heap)| state.install_runtime(globals, heap));
                host.store_scratch(scratch);
                let end = match (result, installed) {
                    (Ok(outcome), Ok(())) => RunEnd::Finished(outcome),
                    (Err(failure), _) => RunEnd::Failed(failure),
                    (Ok(_), Err(error)) => RunEnd::Failed(self::failure(error)),
                };
                return (end, Some(state));
            }
        }
    }
}

/// A process body segment: it runs effect to effect, offering the host a
/// boundary after each one, until it ends or parks.
async fn run_process(
    program: Arc<CompiledProgram>,
    host: Arc<StepHost>,
    start: VmExecutionStart,
    mut state: State,
) -> (RunEnd, Option<State>) {
    let host = host.as_ref();
    let vm = match start {
        VmExecutionStart::Session => Vm::from_state(&program, &mut state, host),
        VmExecutionStart::Continuation(continuation) => {
            match Vm::resume_from(*continuation, &program, host) {
                Ok(vm) => Ok(vm),
                Err(error) => return (RunEnd::Refused(error), Some(state)),
            }
        }
    };
    let mut vm = match vm {
        Ok(vm) => vm,
        Err(error) => return (RunEnd::Failed(failure(error)), None),
    };
    loop {
        let step = if host.config.trace_runtime_errors {
            vm.run_process_traced_until_effect().await
        } else {
            vm.run_process_until_effect().await.map_err(failure)
        };
        match step {
            Ok(VmRunOutcome::EffectCompleted) => {
                if matches!(host.ask(VmRequest::Boundary).await, VmResume::Park) {
                    match vm.suspend() {
                        Ok(continuation) => {
                            vm.flush_profile(host);
                            return (
                                RunEnd::Parked(Box::new(continuation), VmParkReason::Boundary),
                                None,
                            );
                        }
                        Err(error) => {
                            host.ask(VmRequest::ParkDeclined(error)).await;
                        }
                    }
                }
            }
            // The host parked the run on the effect it awaits (FIG-4159).
            Ok(VmRunOutcome::HandedOver)
                if std::mem::take(&mut host.mailbox.lock_recover().parked_on_effect) =>
            {
                match vm.suspend() {
                    Ok(continuation) => {
                        vm.flush_profile(host);
                        return (
                            RunEnd::Parked(Box::new(continuation), VmParkReason::AwaitingEffect),
                            None,
                        );
                    }
                    // The run cannot be captured where it stands: it runs on,
                    // and issues the operation again for the host to answer.
                    Err(error) => {
                        host.ask(VmRequest::ParkDeclined(error)).await;
                    }
                }
            }
            Ok(VmRunOutcome::HandedOver) => {
                vm.flush_profile(host);
                return match vm.suspend() {
                    Ok(continuation) => (
                        RunEnd::Parked(Box::new(continuation), VmParkReason::HandedOver),
                        None,
                    ),
                    Err(error) => (
                        RunEnd::Failed(failure(RuntimeError::OperationParkFailed {
                            source: ExecutionHostError::new(format!(
                                "the parked operation could not be parked: {error}"
                            )),
                        })),
                        None,
                    ),
                };
            }
            Ok(VmRunOutcome::Complete(outcome)) => {
                vm.flush_profile(host);
                let installed = vm
                    .into_state_parts()
                    .and_then(|(globals, heap)| state.install_runtime(globals, heap));
                return match installed {
                    Ok(()) => (RunEnd::Finished(outcome), Some(state)),
                    Err(error) => (RunEnd::Failed(failure(error)), Some(state)),
                };
            }
            Err(failure) => {
                vm.flush_profile(host);
                let _ = vm
                    .into_state_parts()
                    .and_then(|(globals, heap)| state.install_runtime(globals, heap));
                return (RunEnd::Failed(failure), Some(state));
            }
        }
    }
}
