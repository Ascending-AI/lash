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

use crate::span::Span;
use thiserror::Error;

pub(crate) const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

mod access;
mod cache;
mod compiler;
pub use compiler::{
    RESOURCE_OPERATION_EXECUTION_SITE_KIND, execution_site_descriptor, is_pure_expr,
};
mod entry_points;
mod error;
pub use error::{EcmaErrorClass, ErrorTaxonomy, FormatError, RuntimeError};
mod format;
mod heap;
mod host;
mod instruction;
mod javascript;
mod json;
mod ops;
pub(crate) use javascript::*;
mod projected_refresh;
mod projected_wire;
mod projector;
mod record;
mod schema;
mod state;
mod value;
mod vm;

pub use cache::{
    CompiledLinkedProgram, CompiledProcessCache, CompiledProcessCacheKey,
    CompiledProgramCacheStats, LinkedProgramCache, LinkedProgramCacheError,
};
#[allow(unused_imports)]
pub(crate) use compiler::*;
#[cfg(test)]
pub(crate) use entry_points::compile_ast;
pub use entry_points::{Entry, compile, execute, prewarm};
#[cfg(any(test, feature = "testing"))]
pub(crate) use heap::HEAP_OBJECT_KINDS;
pub(crate) use heap::{
    BuiltinFunction, BuiltinPrototype, DateObject, ErrorKind, ErrorObject, Heap, HeapObject,
    HeapRestoreWire, MapObject, PersistedRoots, RegExpMatchObject, RegExpObject, SetObject,
    UrlObject, UrlSearchParamsObject, canonical_regexp_flags, regexp_source, regexp_string,
};
pub use heap::{
    DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, HEAP_GC_ALLOCATION_INTERVAL, HEAP_SIZE_SCHEDULE_VERSION,
    HeapId, is_javascript_builtin_global,
};
pub use host::{
    AbilityOp, AbilityResult, AggregateConsumer, DEFAULT_HOST_MEMORY_LIMIT_BYTES,
    DEFAULT_MAX_VM_FRAME_DEPTH, ExecutionBound, ExecutionBounds, ExecutionEnvironment,
    ExecutionHost, ExecutionHostError, ExecutionMode, ProcessEvent, ProcessEventKind,
    ProcessSignal, ProcessStart, ResourceOperation, ResourceOperationBatch,
    ResourceOperationBatchLeaf, ResourceOperationBatchResult, ResourceOperationResult, Sleep,
    SleepKind,
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
    SchemaScalarKind, ValidationPlan, compile_schema_value, execute_validate_builtin,
    execute_validation_plan,
};
pub(crate) use vm::SlotState;
#[allow(unused_imports)]
pub use vm::{
    ContinuationError, TYPESCRIPT_REGEXP_EXECUTION_FUEL, TYPESCRIPT_REGEXP_FUEL_PER_INSTRUCTION,
    TYPESCRIPT_REGEXP_MAX_NESTING, TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS,
    TypeScriptRegExpValidationError, VM_CONTINUATION_FORMAT_VERSION, Vm, VmContinuation,
    VmFinallyCompletionContinuation, VmFinallyContinuation, VmHandlerContinuation,
    VmHeapContinuation, VmIteratorContinuation, VmIteratorCursor, VmPendingErrorOriginContinuation,
    VmProfileContinuation, VmRunOutcome, validate_typescript_regexp,
    validate_typescript_regexp_shape,
};
// Re-exports of helpers that live in the focused submodules but need to be
// reachable via `use super::*` from sibling submodules + via `super::name`
// from `vm.rs` / `compiler.rs`. These look "unused" from mod.rs's POV but
// are load-bearing for the rest of the runtime crate.
pub(crate) use access::value_contains_tool_handle;
#[allow(unused_imports)]
pub(crate) use access::{
    add_assign_index_number, add_assign_value_number, assign_index, assign_path, assign_path_steps,
    assign_record_field, descend_index, descend_record_field, ensure_no_prototype_chain_wire_key,
    heap_inherited_builtin, inline_inherited_builtin, is_prototype_chain_key, next_assign_index,
    nullish_property_read, prototype_chain_data_key_error, prototype_chain_key_error,
    read_field_ref_direct, read_image_field, read_index_ref_direct, read_javascript_field_direct,
    read_javascript_heap_field, read_javascript_heap_index, read_javascript_index_direct,
    read_javascript_index_direct_with_key, resolve_existing_list_assignment_index, resolve_index,
    unwrap_tool_result,
};
pub use access::{is_process_handle, parse_handle_record};
#[allow(unused_imports)]
pub(crate) use format::*;
#[allow(unused_imports)]
pub(crate) use json::*;
#[allow(unused_imports)]
pub(crate) use ops::*;
pub use state::LASHLANG_SNAPSHOT_VERSION;
pub use state::{
    BINDING_SUMMARY_MAX_CHARS, DurableBaseline, DurableFragment, DurableParts, GlobalPatch,
    GlobalPatchOutcome, Snapshot, SnapshotDecodeError, State,
};
pub use state::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, CanonicalPathSegment,
    validate_canonical_messagepack_structure,
};
pub use value::{
    ImageValue, LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY,
    LASH_HOST_REQUIREMENTS_REF_KEY, LASH_MODULE_REF_KEY, LASH_PROCESS_NAME_KEY,
    LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY, LASH_TYPE_KEY, ListValue, ProjectedBindingError,
    ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, ResourceHandle, StringValue, Value,
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
}

