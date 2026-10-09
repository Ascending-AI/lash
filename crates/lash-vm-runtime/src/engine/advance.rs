//! The lash_vm engine's `advance`: pure, synchronous and effect-free
//! (ADR 0132 §10; D-L6a, FIG-5198).
//!
//! The VM never runs here. Running it is the engine step `vm_run`, which
//! resumes the committed snapshot to its next quiet point and answers the
//! new snapshot with the one operation the VM issued there. `advance` folds
//! that answer into the state and turns the operation into an action; the
//! operation's outcome comes back as an event and feeds the next `vm_run` by
//! the operation's number. Nothing is re-run against a recorded outcome: a
//! settled operation is answered from the state, and only `vm_run`, an
//! effect-free stretch of VM, is ever run again.

use lash_core::{
    EngineAction, EngineEvent, EngineState, EngineStepKind, ProcessInfraError, SettledOutput,
    StepName, StepRequest,
};

use super::state::{
    BatchShape, Decision, Injection, IssuedLeaf, IssuedOperation, LASH_VM_SEGMENT_STATE_VERSION,
    LashVmEngineState, Leaf, Phase, VM_RUN_STEP, VmRunInput, VmRunOutput, Wait,
};
use crate::{LASH_VM_ENGINE_KIND, LashVmProcessFailureCode};

/// The engine's state format.
pub(crate) fn state_format() -> lash_core::EngineStateFormat {
    lash_core::EngineStateFormat {
        kind: LASH_VM_ENGINE_KIND.to_owned(),
        version: LASH_VM_SEGMENT_STATE_VERSION,
    }
}

fn infra(message: impl Into<String>) -> ProcessInfraError {
    ProcessInfraError::new(lash_core::PluginError::StoredDataCorrupt {
        record_kind: "lash_vm engine state".to_owned(),
        message: message.into(),
    })
}

fn encode(state: &LashVmEngineState) -> Result<EngineState, ProcessInfraError> {
    Ok(EngineState {
        format: state_format(),
        bytes: serde_json::to_vec(state).map_err(|error| infra(error.to_string()))?,
    })
}

fn decode(state: &EngineState) -> Result<LashVmEngineState, ProcessInfraError> {
    if state.format != state_format() {
        return Err(infra(format!(
            "state format {:?} is not this engine's {:?}",
            state.format,
            state_format()
        )));
    }
    serde_json::from_slice(&state.bytes).map_err(|error| infra(error.to_string()))
}

/// The engine's transition for `event` over `state`.
pub(crate) fn advance(
    state: EngineState,
    event: EngineEvent,
) -> Result<(EngineState, EngineAction), ProcessInfraError> {
    if let EngineEvent::Started { payload } = event {
        if !state.bytes.is_empty() {
            return Err(infra("a started process already has state"));
        }
        let mut state = LashVmEngineState {
            payload,
            program_hash: None,
            vm: None,
            runs: 0,
            operations: 0,
            phase: Phase::Ended,
        };
        let action = run_vm(&mut state, None)?;
        return Ok((encode(&state)?, action));
    }
    let mut state = decode(&state)?;
    let action = transition(&mut state, event)?;
    Ok((encode(&state)?, action))
}

fn transition(
    state: &mut LashVmEngineState,
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
        EngineEvent::StepSettled {
            step,
            call_id,
            outcome,
        } => step_settled(state, &step, call_id, outcome),
        EngineEvent::Woke => match &state.phase {
            Phase::Parked {
                operation,
                wait: Wait::Sleep { .. },
            } => {
                let operation = *operation;
                run_vm(state, Some(Injection::Woke { operation }))
            }
            Phase::Parked {
                wait: Wait::Leaves { .. },
                ..
            } => timers_woke(state),
            _ => standing(state),
        },
        EngineEvent::ProcessEnded { process, outcome } => match &state.phase {
            Phase::Parked {
                operation,
                wait: Wait::Process {
                    process: awaited, ..
                },
            } if *awaited == process => {
                let operation = *operation;
                run_vm(
                    state,
                    Some(Injection::ProcessEnded {
                        operation,
                        outcome: Box::new(outcome),
                    }),
                )
            }
            _ => standing(state),
        },
        EngineEvent::ProcessWaitTimedOut { .. } => standing(state),
        EngineEvent::KeyPinned { .. }
        | EngineEvent::ExternalResolved { .. }
        | EngineEvent::ExternalTimedOut { .. } => Err(infra(
            "a lash_vm process pins no host key, yet one was reported",
        )),
    }
}

