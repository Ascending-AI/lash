//! Lashlang runtime: bytecode `Compiler`, executor `Vm`, value system,
//! plus the long tail of free helpers (ops/format/json/access).
//!
//! `mod.rs` only owns the cross-cutting types (`RuntimeError`,
//! `RuntimeFailure`, `ExecutionScratch`, `ExecutionOutcome`,
//! `CompiledProgram`, `ProfileReport` + friends) and the `pub use` /
//! `pub(crate) use` wiring that re-exports each focused submodule's items
//! both publicly (for `lashlang::lib.rs`) and crate-internally (so
//! sibling submodules can write `use super::*` without caring which
//! file an item lives in).

use crate::lexer::Span;
use thiserror::Error;

mod access;
mod cache;
mod compiler;
pub(crate) use compiler::is_pure_expr;
mod entry_points;
mod error;
pub use error::{FormatError, RuntimeError};
mod format;
mod host;
mod instruction;
mod json;
mod ops;
mod projector;
mod record;
mod schema;
mod state;
mod value;
mod vm;

pub use cache::{
    CompiledLinkedProgram, CompiledProcessCache, CompiledProcessCacheKey, CompiledProgramCache,
    CompiledProgramCacheStats, LinkedProgramCache, LinkedProgramCacheError,
};
#[allow(unused_imports)]
pub(crate) use compiler::*;
pub use entry_points::{
    ExecutableProgram, compile, compile_linked, compile_linked_process,
    compile_module_artifact_process, compile_process, execute, prewarm,
};
pub use host::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionMode, ProcessEvent, ProcessEventKind, ProcessSignal, ProcessStart,
    ResourceOperation, ResourceOperationBatch, ResourceOperationBatchResult,
    ResourceOperationResult, Sleep, SleepKind,
};
#[allow(unused_imports)]
pub(crate) use instruction::*;
pub use json::from_json;
pub use projector::{
    BudgetedJsonProjectionConfig, BudgetedJsonProjector, ValueProjectionContext, ValueProjector,
};
pub use record::Record;
#[allow(unused_imports)]
pub(crate) use record::{Symbol, intern_symbol, lookup_symbol, record_with_capacity, symbol_name};
#[allow(unused_imports)]
pub(crate) use schema::{
    ValidationPlan, compile_schema_value, execute_validate_builtin, execute_validation_plan,
};
pub(crate) use vm::SlotState;
#[allow(unused_imports)]
pub use vm::{
    ContinuationError, Vm, VmContinuation, VmIteratorContinuation, VmIteratorCursor,
    VmProfileContinuation, VmRunOutcome,
};
// Re-exports of helpers that live in the focused submodules but need to be
// reachable via `use super::*` from sibling submodules + via `super::name`
// from `vm.rs` / `compiler.rs`. These look "unused" from mod.rs's POV but
// are load-bearing for the rest of the runtime crate.
#[allow(unused_imports)]
pub(crate) use access::*;
#[allow(unused_imports)]
pub(crate) use format::*;
#[allow(unused_imports)]
pub(crate) use json::*;
#[allow(unused_imports)]
pub(crate) use ops::*;
#[doc(hidden)]
pub use state::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, validate_canonical_messagepack_structure,
};
pub use state::{Snapshot, SnapshotDecodeError, State};
pub use value::{
    ImageValue, LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY,
    LASH_HOST_REQUIREMENTS_REF_KEY, LASH_MODULE_REF_KEY, LASH_PROCESS_NAME_KEY,
    LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY, LASH_TYPE_KEY, ListValue, ProjectedBindingError,
    ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, ResourceHandle, Value,
};
use vm::IterState;

#[derive(Clone, Debug, Error, PartialEq)]
#[error("{error}")]
pub struct RuntimeFailure {
    pub error: RuntimeError,
    pub span: Option<Span>,
}

#[derive(Default)]
pub struct ExecutionScratch {
    stack: Vec<Value>,
    iter_stack: Vec<IterState>,
    slot_values: Vec<Option<Value>>,
    assigned_globals: std::collections::BTreeSet<String>,
}

impl ExecutionScratch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns and clears the top-level globals assigned by the last execution.
    ///
    /// The VM records assignment roots while it executes, including nested
    /// path assignments. Callers use this to persist only values that may have
    /// changed without comparing or re-encoding the complete global map.
    pub fn take_assigned_globals(&mut self) -> std::collections::BTreeSet<String> {
        std::mem::take(&mut self.assigned_globals)
    }
}

pub(crate) const COOPERATIVE_YIELD_INSTRUCTION_BUDGET: usize = 1024;

#[derive(Clone)]
pub struct CompiledProgram {
    pub(crate) chunk: Chunk,
    pub(crate) compile_stats: CompileStats,
}

impl std::fmt::Debug for CompiledProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledProgram")
            .field("instruction_count", &self.chunk.code.len())
            .field("compile_stats", &self.compile_stats)
            .finish()
    }
}

impl CompiledProgram {
    pub fn compile_stats(&self) -> &CompileStats {
        &self.compile_stats
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionOutcome {
    Continued,
    Finished(Value),
    Failed(Value),
}

#[derive(Clone, Debug, Default)]
pub struct ProfileReport {
    instruction_stats: Vec<ProfileStat>,
    builtin_stats: Vec<ProfileStat>,
    compile_stats: CompileStats,
}

impl ProfileReport {
    pub fn instruction_stats(&self) -> &[ProfileStat] {
        &self.instruction_stats
    }

    pub fn builtin_stats(&self) -> &[ProfileStat] {
        &self.builtin_stats
    }

    pub fn compile_stats(&self) -> &CompileStats {
        &self.compile_stats
    }

    pub fn merge(&mut self, other: &Self) {
        merge_stats(&mut self.instruction_stats, &other.instruction_stats);
        merge_stats(&mut self.builtin_stats, &other.builtin_stats);
        self.compile_stats.merge(&other.compile_stats);
    }
}

/// Compile-time statistics captured when a program is compiled. Independent
/// of run-time profiling — these counts reflect the shape of the compiled
/// program itself (how many Type literals it contains, how many got
/// const-folded, etc.). Runtime cost of `Type` evaluation appears in the
/// instruction profile under `build_type_ref` / `build_record` / etc.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompileStats {
    pub type_literals_total: u64,
    pub type_literals_const_folded: u64,
    pub type_literals_dynamic: u64,
    pub type_ref_sites: u64,
}

impl CompileStats {
    pub fn merge(&mut self, other: &Self) {
        self.type_literals_total += other.type_literals_total;
        self.type_literals_const_folded += other.type_literals_const_folded;
        self.type_literals_dynamic += other.type_literals_dynamic;
        self.type_ref_sites += other.type_ref_sites;
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProfileStat {
    pub name: &'static str,
    pub count: u64,
    pub total_ns: u128,
}

impl ProfileStat {
    pub fn avg_ns(&self) -> u128 {
        if self.count == 0 {
            0
        } else {
            self.total_ns / self.count as u128
        }
    }
}
/// Unwrap a `Value::Record` that carries the `$lash_type` marker back into the
/// inner JSON-Schema value. Returns `None` when the value is not a wrapped
/// Type literal.
pub fn unwrap_type_value(value: &Value) -> Option<&Value> {
    let record = value.as_record()?;
    if record.len() != 1 {
        return None;
    }
    record.get(LASH_TYPE_KEY)
}

#[cfg(test)]
mod tests;
