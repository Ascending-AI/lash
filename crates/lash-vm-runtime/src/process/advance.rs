//! The kernel engine's `advance`: pure, synchronous and effect-free
//! (ADR 0132 §10).
//!
//! The machine never runs here. Running it is the engine step
//! `kernel_run`, which resumes the saved run, hands it the outcomes that
//! settled, runs it until no task is ready and answers the state it
//! exported with the waits it asked for. `advance` folds that answer into
//! the engine state and turns each `perform` into its own step and each
//! `sleep` into a timer. A run is a set of tasks, so it stands on a set of
//! waits: each settles on its own, in any order, and each settlement runs
//! the machine again with that outcome (`K-TASK-012`). Nothing is re-run
//! against a recorded outcome: only `kernel_run`, an effect-free stretch of
//! guest code, is ever run again.

use std::ops::ControlFlow;

use lash_core_store::checkpoint_graph::{GraphRefusal, NodeKind};
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::EngineStateFormat;
use lash_execution_contracts::process::EngineStepKind;
use lash_execution_contracts::process::ProcessInfraError;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_kernel_doc::KernelVersion;

use super::state::{
    Delivery, DeliverySource, HeldAdmission, Issued, KERNEL_RUN_STEP, KernelEngineState,
    KernelRunInput, KernelRunOutput, Park, ParkedRun, Progress, Wait, refused,
};
use super::{KernelProcessFailureCode, failure};

/// The engine's state format for a run under kernel version `kernel`: the
/// state holds the run as that version parks it.
pub(crate) fn state_format(kernel: KernelVersion) -> EngineStateFormat {
    EngineStateFormat {
        kind: lash_protocol_types::LASH_VM_ENGINE_KIND.to_owned(),
        version: kernel.number(),
    }
}

fn infra(message: impl Into<String>) -> ProcessInfraError {
    ProcessInfraError::new(
        lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt {
            record_kind: "kernel process engine state".to_owned(),
            message: message.into(),
        },
    )
}

/// `state` in the format of the kernel version its run is parked under,
/// or `written` when it has not parked yet, standing on its park's page
/// and requiring what that park recorded.
pub(crate) fn encode(
    state: &KernelEngineState,
    written: KernelVersion,
) -> Result<EngineState, ProcessInfraError> {
    let park = state.progress.park();
    let kernel = match park {
        Some(park) => KernelVersion::of(park.run.page.kernel).ok_or_else(|| {
            infra(format!(
                "the run is parked under kernel version {}, which this build does not interpret",
                park.run.page.kernel
            ))
        })?,
        None => written,
    };
    Ok(EngineState {
        staged: Vec::new(),
        format: state_format(kernel),
        bytes: serde_json::to_vec(state).map_err(|error| infra(error.to_string()))?,
        pages: park.map(|park| park.run.page.root),
        requirements: park
            .map(|park| park.requirements.clone())
            .unwrap_or_default(),
    })
}

/// The state and the kernel version its format states. An engine reads
/// the version it writes and every earlier one its build interprets.
pub(crate) fn decode(
    writes: KernelVersion,
    state: &EngineState,
) -> Result<(KernelEngineState, KernelVersion), ProcessInfraError> {
    let written = KernelVersion::of(state.format.version)
        .filter(|written| state.format == state_format(*written) && *written <= writes)
        .ok_or_else(|| {
            infra(format!(
                "state format {:?} is not one this engine reads; it writes {:?}",
                state.format,
                state_format(writes)
            ))
        })?;
    let decoded: KernelEngineState = serde_json::from_slice(&state.bytes)
        .map_err(|error| infra(retired(&state.bytes).unwrap_or_else(|| error.to_string())))?;
    if decoded.progress.park().map(|park| park.run.page.root) != state.pages {
        return Err(infra(
            "the state names another run page than the process's root holds",
        ));
    }
    Ok((decoded, written))
}

/// The refusal of a state in a shape this build retired. Asked only of a
/// state that did not decode, so a state of this build is read once; an
/// old shape is named, never read.
///
/// - The whole-state shape the persistent heap retired (FIG-6193) names
///   its run by the sealed KernelWhole graph itself, as base64 text, where
///   this build names a page.
/// - The phase shape (FIG-7652) kept its park, its requirements and its
///   phase apart, so an ended process could hold its last park; this build
///   keeps them as one `progress`.
fn retired(bytes: &[u8]) -> Option<String> {
    const KERNEL_WHOLE: NodeKind = NodeKind(16);
    let state: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if state
        .get("parked")
        .is_some_and(serde_json::Value::is_string)
    {
        return Some(
            GraphRefusal::RetiredFormat {
                node: None,
                kind: KERNEL_WHOLE.0,
                format: KERNEL_WHOLE.retired_format()?,
            }
            .to_string(),
        );
    }
    state.get("phase").map(|_| {
        "the state is in the phase shape this build retired (FIG-7652); reset the store or \
         recreate the process"
            .to_owned()
    })
}

