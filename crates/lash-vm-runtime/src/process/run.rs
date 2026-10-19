//! The `kernel_run` engine step: resume the saved run, hand it what
//! settled, and run it until no task is ready.
//!
//! The step performs nothing. It answers the CheckpointRoot the machine
//! sealed at its park with the waits it asked for there: the step's outcome
//! publishes the root's pages and owns them (FIG-6193, P1), and the process
//! activation commits the waits, with the admission of each wait's step,
//! in one `process.advance` transaction whose root names the page. The
//! step is `Repeatable`: a run that never committed is a recomputation of
//! an effect-free stretch of guest code from the same saved state and the
//! same outcomes, and its staging is dropped unpublished.

use std::sync::Arc;

use lash_core::ProcessId;
use lash_core::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
use lash_core_store::tool_run::{Material, SettledOutput};
use lash_execution_contracts::process::CapacityCause;
use lash_execution_contracts::process::CapacityVerdict;
use lash_execution_contracts::process::EngineStepAnswer;
use lash_execution_contracts::process::EngineStepRun;
use lash_kernel_doc::{Datum, Document, ErrorDatum, Handle, Integer, Timestamp};
use lash_kernel_vm::{Bounds, End, Request, RunError, Target};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{
    DrivenMachine, KernelFailure, MachineStep, Machines, datum_from_json, datum_to_json,
};
use lash_vm_client::{FrameEpoch, InfrastructureOutcome, OwnerEpoch, RunHost, VmOwner};
use lash_vm_protocol::{PoolFault, ProtocolBreach};
use tokio_util::sync::CancellationToken;

use super::pages::{RunCandidate, StepActivation};
use super::state::{
    Admission, HostReads, Issued, KernelProcessInput, KernelRunInput, KernelRunOutput, ParkedPage,
    RecordedAdmission, RecordedDelivery, RecordedStart, StartedFrom, outcome_digest,
};
use super::{
    EFFECT_ARGUMENTS, EFFECT_UNKNOWN, KernelProcessEngine, KernelProcessFailureCode,
    KernelRecordedSettings, caused_failure, failure,
};
use crate::{HostBoundary, ProjectionCatalog};

/// Why a `kernel_run` reached neither a park nor an end: the run left
/// nothing, and the same run is asked again.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct RunFault(pub(crate) String);

/// Why a `kernel_run` answers no output of its own.
enum Unsettled {
    /// A fault of this attempt, failed within the step's attempts.
    Fault(RunFault),
    /// The host lacked capacity for the run. The run recomputes from the
    /// process's committed state and performed nothing, so recording
    /// nothing is sound: it runs again, uncounted.
    Capacity(CapacityCause, String),
}

impl From<RunFault> for Unsettled {
    fn from(fault: RunFault) -> Self {
        Self::Fault(fault)
    }
}

/// Runs the `kernel_run` step `run` to its outcome, or answers the host's
/// capacity verdict on it.
pub(crate) async fn run_kernel_step(
    engine: &KernelProcessEngine,
    run: EngineStepRun,
    stop: CancellationToken,
) -> EngineStepAnswer {
    let process = run.process.clone();
    match kernel_run(engine, run, stop).await {
        Ok((output, candidate)) => match serde_json::to_string(&output) {
            Ok(text) => {
                let output = Box::new(SettledOutput::Completed(material(&process, text)));
                match candidate {
                    Some(candidate) => EngineStepAnswer::Paged {
                        output,
                        candidate: lash_execution_contracts::process::StepCandidate(Arc::new(
                            candidate,
                        )),
                    },
                    None => EngineStepAnswer::Settled(output),
                }
            }
            Err(error) => failed(&process, &RunFault(error.to_string())).into(),
        },
        Err(Unsettled::Fault(fault)) => failed(&process, &fault).into(),
        Err(Unsettled::Capacity(cause, detail)) => {
            tracing::debug!(process_id = %process, %cause, %detail, "kernel_run met a capacity verdict");
            EngineStepAnswer::Capacity(CapacityVerdict::new(
                cause,
                engine.policy.capacity_backoff,
                detail,
            ))
        }
    }
}

fn material(process: &ProcessId, text: String) -> Material {
    Material::journal_local(
        MaterialOwner::Process {
            process_id: process.clone(),
        },
        MaterialRole::AttemptOutput,
        text,
    )
}

