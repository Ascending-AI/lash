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
