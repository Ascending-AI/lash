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
use lash_kernel_doc::{Datum, Document, ErrorDatum};

use lash_kernel_vm::{Bounds, End, Request, RunError};
use lash_protocol_types::engine_fault::{PoolFault, ProtocolBreach};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{DrivenMachine, KernelFailure, MachineStep, Machines, datum_to_json};
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
    use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
    let process = run.process.clone();
    let mut engine = engine.clone();
    engine.workers = engine
        .workers
        .begin_execution(std::time::Duration::ZERO, Arc::clone(&run.clock));
    match kernel_run(&engine, run, stop).await {
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
    observe: lash_kernel_value::observation::RunObservation,
    reads: std::sync::Mutex<HostReads>,
    read_ordinal: std::sync::atomic::AtomicU64,
    facts: std::sync::Mutex<Vec<lash_protocol_types::ReadFact>>,
    read_panic: std::sync::Mutex<Option<lash_protocol_types::ReadPanic>>,
    effect_reads: Arc<dyn lash_execution_contracts::process::EngineEffectReads>,
    environment: Arc<lash_core_store::tool_surface::ExecutionEnvironment>,
    cancel: CancellationToken,
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
        (self.observe == lash_kernel_value::observation::RunObservation::Counts)
            .then(|| std::mem::take(&mut *self.taped()))
    }
}

#[async_trait::async_trait]
impl RunHost for StepHost {
    fn observe(&self) -> lash_kernel_value::observation::RunObservation {
        self.observe
    }

