//! Bytecode compiler: lowers `crate::ast::Program` into a `Chunk` of
//! instructions plus the supporting compile-time tables (slot maps, format
//! templates, schema cache).
//!
//! All compile-time-only helpers live here too: `is_pure_expr` /
//! `contains_type_literal` (used to decide whether an expression can be
//! evaluated without entering the VM) and the `fold_type` /
//! `interned_scalar_schema` machinery (used to
//! convert `TypeExpr` AST nodes into JSON-Schema-shaped `Value` literals
//! at compile time).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::artifact::CompiledModuleContext;
use crate::ast::{
    AssignPathStep, AssignTarget, BinaryOp, Declaration, Expr, FunctionExpr, JavaScriptLogicalOp,
    LabelMetadata, ListComprehensionClause, ProcessStartExpr, Program, TypeExpr, UnaryOp,
};
use crate::lexer::Span;
use crate::tracking::{LashlangAstPath, LashlangExecutionContext, LashlangExecutionSite};

use super::record::{Symbol, intern_symbol, lookup_symbol, record_with_capacity, symbol_name};
use super::schema::{SchemaScalarKind, ValidationPlan, compile_schema_value};
use super::{
    Chunk, ClosureParameterModel, CompileStats, CompiledAggregateAwaitShape, CompiledAssignPath,
    CompiledAssignPathStep, CompiledFormatTemplate, CompiledFunction,
    CompiledResourceOperationBatch, CompiledResourceOperationBatchLeaf, HandlerScopeExtent,
    Instruction, IntrinsicOp, JavaScriptUriCodec, LASH_HOST_REQUIREMENTS_REF_KEY,
    LASH_MODULE_REF_KEY, LASH_PROCESS_NAME_KEY, LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY,
    LASH_TYPE_KEY, Name, Value, as_number, compile_format_template, eval_binary_values,
    eval_javascript_binary, eval_javascript_unary, execute_integer_div_builtin, execute_len_direct,
    execute_range_builtin, is_comparison_binary_op, is_numeric_binary_op, is_truthy,
    read_field_direct, read_index_direct, read_javascript_field_direct,
    read_javascript_index_direct, transient_name, unwrap_type_value,
};

pub(crate) struct Compiler {
    dialect: CompilationDialect,
    module_context: Option<CompiledModuleContext>,
    lashlang_execution: Option<LashlangExecutionCompileContext>,
    expression_source_spans: FxHashMap<usize, Span>,
    code: Vec<Instruction>,
    spans: Vec<Option<Span>>,
    constants: Vec<Value>,
    names: Vec<Name>,
    name_lookup: FxHashMap<Symbol, usize>,
    slots: Rc<RefCell<SlotTable>>,
    key_lists: Vec<Box<[usize]>>,
    format_templates: Vec<CompiledFormatTemplate>,
    compiled_schemas: Vec<ValidationPlan>,
    assign_paths: Vec<CompiledAssignPath>,
    resource_operation_batches: Vec<CompiledResourceOperationBatch>,
    compile_stats: Rc<RefCell<CompileStats>>,
    const_slots: Vec<Option<Value>>,
    loop_contexts: Vec<LoopContext>,
    handler_scopes: Vec<HandlerScope>,
    handler_scope_extents: Vec<HandlerScopeExtent>,
    pending_finally_sites: Vec<Vec<usize>>,
    functions: Vec<CompiledFunction>,
    pending_functions: Vec<Option<PendingFunction>>,
    /// Chunk function index for each declared `fn`, by name.
    declared_functions: FxHashMap<String, usize>,
}

struct PendingFunction {
    definition: FunctionExpr,
    parameter_model: ClosureParameterModel,
}

/// Source-language choices that affect bytecode while sharing the same AST and
/// heap VM.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CompilationDialect {
    /// Lashlang's value-isolating assignment and aggregate semantics.
    #[default]
    Lashlang,
    /// ECMA-262 reference semantics at assignment, argument, capture, and
    /// aggregate insertion boundaries.
    Typescript,
}

struct LashlangExecutionCompileContext {
    context: LashlangExecutionContext,
    paths: FxHashMap<usize, LashlangAstPath>,
    sites: Vec<Option<LashlangExecutionSite>>,
}

struct LoopContext {
    continue_target: usize,
    break_jumps: SmallVec<[usize; 4]>,
    /// Depth of [`Compiler::handler_scopes`] when the loop was entered. A
    /// `break` or `continue` targeting this loop must unwind every scope above
    /// this depth before it jumps.
    handler_scope_depth: usize,
}

/// One live structured-exception scope on the compile-time scope stack. It
/// mirrors what the VM's handler and finally stacks hold while the enclosed
/// bytecode runs, which is what lets a jump edge emit exactly the instructions
/// a fall-through edge would have executed.
#[derive(Clone, Copy)]
enum HandlerScope {
    /// A `PushHandler` is live: leaving the region pops it and, when the scope
    /// owns a `finally`, runs that block before the jump continues.
    Protected {
        /// Index into [`Compiler::pending_finally_sites`] collecting the
        /// `EnterFinally` sites this scope still has to patch, or `None` when
        /// the scope has no `finally` block.
        finally_sites: Option<usize>,
    },
    /// A `finally` body is executing: leaving it by a jump replaces the pending
    /// completion, discarding whatever the body was going to resume or rethrow.
    FinallyBody,
}

#[derive(Default)]
struct SlotTable {
    names: Vec<Name>,
    lookup: FxHashMap<Symbol, usize>,
}

include!("compiler/entry.rs");
include!("compiler/expr.rs");
include!("compiler/effects.rs");
include!("compiler/helpers.rs");
