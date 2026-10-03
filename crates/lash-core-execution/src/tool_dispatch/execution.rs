use std::sync::Arc;

use crate::plugin::ToolResultHookContext;
#[cfg(any(test, feature = "testing"))]
use crate::{PreparedToolCall, ToolContext};
use crate::{ToolFailureClass, ToolOutcome};

#[cfg(any(test, feature = "testing"))]
use super::context::ToolCallIds;
#[cfg(any(test, feature = "testing"))]
use super::context::{ToolCallLaunch, ToolDispatchOutcome};
use super::context::{ToolDispatchContext, runtime_failure};
use super::directives::apply_after_tool_directives;
#[cfg(any(test, feature = "testing"))]
use super::retry::normalized_outcome;

#[cfg(any(test, feature = "testing"))]
pub(crate) async fn dispatch_prepared_tool_call_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let ids = ToolCallIds::of(&prepared);
    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        context,
        prepared,
        None,
        tool_context,
    )
    .await;
    tool_call_launch_into_done_or_runtime_failure(context, &ids, launch).await
}

#[cfg(any(test, feature = "testing"))]
pub async fn coordinate_prepared_tool_call_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let retry_policy =
        super::retry::resolve_retry_policy(context, &prepared.tool_id, execution_grant.as_deref());
    let turn_cancel_wait = Box::new(
        context.effect_controller.turn_cancel_wait(
            tool_context
                .cancellation_token()
                .cloned()
                .unwrap_or_default(),
        ),
    );
    let dispatch = Arc::new(context.clone());
    Box::pin(super::coordinate_tool_invocation(
        context,
        prepared,
        execution_grant,
        retry_policy,
        None,
        super::ToolAttemptLineage::from_parent(context.parent_invocation.clone()),
        turn_cancel_wait.as_ref(),
        None,
        |completion_key| {
            crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                Arc::clone(&dispatch),
                tool_context.clone(),
                completion_key,
            )
        },
    ))
    .await
    .launch
}

pub async fn finalize_tool_result_with_execution_context(
    context: &ToolDispatchContext<'_>,
    call_id: &lash_sansio::ToolCallId,
    tool_name: &str,
    args: &serde_json::Value,
    result: ToolOutcome,
    duration_ms: u64,
) -> ToolOutcome {
    match context
        .plugins
        .after_tool_call(ToolResultHookContext::new(
            context.owner.runtime_owner(),
            context.plugins.admitted_plugin_config(),
            call_id.clone(),
            tool_name.to_string(),
            args.clone(),
            result.clone(),
            duration_ms,
            context.turn_context.clone(),
            Arc::clone(&context.sessions),
        ))
        .await
    {
        Ok(directives) => Box::pin(apply_after_tool_directives(context, result, directives)).await,
        Err(err) => runtime_failure(
            ToolFailureClass::Internal,
            "after_tool_call_failed",
            err.to_string(),
        ),
    }
}

#[cfg(any(test, feature = "testing"))]
async fn tool_call_launch_into_done_or_runtime_failure(
    context: &ToolDispatchContext<'_>,
    ids: &ToolCallIds,
    launch: ToolCallLaunch,
) -> ToolDispatchOutcome {
    match launch {
        ToolCallLaunch::Done(outcome) => *outcome,
        ToolCallLaunch::Pending(pending) => {
            normalized_outcome(
                context,
                ids,
                pending.tool_name,
                pending.args,
                runtime_failure(
                    ToolFailureClass::Internal,
                    "pending_tool_not_supported_here",
                    "pending tool completion is not supported on this dispatch path",
                ),
            )
            .await
        }
        ToolCallLaunch::ControllerAborted(error) => {
            normalized_outcome(
                context,
                ids,
                "runtime_effect_controller".to_string(),
                serde_json::Value::Null,
                runtime_failure(
                    ToolFailureClass::Internal,
                    error.code.as_str(),
                    error.message,
                ),
            )
            .await
        }
    }
}