/// The transition for `event` over `state`, of an engine that writes
/// kernel version `writes`.
pub(crate) fn advance(
    writes: KernelVersion,
    state: EngineState,
    event: EngineEvent,
) -> Result<lash_execution_contracts::process::EngineTransition, ProcessInfraError> {
    if let EngineEvent::Started { payload } = event {
        if !state.bytes.is_empty() {
            return Err(infra("a started process already has state"));
        }
        let step = run_step(0);
        let request = run_request(
            step.clone(),
            KernelRunInput {
                payload: payload.clone(),
                parked: None,
                admissions: Default::default(),
                deliver: Vec::new(),
            },
        )?;
        let state = KernelEngineState {
            payload,
            progress: Progress::Running { step, from: None },
            admissions: Default::default(),
            runs: 1,
            effects: 0,
            waits: Default::default(),
            settled: Vec::new(),
        };
        let action = EngineAction::Steps {
            steps: vec![request],
            wake: None,
        };
        return Ok(
            lash_execution_contracts::process::EngineTransition::Continue {
                state: encode(&state, writes)?,
                action,
            },
        );
    }
    let (mut decoded, written) = decode(writes, &state)?;
    match transition(&mut decoded, event)? {
        ControlFlow::Break(outcome) => {
            Ok(lash_execution_contracts::process::EngineTransition::Terminal(outcome))
        }
        ControlFlow::Continue(action) => Ok(
            lash_execution_contracts::process::EngineTransition::Continue {
                state: encode(&decoded, written)?,
                action,
            },
        ),
    }
}

fn transition(
    state: &mut KernelEngineState,
    event: EngineEvent,
) -> Result<
    ControlFlow<lash_execution_contracts::process::ProcessOutcome, EngineAction>,
    ProcessInfraError,
> {
    match event {
        EngineEvent::Started { .. } => Err(infra("a process started twice")),
        EngineEvent::Cancelled { origin, .. } => Ok(ControlFlow::Break(cancelled(origin))),
        EngineEvent::StepSettled { step, outcome, .. } => step_settled(state, &step, outcome),
        EngineEvent::Woke => Ok(ControlFlow::Continue(EngineAction::Idle)),
        // A process is awaited through the await effect, which is a step.
        EngineEvent::ProcessEnded { .. } | EngineEvent::ProcessWaitTimedOut { .. } => {
            Ok(ControlFlow::Continue(standing(state)))
        }
        EngineEvent::KeyPinned { .. }
        | EngineEvent::ExternalResolved { .. }
        | EngineEvent::ExternalTimedOut { .. } => Err(infra(
            "a kernel process pins no host key, yet one was reported",
        )),
    }
}

fn step_settled(
    state: &mut KernelEngineState,
    step: &StepName,
    outcome: SettledOutput,
) -> Result<
    ControlFlow<lash_execution_contracts::process::ProcessOutcome, EngineAction>,
    ProcessInfraError,
> {
    if matches!(&state.progress, Progress::Running { step: running, .. } if running == step) {
        return run_settled(state, outcome);
    }
    let Some(wait) = state
        .waits
        .iter()
        .find(|(_, wait)| matches!(wait, Wait::Step { step: named, .. } if named == step))
        .map(|(wait, _)| *wait)
    else {
        // A step the saved run no longer waits on: its owning task cancelled its call.
        return Ok(ControlFlow::Continue(standing(state)));
    };
    let Some(outcome) = effect_outcome(&outcome) else {
        // Not final: the step parked on its own wait and settles later.
        return Ok(ControlFlow::Continue(standing(state)));
    };
    if let Some(Wait::Step { identity, .. }) = state.waits.remove(&wait) {
        state.settled.push(Delivery {
            wait,
            identity,
            outcome: outcome.into(),
            source: DeliverySource::Step { step: step.clone() },
        });
    }
    resume(state, Vec::new()).map(ControlFlow::Continue)
}

