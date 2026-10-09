//! The `kernel_run` engine step: resume the saved run, hand it what
//! settled, and run it until no task is ready.
//!
//! The step performs nothing. It answers the state the machine exported at
//! its park with the waits it asked for there; the process activation
//! commits both, with the admission of each wait's step, in one
//! `process.advance` transaction. The step is `Repeatable`: a run that
//! never committed is a recomputation of an effect-free stretch of guest
//! code from the same saved state and the same outcomes.

use std::sync::Arc;

use lash_core::tool_run::{KnownFailureReason, MaterialOwner, MaterialRole};
use lash_core::{EngineStepRun, Material, ProcessId, SettledOutput};
use lash_kernel_doc::{Datum, Document, ErrorDatum, Handle, Integer, Name, Timestamp};
use lash_kernel_vm::{Bounds, End, Request, RunError, Start, Step, Target};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{
    DrivenMachine, KernelFailure, Machines, datum_from_json, datum_to_json,
};
use lash_vm_client::{FrameEpoch, InfrastructureOutcome, OwnerEpoch, RunHost, VmOwner};
use tokio_util::sync::CancellationToken;

use super::state::{Issued, KernelProcessInput, KernelRunInput, KernelRunOutput};
use super::{
    EFFECT_ARGUMENTS, EFFECT_UNKNOWN, KernelProcessEngine, KernelProcessFailureCode,
    KernelRecordedSettings, definition_of_entry, failure,
};
use crate::{HostBoundary, ProjectionCatalog};

/// Why a `kernel_run` reached neither a park nor an end: the run left
/// nothing, and the same run is asked again.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct RunFault(pub(crate) String);

/// Runs the `kernel_run` step `run` to its outcome.
pub(crate) async fn run_kernel_step(
    engine: &KernelProcessEngine,
    run: EngineStepRun,
    stop: CancellationToken,
) -> SettledOutput {
    let process = run.process.clone();
    match kernel_run(engine, run, stop).await {
        Ok(output) => match serde_json::to_string(&output) {
            Ok(text) => SettledOutput::Completed(material(&process, text)),
            Err(error) => failed(&process, &RunFault(error.to_string())),
        },
        Err(fault) => failed(&process, &fault),
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
    let text = serde_json::to_string(&output).unwrap_or_default();
    SettledOutput::Failed(material(process, text).failure(KnownFailureReason::Reported, None))
}

/// The process's host: the activation's clock reading, the engine's random
/// source and the backend's projection providers. What the body prints is
/// not kept.
struct StepHost {
    now_ms: i64,
    random: Arc<dyn Fn() -> u64 + Send + Sync>,
    providers: ProjectionCatalog,
}

#[async_trait::async_trait]
impl RunHost for StepHost {
    async fn clock(&self) -> Result<Timestamp, ParentFault> {
        Ok(Timestamp {
            nanoseconds: Integer::from(self.now_ms.saturating_mul(1_000_000)),
        })
    }

    async fn random(&self) -> Result<u64, ParentFault> {
        Ok((self.random)())
    }

    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Ok(self.providers.answer(handle, request).await)
    }

    fn print(&self, _value: Datum) -> Result<(), ParentFault> {
        Ok(())
    }
}

fn ended(outcome: lash_core::ProcessOutcome) -> Result<KernelRunOutput, RunFault> {
    Ok(KernelRunOutput::Ended {
        outcome: Box::new(outcome),
    })
}

