//! Logical tool ownership around a standalone cell entry.
use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_code_with_channel_and_bounds_with_trigger_resolver(
    dialect: &dyn crate::dialect::Dialect,
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    deferred_trigger_resolver: Option<lash_lashlang_runtime::SharedDeferredTriggerResolver>,
    session_projected_bindings: RlmProjectedBindings,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
    code_renderer: crate::render::CodeRendererSlot,
) -> impl std::future::Future<Output = ExecResponse> {
    Box::pin(async move {
        if ctx.has_tool_run_owner() {
            return execute_owned_code(
                dialect,
                state,
                ctx,
                request,
                artifact_store,
                lashlang_surface,
                deferred_tool_resolver,
                deferred_trigger_resolver,
                session_projected_bindings,
                execution_bounds,
                channel,
                code_renderer,
            )
            .await;
        }
        let owner = ctx.clone();
        match owner
            .drive_tool_run(None, |ctx| async move {
                let closing = ctx.clone();
                let mut response = execute_owned_code(
                    dialect,
                    state,
                    ctx,
                    request,
                    artifact_store,
                    lashlang_surface,
                    deferred_tool_resolver,
                    deferred_trigger_resolver,
                    session_projected_bindings,
                    execution_bounds,
                    channel,
                    code_renderer,
                )
                .await;
                if !response.suspended
                    && !closing.has_nested_effect_error()
                    && let Err(error) = closing.close_opener_groups().await
                {
                    fail_cell_on_nested_error(&closing, &mut response, error);
                }
                response
            })
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let mut response = exec_setup_failure(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    error.to_string(),
                ));
                fail_cell_on_nested_error(&owner, &mut response, error);
                response
            }
        }
    })
}
