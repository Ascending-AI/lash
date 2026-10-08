//! The lashlang engine's `advance`: pure, synchronous and effect-free
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
    BatchShape, Decision, Injection, IssuedLeaf, IssuedOperation, LASHLANG_SEGMENT_STATE_VERSION,
    LashlangEngineState, Leaf, Phase, QueuedSignal, TIMER_STEP, TimerInput, VM_RUN_STEP,
    VmRunInput, VmRunOutput, Wait,
};
use crate::{LASHLANG_ENGINE_KIND, LashlangProcessFailureCode};

/// How many `vm_run` steps in a row may fail before the process ends.
const VM_RUN_FAULT_BUDGET: u32 = 3;

/// The engine's state format.
pub(crate) fn state_format() -> lash_core::EngineStateFormat {
    lash_core::EngineStateFormat {
        kind: LASHLANG_ENGINE_KIND.to_owned(),
        version: LASHLANG_SEGMENT_STATE_VERSION,
    }
}

fn infra(message: impl Into<String>) -> ProcessInfraError {
    ProcessInfraError::new(lash_core::PluginError::StoredDataCorrupt {
        record_kind: "lashlang engine state".to_owned(),
        message: message.into(),
    })
}

fn encode(state: &LashlangEngineState) -> Result<EngineState, ProcessInfraError> {
    Ok(EngineState {
        format: state_format(),
        bytes: serde_json::to_vec(state).map_err(|error| infra(error.to_string()))?,
    })
}

fn decode(state: &EngineState) -> Result<LashlangEngineState, ProcessInfraError> {
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
        let mut state = LashlangEngineState {
            payload,
            program_hash: None,
            vm: None,
            runs: 0,
            operations: 0,
            faults: 0,
            signals: Default::default(),
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
    state: &mut LashlangEngineState,
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
        EngineEvent::StepSettled { step, outcome } => step_settled(state, &step, outcome),
        EngineEvent::Woke => match &state.phase {
            Phase::Parked {
                operation,
                wait: Wait::Sleep { .. },
            } => {
                let operation = *operation;
                run_vm(state, Some(Injection::Woke { operation }))
            }
            _ => standing(state),
        },
        EngineEvent::Emitted => match &state.phase {
            Phase::Parked {
                operation,
                wait: Wait::Emitted,
            } => {
                let operation = *operation;
                run_vm(state, Some(Injection::Emitted { operation }))
            }
            _ => Err(infra("an append the engine never asked for was reported")),
        },
        EngineEvent::ProcessEnded { process, outcome } => match &state.phase {
            Phase::Parked {
                operation,
                wait: Wait::Process { process: awaited },
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
        EngineEvent::Signal(signal) => {
            let name = signal.identity.signal_name().to_owned();
            let payload = signal.payload;
            match &state.phase {
                Phase::Parked {
                    operation,
                    wait: Wait::Signal { name: awaited },
                } if *awaited == name => {
                    let operation = *operation;
                    run_vm(state, Some(Injection::Signal { operation, payload }))
                }
                _ => {
                    state.signals.push_back(QueuedSignal { name, payload });
                    standing(state)
                }
            }
        }
        EngineEvent::ProcessWaitTimedOut { .. } => standing(state),
        EngineEvent::KeyPinned { .. }
        | EngineEvent::ExternalResolved { .. }
        | EngineEvent::ExternalTimedOut { .. } => Err(infra(
            "a lashlang process pins no host key, yet one was reported",
        )),
    }
}

/// The action that stands while the process waits on what it already
/// asked for: an event it does not wait on changes nothing.
fn standing(state: &LashlangEngineState) -> Result<EngineAction, ProcessInfraError> {
    Ok(match &state.phase {
        Phase::Parked {
            wait: Wait::Sleep { until_ms },
            ..
        } => EngineAction::Sleep {
            until: lash_core::durable_port::DurableInstant(*until_ms),
        },
        Phase::Parked {
            wait: Wait::Process { process },
            ..
        } => EngineAction::AwaitProcess {
            process: process.clone(),
            deadline: None,
        },
        Phase::Running { .. }
        | Phase::Parked {
            wait: Wait::Leaves { .. } | Wait::Emitted,
            ..
        } => EngineAction::Idle,
        Phase::Parked {
            wait: Wait::Signal { name },
            ..
        } => EngineAction::AwaitSignal { name: name.clone() },
        Phase::Ended => return Err(infra("an ended process has no standing action")),
    })
}

fn step_settled(
    state: &mut LashlangEngineState,
    step: &StepName,
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
            if let Leaf::Step { outcome: slot, .. } = &mut leaves[index] {
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
                None => Ok(EngineAction::Idle),
            }
        }
        // A step this engine no longer waits on: a loser of a decided
        // aggregate.
        _ => standing(state),
    }
}

