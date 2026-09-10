use crate::projection::{
    ProjectionRef, ProjectionRegistry, flow_record_to_json_value, flow_record_to_tool_args,
    flow_to_json_value, projected_index,
};
use lash_core::{ProcessObserverRegistry as _, ProcessQuery as _};
use lash_lashlang_runtime::ToolDefinitionBindingExt;
use lash_rlm_types::PROJECTED_JSON_TAG;
use lash_sansio::sync::MutexExt;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionOutcome, ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor,
    ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record as FlowRecord,
    Value as FlowValue,
};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
mod step_trace;
use super::*;
use std::sync::Mutex;

mod deferred_and_processes;
mod lifecycle_and_diagnostics;
mod projections_and_snapshots;
mod triggers;
mod typescript_cells;

use deferred_and_processes::*;
use lifecycle_and_diagnostics::*;
