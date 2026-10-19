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
use lash_vm_client::wire::OutcomeWire;

use super::state::{
    Delivery, DeliverySource, HeldAdmission, Issued, KERNEL_RUN_STEP, KernelEngineState,
    KernelRunInput, KernelRunOutput, ParkedRun, Phase, Wait, refused,
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
/// or `written` when it holds no run yet, standing on its run's page.
pub(crate) fn encode(
    state: &KernelEngineState,
    written: KernelVersion,
) -> Result<EngineState, ProcessInfraError> {
    let kernel = match &state.parked {
        Some(parked) => KernelVersion::of(parked.page.kernel).ok_or_else(|| {
            infra(format!(
                "the run is parked under kernel version {}, which this build does not interpret",
                parked.page.kernel
            ))
        })?,
        None => written,
    };
    Ok(EngineState {
        format: state_format(kernel),
        bytes: serde_json::to_vec(state).map_err(|error| infra(error.to_string()))?,
        pages: state.parked.map(|parked| parked.page.root),
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
    let decoded: KernelEngineState = serde_json::from_slice(&state.bytes).map_err(|error| {
        infra(
            pre_heap(&state.bytes).map_or_else(|| error.to_string(), |refusal| refusal.to_string()),
        )
    })?;
    if decoded.parked.map(|parked| parked.page.root) != state.pages {
        return Err(infra(
            "the state names another run page than the process's root holds",
        ));
    }
    Ok((decoded, written))
}

/// The refusal of a state that parks its run in the whole-state shape the
/// persistent heap retired (FIG-6193): such a state names its run by the
/// sealed KernelWhole graph itself, as base64 text, where this build names
/// a page. Asked only of a state that did not decode, so a state of this
/// build is read once; the old shape is named, never read.
fn pre_heap(bytes: &[u8]) -> Option<GraphRefusal> {
    const KERNEL_WHOLE: NodeKind = NodeKind(16);
    let state: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    state.get("parked")?.is_string().then_some(())?;
    Some(GraphRefusal::RetiredFormat {
        node: None,
        kind: KERNEL_WHOLE.0,
        format: KERNEL_WHOLE.retired_format()?,
    })
}

/// The transition for `event` over `state`, of an engine that writes
/// kernel version `writes`.
pub(crate) fn advance(
    writes: KernelVersion,
    state: EngineState,
    event: EngineEvent,
) -> Result<(EngineState, EngineAction), ProcessInfraError> {
    if let EngineEvent::Started { payload } = event {
        if !state.bytes.is_empty() {
            return Err(infra("a started process already has state"));
        }
        let mut state = KernelEngineState {
            payload,
            parked: None,
            admissions: Default::default(),
            runs: 0,
            effects: 0,
            waits: Default::default(),
            settled: Vec::new(),
            phase: Phase::Ended,
        };
        let action = run(&mut state, Vec::new())?;
        return Ok((encode(&state, writes)?, action));
    }
    let (mut state, written) = decode(writes, &state)?;
    let action = transition(&mut state, event)?;
    Ok((encode(&state, written)?, action))
}

fn transition(
    state: &mut KernelEngineState,
    event: EngineEvent,
) -> Result<EngineAction, ProcessInfraError> {
    if matches!(state.phase, Phase::Ended) {
        return Err(infra("an ended process received an event"));
    }
    match event {
        EngineEvent::Started { .. } => Err(infra("a process started twice")),
        EngineEvent::Cancelled { origin, .. } => {
            state.phase = Phase::Ended;
            Ok(EngineAction::Terminal(cancelled(origin)))
        }
        EngineEvent::StepSettled { step, outcome, .. } => step_settled(state, &step, outcome),
        EngineEvent::Woke => {
            // The earliest timers fire together, in wait order.
            let Some(due) = next_timer(state) else {
                return Ok(standing(state));
            };
            let fired: Vec<lash_protocol_types::WaitId> = state
                .waits
                .iter()
                .filter(
                    |(_, wait)| matches!(wait, Wait::Timer { until_ms, .. } if *until_ms <= due.until.0),
                )
                .map(|(wait, _)| *wait)
                .collect();
            for wait in fired {
                if let Some(Wait::Timer { identity, .. }) = state.waits.remove(&wait) {
                    state.settled.push(Delivery {
                        wait,
                        identity,
                        outcome: OutcomeWire::Elapsed,
                        source: DeliverySource::Elapsed,
                    });
                }
            }
            resume(state, Vec::new())
        }
        // A process is awaited through the await effect, which is a step.
        EngineEvent::ProcessEnded { .. } | EngineEvent::ProcessWaitTimedOut { .. } => {
            Ok(standing(state))
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
) -> Result<EngineAction, ProcessInfraError> {
    if matches!(&state.phase, Phase::Running { step: running } if running == step) {
        return run_settled(state, outcome);
    }
    let Some(wait) = state
        .waits
        .iter()
        .find(|(_, wait)| matches!(wait, Wait::Step { step: named, .. } if named == step))
        .map(|(wait, _)| *wait)
    else {
        // A step the saved run no longer waits on: its wait was withdrawn.
        return Ok(standing(state));
    };
    let Some(outcome) = effect_outcome(&outcome) else {
        // Not final: the step parked on its own wait and settles later.
        return Ok(standing(state));
    };
    if let Some(Wait::Step { identity, .. }) = state.waits.remove(&wait) {
        state.settled.push(Delivery {
            wait,
            identity,
            outcome: outcome.into(),
            source: DeliverySource::Step { step: step.clone() },
        });
    }
    resume(state, Vec::new())
}

/// The outcome a `perform` is answered with for its tool step's record;
/// `None` while the record is not final. A tool's success is its value; a
/// failure it reported is an error the guest may catch, carrying the
/// failure as its data.
pub(super) fn effect_outcome(output: &SettledOutput) -> Option<lash_kernel_vm::Outcome> {
    use lash_vm_broker::kernel::{EFFECT_CANCELLED, EFFECT_RESULT, datum_from_json, outcome_of};
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
        lash_kernel_vm::Outcome::Failed(lash_kernel_doc::ErrorDatum {
            kind: kind.to_owned(),
            message,
            data,
        })
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
        lash_core::ToolCallOutcome::Cancelled(cancelled) => failed(
            EFFECT_CANCELLED,
            cancelled.message.clone(),
            lash_kernel_doc::Datum::Null,
        ),
    })
}

/// Runs the machine with what has settled, unless a run is in flight: that
/// run's own settlement picks the outcomes up.
fn resume(
    state: &mut KernelEngineState,
    steps: Vec<StepRequest>,
) -> Result<EngineAction, ProcessInfraError> {
    if matches!(state.phase, Phase::Running { .. }) {
        return Ok(if steps.is_empty() {
            standing(state)
        } else {
            EngineAction::Steps {
                steps,
                wake: next_timer(state),
            }
        });
    }
    run(state, steps)
}

fn run_settled(
    state: &mut KernelEngineState,
    settled: SettledOutput,
) -> Result<EngineAction, ProcessInfraError> {
    let output = match settled {
        SettledOutput::Completed(output) => {
            serde_json::from_str::<KernelRunOutput>(output.payload())
                .map_err(|error| infra(format!("kernel_run answered {error}")))?
        }
        SettledOutput::Failed(material) => {
            let output = serde_json::from_str::<lash_core::ToolCallOutput>(material.payload())
                .map_err(|error| infra(format!("kernel_run failure answered {error}")))?;
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(
                lash_execution_contracts::process::ProcessOutcome::from_tool_output(output),
            ));
        }
        SettledOutput::TimedOut { cause, .. } => {
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(failure(
                KernelProcessFailureCode::BoundExceeded,
                format!("the run passed its limit ({cause:?}) before it parked"),
                None,
            )));
        }
        // An interrupted or stopped run reached no park, after every retry
        // its step kind's policy allowed: the process ends.
        unsettled => {
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(failure(
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
        KernelRunOutput::Ended { outcome, .. } => {
            // An ended run stands on no page.
            state.parked = None;
            state.phase = Phase::Ended;
            Ok(EngineAction::Terminal(*outcome))
        }
        KernelRunOutput::Parked {
            state: parked,
            issued,
            withdrawn,
            ..
        } => {
            // Runs are sequential: the one settling is the last asked for.
            let settling = state.runs.saturating_sub(1);
            state.parked = Some(ParkedRun {
                page: parked,
                by: settling,
            });
            state.phase = Phase::Parked;
            for wait in withdrawn {
                state.waits.remove(&wait);
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
                        effect,
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
                            effect,
                            input,
                            site: Some(identity),
                            matched: None,
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
                    Issued::Sleep {
                        wait,
                        admission,
                        until_ms,
                    } => {
                        let identity = admission.identity().clone();
                        state.waits.insert(wait, Wait::Timer { until_ms, identity });
                    }
                }
            }
            // The run consumed every outcome it was handed: a wait the
            // parked run still holds is open, or settled since.
            let held: std::collections::BTreeSet<lash_protocol_types::WaitId> = state
                .waits
                .keys()
                .copied()
                .chain(state.settled.iter().map(|delivery| delivery.wait))
                .collect();
            state.admissions.retain(|wait, _| held.contains(wait));
            if !state.settled.is_empty() {
                return run(state, steps);
            }
            if state.waits.is_empty() {
                // The machine parks only on a wait it asked for.
                state.phase = Phase::Ended;
                return Ok(EngineAction::Terminal(failure(
                    KernelProcessFailureCode::RunUnsettled,
                    "the run parked on no wait",
                    None,
                )));
            }
            Ok(if steps.is_empty() {
                standing(state)
            } else {
                EngineAction::Steps {
                    steps,
                    wake: next_timer(state),
                }
            })
        }
    }
}

/// The earliest task timer, retaining the same task's execution identity.
fn next_timer(state: &KernelEngineState) -> Option<lash_execution_contracts::process::EngineWake> {
    state
        .waits
        .values()
        .filter_map(|wait| match wait {
            Wait::Timer { until_ms, identity } => {
                Some(lash_execution_contracts::process::EngineWake {
                    until: lash_core::durable_port::DurableInstant(*until_ms),
                    site: Some(identity.clone()),
                })
            }
            Wait::Step { .. } => None,
        })
        .min_by_key(|wake| wake.until)
}

/// The action that stands while the process waits on what it already
/// asked for: an event it does not wait on changes nothing.
fn standing(state: &KernelEngineState) -> EngineAction {
    match next_timer(state) {
        Some(wake) => EngineAction::Sleep {
            until: wake.until,
            site: wake.site,
        },
        None => EngineAction::Idle,
    }
}

/// Asks for the next `kernel_run`, handing it every settled outcome, and
/// for `steps` beside it.
fn run(
    state: &mut KernelEngineState,
    mut steps: Vec<StepRequest>,
) -> Result<EngineAction, ProcessInfraError> {
    let step = StepName(format!("{KERNEL_RUN_STEP}.{}", state.runs));
    state.runs += 1;
    let input = KernelRunInput {
        payload: state.payload.clone(),
        parked: state.parked,
        admissions: state.admissions.clone(),
        deliver: std::mem::take(&mut state.settled),
    };
    state.phase = Phase::Running { step: step.clone() };
    steps.push(StepRequest::Engine {
        step,
        kind: EngineStepKind::new(KERNEL_RUN_STEP),
        input: serde_json::to_value(input).map_err(|error| infra(error.to_string()))?,
    });
    Ok(EngineAction::Steps {
        steps,
        wake: next_timer(state),
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