fn failed(process: &ProcessId, fault: &RunFault) -> SettledOutput {
    tracing::warn!(process_id = %process, error = %fault, "kernel_run reached no park");
    let output = lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Internal,
        "kernel_run_fault",
        fault.to_string(),
    ));
    let text = serde_json::to_string(&lash_protocol_types::Durable(&output)).unwrap_or_default();
    SettledOutput::Failed(material(process, text).failure(KnownFailureReason::Reported, None))
}

/// The process's host: the activation's clock reading, the engine's random
/// source and the backend's projection providers. What the body prints is
/// not kept. At `observe: counts` it records what its reads answered: the
/// segment's tape (FIG-6370).
struct StepHost {
    trace: Option<super::trace::ProcessTrace>,
    now_ms: i64,
    random: Arc<dyn Fn() -> u64 + Send + Sync>,
    providers: ProjectionCatalog,
    observe: lash_kernel_vm::ObservationMode,
    reads: std::sync::Mutex<HostReads>,
    /// The live deltas this segment reported, which name their trace facts.
    deltas: std::sync::atomic::AtomicU64,
    /// What names this segment's live facts: its start.
    segment: String,
}

impl StepHost {
    fn taped(&self) -> std::sync::MutexGuard<'_, HostReads> {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The segment's tape, at `observe: counts`.
    fn tape(&self) -> Option<HostReads> {
        (self.observe == lash_kernel_vm::ObservationMode::Counts)
            .then(|| std::mem::take(&mut *self.taped()))
    }
}

#[async_trait::async_trait]
impl RunHost for StepHost {
    fn observe(&self) -> lash_kernel_vm::ObservationMode {
        self.observe
    }

    /// A live delta is provisional: the segment's commit makes its total
    /// exact, as `process.ran`.
    fn ran(&self, delta: &lash_kernel_vm::SiteCounts) {
        if let Some(trace) = &self.trace {
            let frame = self
                .deltas
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            trace.ran(&self.segment, frame, delta);
        }
    }

    fn observes_sites(&self) -> bool {
        self.trace.is_some()
    }

    fn enter_site(&self, at: &lash_kernel_doc::EffectId) {
        if let Some(trace) = &self.trace {
            trace.node(at, lash_trace::TraceNodeFact::Started { call_id: None });
        }
    }

    async fn clock(&self) -> Result<Timestamp, ParentFault> {
        if self.observe == lash_kernel_vm::ObservationMode::Counts {
            self.taped().now_ms = Some(self.now_ms);
        }
        Ok(Timestamp {
            nanoseconds: Integer::from(self.now_ms.saturating_mul(1_000_000)),
        })
    }

    async fn random(&self) -> Result<u64, ParentFault> {
        let bits = (self.random)();
        if self.observe == lash_kernel_vm::ObservationMode::Counts {
            self.taped().random.push(bits);
        }
        Ok(bits)
    }

    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Ok(self.providers.answer(handle, request).await)
    }

    /// A process binds no read: its every effect is a recorded step.
    async fn perform(
        &self,
        effect: &lash_kernel_doc::EffectName,
        _args: &[Datum],
        _site: &lash_kernel_doc::PositionLabel,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Err(ParentFault(format!(
            "a kernel process binds no read, and its run performed `{effect}` as one"
        )))
    }

    fn print(&self, _value: Datum) -> Result<(), ParentFault> {
        Ok(())
    }
}

/// What a run answers: its output, with the candidate its park sealed.
type Answered = (KernelRunOutput, Option<RunCandidate>);

fn ended(
    outcome: lash_execution_contracts::process::ProcessOutcome,
) -> Result<Answered, Unsettled> {
    Ok((
        KernelRunOutput::Ended {
            outcome: Box::new(outcome),
            ran: lash_kernel_vm::SiteCounts::new(),
            caught: Vec::new(),
            reads: None,
            start: None,
        },
        None,
    ))
}

