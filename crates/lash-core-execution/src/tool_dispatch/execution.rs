//! The test dispatch path: one prepared call coordinated outside a turn.

use std::sync::Arc;

use crate::ToolFailureClass;
use crate::{PreparedToolCall, ToolContext};

use super::context::{
    ToolCallIds, ToolCallLaunch, ToolDispatchContext, ToolDispatchOutcome, runtime_failure,
};
use super::retry::normalized_outcome;

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

pub async fn coordinate_prepared_tool_call_launch_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: PreparedToolCall,
    execution_grant: Option<Box<crate::ToolExecutionGrant>>,
    tool_context: ToolContext<'run>,
) -> ToolCallLaunch {
    let execution_policy = super::retry::resolve_execution_policy(
        context,
        &prepared.tool_id,
        execution_grant.as_deref(),
    );
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
        execution_policy,
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
