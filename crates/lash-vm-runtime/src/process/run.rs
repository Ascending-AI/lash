//! The `kernel_run` engine step: resume the saved run, hand it what
//! settled, and run it until no task is ready.
//!
//! The step performs only reads. It answers the CheckpointRoot the machine
//! sealed at its park with the waits it asked for there: the step's outcome
//! publishes the root's pages and owns them (FIG-6193, P1), and the process
//! activation commits the waits, with the admission of each wait's step,
//! in one `process.advance` transaction whose root names the page. The
//! step is `Repeatable`: a run that never committed is a recomputation of
//! an effect-free stretch of guest code from the same saved state and the
//! same outcomes, and its staging is dropped unpublished.

use std::sync::Arc;

use lash_core::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
use lash_core_store::tool_run::{Material, SettledOutput};
use lash_execution_contracts::process::CapacityCause;
use lash_execution_contracts::process::CapacityVerdict;
use lash_execution_contracts::process::EngineStepAnswer;
use lash_execution_contracts::process::EngineStepRun;
use lash_kernel_doc::{Datum, Document, ErrorDatum, Handle, Integer, Timestamp};

use lash_kernel_vm::{Bounds, End, Request, RunError, Target};
use lash_protocol_types::engine_fault::{PoolFault, ProtocolBreach};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{
    DrivenMachine, KernelFailure, MachineStep, Machines, datum_from_json, datum_to_json,
};
use lash_vm_client::{FrameEpoch, InfrastructureOutcome, OwnerEpoch, RunHost, VmOwner};
use lash_vocabulary::identity::ProcessId;
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
use crate::ProjectionCatalog;

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
    providers: ProjectionCatalog,
    observe: lash_vocabulary::observation::RunObservation,
    reads: std::sync::Mutex<HostReads>,
    effect_reads: Arc<dyn lash_execution_contracts::process::EngineEffectReads>,
    environment: Arc<lash_core_store::tool_surface::ExecutionEnvironment>,
    cancel: CancellationToken,
    parent_wait: std::time::Duration,
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
        (self.observe == lash_vocabulary::observation::RunObservation::Counts)
            .then(|| std::mem::take(&mut *self.taped()))
    }
}

#[async_trait::async_trait]
impl RunHost for StepHost {
    fn observe(&self) -> lash_vocabulary::observation::RunObservation {
        self.observe
    }

    /// A live delta is provisional: the segment's commit makes its total
    /// exact, as `process.ran`.
    fn ran(&self, delta: &lash_vocabulary::observation::SiteCounts) {
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

    fn enter_site(&self, at: &lash_vocabulary::sites::EffectId) {
        if let Some(trace) = &self.trace {
            trace.node(at, lash_trace::TraceNodeFact::Started { call_id: None });
        }
    }

    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Ok(self.providers.answer(handle, request).await)
    }