async fn kernel_run(
    engine: &KernelProcessEngine,
    run: EngineStepRun,
    stop: CancellationToken,
) -> Result<KernelRunOutput, RunFault> {
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
    let document = match engine.documents.get(&process_input.document).await {
        Ok(Some(document)) => document,
        Ok(None) => {
            return ended(failure(
                KernelProcessFailureCode::DocumentMissing,
                format!("no document `{}` is retained", process_input.document),
                None,
            ));
        }
        Err(error) => return Err(RunFault(error.to_string())),
    };
    let settings: KernelRecordedSettings = match run
        .engine_config
        .as_ref()
        .map(|config| serde_json::from_value(config.clone()))
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
    let boundary = match HostBoundary::of_catalog(&run.tool_catalog) {
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
    );
    if let Some(trace) = &trace {
        if input.parked.is_none() {
            trace.started();
        }
        for delivery in &input.deliver {
            trace.delivered(delivery);
        }
    }
    let machines = lash_vm_client::RemoteMachines::new(
        engine.workers.clone(),
        VmOwner::new(format!("process:{}", run.process)),
        OwnerEpoch(0),
        FrameEpoch(0),
        Arc::new(StepHost {
            now_ms: run.now.0,
            random: Arc::clone(&engine.random),
            providers: ProjectionCatalog::of_backend(run.projection_providers.as_deref()),
        }),
        &document,
        Start {
            target: Target::Entry(process_input.entry.clone()),
            args,
            bindings: Default::default(),
        },
        settings.bounds(),
    )
    .map_err(|error| RunFault(error.to_string()))?;
    let driven = drive(
        &machines,
        input,
        engine.workers.config().tuning.slice,
        &stop,
    )
    .await;
    let (step, state) = match driven {
        Ok(driven) => driven,
        Err(failure) => return worker_failure(failure),
    };
    match step {
        Step::Ended(end) => {
            let outcome = outcome_of_end(end);
            if let Some(trace) = &trace {
                trace.finished(&outcome);
            }
            ended(outcome)
        }
        Step::Slice => Err(RunFault("the run was stopped mid-slice".to_owned())),
        Step::Parked(park) => {
            let Some(state) = state else {
                return Err(RunFault("the run parked without a state".to_owned()));
            };
            let withdrawn: Vec<u64> = park.withdrawn.iter().map(|wait| wait.0).collect();
            let issued = park
                .requests
                .into_iter()
                .filter(|request| {
                    !withdrawn.contains(&match request {
                        Request::Effect(effect) => effect.wait.0,
                        Request::Sleep(sleep) => sleep.wait.0,
                    })
                })
                .map(|request| issue(&boundary, &document, &process_input, run.now.0, request))
                .collect::<Vec<_>>();
            if let Some(trace) = &trace {
                for issued in &issued {
                    trace.issued(issued);
                }
            }
            Ok(KernelRunOutput::Parked {
                state,
                issued,
                withdrawn,
            })
        }
    }
}

/// Starts or resumes the machine, hands it the settled outcomes and runs
/// it until it parks or ends. A parked machine answers its exported state.
async fn drive(
    machines: &lash_vm_client::RemoteMachines,
    input: KernelRunInput,
    slice: u64,
    stop: &CancellationToken,
) -> Result<(Step, Option<lash_vm_client::OpaqueVmState>), KernelFailure> {
    let mut machine = match input.parked {
        Some(parked) => machines.resume(parked).await?,
        None => machines.start().await?,
    };
    for delivery in input.deliver {
        machine
            .deliver(
                lash_kernel_vm::WaitId(delivery.wait),
                delivery.outcome.into(),
            )
            .await?;
    }
    loop {
        match machine.run(slice, false).await? {
            // A stopped step is asked again from the same input.
            Step::Slice if stop.is_cancelled() => return Ok((Step::Slice, None)),
            Step::Slice => {}
            Step::Parked(park) => {
                let state = machine.export().await?;
                machine.release().await?;
                return Ok((Step::Parked(park), Some(state)));
            }
            Step::Ended(end) => {
                machine.release().await?;
                return Ok((Step::Ended(end), None));
            }
        }
    }
}

/// What a failure to drive the machine is to the process: its terminal
/// when every attempt would meet it the same way, or a fault of this
/// attempt, which is asked again.
fn worker_failure(failure_: KernelFailure) -> Result<KernelRunOutput, RunFault> {
    match &failure_ {
        KernelFailure::Start(_) | KernelFailure::Import(_) | KernelFailure::Document(_) => {
            ended(failure(
                KernelProcessFailureCode::RunRefused,
                failure_.to_string(),
                None,
            ))
        }
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
        _ => Err(RunFault(failure_.to_string())),
    }
}

