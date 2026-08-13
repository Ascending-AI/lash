use std::sync::Arc;

#[cfg(test)]
use crate::ToolContext;
use crate::plugin::ToolCallHookContext;
use crate::validate_tool_input;
use crate::{
    ToolExecutionGrant, ToolFailureClass, ToolManifest, ToolPrepareCall, ToolPrepareContext,
};

#[cfg(test)]
use super::context::ToolDispatchOutcome;
use super::context::{
    ToolDispatchContext, ToolPreparationOutcome, completed_preparation, outcome, runtime_failure,
};
use super::directives::apply_before_tool_directives;
#[cfg(test)]
use super::execution::dispatch_prepared_tool_call_with_execution_context;

#[cfg(test)]
pub(crate) async fn dispatch_tool_call(
    context: &ToolDispatchContext<'_>,
    tool_name: String,
    args: serde_json::Value,
) -> ToolDispatchOutcome {
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone())).build();
    Box::pin(dispatch_tool_call_with_execution_context(
        context,
        tool_name,
        args,
        tool_context,
    ))
    .await
}

#[cfg(test)]
pub(crate) async fn dispatch_tool_call_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    tool_name: String,
    args: serde_json::Value,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let pending = crate::sansio::PendingToolCall {
        call_id: tool_context
            .tool_call_id()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("tool:{}", uuid::Uuid::new_v4())),
        tool_name,
        args,
        replay: None,
    };
    match prepare_tool_call_with_context(
        context,
        pending,
        tool_context.tool_call_id().map(str::to_string),
    )
    .await
    {
        ToolPreparationOutcome::Prepared(prepared) => {
            Box::pin(dispatch_prepared_tool_call_with_execution_context(
                context,
                prepared,
                tool_context,
            ))
            .await
        }
        ToolPreparationOutcome::Completed(outcome) => *outcome,
    }
}

pub(crate) async fn prepare_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    pending: crate::sansio::PendingToolCall,
    tool_call_id: Option<String>,
) -> ToolPreparationOutcome {
    let tool_name = pending.tool_name.clone();
    let Some(manifest) = resolve_callable_manifest(context, &tool_name) else {
        return completed_preparation(outcome(
            tool_name,
            pending.args,
            runtime_failure(
                ToolFailureClass::Unavailable,
                "tool_unavailable",
                "Tool is unavailable in this session",
            ),
            0,
        ));
    };
    let Some(contract) = context.tools.resolve_contract(&tool_name) else {
        return completed_preparation(outcome(
            tool_name,
            pending.args,
            runtime_failure(
                ToolFailureClass::Unavailable,
                "tool_contract_unavailable",
                "Tool contract is unavailable in this session",
            ),
            0,
        ));
    };
    prepare_authorized_tool_call_with_context(
        context,
        manifest,
        contract,
        pending,
        tool_call_id,
        None,
    )
    .await
}

pub(crate) async fn prepare_granted_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    grant: &ToolExecutionGrant,
    mut pending: crate::sansio::PendingToolCall,
    tool_call_id: Option<String>,
) -> ToolPreparationOutcome {
    pending.tool_name = grant.manifest.name.clone();
    prepare_authorized_tool_call_with_context(
        context,
        grant.manifest.clone(),
        Arc::new((*grant.contract).clone()),
        pending,
        tool_call_id,
        Some(grant),
    )
    .await
}

