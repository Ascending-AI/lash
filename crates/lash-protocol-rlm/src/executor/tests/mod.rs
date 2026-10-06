use crate::projection::{flow_record_to_tool_args, flow_to_json_value, projected_index};
use lash_rlm_types::PROJECTED_JSON_TAG;
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, ProjectedBindings, ProjectedReadRequest, ProjectedReadResponse,
    ProjectedValue, Record as FlowRecord, Value as FlowValue,
};
use std::sync::atomic::{AtomicUsize, Ordering};
mod step_trace;
use super::*;
use lash_core::facade_support::TraceSink;
use std::sync::Mutex;

mod lifecycle_and_diagnostics;
mod projections_and_snapshots;
mod typescript_cells;
mod typescript_runtime_values;

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
