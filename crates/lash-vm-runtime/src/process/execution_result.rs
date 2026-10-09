//! How a Lash VM process's run becomes the output its awaiter reads.

use super::*;

/// `tool_call_limit` is the typed `max_tool_calls` refusal the process's
/// context met, read when the run failed on it.
pub(crate) fn process_lash_vm_execution_result(
    result: Result<lash_vm::ExecutionOutcome, lash_vm::RuntimeError>,
    tool_call_limit: Option<lash_core::ToolCallLimitExceeded>,
) -> lash_core::ProcessAwaitOutput {
    match result {
        Ok(lash_vm::ExecutionOutcome::Finished(value)) => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                lash_vm_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ))
        }
        Ok(lash_vm::ExecutionOutcome::Failed(value)) => process_lash_vm_failure(
            LashVmProcessFailureCode::ProcessFailed,
            value.to_string(),
            Some(
                lash_vm_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ),
        ),
        Ok(lash_vm::ExecutionOutcome::Continued) => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
        }
        // The session's `max_tool_calls` refused a call the process could
        // not hold (FIG-4546): the process fails with the refusal itself, the
        // same typed failure a cell's call settles with.
        Err(lash_vm::RuntimeError::AggregateHostControl { source })
            if source.tool_failure_code() == Some(lash_core::ToolCallLimitExceeded::CODE) =>
        {
            let mut failure = lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::ResourceLimit,
                lash_core::ToolCallLimitExceeded::CODE,
                source.message(),
            );
            failure.raw = tool_call_limit.map(|exceeded| {
                lash_core::ToolValue::untrusted_json(
                    serde_json::json!({ "tool_call_limit": exceeded }),
                )
            });
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(
                failure,
            ))
        }
        Err(err) => {
            let exhausted = err.is_execution_bound_exhausted();
            #[cfg(any(test, feature = "testing"))]
            assert!(
                !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst) || !exhausted,
                "confidence durable process exhausted a required Lash VM bound: {err}"
            );
            process_lash_vm_failure(
                if exhausted {
                    LashVmProcessFailureCode::ProcessExecutionBoundExhausted
                } else {
                    LashVmProcessFailureCode::ProcessRuntimeError
                },
                crate::host_lifetime_failure_message(&err).unwrap_or_else(|| err.to_string()),
                None,
            )
        }
    }
}

/// What a worker failure is to the process it ran: its terminal, or `None`
/// for a failure of the attempt, which is redriven.
///
/// A limit the run itself exhausted and a refusal of the run's own inputs
/// meet every attempt the same way, so they end the process. A host verdict
/// (its deadline, CPU or attempt accounting, FIG-4451), a lost worker and a
/// broken exchange are this attempt's.
pub(crate) fn process_worker_failure(
    failure: &lash_vm_broker::BrokerFailure,
) -> Option<lash_core::ProcessAwaitOutput> {
    use lash_vm_broker::{BrokerFailure, CheckoutRefusal};
    use lash_vm_protocol::{InfrastructureOutcome, RunRefusal};
    let refused = |refusal: &RunRefusal| {
        let mut terminal = process_lash_vm_failure(
            LashVmProcessFailureCode::ProcessRunRefused,
            format!("the worker refuses the run: {refusal}"),
            Some(serde_json::json!({ "run_refusal": refusal })),
        );
        if let RunRefusal::UnusableSchema { source } = refusal
            && let lash_core::ProcessAwaitOutput::Settled { output } = &mut terminal
            && let lash_core::ToolCallOutcome::Failure(failure) = &mut output.outcome
        {
            failure.cause = Some(Box::new(lash_core::ToolFailureCause::SchemaAdmission {
                source: source.as_ref().clone(),
            }));
        }
        terminal
    };
    let outcome = match failure {
        BrokerFailure::WorkerLost { outcome, .. }
        | BrokerFailure::Unavailable {
            refusal: CheckoutRefusal::Infrastructure(outcome),
        } => outcome,
        BrokerFailure::StateRefused { refusal } => {
            return Some(refused(&RunRefusal::State {
                refusal: refusal.clone(),
            }));
        }
        BrokerFailure::Unavailable {
            refusal:
                CheckoutRefusal::QueueFull
                | CheckoutRefusal::TimedOut { .. }
                | CheckoutRefusal::RestartStorm
                | CheckoutRefusal::Closed,
        }
        | BrokerFailure::Interrupted
        | BrokerFailure::FrameRetired
        | BrokerFailure::Parent { .. }
        | BrokerFailure::Checkpoint { .. } => return None,
    };
    match outcome {
        InfrastructureOutcome::WorkerLimitExceeded { limit } if !limit.is_host_verdict() => {
            #[cfg(any(test, feature = "testing"))]
            assert!(
                !EXECUTION_BOUND_EXHAUSTION_LOUD.load(Ordering::SeqCst),
                "confidence durable process exhausted a required Lash VM bound: {limit:?}"
            );
            Some(process_lash_vm_failure(
                LashVmProcessFailureCode::ProcessExecutionBoundExhausted,
                format!("worker execution bound exhausted: {limit}"),
                Some(serde_json::json!({ "worker_limit": limit })),
            ))
        }
        InfrastructureOutcome::RunRefused { refusal } => Some(refused(refusal)),
        InfrastructureOutcome::WorkerDeployment { .. }
        | InfrastructureOutcome::WorkerLimitExceeded { .. }
        | InfrastructureOutcome::WorkerCrashed { .. }
        | InfrastructureOutcome::WorkerUnresponsive { .. }
        | InfrastructureOutcome::ProtocolViolation { .. } => None,
    }
}

pub(crate) fn process_lash_vm_failure(
    code: LashVmProcessFailureCode,
    message: impl Into<String>,
    raw: Option<serde_json::Value>,
) -> lash_core::ProcessAwaitOutput {
    let mut failure = lash_core::ToolFailure::runtime(
        lash_core::ToolFailureClass::Execution,
        code.as_str(),
        message,
    );
    failure.raw = raw.map(lash_core::ToolValue::untrusted_json);
    lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::failure(failure))
}