async fn kernel_run(
    engine: &KernelProcessEngine,
    run: EngineStepRun,
    stop: CancellationToken,
) -> Result<Answered, Unsettled> {
    let input: KernelRunInput = serde_json::from_value(run.input)
        .map_err(|error| RunFault(format!("kernel_run input: {error}")))?;
    let process_input = match KernelProcessInput::from_payload(&input.payload) {
        Ok(process_input) => process_input,
        Err(error) => {
            return ended(failure(
                KernelProcessFailureCode::PayloadInvalid,
                format!("invalid kernel process payload: {error}"),
                None,
            ));
        }
    };
    let document = match engine.documents.get_shared(&process_input.document).await {
        Ok(Some(document)) => document,
        Ok(None) => {
            return ended(failure(
                KernelProcessFailureCode::DocumentMissing,
                format!("no document `{}` is retained", process_input.document),
                None,
            ));
        }
        Err(error) => return Err(RunFault(error.to_string()).into()),
    };
    let settings: KernelRecordedSettings = match run
        .engine_settings
        .as_ref()
        .map(|settings| serde_json::from_value(settings.value().clone()))
    {
        Some(Ok(settings)) => settings,
        Some(Err(error)) => {
            return ended(failure(
                KernelProcessFailureCode::SettingsInvalid,
                format!("the process's recorded settings do not decode: {error}"),
                None,
            ));
        }
        None => {
            return ended(failure(
                KernelProcessFailureCode::SettingsInvalid,
                "the process recorded no settings",
                None,
            ));
        }
    };
    let boundary = match HostBoundary::of_links(&run.effects) {
        Ok(boundary) => boundary,
        Err(error) => {
            return ended(failure(
                KernelProcessFailureCode::BoundaryInvalid,
                error.to_string(),
                None,
            ));
        }
    };
    let args = match entry_args(&document, &process_input) {
        Ok(args) => args,
        Err(message) => {
            return ended(failure(
                KernelProcessFailureCode::ArgumentsInvalid,
                message,
                None,
            ));
        }
    };
    let trace = super::trace::ProcessTrace::new(
        engine.trace_runtime.as_ref(),
        &run.process,
        &process_input,
        settings.observe,
    );
    if let Some(trace) = &trace {
        if input.parked.is_none() {
            trace.started();
        }
        for delivery in &input.deliver {
            trace.delivered(delivery);
        }
    }
    let counts = settings.observe == lash_kernel_vm::ObservationMode::Counts;
    let start = counts
        .then(|| recorded_start(process_input.document, &input))
        .flatten();
    // A recovered wait is reported by the occurrence it was delivered as.
    let delivered: Vec<(lash_protocol_types::WaitId, lash_kernel_doc::EffectId)> = input
        .deliver
        .iter()
        .map(|delivery| (delivery.wait, delivery.identity.clone()))
        .collect();
    let host = Arc::new(StepHost {
        trace: trace.clone(),
        now_ms: run.now.0,
        random: Arc::clone(&engine.random),
        providers: ProjectionCatalog::of_backend(run.projection_providers.as_deref()),
        observe: settings.observe,
        reads: std::sync::Mutex::default(),
        deltas: std::sync::atomic::AtomicU64::new(0),
        segment: input
            .parked
            .as_ref()
            .map_or_else(|| "entry".to_owned(), |parked| parked.root.to_hex()),
    });
    let machines = lash_vm_client::RemoteMachines::new(
        engine.workers.clone(),
        VmOwner::new(format!("process:{}", run.process)),
        OwnerEpoch(0),
        FrameEpoch(0),
        Arc::clone(&host) as Arc<dyn RunHost>,
        &document,
        &settings.dialect,
        lash_vm_client::RunStart {
            target: Target::Entry(process_input.entry.clone()),
            args,
            session: None,
            modes: lash_kernel_vm::EffectModes::recorded(),
            host: Vec::new(),
            save: None,
        },
        settings.bounds(),
    )
    .map_err(|error| RunFault(error.to_string()))?;
    let Some(step_pages) = run.pages.as_ref() else {
        return Err(RunFault("the store holds no heap pages for the run".to_owned()).into());
    };
    let driven = drive(
        &machines,
        engine,
        step_pages,
        &run.process,
        Arc::clone(&run.clock),
        input,
        &stop,
    )
    .await;
    let Driven {
        step,
        parked: state,
        ran,
        caught,
        ..
    } = match driven {
        Ok(driven) => driven,
        Err(failure) => return worker_failure(failure),
    };
    let caught = if counts {
        caught
            .iter()
            .filter_map(|wait| {
                delivered
                    .iter()
                    .find(|(delivered, _)| delivered == wait)
                    .map(|(_, at)| at.clone())
            })
            .collect()
    } else {
        Vec::new()
    };
    let reads = host.tape();
    let start = start.map(Box::new);
    match step {
        MachineStep::Ended(end) => {
            let outcome = outcome_of_end(end);
            if let Some(trace) = &trace {
                trace.finished(&outcome);
            }
            Ok((
                KernelRunOutput::Ended {
                    outcome: Box::new(outcome),
                    ran,
                    caught,
                    reads,
                    start,
                },
                None,
            ))
        }
        MachineStep::Slice => Err(RunFault("the run was stopped mid-slice".to_owned()).into()),
        MachineStep::Parked(park) => {
            let Some((state, candidate)) = state else {
                return Err(RunFault("the run parked without a state".to_owned()).into());
            };
            let withdrawn: Vec<lash_protocol_types::WaitId> = park.withdrawn.clone();
            let issued = park
                .requests
                .into_iter()
                .filter(|request| {
                    !withdrawn.contains(&match request {
                        Request::Effect(effect) => effect.wait,
                        Request::Sleep(sleep) => sleep.wait,
                    })
                })
                .map(|request| issue(&boundary, run.now.0, request))
                .collect::<Vec<_>>();
            if let Some(trace) = &trace {
                for issued in &issued {
                    trace.issued(issued);
                }
            }
            Ok((
                KernelRunOutput::Parked {
                    state,
                    issued,
                    withdrawn,
                    ran,
                    caught,
                    reads,
                    start,
                },
                Some(candidate),
            ))
        }
    }
}

