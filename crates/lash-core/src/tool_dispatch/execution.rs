use std::sync::Arc;

use crate::plugin::ToolResultHookContext;
use crate::{PreparedToolCall, ToolContext, ToolFailureClass, ToolResult};
use futures_util::FutureExt as _;

use super::context::ToolDispatchOutcome;
use super::context::{
    PendingToolDispatchOutcome, ToolCallLaunch, ToolDispatchContext, launch_done, outcome,
    runtime_failure,
};
use super::directives::apply_after_tool_directives;
use super::retry::{execute_granted_leaf_tool_attempt, execute_leaf_tool_attempt};

/// Runs an authored process-replay tool body without creating a ToolAttempt
/// frame. Any durable operations the body issues are consequently direct
/// children of the enclosing process replay and must be awaited by the body.
pub(crate) async fn execute_orchestrating_tool<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let started = context.clock.now();
    let tool_name = prepared.tool_name.clone();
    let args = prepared.args.clone();
    let orchestration_context = crate::OrchestrationContext::from_tool_context(
        tool_context.with_prepared_payload(prepared.prepared_payload.clone()),
    );
    let result = std::panic::AssertUnwindSafe(context.tools.execute_orchestration_by_id(
        &prepared.tool_id,
        &prepared.args,
        &orchestration_context,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| {
        let message = crate::panic_containment::payload_message(payload.as_ref());
        crate::panic_containment::enforce_loudness(payload);
        ToolResult::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "tool_panicked",
            message,
        ))
    });
    let duration_ms = context.clock.now().duration_since(started).as_millis() as u64;
    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;
    let output = match result {
        ToolResult::Done(output) => *output,
        ToolResult::Pending(_) => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "orchestrating_tool_returned_pending",
            "orchestrating tools must immediately await journaled actions and return a completed result",
        )),
    };
    ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: Some(prepared.call_id),
            tool: tool_name,
            args,
            output,
            duration_ms,
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

/// Runs an internal process-body tool without creating a `ToolAttempt` frame.
///
/// Unlike authored orchestration, this is the owner-bound activity of the
/// process itself and may perform host I/O. It is available only to
/// `ToolActivation::Internal` process inputs, never to model-facing calls.
pub(crate) async fn execute_internal_process_tool<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let started = context.clock.now();
    let tool_name = prepared.tool_name.clone();
    let args = prepared.args.clone();
    let tool_context = tool_context.with_prepared_payload(prepared.prepared_payload.clone());
    let result = std::panic::AssertUnwindSafe(context.tools.execute_by_id(
        &prepared.tool_id,
        &prepared.args,
        &tool_context,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|payload| {
        let message = crate::panic_containment::payload_message(payload.as_ref());
        crate::panic_containment::enforce_loudness(payload);
        ToolResult::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "tool_panicked",
            message,
        ))
    });
    let duration_ms = context.clock.now().duration_since(started).as_millis() as u64;
    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;
    let output = match result {
        ToolResult::Done(output) => *output,
        ToolResult::Pending(_) => crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            ToolFailureClass::Internal,
            "internal_process_tool_returned_pending",
            "internal process-body tools must return a completed result",
        )),
    };
    ToolDispatchOutcome {
        record: crate::ToolCallRecord {
            call_id: Some(prepared.call_id),
            tool: tool_name,
            args,
            output,
            duration_ms,
        },
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) async fn dispatch_prepared_tool_call_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    coordinate_prepared_tool_call_launch_with_execution_context(
        context,
        prepared,
        None,
        tool_context,
    )
    .await
    .into_done_or_runtime_failure()
}

#[cfg(test)]
pub(crate) async fn coordinate_prepared_tool_call_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let retry_policy = execution_grant
        .as_ref()
        .map(|grant| grant.manifest.retry_policy)
        .or_else(|| {
            super::preparation::resolve_callable_manifest_by_id(context, &prepared.tool_id)
                .map(|manifest| manifest.retry_policy)
        })
        .unwrap_or(crate::ToolRetryPolicy::Never);
    let cancellation = tool_context.cancellation_token().cloned();
    let dispatch = Arc::new(context.clone());
    super::coordinate_tool_invocation(
        context,
        prepared,
        execution_grant,
        retry_policy,
        super::ToolAttemptEffectIdentity::Scalar {
            parent: context.parent_invocation.clone(),
        },
        cancellation,
        None,
        None,
        |completion_key| {
            crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&dispatch),
                tool_context.clone(),
                completion_key,
            )
        },
    )
    .await
    .launch
}

pub(super) async fn dispatch_prepared_tool_attempt_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let prepared_tool_name = prepared.tool_name.clone();
    let args = prepared.args.clone();
    let Some(manifest) =
        super::preparation::resolve_callable_manifest_by_id(context, &prepared.tool_id)
    else {
        return launch_done(outcome(
            prepared_tool_name,
            args,
            runtime_failure(
                ToolFailureClass::Unavailable,
                "tool_unavailable",
                "Tool is unavailable in this session",
            ),
            0,
        ));
    };
    let tool_name = manifest.name.clone();

    let tool_start = context.clock.now();
    let tool_context = tool_context.with_prepared_payload(prepared.prepared_payload.clone());
    let completion_context = tool_context.clone();
    let attempt_result = execute_leaf_tool_attempt(
        context,
        &manifest,
        &prepared,
        tool_context,
        attempt,
        max_attempts,
    )
    .await;
    let duration_ms = context.clock.now().duration_since(tool_start).as_millis() as u64;
    let (result, intents) = match attempt_result {
        crate::ToolAttemptResult::Done { result, intents } => {
            (ToolResult::from_output(result.into_output()), intents)
        }
        crate::ToolAttemptResult::Pending(pending) => {
            let key = match completion_context.take_completion_key() {
                Some(key) => key,
                None => {
                    return launch_done(outcome(
                        tool_name,
                        args,
                        runtime_failure(
                            ToolFailureClass::Internal,
                            "pending_tool_missing_completion_key",
                            "tool returned Pending without first obtaining a completion key",
                        ),
                        duration_ms,
                    ));
                }
            };
            return ToolCallLaunch::Pending(Box::new(PendingToolDispatchOutcome {
                tool_name,
                args,
                key,
                pending,
                duration_ms,
                attempts: Vec::new(),
            }));
        }
    };

    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;

    let mut outcome = outcome(tool_name, args, result, duration_ms);
    outcome.intents = intents;
    launch_done(outcome)
}

