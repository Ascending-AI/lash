//! How a Lashlang process's run becomes the output its awaiter reads.

use super::*;

/// `tool_call_limit` is the typed `max_tool_calls` refusal the process's
/// context met, read when the run failed on it.
pub(super) fn process_lashlang_execution_result(
    result: Result<lashlang::ExecutionOutcome, lashlang::RuntimeError>,
    tool_call_limit: Option<lash_core::ToolCallLimitExceeded>,
) -> lash_core::ProcessAwaitOutput {
    match result {
        Ok(lashlang::ExecutionOutcome::Finished(value)) => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                lashlang_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ))
        }
        Ok(lashlang::ExecutionOutcome::Failed(value)) => process_lashlang_failure(
            LashlangProcessFailureCode::ProcessFailed,
            value.to_string(),
            Some(
                lashlang_value_to_json(&value)
                    .unwrap_or_else(|err| serde_json::json!({ "error": err.to_string() })),
            ),
        ),
        Ok(lashlang::ExecutionOutcome::Continued) => {
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
        }
        // The session's `max_tool_calls` refused a call the process could
        // not hold (FIG-4546): the process fails with the refusal itself, the
        // same typed failure a cell's call settles with.
        Err(lashlang::RuntimeError::AggregateHostControl { source })
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
                "confidence durable process exhausted a required Lashlang bound: {err}"
            );
            process_lashlang_failure(
                if exhausted {
                    LashlangProcessFailureCode::ProcessExecutionBoundExhausted
                } else {
                    LashlangProcessFailureCode::ProcessRuntimeError
                },
                crate::host_lifetime_failure_message(&err).unwrap_or_else(|| err.to_string()),
                None,
            )
        }
    }
}

pub(super) fn process_lashlang_failure(
    code: LashlangProcessFailureCode,
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