/// Where `input` starts, as a segment records it for a replay: everything
/// a retained record holds by reference to it, and only the rest whole.
/// A parked page is recorded only by the output that holds it: one no
/// recorded output holds has no record to be read back under, and the
/// segment records no start.
fn recorded_start(
    document: lash_kernel_doc::DocumentId,
    input: &KernelRunInput,
) -> Option<RecordedStart> {
    let from = match (&input.parked, input.parked_by) {
        (None, _) => StartedFrom::Entry,
        (Some(parked), Some(run)) => StartedFrom::Output {
            run,
            root: parked.root.to_hex(),
        },
        (Some(_), None) => return None,
    };
    let admissions = input
        .admissions
        .iter()
        .enumerate()
        .map(
            |(index, admission)| match input.admitted_by.get(index).copied().flatten() {
                Some((wait, run)) => RecordedAdmission::Issued { wait, run },
                None => RecordedAdmission::Whole(Box::new(admission.clone())),
            },
        )
        .collect();
    let deliver = input
        .deliver
        .iter()
        .map(|delivery| match &delivery.source {
            Some(source) => RecordedDelivery::Of {
                wait: delivery.wait,
                source: source.clone(),
                digest: outcome_digest(&delivery.outcome),
            },
            None => RecordedDelivery::Whole(Box::new(delivery.clone())),
        })
        .collect();
    Some(RecordedStart {
        document,
        from,
        admissions,
        deliver,
    })
}

/// What driving a run segment reached: its step, the root it sealed at a
/// park with the candidate that publishes it, and what it observed.
pub(super) struct Driven {
    pub(super) step: MachineStep,
    pub(super) parked: Option<(ParkedPage, RunCandidate)>,
    pub(super) ran: lash_kernel_vm::SiteCounts,
    pub(super) path: lash_kernel_vm::ExecutionPath,
    /// The waits whose failed outcome a `catch` recovered from.
    pub(super) caught: Vec<lash_protocol_types::WaitId>,
}

/// Starts or resumes the machine over a page activation of the process's
/// run, hands it the settled outcomes and runs it until it parks or ends.
/// A parked machine answers the root it sealed, checked, with the
/// candidate that publishes it.
async fn drive(
    machines: &lash_vm_client::RemoteMachines,
    engine: &KernelProcessEngine,
    step: &lash_execution_contracts::process::StepPages,
    process: &ProcessId,
    clock: std::sync::Arc<dyn lash_core_execution::runtime::Clock>,
    input: KernelRunInput,
    stop: &CancellationToken,
) -> Result<Driven, KernelFailure> {
    let binding = lash_vm_broker::kernel::graph::owner_binding_node(
        &lash_durable::domain::ExecKey::Process(process.clone()),
    )
    .map_err(|refusal| KernelFailure::State(refusal.into()))?;
    let config = engine.workers.config();
    let spools = engine.workers.spools()?;
    let mut activation = StepActivation::open(
        std::sync::Arc::clone(&step.store),
        process,
        input
            .parked
            .map(|parked| (parked, super::pages::ParkedUnder::Process(step))),
        input
            .admissions
            .iter()
            .map(|admission| admission.0.clone())
            .collect(),
        binding,
        machines.releases()?,
        &spools,
        clock,
        lash_vm_broker::kernel::pages::PageLimits::within(config.protocol.decode),
    )
    .await?;
    let driven = run_machine(
        machines,
        &mut activation,
        binding,
        input
            .deliver
            .into_iter()
            .map(|delivery| (delivery.wait, delivery.outcome))
            .collect(),
        config.tuning.slice,
        stop,
    )
    .await;
    activation.end_reads().await;
    driven
}