async fn prepare_authorized_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    manifest: ToolManifest,
    contract: Arc<crate::ToolContract>,
    pending: crate::sansio::PendingToolCall,
    tool_call_id: Option<String>,
    grant: Option<&ToolExecutionGrant>,
) -> ToolPreparationOutcome {
    let tool_name = manifest.name.clone();
    let mut pending = pending;
    let mut args = pending.args;

    let directives = match context
        .plugins
        .before_tool_call(ToolCallHookContext::new(
            context.session_id.clone(),
            tool_name.clone(),
            args.clone(),
            manifest.argument_projection.clone(),
            context.turn_context.clone(),
            Arc::clone(&context.sessions),
        ))
        .await
    {
        Ok(directives) => directives,
        Err(err) => {
            return completed_preparation(outcome(
                tool_name,
                args,
                runtime_failure(
                    ToolFailureClass::Internal,
                    "before_tool_call_failed",
                    err.to_string(),
                ),
                0,
            ));
        }
    };

    let applied = apply_before_tool_directives(context, args, directives).await;
    args = applied.args;
    if let Some(result) = applied.short_circuit {
        return completed_preparation(outcome(tool_name, args, result, 0));
    }
    if let Err(err) = validate_tool_input(&contract, &args) {
        return completed_preparation(outcome(
            tool_name,
            args,
            runtime_failure(ToolFailureClass::InvalidRequest, "invalid_tool_args", err),
            0,
        ));
    }

    pending.args = args.clone();
    let execution_binding = grant
        .map(|grant| grant.execution_binding.clone())
        .unwrap_or(serde_json::Value::Null);
    let prepare_context = ToolPrepareContext::with_execution_binding(
        context.session_id.clone(),
        Arc::clone(&context.sessions),
        context.turn_context.clone(),
        tool_call_id,
        execution_binding,
    );
    let prepare_call = ToolPrepareCall {
        tool_id: manifest.id.clone(),
        pending,
        context: &prepare_context,
    };
    let prepared = if let Some(grant) = grant {
        context
            .tools
            .prepare_granted_tool_call(grant, prepare_call)
            .await
    } else {
        context.tools.prepare_tool_call(prepare_call).await
    };
    match prepared {
        Ok(prepared) if prepared.tool_id == manifest.id => {
            ToolPreparationOutcome::Prepared(prepared)
        }
        Ok(prepared) => completed_preparation(outcome(
            tool_name,
            args,
            runtime_failure(
                ToolFailureClass::Internal,
                "prepared_tool_id_mismatch",
                format!(
                    "Tool provider prepared id `{}` for tool `{}`, expected `{}`",
                    prepared.tool_id, prepared.tool_name, manifest.id
                ),
            ),
            0,
        )),
        Err(result) => completed_preparation(outcome(tool_name, args, result, 0)),
    }
}

pub(crate) fn resolve_callable_manifest(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
) -> Option<ToolManifest> {
    // Tool Catalog membership is callability: a catalog member is callable.
    if let Some(entry) = context.tool_catalog.tools.iter().find(|tool| {
        tool.manifest.name == tool_name
            && tool.manifest.activation != crate::ToolActivation::Internal
    }) {
        return Some(entry.manifest.clone());
    }
    None
}

pub(crate) fn resolve_callable_manifest_by_id(
    context: &ToolDispatchContext<'_>,
    tool_id: &crate::ToolId,
) -> Option<ToolManifest> {
    if let Some(entry) = context.tool_catalog.tools.iter().find(|tool| {
        tool.manifest.id == *tool_id && tool.manifest.activation != crate::ToolActivation::Internal
    }) {
        return Some(entry.manifest.clone());
    }
    None
}

pub(crate) fn resolve_internal_manifest_by_id(
    context: &ToolDispatchContext<'_>,
    tool_id: &crate::ToolId,
) -> Option<ToolManifest> {
    context
        .tool_catalog
        .tools
        .iter()
        .find(|tool| {
            tool.manifest.id == *tool_id
                && tool.manifest.activation == crate::ToolActivation::Internal
        })
        .map(|entry| entry.manifest.clone())
}

#[cfg(test)]
pub(crate) fn resolve_tool_argument_projection_policy(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
) -> crate::ToolArgumentProjectionPolicy {
    context
        .tool_catalog
        .tools
        .iter()
        .find(|def| def.manifest.name == tool_name)
        .map(|def| def.manifest.argument_projection.clone())
        .unwrap_or_default()
}