/// The outcome a `perform` is answered with for its tool step's record;
/// `None` while the record is not final. A tool's success is its value; a
/// failure it reported is an error the guest may catch, carrying the
/// failure as its data.
pub(super) fn effect_outcome(output: &SettledOutput) -> Option<lash_kernel_vm::Outcome> {
    use lash_vm_broker::kernel::{EFFECT_RESULT, datum_from_json, outcome_of};
    let generic = outcome_of(output)?;
    let payload = match output {
        SettledOutput::Completed(material) => material.payload(),
        SettledOutput::Failed(material) => material.payload(),
        _ => return Some(generic),
    };
    let Ok(output) = serde_json::from_str::<lash_core::ToolCallOutput>(payload) else {
        return Some(generic);
    };
    let failed = |kind: &str, message: String, data| {
        lash_kernel_vm::Outcome::Failed(lash_kernel_doc::ErrorDatum::new(
            kind.to_owned(),
            message,
            data,
        ))
    };
    Some(match &output.outcome {
        lash_core::ToolCallOutcome::Success(_) => {
            match datum_from_json(&output.value_for_projection().to_string()) {
                Ok(value) => lash_kernel_vm::Outcome::Completed(value),
                Err(invalid) => failed(
                    EFFECT_RESULT,
                    invalid.to_string(),
                    lash_kernel_doc::Datum::Null,
                ),
            }
        }
        lash_core::ToolCallOutcome::Failure(failure) => failed(
            super::TOOL_FAILED,
            failure.message.clone(),
            datum_from_json(&failure.to_json_value().to_string())
                .unwrap_or(lash_kernel_doc::Datum::Null),
        ),
        lash_core::ToolCallOutcome::Cancelled(cancellation) => {
            return lash_vm_broker::kernel::store::outcome_of(&SettledOutput::Cancelled {
                cancellation: cancellation.clone(),
                evidence: lash_core_store::tool_run::AvailableEvidence::default(),
            });
        }
    })
}

/// Runs the machine with what has settled, unless a run is in flight: that
/// run's own settlement picks the outcomes up.
fn resume(
    state: &mut KernelEngineState,
    steps: Vec<StepRequest>,
) -> Result<EngineAction, ProcessInfraError> {
    let Progress::Parked(park) = &state.progress else {
        return Ok(if steps.is_empty() {
            standing(state)
        } else {
            EngineAction::Steps { steps, wake: None }
        });
    };
    let park = park.clone();
    run(state, Some(park), steps)
}

fn run_settled(
    state: &mut KernelEngineState,
    settled: SettledOutput,
) -> Result<
    ControlFlow<lash_execution_contracts::process::ProcessOutcome, EngineAction>,
    ProcessInfraError,
