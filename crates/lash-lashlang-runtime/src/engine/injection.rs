//! What a resumed VM is answered with (ADR 0132 §8; FIG-5198).
//!
//! A VM parked on an operation issues it again when its snapshot resumes.
//! That reissue is answered here from the outcome the state holds for the
//! operation: nothing is dispatched, and no host code runs again.
//!
//! A catalog tool step's payload is its `ToolCallOutput`, as the step's
//! outcome recorded it.

use lash_core::SettledOutcome;
use lash_core::tool_run::AttemptOutcome;
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionHostError, ResourceOperationBatchOutcome,
    ResourceOperationOutcome, Value,
};

use super::state::{Decision, Injection, Leaf};
use crate::bridge::{ExecutionCancellation, protocol_tool_output_to_lashlang_value};

/// The code a `Once` call's failure carries when it started and never
/// settled: it is not run again, and the guest may catch it.
pub(crate) const TOOL_INTERRUPTED_CODE: &str = "tool_interrupted";

/// The code a call's failure carries when it ran past its limit.
pub(crate) const TOOL_TIMED_OUT_CODE: &str = "tool_timed_out";

/// Why an injection could not answer the reissued operation: the state and
/// the snapshot disagree, which no guest may observe.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InjectionFault {
    #[error("the resumed VM issued {issued}, but the state holds the outcome of {held}")]
    Mismatch {
        issued: &'static str,
        held: &'static str,
    },
    #[error("a step outcome's payload is not a tool output: {0}")]
    Payload(serde_json::Error),
    #[error("an outcome settled at issue does not decode: {0}")]
    Settled(String),
}

fn decode_output(payload: &str) -> Result<lash_core::ToolCallOutput, InjectionFault> {
    serde_json::from_str(payload).map_err(InjectionFault::Payload)
}

/// Whether a step's settlement fulfils: a completion whose tool output is a
/// success.
pub(crate) fn fulfilled(outcome: &SettledOutcome) -> bool {
    matches!(outcome.outcome(), AttemptOutcome::Completed(_))
        && outcome
            .payload()
            .and_then(|payload| decode_output(payload).ok())
            .is_some_and(|output| matches!(output.outcome, lash_core::ToolCallOutcome::Success(_)))
}

/// One step's settlement as the VM reads it.
fn step_result(
    key: &str,
    timer: bool,
    outcome: &SettledOutcome,
    cancellation: &ExecutionCancellation,
) -> Result<Result<Value, ExecutionHostError>, InjectionFault> {
    let failure = |class, code: &str, message: String| {
        Err(ExecutionHostError::from_tool_failure(
            &lash_core::ToolFailure::runtime(class, code, message),
            key,
        ))
    };
    Ok(match (outcome.outcome(), outcome.payload()) {
        (AttemptOutcome::Completed(_), Some(_)) if timer => Ok(Value::Undefined),
        (AttemptOutcome::Completed(_) | AttemptOutcome::Failed(_), Some(payload)) => {
            protocol_tool_output_to_lashlang_value(&decode_output(payload)?, key, cancellation)
        }
        (AttemptOutcome::Interrupted, _) => failure(
            lash_core::ToolFailureClass::Execution,
            TOOL_INTERRUPTED_CODE,
            format!("call `{key}` started and never settled; it is not run again"),
        ),
        (AttemptOutcome::TimedOut { cause, .. }, _) => failure(
            lash_core::ToolFailureClass::Timeout,
            TOOL_TIMED_OUT_CODE,
            format!("call `{key}` ran past its limit ({cause:?})"),
        ),
        (AttemptOutcome::Cancelled { .. }, _) => {
            cancellation.cancel();
            Err(crate::LashlangHostError::ToolCancelled {
                message: format!("call `{key}` was cancelled"),
            }
            .into())
        }
        (AttemptOutcome::Waiting(_), _)
        | (AttemptOutcome::Completed(_) | AttemptOutcome::Failed(_), None) => failure(
            lash_core::ToolFailureClass::Internal,
            TOOL_INTERRUPTED_CODE,
            format!("call `{key}` settled without a result"),
        ),
    })
}

fn leaf_result(
    leaf: &Leaf,
    cancellation: &ExecutionCancellation,
) -> Result<ResourceOperationOutcome, InjectionFault> {
    match leaf {
        Leaf::Step {
            step,
            timer,
            outcome: Some(outcome),
        } => Ok(ResourceOperationOutcome::from_result(step_result(
            &step.0,
            *timer,
            outcome,
            cancellation,
        )?)),
        Leaf::Step {
            step,
            outcome: None,
            ..
        } => Ok(ResourceOperationOutcome::Error(ExecutionHostError::new(
            format!(
                "aggregate leaf `{}` was answered without a settlement",
                step.0
            ),
        ))),
        Leaf::Settled { outcome, .. } => outcome
            .decode()
            .map_err(|error| InjectionFault::Settled(error.to_string())),
    }
}