fn vm_run_settled(
    state: &mut LashlangEngineState,
    settled: SettledOutput,
) -> Result<EngineAction, ProcessInfraError> {
    let output = match settled {
        SettledOutput::Completed(output) => {
            serde_json::from_str::<VmRunOutput>(output.payload())
                .map_err(|error| infra(format!("vm_run answered {error}")))?
        }
        SettledOutput::TimedOut { cause, .. } => {
            state.phase = Phase::Ended;
            return Ok(EngineAction::Terminal(
                crate::process::process_lashlang_failure(
                    LashlangProcessFailureCode::ProcessExecutionBoundExhausted,
                    format!("the VM ran past its limit ({cause:?}) before a quiet point"),
                    None,
                ),
            ));
        }
        // A failed, interrupted or stopped run left nothing: the snapshot it started
        // from is still the committed one, so the same run is asked again.
        _ => {
            state.faults += 1;
            if state.faults >= VM_RUN_FAULT_BUDGET {
                state.phase = Phase::Ended;
                return Ok(EngineAction::Terminal(
                    crate::process::process_lashlang_failure(
                        LashlangProcessFailureCode::ProcessSegmentResumeFailed,
                        format!(
                            "the VM failed to reach a quiet point {} times in a row",
                            state.faults
                        ),
                        None,
                    ),
                ));
            }
            let inject = match &state.phase {
                Phase::Running { inject, .. } => inject.clone(),
                _ => None,
            };
            return run_vm(state, inject);
        }
    };
    state.faults = 0;
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
            park(state, operation, issued)
        }
    }
}

/// Fold the operation the VM issued at its quiet point into the state, and
/// answer the action that performs it.
fn park(
    state: &mut LashlangEngineState,
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
                        IssuedLeaf::Tool {
                            tool,
                            input,
                            site,
                            language_execution,
                        } => {
                            steps.push(StepRequest::Tool {
                                language_execution,
                                step: step.clone(),
                                tool,
                                input,
                                site,
                            });
                            Ok(Leaf::Step {
                                step,
                                timer: false,
                                outcome: None,
                            })
                        }
                        IssuedLeaf::Host {
                            operation,
                            input,
                            site,
                            language_execution,
                        } => {
                            steps.push(StepRequest::Host {
                                language_execution,
                                step: step.clone(),
                                operation,
                                input,
                                site,
                            });
                            Ok(Leaf::Step {
                                step,
                                timer: false,
                                outcome: None,
                            })
                        }
                        IssuedLeaf::Timer { until_ms } => {
                            steps.push(StepRequest::Engine {
                                step: step.clone(),
                                kind: EngineStepKind::new(TIMER_STEP),
                                input: serde_json::to_value(TimerInput { until_ms })
                                    .map_err(|error| infra(error.to_string()))?,
                            });
                            Ok(Leaf::Step {
                                step,
                                timer: true,
                                outcome: None,
                            })
                        }
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
                    if let EngineAction::Steps(run) = &mut action {
                        steps.append(run);
                        *run = steps;
                    }
                    Ok(action)
                }
                None => {
                    state.phase = Phase::Parked {
                        operation,
                        wait: Wait::Leaves {
                            batch,
                            leaves,
                            settled,
                        },
                    };
                    Ok(EngineAction::Steps(steps))
                }
            }
        }
        IssuedOperation::Sleep { until_ms } => {
            state.phase = Phase::Parked {
                operation,
                wait: Wait::Sleep { until_ms },
            };
            standing(state)
        }
        IssuedOperation::AwaitProcess { process } => {
            state.phase = Phase::Parked {
                operation,
                wait: Wait::Process { process },
            };
            standing(state)
        }
        IssuedOperation::WaitSignal { name } => {
            if let Some(index) = state.signals.iter().position(|signal| signal.name == name) {
                let signal = state.signals.remove(index).ok_or_else(|| infra("queue"))?;
                return run_vm(
                    state,
                    Some(Injection::Signal {
                        operation,
                        payload: signal.payload,
                    }),
                );
            }
            state.phase = Phase::Parked {
                operation,
                wait: Wait::Signal { name },
            };
            standing(state)
        }
        IssuedOperation::Emit {
            event_type,
            payload,
        } => {
            let event_type = crate::lashlang_process_event_types()
                .into_iter()
                .find(|declared| declared.name == event_type)
                .ok_or_else(|| infra(format!("`{event_type}` is no lashlang event type")))?;
            state.phase = Phase::Parked {
                operation,
                wait: Wait::Emitted,
            };
            Ok(EngineAction::Emit {
                event_type,
                payload,
            })
        }
    }
}

/// Ask for the next `vm_run`, resuming the committed snapshot with `inject`.
fn run_vm(
    state: &mut LashlangEngineState,
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
    Ok(EngineAction::Steps(vec![StepRequest::Engine {
        step,
        kind: EngineStepKind::new(VM_RUN_STEP),
        input: serde_json::to_value(input).map_err(|error| infra(error.to_string()))?,
    }]))
}

/// Whether the leaves settled so far decide the operation (ADR 0099 §10):
/// a lone resource operation when it settles; an aggregate by its
/// consumer, over its settlements in order.
fn decide(batch: Option<BatchShape>, leaves: &[Leaf], settled: &[usize]) -> Option<Decision> {
    let Some(batch) = batch else {
        return (!settled.is_empty()).then_some(Decision::Single);
    };
    use lashlang::AggregateConsumer;
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
        lash_core::ToolCancellation::runtime("the lashlang process was cancelled"),
    ))
    .with_cancel_origin(Some(origin))
}

#[cfg(test)]
pub(crate) fn decode_for_tests(state: &EngineState) -> LashlangEngineState {
    decode(state).expect("the engine state decodes")
}