pub(super) async fn dispatch_granted_prepared_tool_attempt_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    grant: &crate::ToolExecutionGrant,
    prepared: PreparedToolCall,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let tool_name = grant.manifest.name.clone();
    let args = prepared.args.clone();
    if prepared.tool_id != grant.manifest.id {
        return launch_done(outcome(
            tool_name,
            args,
            runtime_failure(
                ToolFailureClass::Internal,
                "granted_tool_id_mismatch",
                format!(
                    "Prepared granted tool id `{}` does not match grant id `{}`",
                    prepared.tool_id, grant.manifest.id
                ),
            ),
            0,
        ));
    }

    let tool_start = context.clock.now();
    let tool_context = tool_context
        .with_prepared_payload(prepared.prepared_payload.clone())
        .with_tool_execution_binding(grant.execution_binding.clone());
    let completion_context = tool_context.clone();
    let attempt_result = execute_granted_leaf_tool_attempt(
        context,
        grant,
        &prepared,
        tool_context,
        attempt,
        max_attempts,
    )
    .await;
    let duration_ms = context.clock.now().duration_since(tool_start).as_millis() as u64;
    let (result, intents) = match attempt_result {
        crate::ToolAttemptResult::Done { result, intents } => {
            (ToolResult::from_output(result.into_output()), intents)
        }
        crate::ToolAttemptResult::Pending(pending) => {
            let key = match completion_context.take_completion_key() {
                Some(key) => key,
                None => {
                    return launch_done(outcome(
                        tool_name,
                        args,
                        runtime_failure(
                            ToolFailureClass::Internal,
                            "pending_tool_missing_completion_key",
                            "tool returned Pending without first obtaining a completion key",
                        ),
                        duration_ms,
                    ));
                }
            };
            return ToolCallLaunch::Pending(Box::new(PendingToolDispatchOutcome {
                tool_name,
                args,
                key,
                pending,
                duration_ms,
                attempts: Vec::new(),
            }));
        }
    };

    let result = finalize_tool_result_with_execution_context(
        context,
        &tool_name,
        &args,
        result,
        duration_ms,
    )
    .await;

    let mut outcome = outcome(tool_name, args, result, duration_ms);
    outcome.intents = intents;
    launch_done(outcome)
}

pub(crate) async fn execute_prepared_tool_attempt_effect<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    attempt: u32,
    max_attempts: u32,
    tool_context: ToolContext<'run>,
) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
    let call_id = prepared.call_id.clone();
    let launch = if let Some(grant) = execution_grant.as_ref() {
        Box::pin(
            dispatch_granted_prepared_tool_attempt_launch_with_execution_context(
                context,
                grant,
                prepared,
                attempt,
                max_attempts,
                tool_context,
            ),
        )
        .await
    } else {
        Box::pin(
            dispatch_prepared_tool_attempt_launch_with_execution_context(
                context,
                prepared,
                attempt,
                max_attempts,
                tool_context,
            ),
        )
        .await
    };
    let launch = match launch {
        ToolCallLaunch::Done(outcome) => {
            let mut record = outcome.record;
            record.call_id = Some(call_id);
            crate::ToolAttemptLaunch::Done {
                record: Box::new(record),
                intents: outcome.intents,
            }
        }
        ToolCallLaunch::Pending(pending) => crate::ToolAttemptLaunch::Pending {
            key: Box::new(pending.key),
            pending: pending.pending,
            duration_ms: pending.duration_ms,
        },
    };
    let triggers = context.trigger_outcomes.drain();
    Ok(crate::ToolAttemptEffectOutcome { launch, triggers })
}

pub(crate) async fn finalize_tool_result_with_execution_context(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    args: &serde_json::Value,
    result: ToolResult,
    duration_ms: u64,
) -> ToolResult {
    match context
        .plugins
        .after_tool_call(ToolResultHookContext::new(
            context.session_id.clone(),
            tool_name.to_string(),
            args.clone(),
            result.clone(),
            duration_ms,
            context.turn_context.clone(),
            Arc::clone(&context.sessions),
        ))
        .await
    {
        Ok(directives) => apply_after_tool_directives(context, result, directives).await,
        Err(err) => runtime_failure(
            ToolFailureClass::Internal,
            "after_tool_call_failed",
            err.to_string(),
        ),
    }
}

#[cfg(test)]
trait ToolCallLaunchExt {
    fn into_done_or_runtime_failure(self) -> ToolDispatchOutcome;
}

#[cfg(test)]
impl ToolCallLaunchExt for ToolCallLaunch {
    fn into_done_or_runtime_failure(self) -> ToolDispatchOutcome {
        match self {
            ToolCallLaunch::Done(outcome) => *outcome,
            ToolCallLaunch::Pending(pending) => outcome(
                pending.tool_name,
                pending.args,
                runtime_failure(
                    ToolFailureClass::Internal,
                    "pending_tool_not_supported_here",
                    "pending tool completion is not supported on this dispatch path",
                ),
                pending.duration_ms,
            ),
        }
    }
}