/// The action that stands while the process waits on what it already
/// asked for: an event it does not wait on changes nothing.
fn standing(state: &LashVmEngineState) -> Result<EngineAction, ProcessInfraError> {
    Ok(match &state.phase {
        Phase::Parked {
            wait: Wait::Sleep { until_ms, site },
            ..
        } => EngineAction::Sleep {
            until: lash_core::durable_port::DurableInstant(*until_ms),
            site: site.clone(),
        },
        Phase::Parked {
            wait: Wait::Process { process, site },
            ..
        } => EngineAction::AwaitProcess {
            process: process.clone(),
            // A program's `await` has no deadline of its own: the wait
            // lasts until the awaited process ends or this one's scope does.
            bound: lash_core::ParkBound::UntilScopeEnd,
            site: site.clone(),
        },
        Phase::Parked {
            wait: Wait::Leaves { leaves, .. },
            ..
        } => match next_timer(leaves) {
            // An aggregate's timers are leaves of one node, not a sleep at it.
            Some(until) => EngineAction::Sleep { until, site: None },
            None => EngineAction::Idle,
        },
        Phase::Running { .. } => EngineAction::Idle,
        Phase::Ended => return Err(infra("an ended process has no standing action")),
    })
}

fn step_settled(
    state: &mut LashVmEngineState,
    step: &StepName,
    call_id: Option<lash_core::ToolCallId>,
    outcome: SettledOutput,
) -> Result<EngineAction, ProcessInfraError> {
    match &mut state.phase {
        Phase::Running { step: running, .. } if running == step => vm_run_settled(state, outcome),
        Phase::Parked {
            operation,
            wait:
                Wait::Leaves {
                    batch,
                    leaves,
                    settled,
                },
        } => {
            let Some(index) = leaves.iter().position(|leaf| {
                matches!(leaf, Leaf::Step { step: named, outcome: None, .. } if named == step)
            }) else {
                return standing(state);
            };
            if let Leaf::Step {
                outcome: slot,
                call_id: identity,
                ..
            } = &mut leaves[index]
            {
                *identity = call_id;
                *slot = Some(Box::new(outcome));
            }
            settled.push(index);
            let operation = *operation;
            match decide(*batch, leaves, settled) {
                Some(decision) => {
                    let leaves = leaves.clone();
                    run_vm(
                        state,
                        Some(Injection::Leaves {
                            operation,
                            decision,
                            leaves,
                        }),
                    )
                }
                None => standing(state),
            }
        }
        // A step this engine no longer waits on: a loser of a decided
        // aggregate.
        _ => standing(state),
    }
}

/// The earliest deadline of the parked operation's timers that have not
/// fired: what the process sleeps until while it waits on its leaves.
fn next_timer(leaves: &[Leaf]) -> Option<lash_core::durable_port::DurableInstant> {
    leaves
        .iter()
        .filter_map(|leaf| match leaf {
            Leaf::Timer {
                until_ms,
                fired: false,
            } => Some(*until_ms),
            _ => None,
        })
        .min()
        .map(lash_core::durable_port::DurableInstant)
}

/// The sleep the parked operation's leaves asked for ended: its earliest
/// timers fire, in leaf order, and decide the operation if they can.
fn timers_woke(state: &mut LashVmEngineState) -> Result<EngineAction, ProcessInfraError> {
    let Phase::Parked {
        operation,
        wait: Wait::Leaves {
            batch,
            leaves,
            settled,
        },
    } = &mut state.phase
    else {
        return standing(state);
    };
    let Some(due) = next_timer(leaves) else {
        return standing(state);
    };
    for (index, leaf) in leaves.iter_mut().enumerate() {
        if let Leaf::Timer { until_ms, fired } = leaf
            && !*fired
            && *until_ms <= due.0
        {
            *fired = true;
            settled.push(index);
        }
    }
    let operation = *operation;
    match decide(*batch, leaves, settled) {
        Some(decision) => {
            let leaves = leaves.clone();
            run_vm(
                state,
                Some(Injection::Leaves {
                    operation,
                    decision,
                    leaves,
                }),
            )
        }
        None => standing(state),
    }
}