impl ExecutionScratch {
    pub fn new() -> Self {
        Self::default()
    }
}

pub(crate) const COOPERATIVE_YIELD_INSTRUCTION_BUDGET: usize = 1024;

/// The instruction accounting a run's cancel checkpoints are placed by
/// (FIG-3672 P9): what the compiler emits for a program, what each builtin
/// charges (the collection-work, intrinsic, callback and regexp
/// charges), the cooperative-yield granularity, and the checkpoint
/// schedule ([`cancel_checkpoint_reached`]). A checkpoint is a journaled gate
/// peek, and a code cell replays by re-execution, so a change to any of these
/// moves checkpoints to other journal positions: it bumps this version, and
/// with it the cell journal grammar, which refuses a journal written under
/// another before its cell runs.
pub const INSTRUCTION_ACCOUNTING_VERSION: u32 = 1;

/// Instructions before a run's first cancel checkpoint (FIG-3672 P9). The VM
/// hands its host checkpoint `n` when its executed-instruction count reaches
/// the `n`th position of the schedule [`cancel_checkpoint_reached`] counts.
pub const CANCEL_CHECKPOINT_INSTRUCTIONS: u64 = 1 << 20;

/// The widest gap between two cancel checkpoints: each gap doubles the one
/// before it, from [`CANCEL_CHECKPOINT_INSTRUCTIONS`], until it reaches this.
pub const CANCEL_CHECKPOINT_INTERVAL_CAP: u64 = 1 << 28;

/// The doublings from the first gap to the cap.
const CANCEL_CHECKPOINT_DOUBLINGS: u32 = CANCEL_CHECKPOINT_INTERVAL_CAP.trailing_zeros()
    - CANCEL_CHECKPOINT_INSTRUCTIONS.trailing_zeros();

/// How many cancel checkpoints a run has reached after `instructions`
/// executed instructions.
///
/// Gap `k` (from 1) is `CANCEL_CHECKPOINT_INSTRUCTIONS << (k - 1)`, capped at
/// [`CANCEL_CHECKPOINT_INTERVAL_CAP`]: checkpoints fall at 1, 3, 7, ... 511
/// times 2^20 instructions, then every 2^28. A run of `I` instructions reaches
/// at most `9 + (I - 511 * 2^20) / 2^28` checkpoints — nine in its first ~536M
/// instructions, then one per ~268M — and each journals one gate peek, or two
/// while an `AfterStep` request is pending (the second reads its escalation).
/// The schedule is a pure function of the deterministic instruction count, so
/// a replay reaches the same checkpoints at the same points.
pub fn cancel_checkpoint_reached(instructions: u64) -> u64 {
    let first = CANCEL_CHECKPOINT_INSTRUCTIONS;
    let doubling_span = first * ((1 << (CANCEL_CHECKPOINT_DOUBLINGS + 1)) - 1);
    if instructions < doubling_span {
        // Checkpoint `n` sits at `first * (2^n - 1)`.
        return u64::from((instructions / first + 1).ilog2());
    }
    u64::from(CANCEL_CHECKPOINT_DOUBLINGS + 1)
        + (instructions - doubling_span) / CANCEL_CHECKPOINT_INTERVAL_CAP
}

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
pub fn unwrap_type_value(value: &Value) -> Option<&Value> {
    let record = value.as_record()?;
    if record.len() != 1 {
        return None;
    }
    record.get(LASH_TYPE_KEY)
}

#[cfg(test)]
mod tests;
