use std::sync::Arc;

#[cfg(any(test, feature = "testing"))]
use crate::ToolContext;
use crate::validate_tool_input;
use crate::{
    ToolExecutionGrant, ToolFailureClass, ToolManifest, ToolPrepareCall, ToolPrepareContext,
};

#[cfg(any(test, feature = "testing"))]
use super::context::ToolDispatchOutcome;
use super::context::{
    ToolCallIds, ToolDispatchContext, ToolPreparationOutcome, completed_preparation,
    runtime_failure,
};
#[cfg(any(test, feature = "testing"))]
use super::execution::dispatch_prepared_tool_call_with_execution_context;
use super::retry::normalized_outcome;

/// Dispatches one call on `tool_name` directly: a test's shortcut past the
/// turn that would admit it, named `ToolCallId::fixture("dispatch")`.
#[cfg(any(test, feature = "testing"))]
pub async fn dispatch_tool_call(
    context: &ToolDispatchContext<'_>,
    tool_name: String,
    args: serde_json::Value,
) -> ToolDispatchOutcome {
    let pending = crate::sansio::PendingToolCall {
        call_id: crate::ToolCallId::fixture("dispatch"),
        provider_call_id: None,
        tool_name,
        args,
        replay: None,
    };
    match prepare_tool_call_with_context(context, pending).await {
        ToolPreparationOutcome::Prepared(prepared) => {
            let tool_context =
                ToolContext::from_dispatch(Arc::new(context.clone()), &prepared).build();
            Box::pin(dispatch_prepared_tool_call_with_execution_context(
                context,
                *prepared,
                tool_context,
            ))
            .await
        }
        ToolPreparationOutcome::Completed(outcome) => *outcome,
    }
}

/// Dispatches `tool_name` as the admitted call `tool_context` runs, under
/// the dispatch state it carries.
#[cfg(any(test, feature = "testing"))]
pub async fn dispatch_tool_call_with_execution_context<'run>(
    context: &ToolDispatchContext<'run>,
    tool_name: String,
    args: serde_json::Value,
    tool_context: ToolContext<'run>,
) -> ToolDispatchOutcome {
    let pending = crate::sansio::PendingToolCall {
        call_id: tool_context.call_id().clone(),
        provider_call_id: None,
        tool_name,
        args,
        replay: None,
    };
    match prepare_tool_call_with_context(context, pending).await {
        ToolPreparationOutcome::Prepared(prepared) => {
            Box::pin(dispatch_prepared_tool_call_with_execution_context(
                context,
                *prepared,
                tool_context,
            ))
            .await
        }
        ToolPreparationOutcome::Completed(outcome) => *outcome,
    }
}

pub async fn prepare_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    pending: crate::sansio::PendingToolCall,
) -> ToolPreparationOutcome {
    let tool_name = pending.tool_name.clone();
    let ids = ToolCallIds::of_pending(&pending);
    let Some(definition) = resolve_callable_definition(context, &tool_name) else {
        return completed_preparation(
            normalized_outcome(
                context,
                &ids,
                tool_name,
                pending.args,
                runtime_failure(
                    ToolFailureClass::Unavailable,
                    "tool_unavailable",
                    "Tool is unavailable in this session",
                ),
            )
            .await,
        );
    };
    prepare_authorized_tool_call_with_context(
        context,
        definition.manifest.clone(),
        Arc::clone(&definition.contract),
        pending,
        ProviderPreparation::Live(None),
    )
    .await
}

fn resolve_callable_definition<'a>(
    context: &'a ToolDispatchContext<'_>,
    tool_name: &str,
) -> Option<&'a crate::ToolCatalogEntry> {
    context
        .tool_catalog
        .tools
        .iter()
        .find(|tool| tool.manifest.name == tool_name)
}

pub async fn prepare_granted_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    grant: &ToolExecutionGrant,
    mut pending: crate::sansio::PendingToolCall,
) -> ToolPreparationOutcome {
    pending.tool_name = grant.manifest().name.clone();
    prepare_authorized_tool_call_with_context(
        context,
        grant.manifest().clone(),
        Arc::new(grant.contract().clone()),
        pending,
        ProviderPreparation::Live(Some(grant)),
    )
    .await
}

