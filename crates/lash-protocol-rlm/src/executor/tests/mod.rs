use crate::projection::{flow_record_to_tool_args, flow_to_json_value, projected_index};
use lash_rlm_types::PROJECTED_JSON_TAG;
use lash_sansio::sync::MutexExt;
use lash_vm::{
    AbilityOp, AbilityOutcome, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, ProjectedBindings, ProjectedReadRequest, ProjectedReadResponse,
    ProjectedValue, Record as FlowRecord, Value as FlowValue,
};
use lash_vm_runtime::ToolDefinitionBindingExt;
use std::sync::atomic::{AtomicUsize, Ordering};
mod step_trace;
use super::*;
use lash_core::facade_support::TraceSink;
use std::sync::Mutex;

mod deferred_and_processes;
mod frame_referrers;
mod lifecycle_and_diagnostics;
mod observations;
mod one_slot_process_await;
mod output_retention;
mod projections_and_snapshots;
mod typescript_cells;

fn test_render_context(ctx: RuntimeExecutionContext<'_>) -> RuntimeExecutionContext<'_> {
    ctx.with_recorded_render(crate::testing::recorded_test_render())
}

#[allow(clippy::too_many_arguments)]
async fn execute_code_with_test_render(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lash_vm::LashVmArtifacts,
    lash_vm_surface: LashVmSurface,
    deferred_tool_resolver: Option<lash_vm_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    execution_trace: Option<lash_core::plugin::PluginExecutionTrace>,
    execution_bounds: lash_vm::ExecutionBounds,
    channel: crate::plugin::RlmChannel,
) -> ExecResponse {
    crate::testing::execute_code_with_channel_and_bounds(
        state,
        test_render_context(ctx),
        request,
        artifact_store,
        lash_vm_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        execution_trace,
        execution_bounds,
        channel,
        crate::render::CodeRendererSlot::default(),
    )
    .await
}

/// [`execute_code_with_test_render`] with no execution bounds, on the cell
/// channel.
#[allow(clippy::too_many_arguments)]
async fn execute_code_unbounded_with_test_render(
    state: &mut RlmExecutionState,
    ctx: RuntimeExecutionContext<'_>,
    request: ExecRequest,
    artifact_store: lash_vm::LashVmArtifacts,
    lash_vm_surface: LashVmSurface,
    deferred_tool_resolver: Option<lash_vm_runtime::SharedDeferredToolResolver>,
    session_projected_bindings: RlmProjectedBindings,
    execution_trace: Option<lash_core::plugin::PluginExecutionTrace>,
) -> ExecResponse {
    execute_code_with_test_render(
        state,
        ctx,
        request,
        artifact_store,
        lash_vm_surface,
        deferred_tool_resolver,
        session_projected_bindings,
        execution_trace,
        lash_vm::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
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
        .with_content(lash_trace::TelemetryContent::Captured)
        .with_product_observer(sink);
    lash_core::plugin::PluginExecutionTrace::new(runtime.unreplayed(None))
}