/// Runs the machine of `activation`, resumed from its parked root or
/// started fresh, hands it `deliver` and runs it until it parks or ends.
/// A park answers the root it sealed, checked, with the candidate that
/// publishes it; nothing publishes unless the caller does.
pub(super) async fn run_machine(
    machines: &lash_vm_client::RemoteMachines,
    activation: &mut StepActivation,
    binding: lash_core_store::checkpoint_graph::NodeId,
    deliver: Vec<(
        lash_protocol_types::WaitId,
        lash_vm_client::wire::OutcomeWire,
    )>,
    slice: u64,
    stop: &CancellationToken,
) -> Result<Driven, KernelFailure> {
    let pages = std::sync::Arc::clone(&activation.pages);
    let mut machine = match &activation.parked {
        Some(parked) => machines.resume(parked, binding, pages).await?,
        None => machines.start(binding, pages).await?,
    };
    for (wait, outcome) in deliver {
        machine.deliver(wait, outcome.into()).await?;
    }
    loop {
        let step = machine.run(slice, false).await?;
        let parked = match &step {
            // A stopped step is asked again from the same input.
            MachineStep::Slice if stop.is_cancelled() => None,
            MachineStep::Slice => continue,
            MachineStep::Parked(_) => {
                let sealed = lash_vm_broker::kernel::run::sealed(
                    &mut machine,
                    &activation.pages,
                    binding,
                    &machines.document(),
                )
                .await?;
                let page = ParkedPage {
                    root: sealed.root,
                    kernel: sealed.header.kernel,
                };
                let candidate = activation.candidate(
                    sealed.root,
                    lash_core_store::checkpoint_graph::NodeId(sealed.header.release.0),
                );
                Some((page, candidate))
            }
            MachineStep::Ended(_) => None,
        };
        let (ran, path, caught) = (
            machine.take_ran(),
            machine.take_path(),
            machine.take_caught(),
        );
        if !matches!(step, MachineStep::Slice) {
            machine.release().await?;
        }
        return Ok(Driven {
            step,
            parked,
            ran,
            path,
            caught,
        });
    }
}

/// What a failure to drive the machine is to the process: its terminal
/// when every attempt would meet it the same way, the host's capacity
/// verdict, which records nothing and runs again uncounted, or a fault of
/// this attempt, which is asked again within the step's attempts.
fn worker_failure(failure_: KernelFailure) -> Result<Answered, Unsettled> {
    if let KernelFailure::Worker { outcome } = &failure_
        && let Some(cause) = capacity_cause(outcome)
    {
        return Err(Unsettled::Capacity(cause, failure_.to_string()));
    }
    match &failure_ {
        KernelFailure::Start(_) | KernelFailure::Open(_) => ended(failure(
            KernelProcessFailureCode::RunRefused,
            failure_.to_string(),
            None,
        )),
        KernelFailure::Worker {
            outcome: InfrastructureOutcome::RunRefused { refusal },
        } => ended(failure(
            KernelProcessFailureCode::RunRefused,
            format!("the worker refuses the run: {refusal}"),
            Some(serde_json::json!({ "run_refusal": refusal })),
        )),
        KernelFailure::Worker {
            outcome: InfrastructureOutcome::WorkerLimitExceeded { limit },
        } if !limit.is_host_verdict() => ended(failure(
            KernelProcessFailureCode::BoundExceeded,
            format!("worker execution bound exhausted: {limit}"),
            Some(serde_json::json!({ "worker_limit": limit })),
        )),
        _ => Err(RunFault(failure_.to_string()).into()),
    }
}