/// The entry's arguments in parameter order, from the start's arguments by
/// name. An omitted optional parameter is absent (`K-FN-004`).
fn entry_args(document: &Document, input: &KernelProcessInput) -> Result<Vec<Datum>, String> {
    let signature = document.entries.get(&input.entry).ok_or_else(|| {
        format!(
            "document `{}` has no entry `{}`",
            input.document, input.entry
        )
    })?;
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

/// Resolves one wait the machine asked for against the host boundary.
fn issue(
    boundary: &HostBoundary,
    document: &Document,
    process: &KernelProcessInput,
    now_ms: i64,
    request: Request,
) -> Issued {
    let request = match request {
        Request::Sleep(sleep) => {
            let millis = i64::try_from(sleep.duration.as_millis()).unwrap_or(i64::MAX);
            return Issued::Sleep {
                wait: sleep.wait.0,
                identity: sleep.identity,
                until_ms: now_ms.saturating_add(millis),
            };
        }
        Request::Effect(request) => request,
    };
    let wait = request.wait.0;
    let Some(effect) = boundary.effect(&request.effect) else {
        return Issued::Refused {
            wait,
            identity: request.identity,
            kind: EFFECT_UNKNOWN.to_owned(),
            message: format!("the host offers no effect `{}`", request.effect),
        };
    };
    // A tool is one effect that takes its input as one record.
    let input = match request.args.as_slice() {
        [input] => effect_input(document, process, input),
        args => Err(format!(
            "the effect `{}` takes one input record, and {} arguments were passed",
            request.effect,
            args.len()
        )),
    };
    match input {
        Ok(input) => Issued::Effect {
            wait,
            identity: request.identity,
            tool: effect.tool.clone(),
            input,
        },
        Err(message) => Issued::Refused {
            wait,
            identity: request.identity,
            kind: EFFECT_ARGUMENTS.to_owned(),
            message,
        },
    }
}

/// An effect's input as the tool reads it. A function reference in it
/// names an entry of the process's own document and crosses the boundary
/// as that entry's definition, which is what the start effect starts.
fn effect_input(
    document: &Document,
    process: &KernelProcessInput,
    input: &Datum,
) -> Result<serde_json::Value, String> {
    let input = with_definitions(input, &|function| {
        let definition = definition_of_entry(document, process.document, function)?;
        datum_from_json(&definition.to_string()).map_err(|error| error.to_string())
    })?;
    let text = datum_to_json(&input).map_err(|error| error.to_string())?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

/// `value` with every function reference replaced by what `definition`
/// answers for it.
pub fn with_definitions(
    value: &Datum,
    definition: &dyn Fn(&Name) -> Result<Datum, String>,
) -> Result<Datum, String> {
    let each = |items: &[Datum]| {
        items
            .iter()
            .map(|item| with_definitions(item, definition))
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(match value {
        Datum::Function(name) => definition(name)?,
        Datum::Tuple(items) => Datum::Tuple(each(items)?),
        Datum::List(items) => Datum::List(each(items)?),
        Datum::Set(items) => Datum::Set(each(items)?),
        Datum::Map(entries) => Datum::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    Ok((
                        with_definitions(key, definition)?,
                        with_definitions(value, definition)?,
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?,
        ),
        Datum::Record(fields) => Datum::Record(
            fields
                .iter()
                .map(|(name, value)| Ok((name.clone(), with_definitions(value, definition)?)))
                .collect::<Result<Vec<_>, String>>()?,
        ),
        other => other.clone(),
    })
}

/// How a run's end becomes the output its awaiter reads.
fn outcome_of_end(end: End) -> lash_core::ProcessOutcome {
    let json = |value: &Datum| {
        datum_to_json(value)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
    };
    match end {
        End::Finished(finished) => match json(&finished.result) {
            Some(value) => lash_core::ProcessAwaitOutput::from_tool_output(
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
        }) => failure(
            KernelProcessFailureCode::TasksOutstanding,
            format!(
                "the process ended with {} unfinished and {} unobserved failed tasks",
                unfinished.len(),
                unobserved.len()
            ),
            Some(serde_json::json!({ "unfinished": unfinished, "unobserved": unobserved })),
        ),
        End::Error(error) => failure(
            KernelProcessFailureCode::RuntimeError,
            error.to_string(),
            None,
        ),
        End::Cancelled => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
                lash_core::ToolCancellation::runtime("the process was cancelled"),
            ))
        }
    }
}

impl KernelRecordedSettings {
    pub(crate) fn bounds(&self) -> Bounds {
        Bounds {
            charge: self.charge,
            memory: self.memory,
            call_depth: self.call_depth,
            live_tasks: self.live_tasks,
            requests_per_park: self.requests_per_park,
            join_members: self.join_members,
        }
    }
}