    /// Read through the runner the process's environment recorded.
    async fn perform(
        &self,
        effect: &lash_vocabulary::names::OperationUse,
        args: &[Datum],
        site: &lash_vocabulary::sites::PositionLabel,
        origin: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        let requirement = self
            .environment
            .fixed(effect)
            .map(|fixed| &fixed.requirement)
            .ok_or_else(|| {
                ParentFault(format!("the process reads undeclared effect `{effect}`"))
            })?;
        let input = crate::tool_input(
            args,
            |input| {
                datum_to_json(input)
                    .map_err(|error| error.to_string())
                    .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
            },
            |count| {
                format!(
                    "the effect `{effect}` takes one input record, and {count} arguments were passed"
                )
            },
        );
        let deadline = std::time::Instant::now() + self.parent_wait;
        let answer = super::reads::answer(
            &requirement.signature.result,
            self.effect_reads
                .read(effect, input, &self.cancel, deadline)
                .await
                .map_err(|error| ParentFault(error.to_string()))?,
        );
        if self.observe == lash_vocabulary::observation::RunObservation::Counts {
            self.taped().effects.push(super::state::EffectRead {
                use_: *effect,
                args: args.to_vec(),
                site: site.clone(),
                origin: origin.clone(),
                answer: answer.clone(),
            });
        }
        Ok(answer)
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
            ran: lash_vocabulary::observation::SiteCounts::new(),
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
    let loaded_execution = engine
        .load_execution(&process_input.code)
        .await
        .map_err(|error| RunFault(error.to_string()))?;
    let content = loaded_execution.content;
    let reach = loaded_execution.reach;
    let (root_link, _) = process_input
        .code
        .linked()
        .ok_or_else(|| RunFault("unrealized process input".into()))?;
    let root = content
        .links
        .get(&root_link)
        .ok_or_else(|| RunFault("process Link is absent".into()))?;
    let document = content
        .documents
        .get(&root.document)
        .ok_or_else(|| RunFault("process Document is absent".into()))?
        .clone();
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
    let loaded = lash_vm_broker::kernel::loader::declare_reach(
        &document,
        &content
            .documents
            .iter()
            .map(|(id, document)| (*id, Arc::new(document.clone())))
            .collect(),
        &reach.functions,
        &reach.machine,
        &engine.functions,
    )
    .map_err(|error| RunFault(error.to_string()))?;
    let closure = loaded.closure;
    let mut requirements = run.environment.requirements.clone();
    requirements.machine.extend(&reach.machine);
    let mut program = lash_kernel_vm::Program::prepare(
        root_link,
        content
            .documents
            .iter()
            .map(|(id, document)| (*id, Arc::new(document.clone())))
            .collect(),
        &content.links,
        lash_kernel_doc::heap::PageId(closure.reference().0),
        lash_kernel_vm::PreparedLibrary::with_charge(loaded.registry, settings.charge),
    )
    .map_err(|error| RunFault(error.to_string()))?;
    program.functions = closure.body().identities().collect();
    let counts = settings.observe == lash_kernel_vm::ObservationMode::Counts;
    let start = counts.then(|| recorded_start(process_input.code.clone(), &input));
    // A recovered wait is reported by the occurrence it was delivered as.
    let delivered: Vec<(
        lash_vocabulary::sites::WaitId,
        lash_vocabulary::sites::EffectId,
    )> = input
        .deliver
        .iter()
        .map(|delivery| (delivery.wait, delivery.identity.clone()))
        .collect();
    let modes: lash_kernel_vm::EffectModes = run
        .environment
        .uses
        .keys()
        .map(|use_| {
            (
                *use_,
                run.environment
                    .mode(use_)
                    .unwrap_or(lash_vocabulary::package::OperationMode::Waited),
            )
        })
        .collect();
    let host = Arc::new(StepHost {
        trace: trace.clone(),
        providers: ProjectionCatalog::of_backend(run.projection_providers.as_deref()),
        observe: settings.observe,
        reads: std::sync::Mutex::new(HostReads {
            read_effects: modes
                .iter()
                .filter_map(|(use_, mode)| {
                    (*mode == lash_vocabulary::package::OperationMode::Read).then_some(*use_)
                })
                .collect(),
            ..HostReads::default()
        }),
        effect_reads: Arc::clone(&run.reads),
        environment: Arc::clone(&run.environment),
        cancel: stop.clone(),
        parent_wait: engine.workers.config().tuning.parent_wait,
        deltas: std::sync::atomic::AtomicU64::new(0),
        segment: input
            .parked
            .as_ref()
            .map_or_else(|| "entry".to_owned(), |parked| parked.page.root.to_hex()),
    });
    let machines = lash_vm_client::RemoteMachines::new(
        engine.workers.clone(),
        VmOwner::new(format!("process:{}", run.process)),
        OwnerEpoch(0),
        FrameEpoch(0),
        Arc::clone(&host) as Arc<dyn RunHost>,
        &lash_kernel_doc::IdentifiedDocument::new(document.clone()),
        &settings.dialect,
        lash_vm_client::RunStart {
            link: program.link,
            instances: program.instances,
            documents: program.documents,
            target: process_input.code.target().map_err(RunFault)?,
            args,
            session: None,
            modes,
            host: Vec::new(),
            save: None,
        },
        settings.bounds(),
        closure,
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
            let withdrawn: Vec<lash_vocabulary::sites::WaitId> = park.withdrawn.clone();
            let issued = park
                .requests
                .into_iter()
                .filter(|request| {
                    !withdrawn.contains(&match request {
                        Request::Effect(effect) => effect.wait,
                    })
                })
                .map(|request| {
                    if let (Some(trace), Request::Effect(effect)) = (&trace, &request) {
                        trace.origin(&effect.identity, &effect.origin);
                    }
                    issue(&run.environment, request)
                })
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
                    requirements,
                },
                Some(candidate),
            ))
        }
    }
}

/// Where `input` starts, as a segment records it for a replay: each part
/// by reference to the retained record that holds it.
fn recorded_start(code: super::InlineRequest, input: &KernelRunInput) -> RecordedStart {
    let from = match &input.parked {
        None => StartedFrom::Entry,
        Some(parked) => StartedFrom::Output {
            run: parked.by,
            root: parked.page.root.to_hex(),
        },
    };
    let admissions = input
        .admissions
        .iter()
        .map(|(wait, held)| RecordedAdmission {
            wait: *wait,
            run: held.by,
        })
        .collect();
    let deliver = input
        .deliver
        .iter()
        .map(|delivery| RecordedDelivery {
            wait: delivery.wait,
            source: delivery.source.clone(),
            digest: outcome_digest(&delivery.outcome),
        })
        .collect();
    RecordedStart {
        code,
        from,
        admissions,
        deliver,
    }
}