fn vm_run_settled(
    state: &mut LashVmEngineState,
    settled: SettledOutput,
) -> Result<EngineAction, ProcessInfraError> {
    let output = match settled {
        SettledOutput::Completed(output) => {
            serde_json::from_str::<VmRunOutput>(output.payload())
                .map_err(|error| infra(format!("vm_run answered {error}")))?
        }
        SettledOutput::Failed(failure) => {
            let output = serde_json::from_str::<lash_core::ToolCallOutput>(failure.payload())
                .map_err(|error| infra(format!("vm_run failure answered {error}")))?;
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(
                lash_core::ProcessOutcome::from_tool_output(output),
            ));
        }
        SettledOutput::TimedOut { cause, .. } => {
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(
                crate::process::process_lash_vm_failure(
                    LashVmProcessFailureCode::ProcessExecutionBoundExhausted,
                    format!("the VM ran past its limit ({cause:?}) before a quiet point"),
                    None,
                ),
            ));
        }
        // An interrupted or stopped run reached no quiet point, after
        // every retry its step kind's policy allowed (`vm_run`'s
        // declaration, which the host may override): the process ends.
        unsettled => {
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(
                crate::process::process_lash_vm_failure(
                    LashVmProcessFailureCode::ProcessSegmentResumeFailed,
                    format!(
                        "the VM reached no quiet point: its run settled {:?}",
                        unsettled.outcome()
                    ),
                    None,
                ),
            ));
        }
    };
    match output {
        VmRunOutput::Ended { outcome } => {
            state.phase = Phase::Ended;
            Ok(EngineAction::Terminal(*outcome))
        }
        VmRunOutput::Parked {
            program_hash,
            vm,
            issued,
        } => {
            state.program_hash = Some(program_hash);
            state.vm = Some(vm);
            let operation = state.operations;
            state.operations += 1;
            park(state, operation, *issued)
        }
    }
}

/// Fold the operation the VM issued at its quiet point into the state, and
/// answer the action that performs it.
fn park(
    state: &mut LashVmEngineState,
    operation: u64,
    issued: IssuedOperation,
) -> Result<EngineAction, ProcessInfraError> {
    match issued {
        IssuedOperation::Leaves { batch, leaves } => {
            let mut steps = Vec::new();
            let leaves = leaves
                .into_iter()
                .enumerate()
                .map(|(index, leaf)| {
                    let step = StepName(format!("op.{operation}.{index}"));
                    match leaf {
                        IssuedLeaf::Tool { tool, input, site } => {
                            steps.push(StepRequest::Tool {
                                step: step.clone(),
                                tool,
                                input,
                                site,
                            });
                            Ok(Leaf::Step {
                                step,
                                call_id: None,
                                outcome: None,
                            })
                        }
                        // A timer runs no step: the process sleeps until
                        // it on a durable wake ([`next_timer`]).
                        IssuedLeaf::Timer { until_ms } => Ok(Leaf::Timer {
                            until_ms,
                            fired: false,
                        }),
                        IssuedLeaf::Settled { fulfilled, outcome } => {
                            Ok(Leaf::Settled { fulfilled, outcome })
                        }
                    }
                })
                .collect::<Result<Vec<_>, ProcessInfraError>>()?;
            // Leaves settled at issue settle first, in leaf order.
            let settled: Vec<usize> = leaves
                .iter()
                .enumerate()
                .filter(|(_, leaf)| matches!(leaf, Leaf::Settled { .. }))
                .map(|(index, _)| index)
                .collect();
            match decide(batch, &leaves, &settled) {
                // Decided at issue: every pending leaf is still admitted
                // before the VM is answered (ADR 0099 §11 clause 3).
                Some(decision) => {
                    let mut action = run_vm(
                        state,
                        Some(Injection::Leaves {
                            operation,
                            decision,
                            leaves,
                        }),
                    )?;
                    if let EngineAction::Steps { steps: run, .. } = &mut action {
                        steps.append(run);
                        *run = steps;
                    }
                    Ok(action)
                }
                None => {
                    let wake = next_timer(&leaves);
                    state.phase = Phase::Parked {
                        operation,
                        wait: Wait::Leaves {
                            batch,
                            leaves,
                            settled,
                        },
                    };
                    if steps.is_empty() {
                        standing(state)
                    } else {
                        Ok(EngineAction::Steps { steps, wake })
                    }
                }
            }
        }
        IssuedOperation::Sleep { until_ms, site } => {
            state.phase = Phase::Parked {
                operation,
                wait: Wait::Sleep { until_ms, site },
            };
            standing(state)
        }
        IssuedOperation::AwaitProcess { process, site } => {
            state.phase = Phase::Parked {
                operation,
                wait: Wait::Process { process, site },
            };
            standing(state)
        }
    }
}