> {
    let output = match settled {
        SettledOutput::Completed(output) => {
            serde_json::from_str::<KernelRunOutput>(output.payload())
                .map_err(|error| infra(format!("kernel_run answered {error}")))?
        }
        SettledOutput::Failed(material) => {
            let output = serde_json::from_str::<lash_core::ToolCallOutput>(material.payload())
                .map_err(|error| infra(format!("kernel_run failure answered {error}")))?;
            return Ok(ControlFlow::Break(
                lash_execution_contracts::process::ProcessOutcome::from_tool_output(output),
            ));
        }
        SettledOutput::TimedOut { cause, .. } => {
            return Ok(ControlFlow::Break(failure(
                KernelProcessFailureCode::BoundExceeded,
                format!("the run passed its limit ({cause:?}) before it parked"),
                None,
            )));
        }
        // An interrupted or stopped run reached no park, after every retry
        // its step kind's policy allowed: the process ends.
        unsettled => {
            return Ok(ControlFlow::Break(failure(
                KernelProcessFailureCode::RunUnsettled,
                format!(
                    "the run reached no park: its step settled {:?}",
                    unsettled.outcome()
                ),
                None,
            )));
        }
    };
    match output {
        KernelRunOutput::Ended { outcome, .. } => Ok(ControlFlow::Break(*outcome)),
        KernelRunOutput::Parked {
            read_count,
            state: parked,
            issued,
            cancel_requests,
            requirements,
            ..
        } => {
            // Runs are sequential: the one settling is the last asked for.
            let settling = state.runs.saturating_sub(1);
            let park = Park {
                run: ParkedRun {
                    read_count,
                    page: parked,
                    by: settling,
                },
                requirements: *requirements,
            };
            let explicit_cancel = !cancel_requests.is_empty();
            let mut cancelled = Vec::new();
            for request in cancel_requests {
                let wait = if request.retains_wait() {
                    state.waits.get(&request.wait()).cloned()
                } else {
                    state.waits.remove(&request.wait())
                };
                if let Some(Wait::Step { step, .. }) = wait {
                    cancelled.push(step);
                }
            }
            let mut steps = Vec::new();
            for issued in issued {
                state.admissions.insert(
                    issued.wait(),
                    HeldAdmission {
                        admission: issued.admission().clone(),
                        by: settling,
                    },
                );
                match issued {
                    Issued::Effect {
                        wait,
                        admission,
                        use_,
                        origin,
                        input,
                    } => {
                        let step = StepName(format!("effect.{}", state.effects));
                        state.effects += 1;
                        let identity = admission.identity().clone();
                        state.waits.insert(
                            wait,
                            Wait::Step {
                                step: step.clone(),
                                identity: identity.clone(),
                            },
                        );
                        steps.push(StepRequest::Tool {
                            step,
                            use_,
                            input,
                            site: Some(Box::new(lash_kernel_value::sites::EffectLocation {
                                at: identity,
                                origin,
                            })),
                        });
                    }
                    Issued::Refused {
                        wait,
                        admission,
                        kind,
                        message,
                    } => state.settled.push(Delivery {
                        wait,
                        identity: admission.identity().clone(),
                        outcome: refused(&kind, message),
                        source: DeliverySource::Refused { run: settling },
                    }),
                }
            }
            // The run consumed every outcome it was handed: a wait the
            // parked run still holds is open, or settled since.
            let held: std::collections::BTreeSet<lash_kernel_value::sites::WaitId> = state
                .waits
                .keys()
                .copied()
                .chain(state.settled.iter().map(|delivery| delivery.wait))
                .collect();
            state.admissions.retain(|wait, _| held.contains(wait));
            if explicit_cancel {
                let next = run(state, Some(park), steps)?;
                return Ok(ControlFlow::Continue(EngineAction::CancelCalls {
                    steps: cancelled,
                    cancellation: lash_protocol_types::ToolCancellation::runtime(
                        "the guest task cancelled its call",
                    )
                    .with_origin(lash_protocol_types::CancelOrigin::ModelRequested),
                    next: Box::new(next),
                }));
            }
            if !state.settled.is_empty() {
                return run(state, Some(park), steps).map(ControlFlow::Continue);
            }
            if state.waits.is_empty() {
                // The machine parks only on a wait it asked for.
                return Ok(ControlFlow::Break(failure(
                    KernelProcessFailureCode::RunUnsettled,
                    "the run parked on no wait",
                    None,
                )));
            }
            state.progress = Progress::Parked(park);
            Ok(ControlFlow::Continue(if steps.is_empty() {
                standing(state)
            } else {
                EngineAction::Steps { steps, wake: None }
            }))
        }
    }
}

fn standing(_state: &KernelEngineState) -> EngineAction {
    EngineAction::Idle
}

/// Asks for the next `kernel_run`, resuming `from` and handing it every
/// settled outcome, and for `steps` beside it.
fn run(
    state: &mut KernelEngineState,
    from: Option<Park>,
    mut steps: Vec<StepRequest>,
) -> Result<EngineAction, ProcessInfraError> {
    let step = run_step(state.runs);
    state.runs += 1;
    steps.push(run_request(
        step.clone(),
        KernelRunInput {
            payload: state.payload.clone(),
            parked: from.as_ref().map(|park| park.run),
            admissions: state.admissions.clone(),
            deliver: std::mem::take(&mut state.settled),
        },
    )?);
    state.progress = Progress::Running { step, from };
    Ok(EngineAction::Steps { steps, wake: None })
}

/// The name of the process's `run`th `kernel_run`.
fn run_step(run: u64) -> StepName {
    StepName(format!("{KERNEL_RUN_STEP}.{run}"))
}

fn run_request(step: StepName, input: KernelRunInput) -> Result<StepRequest, ProcessInfraError> {
    Ok(StepRequest::Engine {
        step,
        kind: EngineStepKind::new(KERNEL_RUN_STEP),
        input: serde_json::to_value(input).map_err(|error| infra(error.to_string()))?,
    })
}

fn cancelled(
    origin: lash_protocol_types::CancelOrigin,
) -> lash_execution_contracts::process::ProcessOutcome {
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime(
            "the process was cancelled",
        )),
    )
    .with_cancel_origin(Some(origin))
}
