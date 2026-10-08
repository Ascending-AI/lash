//! What a resumed VM is answered with (ADR 0132 §8; FIG-5198).
//!
//! A VM parked on an operation issues it again when its snapshot resumes.
//! That reissue is answered here from the outcome the state holds for the
//! operation: nothing is dispatched, and no host code runs again.
//!
//! A catalog tool step's payload is its `ToolCallOutput`, as the step's
//! outcome recorded it.

use lash_core::SettledOutput;
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionHostError, ResourceOperationBatchOutcome,
    ResourceOperationOutcome, Value,
};

use super::state::{Decision, Injection, Leaf};
use crate::bridge::{ExecutionCancellation, protocol_tool_output_to_lashlang_value};

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
    #[error("step `{0}` settled as a park, which answers nothing")]
    Parked(String),
}

fn decode_output(payload: &str) -> Result<lash_core::ToolCallOutput, InjectionFault> {
    serde_json::from_str(payload).map_err(InjectionFault::Payload)
}

/// Whether a step's settlement fulfils: a completion whose tool output is a
/// success.
pub(crate) fn fulfilled(outcome: &SettledOutput) -> bool {
    match outcome {
        SettledOutput::Completed(output) => decode_output(output.payload())
            .is_ok_and(|output| matches!(output.outcome, lash_core::ToolCallOutcome::Success(_))),
        _ => false,
    }
}

/// One step's settlement as the VM reads it: its tool output, or the answer
/// every reader gives an interruption, a limit or a cancel.
fn step_result(
    key: &str,
    timer: bool,
    outcome: &SettledOutput,
    cancellation: &ExecutionCancellation,
) -> Result<Result<Value, ExecutionHostError>, InjectionFault> {
    let output = match outcome {
        SettledOutput::Completed(_) if timer => return Ok(Ok(Value::Undefined)),
        SettledOutput::Completed(output) => decode_output(output.payload())?,
        SettledOutput::Failed(failure) => decode_output(failure.payload())?,
        // A step settles once its park ends: a park is never its settlement.
        unanswered => unanswered
            .stopped_answer()
            .ok_or_else(|| InjectionFault::Parked(key.to_owned()))?,
    };
    Ok(protocol_tool_output_to_lashlang_value(
        &output,
        key,
        cancellation,
    ))
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

        AbilityOp::Sleep(_) => "a sleep",
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

        (_, inject) => return Err(mismatch(&inject)),
    })
}

#[cfg(test)]
mod tests {
    use lash_core::SettledOutput;

    use super::super::state::Leaf;
    use super::step_result;
    use crate::bridge::ExecutionCancellation;

    /// A step that started and never settled reads as the turn presents an
    /// interrupted call: the one `tool_interrupted` failure, with its
    /// `Interrupted` cause, for every reader of an attempt's outcome.
    #[test]
    fn an_interrupted_step_is_answered_as_a_turn_presents_an_interrupted_call() {
        let answer = step_result(
            "charge",
            false,
            &SettledOutput::Interrupted,
            &ExecutionCancellation::new(),
        )
        .expect("an interrupted step is answered");
        let turn = lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::Execution,
            "tool_interrupted",
            "tool was interrupted by a runtime restart; it may or may not have taken effect, and may still be running.",
        )
        .with_cause(lash_core::ToolFailureCause::Interrupted);
        assert_eq!(
            answer,
            Err(lashlang::ExecutionHostError::from_tool_failure(
                &turn, "charge"
            ))
        );
    }

    /// A parked operation's step leaf decodes only with the payload its
    /// outcome names: a missing payload or other bytes are refused.
    #[test]
    fn a_step_leaf_decodes_only_with_the_payload_its_outcome_names() {
        let process = lash_core::ProcessId::fixture("leaf-law");
        let leaf = Leaf::Step {
            step: lash_core::StepName("op.0.0".to_owned()),
            timer: false,
            outcome: Some(Box::new(super::super::vm_run::completed(
                &process,
                "\"alpha\"".to_owned(),
            ))),
        };
        let json = serde_json::to_string(&leaf).expect("a leaf encodes");
        assert_eq!(
            serde_json::from_str::<Leaf>(&json).expect("a leaf decodes"),
            leaf
        );
        for forged in [
            json.replace(r#""\"alpha\"""#, r#""\"omega\"""#),
            json.replace(r#""\"alpha\"""#, "null"),
        ] {
            assert_ne!(forged, json);
            assert!(
                serde_json::from_str::<Leaf>(&forged).is_err(),
                "{forged} is refused"
            );
        }
    }
}