/// The capacity verdict a worker failure is, by its typed outcome: the
/// pool's queue, checkout wait or restart window, or retryable worker
/// infrastructure (FIG-4451). A refusal of the run, a limit of the run
/// itself and a deployment or configuration fault are none. Neither is a
/// worker that died running the run: that is a fault of the attempt, which
/// spends one of the step's attempts, so a run that keeps killing its
/// worker ends once they are spent (FIG-6786).
fn capacity_cause(outcome: &InfrastructureOutcome) -> Option<CapacityCause> {
    match outcome {
        InfrastructureOutcome::ProtocolViolation {
            breach: ProtocolBreach::Pool { fault },
        } => match fault {
            PoolFault::QueueFull { .. } => Some(CapacityCause::QueueFull),
            PoolFault::CheckoutTimedOut => Some(CapacityCause::CheckoutTimedOut),
            PoolFault::RestartStorm => Some(CapacityCause::RestartStorm),
            PoolFault::Recovery
            | PoolFault::InvalidConfiguration
            | PoolFault::UnsupportedPlatform
            | PoolFault::Io { .. }
            | PoolFault::Cgroup { .. } => None,
        },
        InfrastructureOutcome::WorkerLimitExceeded { limit } => limit
            .is_host_verdict()
            .then_some(CapacityCause::RetryableInfrastructure),
        InfrastructureOutcome::WorkerCrashed { .. }
        | InfrastructureOutcome::WorkerUnresponsive { .. }
        | InfrastructureOutcome::ProtocolViolation { .. } => {
            Some(CapacityCause::RetryableInfrastructure)
        }
        InfrastructureOutcome::PageRefused { .. } | InfrastructureOutcome::HeapFault { .. } => {
            outcome
                .is_retryable()
                .then_some(CapacityCause::RetryableInfrastructure)
        }
        InfrastructureOutcome::RunRefused { .. }
        | InfrastructureOutcome::RunCrashedWorker { .. }
        | InfrastructureOutcome::WorkerDeployment { .. } => None,
    }
}

/// The entry's arguments in parameter order, from the start's arguments by
/// name. An omitted optional parameter is absent (`K-FN-004`).
pub(super) fn entry_args(
    document: &Document,
    input: &KernelProcessInput,
) -> Result<Vec<Datum>, String> {
    let entry = document.entries.get(&input.entry).ok_or_else(|| {
        format!(
            "document `{}` has no entry `{}`",
            input.document, input.entry
        )
    })?;
    let signature = &entry.signature;
    if let Some(unknown) = input.args.keys().find(|name| {
        !signature
            .params
            .iter()
            .any(|param| param.name.as_str() == name.as_str())
    }) {
        return Err(format!(
            "entry `{}` has no parameter `{unknown}`",
            input.entry
        ));
    }
    signature
        .params
        .iter()
        .map(|param| match input.args.get(param.name.as_str()) {
            Some(value) => datum_from_json(&value.to_string()).map_err(|error| {
                format!("argument `{}` is not an effect value: {error}", param.name)
            }),
            None if param.optional => Ok(Datum::Absent),
            None => Err(format!(
                "entry `{}` requires argument `{}`",
                input.entry, param.name
            )),
        })
        .collect()
}

/// The wait a request names and its admission.
pub(super) fn admission_of(request: &Request) -> (lash_protocol_types::WaitId, Admission) {
    match request {
        Request::Sleep(sleep) => (
            sleep.wait,
            Admission(lash_kernel_state::AdmittedWait {
                identity: sleep.identity.clone(),
                request: lash_kernel_state::AdmittedRequest::Sleep {
                    duration: sleep.duration.into(),
                },
            }),
        ),
        Request::Effect(request) => (
            request.wait,
            Admission(lash_kernel_state::AdmittedWait {
                identity: request.identity.clone(),
                request: lash_kernel_state::AdmittedRequest::Effect {
                    effect: request.effect.clone(),
                    args: request.args.clone(),
                },
            }),
        ),
    }
}

/// Resolves one wait the machine asked for against the host boundary.
fn issue(boundary: &HostBoundary, now_ms: i64, request: Request) -> Issued {
    let (wait, admission) = admission_of(&request);
    let request = match request {
        Request::Sleep(sleep) => {
            let millis = i64::try_from(sleep.duration.as_millis()).unwrap_or(i64::MAX);
            return Issued::Sleep {
                wait,
                admission,
                until_ms: now_ms.saturating_add(millis),
            };
        }
        Request::Effect(request) => request,
    };
    if boundary.effect(&request.effect).is_none() {
        return Issued::Refused {
            wait,
            admission,
            kind: EFFECT_UNKNOWN.to_owned(),
            message: format!("the host offers no effect `{}`", request.effect),
        };
    }
    let input = crate::tool_input(
        &request.args,
        |input| {
            let text = datum_to_json(input).map_err(|error| error.to_string())?;
            serde_json::from_str(&text).map_err(|error| error.to_string())
        },
        |count| {
            format!(
                "the effect `{}` takes one input record, and {count} arguments were passed",
                request.effect
            )
        },
    );
    match input {
        Ok(input) => Issued::Effect {
            wait,
            admission,
            effect: request.effect.clone(),
            input,
        },
        Err(message) => Issued::Refused {
            wait,
            admission,
            kind: EFFECT_ARGUMENTS.to_owned(),
            message,
        },
    }
}