/// Prepares a call a replayed code cell makes on a host tool binding that
/// drifted since the pass that wrote its journal (FIG-3587): against the
/// binding's recorded manifest and contract, with identity preparation, so
/// the call's envelope is the one the journal recorded and it is served from
/// there without the live tool being consulted.
///
/// A tool whose recorded declaration may defer is the exception: its
/// preparation seals the declared start its recorded attempt carries
/// (ADR 0116 §4), so its live provider prepares it. The answer is the
/// recorded binding's, never the drifted live tool's.
pub async fn prepare_recorded_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    binding: &crate::ToolDefinition,
    mut pending: crate::sansio::PendingToolCall,
) -> ToolPreparationOutcome {
    pending.tool_name = binding.manifest.name.clone();
    let preparation = if binding.manifest.declaration.may_defer {
        ProviderPreparation::Live(None)
    } else {
        ProviderPreparation::Recorded
    };
    prepare_authorized_tool_call_with_context(
        context,
        binding.manifest.clone(),
        Arc::new(binding.contract.clone()),
        pending,
        preparation,
    )
    .await
}

/// Who prepares an authorized call once its arguments are validated.
enum ProviderPreparation<'grant> {
    /// The live provider, under a grant's route when the call carries one.
    Live(Option<&'grant ToolExecutionGrant>),
    /// No provider: a recorded binding is prepared as its identity.
    Recorded,
}

/// Admits one call (ADR 0128): argument transforms chain, the result is
/// validated, the provider prepares the call and its identity is checked,
/// then every before-check inspects that one sealed call. No argument
/// changes after the checks.
async fn prepare_authorized_tool_call_with_context(
    context: &ToolDispatchContext<'_>,
    manifest: ToolManifest,
    contract: Arc<crate::ToolContract>,
    pending: crate::sansio::PendingToolCall,
    preparation: ProviderPreparation<'_>,
) -> ToolPreparationOutcome {
    let tool_name = manifest.name.clone();
    let ids = ToolCallIds::of_pending(&pending);
    // Admission precedes every hook and the provider's own preparation: a
    // refused call runs no callback of any kind.
    if let Err(refusal) = super::admission::admit_tool(&manifest) {
        let failure = super::admission::admission_failure(&tool_name, refusal);
        return completed_preparation(
            normalized_outcome(
                context,
                &ids,
                tool_name,
                pending.args,
                crate::ToolOutcome::failure(failure),
            )
            .await,
        );
    }
    let mut pending = pending;
    let hook_context = super::hooks::hook_context(
        context,
        &pending.call_id,
        &manifest.id,
        &tool_name,
        manifest.argument_projection.clone(),
    );
    let original_args = Arc::new(std::mem::take(&mut pending.args));
    let args = match context
        .plugins
        .transform_tool_args(&hook_context, (*original_args).clone())
        .await
    {
        Ok(args) => args,
        Err(failure) => {
            return completed_preparation(
                normalized_outcome(
                    context,
                    &ids,
                    tool_name,
                    (*original_args).clone(),
                    crate::ToolOutcome::failure(*failure),
                )
                .await,
            );
        }
    };
    if let Err(result) = validate_args(&contract, &args, "invalid_tool_args") {
        return completed_preparation(
            normalized_outcome(context, &ids, tool_name, args, result).await,
        );
    }

    pending.args = args.clone();
    let prepared = match preparation {
        ProviderPreparation::Live(grant) => {
            match prepare_with_provider(context, &manifest, &ids, grant, pending).await {
                Ok(prepared) => prepared,
                Err(result) => {
                    return completed_preparation(
                        normalized_outcome(context, &ids, tool_name, args, result).await,
                    );
                }
            }
        }
        ProviderPreparation::Recorded => {
            crate::PreparedToolCall::identity(manifest.id.clone(), pending)
        }
    };
    if prepared.args != args
        && let Err(result) = validate_args(&contract, &prepared.args, "invalid_prepared_tool_args")
    {
        return completed_preparation(
            normalized_outcome(context, &ids, tool_name, args, result).await,
        );
    }

    let prepared = crate::plugin::PreparedCallReadView::new(prepared);
    match super::hooks::check_prepared_call(context, &hook_context, &original_args, &prepared).await
    {
        crate::plugin::BeforeSelection::Execute => {
            ToolPreparationOutcome::Prepared(Box::new(prepared.into_prepared()))
        }
        crate::plugin::BeforeSelection::Cached(candidate) => {
            let args = prepared.args().clone();
            let result = super::finalize_tool_result_with_execution_context(
                context,
                &prepared,
                crate::plugin::ToolHookOccurrence::Cached,
                crate::ToolOutcome::from_output(candidate.into_output(None)),
            )
            .await;
            completed_preparation(normalized_outcome(context, &ids, tool_name, args, result).await)
        }
        crate::plugin::BeforeSelection::Terminal(output) => {
            let args = prepared.args().clone();
            completed_preparation(
                normalized_outcome(
                    context,
                    &ids,
                    tool_name,
                    args,
                    crate::ToolOutcome::from_output(output),
                )
                .await,
            )
        }
    }
}