    /// A live delta is provisional: the segment's commit makes its total
    /// exact, as `process.ran`.
    fn ran(&self, delta: &lash_kernel_value::observation::SiteCounts) {
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

    fn enter_site(&self, location: &lash_kernel_value::sites::EffectLocation) {
        if let Some(trace) = &self.trace {
            trace.origin(&location.at, &location.origin);
            trace.node(
                &location.at,
                lash_trace::TraceNodeFact::Started { call_id: None },
            );
        }
    }

    /// Read through the runner the process's environment recorded.
    async fn perform(
        &self,
        effect: &lash_kernel_value::names::OperationUse,
        args: &[Datum],
        site: &lash_kernel_value::sites::PositionLabel,
        origin: &lash_kernel_value::sites::Site,
        parent_remaining: std::time::Duration,
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
        let result = self
            .effect_reads
            .read(
                effect,
                input,
                lash_execution_contracts::process::EngineReadSite {
                    ordinal: self
                        .read_ordinal
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1,
                    site,
                    origin,
                },
                &self.cancel,
                parent_remaining,
            )
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        self.facts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(result.fact);
        let answer = match lash_vm_broker::kernel::value::read_answer(
            &requirement.signature.result,
            result.answer,
        ) {
            Ok(answer) => answer,
            Err(panic) => {
                *self
                    .read_panic
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(panic.clone());
                return Err(ParentFault(panic.message));
            }
        };
        if self.observe == lash_kernel_value::observation::RunObservation::Counts {
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
            read_facts: Vec::new(),
            ran: lash_kernel_value::observation::SiteCounts::new(),
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
    let loaded_execution = engine
        .documents
        .load_execution_content(&process_input.code, &run.environment.retained_code)
        .await
        .map_err(|error| RunFault(error.to_string()))?;
    let content = &loaded_execution.content;
    let restore_selection = if let Some(parked) = &input.parked {
        let step = run
            .pages
            .as_ref()
            .ok_or_else(|| RunFault("the store holds no heap pages for the run".into()))?;
        let exec = lash_durable::domain::ExecKey::Process(run.process.clone());
        let row = step
            .store
            .exec_root(&exec)
            .await
            .map_err(|error| RunFault(error.to_string()))?
            .ok_or_else(|| RunFault("the process has no execution root".into()))?;
        let binding = lash_vm_broker::kernel::graph::owner_binding_node(&exec)
            .map_err(|error| RunFault(error.to_string()))?;
        let checked = super::pages::check_parked_helpers(
            Arc::clone(&step.store),
            lash_durable::domain::pages::RootLeaseRequest {
                owner: lash_durable::domain::RootOwner::Exec(exec),
                expected: row.root,
                fence: lash_durable::domain::pages::LeaseFence::Actor {
                    actor: step.actor.clone(),
                    epoch: step.epoch,
                },
                ttl_millis: 60_000,
            },
            parked.page.root,
            binding,
            content,
            Arc::clone(&engine.helpers),
        )
        .await;
        match checked {
            Ok(selection) => selection,
            Err(failure) => {
                return worker_failure(
                    failure,
                    &settings.bounds(),
                    worker_bound_readings(&engine.workers),
                );
            }
        }
    } else {
        lash_kernel_doc::Selection::Fresh
    };

    let loaded_execution = engine
        .documents
        .admit_execution(loaded_execution, &restore_selection)
        .map_err(|error| RunFault(error.to_string()))?;
    let ordinary = loaded_execution.ordinary;
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
    let args = match entry_args(
        &document,
        &process_input,
        &engine.helpers,
        &restore_selection,
    ) {
        Ok(args) => args,
        Err(error) => {
            let cause = match &error {
                EntryArgumentsError::Boundary(
                    lash_kernel_check::StartBoundaryRefusal::TypeMismatch(source),
                ) => Some(lash_core::ToolFailureCause::EntryTypeMismatch {
                    source: (**source).clone(),
                }),
                _ => None,
            };
            return ended(caused_failure(
                KernelProcessFailureCode::ArgumentsInvalid,
                error.to_string(),
                None,
                cause,
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
            .map(|(id, document)| (*id, Arc::clone(document)))
            .collect(),
        &ordinary,
        &reach.machine,
        &engine.functions,
        &engine.helpers,
        &restore_selection,
    )
    .map_err(|error| RunFault(error.to_string()))?;
    let closure = loaded.closure;
    let requirements = lash_vocabulary::claims::ClaimRequirements {
        machine: reach.machine.clone(),
        ..Default::default()
    };
    let mut program = lash_kernel_vm::Program::prepare(
        root_link,
        content
            .documents
            .iter()
            .map(|(id, document)| (*id, Arc::clone(document)))
            .collect(),
        &content.links,
        lash_kernel_doc::heap::PageId(closure.reference().0),
        lash_kernel_vm::PreparedLibrary::with_charge(loaded.registry, settings.charge)
            .with_helper_handle(Arc::clone(&engine.helpers)),
        restore_selection.clone(),
    )
    .map_err(|error| RunFault(error.to_string()))?;
    program.functions = closure.body().identities().collect();
    let counts = settings.observe == lash_kernel_vm::ObservationMode::Counts;
    let start = counts.then(|| {
        recorded_start(
            process_input.code.clone(),
            &input,
            &run.environment.retained_code,
        )
    });
    // A recovered wait is reported by the occurrence it was delivered as.
    let delivered: Vec<(
        lash_kernel_value::sites::WaitId,
        lash_kernel_value::sites::EffectId,
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
                    .unwrap_or(lash_kernel_value::package::OperationMode::Waited),
            )
        })
        .collect();
    let host = Arc::new(StepHost {
        trace: trace.clone(),
        observe: settings.observe,
        reads: std::sync::Mutex::new(HostReads {
            read_effects: modes
                .iter()
                .filter_map(|(use_, mode)| {
                    (*mode == lash_kernel_value::package::OperationMode::Read).then_some(*use_)
                })
                .collect(),
            ..HostReads::default()
        }),
        facts: std::sync::Mutex::new(Vec::new()),
        read_panic: std::sync::Mutex::new(None),
        read_ordinal: std::sync::atomic::AtomicU64::new(
            input.parked.as_ref().map_or(0, |parked| parked.read_count),
        ),
        effect_reads: Arc::clone(&run.reads),
        environment: Arc::clone(&run.environment),
        cancel: stop.clone(),
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
        &lash_kernel_doc::IdentifiedDocument::from_shared(Arc::clone(&document)),
        &settings.dialect,
        lash_vm_client::RunStart {
            link: program.link,
            instances: program.instances,
            documents: program
                .documents
                .into_iter()
                .map(|(id, document)| {
                    (
                        id,
                        lash_kernel_doc::IdentifiedDocument::from_shared(document),
                    )
                })
                .collect(),
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
    let machines = match restore_selection {
        lash_kernel_doc::Selection::Fresh => machines,
        lash_kernel_doc::Selection::Restored(selection) => {
            machines.with_restore_selection(selection)
        }
    };
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
    let read_facts = std::mem::take(
        &mut *host
            .facts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let read_panic = host
        .read_panic
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(panic) = read_panic {
        let outcome = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::failure(panic.failure()),
        );
        return Ok((
            KernelRunOutput::Ended {
                outcome: Box::new(outcome),
                read_facts,
                ran: Default::default(),
                caught: Vec::new(),
                reads: host.tape(),
                start: start.map(Box::new),
            },
            None,
        ));
    }
    let Driven {
        step,
        meters,
        parked: state,
        ran,
        caught,
        ..
    } = match driven {
        Ok(driven) => driven,
        Err(failure) => {
            let result = worker_failure(
                failure,
                &settings.bounds(),
                worker_bound_readings(&engine.workers),
            );
            if let Ok((KernelRunOutput::Ended { outcome, .. }, _)) = &result
                && let Some(trace) = &trace
            {
                trace.finished(outcome);
            }
            return result;
        }
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
            let outcome = outcome_of_end(end, meters);
            if let Some(trace) = &trace {
                trace.finished(&outcome);
            }
            Ok((
                KernelRunOutput::Ended {
                    read_facts,
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
            let cancel_requests: Vec<lash_kernel_value::sites::WaitId> =
                park.cancel_requests.clone();
            let issued = park
                .requests
                .into_iter()
                .filter(|request| {
                    !cancel_requests.contains(&match request {
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
                    read_facts,
                    read_count: host.read_ordinal.load(std::sync::atomic::Ordering::Relaxed),
                    state,
                    issued,
                    cancel_requests,
                    ran,
                    caught,
                    reads,
                    start,
                    requirements: Box::new(requirements),
                },
                Some(candidate),
            ))
        }
    }
}

/// Where `input` starts, as a segment records it for a replay: each part
/// by reference to the retained record that holds it.
fn recorded_start(
    code: super::InlineRequest,
    input: &KernelRunInput,
    retained: &std::collections::BTreeSet<lash_kernel_value::CallableId>,
) -> RecordedStart {
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
        retained_code: retained.clone(),
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
    pub(super) meters: Option<lash_kernel_vm::Meters>,
    pub(super) parked: Option<(ParkedPage, RunCandidate)>,
    pub(super) ran: lash_kernel_value::observation::SiteCounts,
    pub(super) path: lash_kernel_value::observation::ExecutionPath,
    /// The waits whose failed outcome a `catch` recovered from.
    pub(super) caught: Vec<lash_kernel_value::sites::WaitId>,
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
        lash_kernel_value::sites::WaitId,
        lash_vm_client::wire::OutcomeWire,
    )>,
    slice: u64,
    stop: &CancellationToken,
) -> Result<Driven, KernelFailure> {
    let mut owner = ProcessKernelOwner {
        machines,
        activation,
        binding,
        slice,
        stop,
        machine: None,
        deliver: Some(deliver),
    };
    lash_core_execution::kernel_runner::run(&mut owner).await
}

struct ProcessKernelOwner<'a> {
    machines: &'a lash_vm_client::RemoteMachines,
    activation: &'a mut StepActivation,
    binding: lash_core_store::checkpoint_graph::NodeId,
    slice: u64,
    stop: &'a CancellationToken,
    machine: Option<lash_vm_client::RemoteMachine>,
    deliver: Option<
        Vec<(
            lash_kernel_value::sites::WaitId,
            lash_vm_client::wire::OutcomeWire,
        )>,
    >,
}

impl ProcessKernelOwner<'_> {
    async fn finish(&mut self, step: MachineStep) -> Result<Driven, KernelFailure> {
        let machine = self
            .machine
            .as_mut()
            .ok_or(KernelFailure::EmptyCheckpoint)?;
        let parked = if matches!(step, MachineStep::Parked(_)) {
            let sealed = lash_vm_broker::kernel::run::sealed(
                machine,
                &self.activation.pages,
                self.binding,
                &self.machines.document(),
                self.machines.closure()?.reference(),
            )
            .await?;
            let page = ParkedPage {
                root: sealed.root,
                kernel: sealed.header.kernel,
            };
            Some((
                page,
                self.activation
                    .candidate(sealed.root, self.machines.closure()?.clone()),
            ))
        } else {
            None
        };
        let machine_meters = machine.meters();
        let (ran, path, caught) = (
            machine.take_ran(),
            machine.take_path(),
            machine.take_caught(),
        );
        if !matches!(step, MachineStep::Slice)
            && let Some(machine) = self.machine.take()
        {
            machine.release().await?;
        }
        Ok(Driven {
            step,
            parked,
            ran,
            path,
            caught,
            meters: machine_meters,
        })
    }
}

#[async_trait::async_trait]
impl lash_core_execution::kernel_runner::KernelOwner for ProcessKernelOwner<'_> {
    type Error = KernelFailure;
    type Output = Driven;

    fn bounds(&self) -> Bounds {
        self.machines.bounds()
    }

    async fn next(&mut self) -> Result<MachineStep, KernelFailure> {
        let pages = Arc::clone(&self.activation.pages);
        let work = async {
            if self.machine.is_none() {
                self.machine = Some(match &self.activation.parked {
                    Some(parked) => {
                        self.machines
                            .resume(parked, self.binding, Arc::clone(&pages))
                            .await?
                    }
                    None => {
                        self.machines
                            .start(self.binding, Arc::clone(&pages))
                            .await?
                    }
                });
            }
            let machine = self
                .machine
                .as_mut()
                .ok_or(KernelFailure::EmptyCheckpoint)?;
            if let Some(deliver) = self.deliver.take() {
                for (wait, outcome) in deliver {
                    machine.deliver(wait, outcome.into()).await?;
                }
            }
            machine.run(self.slice, false).await
        };
        lash_vm_broker::kernel::pages::shared::while_held(&self.activation.pages, work).await
    }

    async fn sliced(&mut self) -> Result<Option<Driven>, KernelFailure> {
        if self.stop.is_cancelled() {
            self.finish(MachineStep::Slice).await.map(Some)
        } else {
            Ok(None)
        }
    }

    async fn park(
        &mut self,
        park: lash_core_execution::kernel_runner::MachinePark,
    ) -> Result<Option<Driven>, KernelFailure> {
        self.finish(MachineStep::Parked(park)).await.map(Some)
    }

    async fn end(&mut self, end: End) -> Result<Driven, KernelFailure> {
        self.finish(MachineStep::Ended(end)).await
    }
}

/// What a failure to drive the machine is to the process: its terminal
/// when every attempt would meet it the same way, the host's capacity
/// verdict, which records nothing and runs again uncounted, or a fault of
/// this attempt, which is asked again within the step's attempts.
fn worker_failure(
    failure_: KernelFailure,
    bounds: &Bounds,
    readings: lash_core_execution::kernel_runner::BoundReadings,
) -> Result<Answered, Unsettled> {
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
        } if !limit.is_host_verdict() => ended(caused_failure(
            KernelProcessFailureCode::BoundExceeded,
            format!("worker execution bound exhausted: {limit}"),
            Some(serde_json::json!({ "worker_limit": limit })),
            lash_core_execution::kernel_runner::worker_bound_fact(*limit, bounds, readings)
                .map(|fact| lash_core::ToolFailureCause::ExecutionBound { fact }),
        )),
        _ => Err(RunFault(failure_.to_string()).into()),
    }
}

fn worker_bound_readings(
    service: &lash_vm_client::service::Service,
) -> lash_core_execution::kernel_runner::BoundReadings {
    use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
    let budget = service.execution_budget();
    let meters = budget.and_then(|budget| budget.meters());
    let nanos =
        |duration: std::time::Duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    lash_core_execution::kernel_runner::BoundReadings {
        charged: meters.and_then(|meters| meters.charged),
        memory: meters.map(|meters| meters.memory),
        worker_cpu_nanos: budget.and_then(|budget| budget.cpu()).map(nanos),
        held_wall_nanos: budget.map(|budget| nanos(budget.held())),
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
            PoolFault::RestartStorm { .. } => Some(CapacityCause::RestartStorm),
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
#[derive(Debug, thiserror::Error)]
pub(super) enum EntryArgumentsError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Boundary(#[from] lash_kernel_check::StartBoundaryRefusal),
    #[error(transparent)]
    Callable(#[from] lash_kernel_doc::FunctionValueRefusal),
    #[error(transparent)]
    Decode(#[from] lash_kernel_doc::DecodeRefusal),
}
impl From<&str> for EntryArgumentsError {
    fn from(message: &str) -> Self {
        Self::Message(message.into())
    }
}
impl From<String> for EntryArgumentsError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}
pub(super) fn entry_args(
    document: &Document,
    input: &KernelProcessInput,
    helpers: &lash_kernel_doc::BuildHelpers,
    selection: &lash_kernel_doc::Selection,
) -> Result<Vec<Datum>, EntryArgumentsError> {
    use lash_kernel_value::names::StartSelector;
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
                _ => return Err(format!("{name} is not a function Export").into()),
            },
        },
        super::InlineRequest::Callable { function, .. } => &function.signature,
        super::InlineRequest::Unlinked { .. } => return Err("unrealized process input".into()),
    };
    let callable = match &input.code {
        super::InlineRequest::Linked { start } => {
            let lash_kernel_value::StartSelector::Export(name) = &start.selector else {
                unreachable!("Main returned above")
            };
            let Some(lash_kernel_doc::Export::Function(export)) = document.exports.get(name) else {
                unreachable!("checked above")
            };
            lash_kernel_value::CallableId {
                link: start.link,
                function: export.function,
            }
        }
        super::InlineRequest::Callable { function, .. } => {
            function.entry_export(document, helpers, selection)?;
            function.callable
        }
        _ => unreachable!("unlinked input returned above"),
    };
    for (name, value) in &input.args {
        let param = signature
            .params
            .iter()
            .find(|param| param.name.as_str() == name)
            .ok_or_else(|| format!("unknown argument {name}"))?;
        let value = lash_vm_broker::kernel::value::datum_from_tool_json(value, &param.ty).map_err(
            |error| match error {
                lash_kernel_doc::DecodeRefusal::Type { mismatch } => EntryArgumentsError::Boundary(
                    lash_kernel_check::StartBoundaryRefusal::TypeMismatch(Box::new(
                        lash_kernel_value::EntryTypeMismatch {
                            callable,
                            mismatch: mismatch.within(param.name.as_str()).within("input"),
                        },
                    )),
                ),
                other => EntryArgumentsError::Decode(other),
            },
        )?;
        supplied.insert(lash_kernel_value::names::Name::new(name), value);
    }
    if let super::InlineRequest::Linked { start } = &input.code {
        lash_kernel_check::validate_start_input(document, &start.selector, start.link, &supplied)?;
    } else {
        let decoder = lash_kernel_doc::Decoder {
            max_depth: lash_kernel_value::data_limits::KernelDataLimits::standard()
                .document_depth(),
            policy: document.numbers,
            declared: &|id| document.functions.contains_key(&id.function),
        };
        for param in &signature.params {
            let Some(value) = supplied.get(&param.name) else {
                if param.optional {
                    continue;
                }
                return Err(lash_kernel_check::StartBoundaryRefusal::MissingArgument {
                    name: param.name.clone(),
                }
                .into());
            };
            if param.optional && matches!(value, Datum::Absent) {
                continue;
            }
            decoder
                .decode(value, &param.ty)
                .map_err(|error| match error {
                    lash_kernel_doc::DecodeRefusal::Type { mismatch } => {
                        EntryArgumentsError::Boundary(
                            lash_kernel_check::StartBoundaryRefusal::TypeMismatch(Box::new(
                                lash_kernel_value::EntryTypeMismatch {
                                    callable,
                                    mismatch: mismatch.within(param.name.as_str()).within("input"),
                                },
                            )),
                        )
                    }
                    other => EntryArgumentsError::Decode(other),
                })?;
        }
    }
    Ok(signature
        .params
        .iter()
        .map(|param| supplied.get(&param.name).cloned().unwrap_or(Datum::Absent))
        .collect())
}

/// The wait a request names and its admission.
pub(super) fn admission_of(request: &Request) -> (lash_kernel_value::sites::WaitId, Admission) {
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
    let deferred = matches!(
        environment.uses.get(&request.use_),
        Some(lash_core_store::tool_surface::UseBinding::Deferred(_))
    );
    let args = crate::tool_arguments(&request.args, deferred);
    let input = args.and_then(|args| {
        crate::tool_input(
            args,
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
        )
    });
    match input {
        Ok(input) => Issued::Effect {
            wait,
            admission,
            use_: request.use_,
            origin: request.origin,
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
pub(super) fn outcome_of_end(
    end: End,
    meters: Option<lash_kernel_vm::Meters>,
) -> lash_execution_contracts::process::ProcessOutcome {
    let json = |value: &Datum| -> Option<serde_json::Value> {
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
        End::Error(RunError::Bound(exceeded)) => caused_failure(
            KernelProcessFailureCode::BoundExceeded,
            exceeded.to_string(),
            None,
            Some(lash_core::ToolFailureCause::ExecutionBound {
                fact: lash_core_execution::kernel_runner::bound_fact(&exceeded, meters),
            }),
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
    site: Option<lash_kernel_value::sites::Site>,
) -> lash_execution_contracts::process::ProcessOutcome {
    let json = |value: &Datum| -> Option<serde_json::Value> {
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
    let cause = match (error_kind.as_deref(), raw.as_ref()) {
        (Some("entry_type_mismatch" | "function_argument"), Some(raw)) => {
            serde_json::from_value(raw.clone())
                .ok()
                .map(|source| lash_core::ToolFailureCause::EntryTypeMismatch { source })
        }
        (Some("effect_result"), Some(raw)) => serde_json::from_value(raw.clone())
            .ok()
            .map(|source| lash_core::ToolFailureCause::TypeMismatch { source }),
        _ => None,
    }
    .unwrap_or(lash_core::ToolFailureCause::UncaughtError { error_kind, site });
    caused_failure(
        KernelProcessFailureCode::RuntimeError,
        message,
        raw,
        Some(cause),
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

    fn test_bounds() -> Bounds {
        Bounds {
            memory_meter: Default::default(),
            charge: Default::default(),
            charge_limit: 1_000_000,
            memory: 64 * 1024 * 1024,
            call_depth: Bounds::DEFAULT_CALL_DEPTH,
            live_tasks: 100,
            requests_per_park: 100,
            join_members: 100,
        }
    }

    /// FIG-7143: the plugin/host boundary keeps the kernel's typed mismatch.
    #[test]
    fn entry_mismatch_stays_typed_in_process_failure() {
        let source = lash_kernel_value::EntryTypeMismatch {
            callable: lash_kernel_value::CallableId {
                link: lash_kernel_value::LinkId::from_bytes([1; 32]),
                function: lash_kernel_value::FunctionId::from_bytes([2; 32]),
            },
            mismatch: lash_kernel_value::TypeMismatch::new(
                lash_kernel_value::Type::Text,
                lash_kernel_value::Type::Bool,
            )
            .within("actor")
            .within("output"),
        };
        let outcome = uncaught(
            Datum::Error(Box::new(ErrorDatum {
                kind: "entry_type_mismatch".into(),
                message: "entry_type_mismatch".into(),
                data: lash_kernel_doc::diagnostic_datum(&source).expect("diagnostic data"),
            })),
            None,
        );
        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = outcome
        else {
            panic!("expected settled failure")
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("expected failure")
        };
        assert_eq!(
            failure.cause.as_deref(),
            Some(&lash_core::ToolFailureCause::EntryTypeMismatch { source })
        );
    }

    /// FIG-6786: a worker that died running the run is a fault of the
    /// attempt, which spends one of the step's attempts, so a run that keeps
    /// killing its worker ends once they are spent. A worker lost before it
    /// computed the run stays the host's capacity verdict, which spends
    /// none.
    #[test]
    fn a_worker_the_run_crashed_spends_an_attempt_and_one_lost_before_it_ran_does_not() {
        let evidence =
            lash_protocol_types::engine_fault::SupervisorEvidence::Signalled { signal: 9 };
        let ran = worker_failure(
            KernelFailure::Worker {
                outcome: InfrastructureOutcome::RunCrashedWorker { evidence },
            },
            &test_bounds(),
            Default::default(),
        );
        assert!(matches!(ran, Err(Unsettled::Fault(_))));
        let lost = worker_failure(
            KernelFailure::Worker {
                outcome: InfrastructureOutcome::WorkerCrashed { evidence },
            },
            &test_bounds(),
            Default::default(),
        );
        assert!(matches!(
            lost,
            Err(Unsettled::Capacity(
                CapacityCause::RetryableInfrastructure,
                _
            ))
        ));
    }
    /// FIG-7557: a worker-enforced CPU stop reaches the same process bound contract.
    #[test]
    fn a_process_worker_cpu_bound_retains_the_structured_fact() {
        for observed in [None, Some(101_000_000)] {
            let (output, _) = worker_failure(
                lash_vm_broker::kernel::KernelFailure::Worker {
                    outcome: lash_vm_client::InfrastructureOutcome::WorkerLimitExceeded {
                        limit: lash_vm_client::WorkerLimit::Cpu { bound_ms: 100 },
                    },
                },
                &test_bounds(),
                lash_core_execution::kernel_runner::BoundReadings {
                    worker_cpu_nanos: observed,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|_| panic!("a worker CPU stop is terminal"));
            let KernelRunOutput::Ended { outcome, .. } = output else {
                panic!("the process ended")
            };
            let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } =
                *outcome
            else {
                panic!("the process failed")
            };
            let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
                panic!("the CPU stop failed")
            };
            assert_eq!(
                failure.cause.as_deref(),
                Some(&lash_core::ToolFailureCause::ExecutionBound {
                    fact: lash_core::ExecutionBoundFact {
                        resource: lash_core::CellResource::WorkerCpuNanos,
                        limit: 100_000_000,
                        observed,
                        consumed: observed,
                        function: None,
                        site: None
                    },
                })
            );
        }
    }
}
