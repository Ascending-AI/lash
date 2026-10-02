use crate::projection::{
    flow_record_to_json_value, flow_record_to_tool_args, flow_to_json_value, projected_index,
};
use lash_lashlang_runtime::ToolDefinitionBindingExt;
use lash_rlm_types::PROJECTED_JSON_TAG;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, ProjectedBindings, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, Record as FlowRecord, Value as FlowValue,
};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
mod step_trace;
use super::*;
use lash_core::facade_support::TraceSink;
use std::sync::Mutex;

mod cell_segment_handover;
mod deferred_and_processes;
mod frame_referrers;
mod lifecycle_and_diagnostics;
mod one_slot_process_await;
mod output_retention;
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
    execution_trace: Option<lash_core::plugin::PluginExecutionTrace>,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
) -> ExecResponse {
    crate::testing::execute_code_with_channel_and_bounds(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        execution_trace,
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
    execution_trace: Option<lash_core::plugin::PluginExecutionTrace>,
    execution_bounds: lashlang::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
    code_renderer: crate::render::CodeRendererSlot,
) -> ExecResponse {
    let ctx = match execution_trace {
        Some(trace) => ctx.with_trace_standing(trace.into_standing()),
        None => ctx,
    };
    super::execute_code_with_channel_and_bounds_with_trigger_resolver(
        &crate::dialect::TypescriptDialect,
        state,
        test_render_context(ctx),
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
    execution_trace: Option<lash_core::plugin::PluginExecutionTrace>,
) -> ExecResponse {
    crate::testing::execute_code_unbounded_for_tests(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        execution_trace,
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
    execution_trace: Option<lash_core::plugin::PluginExecutionTrace>,
    execution_bounds: lashlang::ExecutionBounds,
) -> ExecResponse {
    crate::testing::execute_code_with_bounds(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lashlang_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        execution_trace,
        execution_bounds,
    )
    .await
}

use deferred_and_processes::*;
use lifecycle_and_diagnostics::*;

fn test_trace(sink: Arc<dyn TraceSink>) -> lash_core::plugin::PluginExecutionTrace {
    test_trace_with_clock(sink, Arc::new(lash_core::facade_support::SystemClock))
}

fn test_trace_with_clock(
    sink: Arc<dyn TraceSink>,
    clock: Arc<dyn lash_core::Clock>,
) -> lash_core::plugin::PluginExecutionTrace {
    struct External(Arc<dyn TraceSink>);
    impl TraceSink for External {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            if matches!(
                record.event,
                lash_trace::TraceEvent::LanguageExecution { .. }
            ) {
                return Ok(());
            }
            self.0.append(record)
        }
    }
    let runtime = lash_core::trace::TraceRuntime::new(clock)
        .with_trace_sink(Arc::new(External(sink.clone())))
        .with_product_observer(sink);
    lash_core::plugin::PluginExecutionTrace::new(runtime.unreplayed(None))
}