/// How a run's end becomes the output its awaiter reads.
pub(super) fn outcome_of_end(end: End) -> lash_execution_contracts::process::ProcessOutcome {
    let json = |value: &Datum| {
        datum_to_json(value)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
    };
    match end {
        End::Finished(finished) => match json(&finished.result) {
            Some(value) => lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(value),
            ),
            None => failure(
                KernelProcessFailureCode::ResultNotData,
                "the process finished with a value that is not effect data",
                None,
            ),
        },
        End::Failed(value) => failure(
            KernelProcessFailureCode::Failed,
            "the process failed",
            json(&value),
        ),
        End::Error(RunError::Bound(exceeded)) => failure(
            KernelProcessFailureCode::BoundExceeded,
            exceeded.to_string(),
            None,
        ),
        End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
            ..
        }) => failure(
            KernelProcessFailureCode::TasksOutstanding,
            format!(
                "the process ended with {} unfinished and {} unobserved failed tasks",
                unfinished.len(),
                unobserved.len()
            ),
            Some(serde_json::json!({ "unfinished": unfinished, "unobserved": unobserved })),
        ),
        End::Error(RunError::Uncaught { error, site }) => uncaught(error, site),
        End::Error(error) => failure(
            KernelProcessFailureCode::RuntimeError,
            error.to_string(),
            None,
        ),
        End::Cancelled => lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
                "the process was cancelled",
            )),
        ),
    }
}

/// The terminal of a run that ended in an error no `catch` took
/// (FIG-6810): the error's kind and the site that raised it as the typed
/// cause, its message as the failure's, its data as the raw value.
fn uncaught(
    error: Datum,
    site: Option<lash_kernel_doc::Site>,
) -> lash_execution_contracts::process::ProcessOutcome {
    let json = |value: &Datum| {
        datum_to_json(value)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
    };
    let (error_kind, message, raw) = match error {
        Datum::Error(error) => {
            let raw = match &error.data {
                Datum::Null => None,
                data => json(data),
            };
            (Some(error.kind), error.message, raw)
        }
        value => (
            None,
            "the process ended with a raised value that is not an error".to_owned(),
            json(&value),
        ),
    };
    caused_failure(
        KernelProcessFailureCode::RuntimeError,
        message,
        raw,
        Some(lash_core::ToolFailureCause::UncaughtError { error_kind, site }),
    )
}

impl KernelRecordedSettings {
    pub(crate) fn bounds(&self) -> Bounds {
        Bounds {
            memory_meter: self.memory_meter,
            charge: self.charge,
            charge_limit: self.charge_limit,
            memory: self.memory,
            call_depth: self.call_depth,
            live_tasks: self.live_tasks,
            requests_per_park: self.requests_per_park,
            join_members: self.join_members,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-6786: a worker that died running the run is a fault of the
    /// attempt, which spends one of the step's attempts, so a run that keeps
    /// killing its worker ends once they are spent. A worker lost before it
    /// computed the run stays the host's capacity verdict, which spends
    /// none.
    #[test]
    fn a_worker_the_run_crashed_spends_an_attempt_and_one_lost_before_it_ran_does_not() {
        let evidence = lash_vm_protocol::SupervisorEvidence::Signalled { signal: 9 };
        let ran = worker_failure(KernelFailure::Worker {
            outcome: InfrastructureOutcome::RunCrashedWorker { evidence },
        });
        assert!(matches!(ran, Err(Unsettled::Fault(_))));
        let lost = worker_failure(KernelFailure::Worker {
            outcome: InfrastructureOutcome::WorkerCrashed { evidence },
        });
        assert!(matches!(
            lost,
            Err(Unsettled::Capacity(
                CapacityCause::RetryableInfrastructure,
                _
            ))
        ));
    }
}