fn validate_args(
    contract: &crate::ToolContract,
    args: &serde_json::Value,
    code: &'static str,
) -> Result<(), crate::ToolOutcome> {
    validate_tool_input(contract, args).map_err(|err| {
        crate::ToolOutcome::failure(
            crate::ToolFailure::runtime(ToolFailureClass::InvalidRequest, code, err.to_string())
                .with_cause(crate::ToolFailureCause::ValueMismatch { source: err }),
        )
    })
}

/// The bound provider's preparation of `pending`, whose identity is fixed:
/// a preparation that names another call or tool fails the call.
async fn prepare_with_provider(
    context: &ToolDispatchContext<'_>,
    manifest: &ToolManifest,
    ids: &ToolCallIds,
    grant: Option<&ToolExecutionGrant>,
    pending: crate::sansio::PendingToolCall,
) -> Result<crate::PreparedToolCall, crate::ToolOutcome> {
    let execution_binding = grant
        .map(|grant| grant.execution_binding.clone())
        .unwrap_or(serde_json::Value::Null);
    let prepare_context = ToolPrepareContext::with_execution_binding(
        context.owner.runtime_owner(),
        Arc::clone(&context.sessions),
        context.turn_context.clone(),
        pending.call_id.clone(),
        execution_binding,
    )
    .with_dispatch_catalog(Arc::clone(&context.tool_catalog))
    .with_process_originator(context.process_originator.clone());
    let prepare_context = match grant {
        Some(grant) => prepare_context.with_granted_source_id(grant.source_id.clone()),
        None => prepare_context,
    };
    let prepare_call = ToolPrepareCall {
        tool_id: manifest.id.clone(),
        pending,
        context: &prepare_context,
    };
    let prepared = context.tools.prepare_tool_call(prepare_call).await?;
    if prepared.call_id != ids.call_id || prepared.provider_call_id != ids.provider_call_id {
        return Err(runtime_failure(
            ToolFailureClass::Internal,
            "prepared_call_id_mismatch",
            format!(
                "Tool provider prepared call `{}` for admitted call `{}`: a call's identity is fixed at admission",
                prepared.call_id, ids.call_id
            ),
        ));
    }
    if prepared.tool_id != manifest.id {
        return Err(runtime_failure(
            ToolFailureClass::Internal,
            "prepared_tool_id_mismatch",
            format!(
                "Tool provider prepared id `{}` for tool `{}`, expected `{}`",
                prepared.tool_id, prepared.tool_name, manifest.id
            ),
        ));
    }
    if prepared.tool_name != manifest.name {
        return Err(runtime_failure(
            ToolFailureClass::Internal,
            "prepared_tool_name_mismatch",
            format!(
                "Tool provider prepared name `{}` for tool `{}`, expected `{}`",
                prepared.tool_name, prepared.tool_id, manifest.name
            ),
        ));
    }
    Ok(prepared)
}

pub fn resolve_callable_manifest(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
) -> Option<ToolManifest> {
    // Tool Catalog membership is callability: a catalog member is callable.
    resolve_callable_definition(context, tool_name).map(|entry| entry.manifest.clone())
}

pub fn resolve_callable_manifest_by_id(
    context: &ToolDispatchContext<'_>,
    tool_id: &crate::ToolId,
) -> Option<ToolManifest> {
    context
        .tool_catalog
        .tools
        .iter()
        .find(|tool| tool.manifest.id == *tool_id)
        .map(|entry| entry.manifest.clone())
}

#[cfg(any(test, feature = "testing"))]
pub fn resolve_tool_argument_projection_policy(
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