/// Ask for the next `vm_run`, resuming the committed snapshot with `inject`.
fn run_vm(
    state: &mut LashVmEngineState,
    inject: Option<Injection>,
) -> Result<EngineAction, ProcessInfraError> {
    let step = StepName(format!("{VM_RUN_STEP}.{}", state.runs));
    state.runs += 1;
    let input = VmRunInput {
        payload: state.payload.clone(),
        program_hash: state.program_hash.clone(),
        vm: state.vm.clone(),
        inject: inject.clone(),
    };
    state.phase = Phase::Running {
        step: step.clone(),
        inject,
    };
    Ok(EngineAction::Steps {
        steps: vec![StepRequest::Engine {
            step,
            kind: EngineStepKind::new(VM_RUN_STEP),
            input: serde_json::to_value(input).map_err(|error| infra(error.to_string()))?,
        }],
        wake: None,
    })
}

/// Whether the leaves settled so far decide the operation (ADR 0099 §10):
/// a lone resource operation when it settles; an aggregate by its
/// consumer, over its settlements in order.
fn decide(batch: Option<BatchShape>, leaves: &[Leaf], settled: &[usize]) -> Option<Decision> {
    let Some(batch) = batch else {
        return (!settled.is_empty()).then_some(Decision::Single);
    };
    use lash_vm::AggregateConsumer;
    let decides = |fulfilled: bool| match batch.consumer {
        AggregateConsumer::AllSettled => false,
        AggregateConsumer::All => !fulfilled,
        AggregateConsumer::Race => true,
        AggregateConsumer::Any => fulfilled,
    };
    let selected = |within: &dyn Fn(usize) -> bool| {
        settled
            .iter()
            .copied()
            .filter(|leaf| within(*leaf))
            .find(|leaf| leaves[*leaf].fulfilled().is_some_and(&decides))
    };
    if let Some(after) = batch.settled_value_after {
        // The plain value decides unless a leaf before it that settled at
        // issue already did (§10 L5).
        return Some(
            selected(&|leaf| leaf < after && matches!(leaves[leaf], Leaf::Settled { .. }))
                .map_or(Decision::SettledValue, |leaf| Decision::Selected { leaf }),
        );
    }
    if let Some(leaf) = selected(&|_| true) {
        return Some(Decision::Selected { leaf });
    }
    (settled.len() == leaves.len()).then_some(match batch.consumer {
        AggregateConsumer::Any if !leaves.is_empty() => Decision::ExhaustedRejections,
        _ => Decision::AllResults,
    })
}

fn cancelled(origin: lash_sansio::CancelOrigin) -> lash_core::ProcessOutcome {
    lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::cancelled(
        lash_core::ToolCancellation::runtime("the lash_vm process was cancelled"),
    ))
    .with_cancel_origin(Some(origin))
}

#[cfg(test)]
pub(crate) fn decode_for_tests(state: &EngineState) -> LashVmEngineState {
    decode(state).expect("the engine state decodes")
}