/// What driving a run segment reached: its step, the root it sealed at a
/// park with the candidate that publishes it, and what it observed.
pub(super) struct Driven {
    pub(super) step: MachineStep,
    pub(super) parked: Option<(ParkedPage, RunCandidate)>,
    pub(super) ran: lash_vocabulary::observation::SiteCounts,
    pub(super) path: lash_vocabulary::observation::ExecutionPath,
    /// The waits whose failed outcome a `catch` recovered from.
    pub(super) caught: Vec<lash_vocabulary::sites::WaitId>,
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
            .map(|parked| (parked.page, super::pages::ParkedUnder::Process(step))),
        input
            .admissions
            .values()
            .map(|held| held.admission.0.clone())
            .collect(),
        binding,
        machines.closure()?,
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
        lash_vocabulary::sites::WaitId,
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
                    machines.closure()?.reference(),
                )
                .await?;
                let page = ParkedPage {
                    root: sealed.root,
                    kernel: sealed.header.kernel,
                };
                let candidate = activation.candidate(sealed.root, machines.closure()?.clone());
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

/// Why a run's pinned code is not admitted.
pub(super) enum Unpinned {
    /// The store does not keep a definition the run reaches.
    Unretained(super::documents::Unretained),
    /// A stored definition is refused (R5).
    Refused(lash_vm_broker::kernel::loader::LoadRefusal),
}

/// The execution closure of `document` over the definitions it was
/// written against (FIG-6985): each the engine holds as it holds it, and
/// each other as the store keeps it, admitted under the engine's
/// internal-operation authority.
///
/// # Errors
///
/// [`RunFault`] when the store does not answer or a closure node does not
/// encode.
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
            | PoolFault::Cgroup { .. }
            | PoolFault::Library { .. } => None,
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

/// Start arguments follow the selected function Export's signature. Main
/// takes no arguments and is never run before an Export start.
pub(super) fn entry_args(
    document: &Document,
    input: &KernelProcessInput,
) -> Result<Vec<Datum>, String> {
    use lash_vocabulary::names::StartSelector;
    let mut supplied = std::collections::BTreeMap::new();
    let signature = match &input.code {
        super::InlineRequest::Linked { start } => match &start.selector {
            StartSelector::Main => {
                if !input.args.is_empty() {
                    return Err("Main takes no arguments".into());
                }
                return Ok(Vec::new());
            }
            StartSelector::Export(name) => match document.exports.get(name) {
                Some(lash_kernel_doc::Export::Function(export)) => &export.signature,
                _ => return Err(format!("{name} is not a function Export")),
            },
        },
        super::InlineRequest::Callable { function, .. } => &function.signature,
        super::InlineRequest::Unlinked { .. } => return Err("unrealized process input".into()),
    };
    for (name, value) in &input.args {
        let param = signature
            .params
            .iter()
            .find(|param| param.name.as_str() == name)
            .ok_or_else(|| format!("unknown argument {name}"))?;
        let value = lash_vm_broker::kernel::value::datum_from_tool_json(value, &param.ty)?;
        supplied.insert(lash_vocabulary::names::Name::new(name), value);
    }
    if let super::InlineRequest::Linked { start } = &input.code {
        lash_kernel_check::validate_start_input(document, &start.selector, &supplied)
            .map_err(|error| error.to_string())?;
    } else {
        let decoder = lash_kernel_doc::Decoder {
            max_depth: lash_vocabulary::data_limits::KernelDataLimits::standard().document_depth(),
            policy: document.numbers,
            declared: &|id| document.functions.contains_key(&id.function),
        };
        for param in &signature.params {
            decoder
                .decode(
                    supplied.get(&param.name).unwrap_or(&Datum::Absent),
                    &param.ty,
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(signature
        .params
        .iter()
        .map(|param| supplied.get(&param.name).cloned().unwrap_or(Datum::Absent))
        .collect())
}

/// The wait a request names and its admission.
pub(super) fn admission_of(request: &Request) -> (lash_vocabulary::sites::WaitId, Admission) {
    let Request::Effect(request) = request;
    (
        request.wait,
        Admission(lash_kernel_state::AdmittedWait {
            identity: request.identity.clone(),
            request: lash_kernel_state::AdmittedRequest::Effect {
                use_: request.use_,
                args: request.args.clone(),
            },
        }),
    )
}

/// A waited request retains its defining use. Core resolves and commits a
/// deferred use before fitting and admitting this invocation's input.
fn issue(
    environment: &lash_core_store::tool_surface::ExecutionEnvironment,
    request: Request,
) -> Issued {
    let (wait, admission) = admission_of(&request);
    let Request::Effect(request) = request;
    if !environment.uses.contains_key(&request.use_) {
        return Issued::Refused {
            wait,
            admission,
            kind: EFFECT_UNKNOWN.into(),
            message: format!("the host offers no operation use `{}`", request.use_),
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
                "operation use `{}` takes one input record; received {count} arguments",
                request.use_
            )
        },
    );
    match input {
        Ok(input) => Issued::Effect {
            wait,
            admission,
            use_: request.use_,
            input,
        },
        Err(message) => Issued::Refused {
            wait,
            admission,
            kind: EFFECT_ARGUMENTS.into(),
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
    site: Option<lash_vocabulary::sites::Site>,
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
        let evidence =
            lash_protocol_types::engine_fault::SupervisorEvidence::Signalled { signal: 9 };
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
