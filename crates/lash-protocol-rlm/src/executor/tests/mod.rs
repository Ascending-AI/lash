use crate::projection::{
    flow_record_to_json_value, flow_record_to_tool_args, flow_to_json_value, projected_index,
};
use lash_lashlang_runtime::ToolDefinitionBindingExt;
use lash_rlm_types::PROJECTED_JSON_TAG;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, ProjectedBindings, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, Record as FlowRecord, Value as FlowValue,
};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
mod step_trace;
use super::*;
use std::sync::Mutex;

mod deferred_and_processes;
mod lifecycle_and_diagnostics;
mod per_process_surface;
mod production_map_law;
mod projections_and_snapshots;
mod session_globals_law;
mod triggers;
mod typescript_cells;
mod typescript_runtime_values;

fn test_render_context(ctx: RuntimeExecutionContext<'_>) -> RuntimeExecutionContext<'_> {
    ctx.with_recorded_render(crate::testing::recorded_test_render())
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_with_test_render(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
) -> ExecResponse {
    super::execute_code_with_channel_and_bounds(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        lashlang_execution_trace_config,
        execution_bounds,
        channel,
        crate::render::CodeRendererSlot::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_with_trigger_test_render(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    deferred_trigger_resolver: Option<lash_lashlang_runtime::SharedDeferredTriggerResolver>,
    session_projected_bindings: RlmProjectedBindings,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
    code_renderer: crate::render::CodeRendererSlot,
) -> ExecResponse {
    super::execute_code_with_channel_and_bounds_with_trigger_resolver(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        deferred_trigger_resolver,
        session_projected_bindings,
        lashlang_execution_trace_config,
        execution_bounds,
        channel,
        code_renderer,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_unbounded_with_test_render(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
) -> ExecResponse {
    super::execute_code_unbounded_for_tests(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        lashlang_execution_trace_config,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_with_bounds_test_render(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lashlang::LashlangArtifacts,
    lashlang_surface: LashlangSurface,
    deferred_tool_resolver: Option<lash_lashlang_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    lashlang_execution_trace_config: RlmLashlangExecutionTraceConfig,
    execution_bounds: lashlang::ExecutionBounds,
) -> ExecResponse {
    super::execute_code_with_bounds(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        lashlang_execution_trace_config,
        execution_bounds,
    )
    .await
}

use deferred_and_processes::*;
use lifecycle_and_diagnostics::*;