fn batch_outcome(
    decision: Decision,
    leaves: &[Leaf],
    cancellation: &ExecutionCancellation,
) -> Result<ResourceOperationBatchOutcome, InjectionFault> {
    Ok(match decision {
        Decision::Single | Decision::AllResults => ResourceOperationBatchOutcome::AllResults(
            leaves
                .iter()
                .map(|leaf| leaf_result(leaf, cancellation))
                .collect::<Result<_, _>>()?,
        ),
        Decision::Selected { leaf } => ResourceOperationBatchOutcome::Selected {
            leaf,
            result: leaf_result(&leaves[leaf], cancellation)?,
        },
        Decision::ExhaustedRejections => ResourceOperationBatchOutcome::ExhaustedRejections(
            leaves
                .iter()
                .filter_map(|leaf| match leaf_result(leaf, cancellation) {
                    Ok(ResourceOperationOutcome::Error(error)) => Some(Ok(error)),
                    Ok(ResourceOperationOutcome::Value(_)) => None,
                    Err(fault) => Some(Err(fault)),
                })
                .collect::<Result<_, _>>()?,
        ),
        Decision::SettledValue => ResourceOperationBatchOutcome::SettledValue,
    })
}

fn process_value(
    outcome: &lash_core::ProcessOutcome,
    cancellation: &ExecutionCancellation,
) -> Result<Value, ExecutionHostError> {
    match outcome {
        lash_core::ProcessAwaitOutput::Settled { output } => {
            protocol_tool_output_to_lashlang_value(output, "await", cancellation)
        }
        lash_core::ProcessAwaitOutput::Abandoned { .. } => Err(ExecutionHostError::new(
            "the awaited process was abandoned without an outcome",
        )),
        lash_core::ProcessAwaitOutput::NoLongerRetained { .. } => Err(ExecutionHostError::new(
            "the awaited process is no longer retained",
        )),
    }
}

fn op_name(op: &AbilityOp) -> &'static str {
    match op {
        AbilityOp::ResourceOperation(_) => "a resource operation",
        AbilityOp::ResourceOperationBatch(_) => "an aggregate",
        AbilityOp::Await(_) => "an await",
        AbilityOp::Print(_) => "a print",
        AbilityOp::Finish(_) => "a finish",
        AbilityOp::Fail(_) => "a fail",
        AbilityOp::ProcessEvent(_) => "an event",
        AbilityOp::Sleep(_) => "a sleep",
        AbilityOp::WaitSignal { .. } => "a signal wait",
    }
}

fn injection_name(inject: &Injection) -> &'static str {
    match inject {
        Injection::Leaves {
            decision: Decision::Single,
            ..
        } => "a resource operation",
        Injection::Leaves { .. } => "an aggregate",
        Injection::Woke { .. } => "a sleep",
        Injection::ProcessEnded { .. } => "an await",
        Injection::Signal { .. } => "a signal wait",
        Injection::Emitted { .. } => "an event",
    }
}

/// Answer `op`, the operation a resumed VM issued again, from `inject`.
pub(crate) fn answer(
    op: &AbilityOp,
    inject: Injection,
    cancellation: &ExecutionCancellation,
) -> Result<Result<AbilityOutcome, ExecutionHostError>, InjectionFault> {
    let mismatch = |inject: &Injection| InjectionFault::Mismatch {
        issued: op_name(op),
        held: injection_name(inject),
    };
    Ok(match (op, inject) {
        (
            AbilityOp::ResourceOperation(_),
            Injection::Leaves {
                decision: Decision::Single,
                leaves,
                ..
            },
        ) if leaves.len() == 1 => match leaf_result(&leaves[0], cancellation)? {
            ResourceOperationOutcome::Value(value) => Ok(AbilityOutcome::Value(value)),
            ResourceOperationOutcome::Error(error) => Err(error),
        },
        (
            AbilityOp::ResourceOperationBatch(_),
            Injection::Leaves {
                decision, leaves, ..
            },
        ) if decision != Decision::Single => Ok(AbilityOutcome::ResourceOperationBatch(
            batch_outcome(decision, &leaves, cancellation)?,
        )),
        (AbilityOp::Await(_), Injection::ProcessEnded { outcome, .. }) => {
            process_value(&outcome, cancellation).map(AbilityOutcome::Value)
        }
        (AbilityOp::Sleep(_), Injection::Woke { .. }) => Ok(AbilityOutcome::Value(Value::Null)),
        (AbilityOp::WaitSignal { .. }, Injection::Signal { payload, .. }) => {
            Ok(AbilityOutcome::Value(lashlang::from_json(payload)))
        }
        (AbilityOp::ProcessEvent(_), Injection::Emitted { .. }) => Ok(AbilityOutcome::Unit),
        (_, inject) => return Err(mismatch(&inject)),
    })
}
