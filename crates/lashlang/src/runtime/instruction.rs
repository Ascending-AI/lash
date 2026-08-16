//! Bytecode instruction set + the inert data types that flow from the
//! compiler to the VM: `Chunk`, `Name`, `Instruction`, `IntrinsicOp`, the
//! profile-tag enums and accumulator, and the format-template / assign-path
//! shapes.
//!
//! Everything here is internal to the runtime crate — the visibility is
//! `pub(crate)` because compiler.rs produces these structures and vm.rs
//! consumes them. None of these types are part of the lashlang public API.

use std::sync::{Arc, OnceLock};

use crate::artifact::CompiledModuleContext;
use crate::ast::{BinaryOp, JavaScriptBinaryOp, JavaScriptUnaryOp, UnaryOp};
use crate::lexer::Span;
use crate::tracking::LashlangExecutionSite;

use super::record::{Symbol, intern_symbol, symbol_name};
use super::schema::ValidationPlan;
use super::{CompileStats, FormatError, ProfileReport, ProfileStat, Value};

#[derive(Clone)]
pub(crate) struct Chunk {
    pub(crate) module_context: Option<CompiledModuleContext>,
    pub(crate) code: Vec<Instruction>,
    pub(crate) spans: Vec<Option<Span>>,
    pub(crate) lashlang_execution_sites: Vec<Option<LashlangExecutionSite>>,
    pub(crate) constants: Vec<Value>,
    pub(crate) names: Vec<Name>,
    pub(crate) slot_names: Vec<Name>,
    pub(crate) key_lists: Vec<Box<[usize]>>,
    pub(crate) format_templates: Vec<CompiledFormatTemplate>,
    pub(crate) compiled_schemas: Vec<ValidationPlan>,
    pub(crate) assign_paths: Vec<CompiledAssignPath>,
    pub(crate) resource_operation_batches: Vec<CompiledResourceOperationBatch>,
    pub(crate) functions: Vec<CompiledFunction>,
    /// Every structured-exception scope the compiler emitted a `PushHandler`
    /// for, sorted by handler target. It is what makes an impossible durable
    /// handler stack unrepresentable: a restored handler must name one of
    /// these scopes, and consecutive handlers in one frame must be a strictly
    /// nested chain of them.
    pub(crate) handler_scopes: Vec<HandlerScopeExtent>,
    pub(crate) root_code_len: usize,
}

/// The bytecode extent of one `try` scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HandlerScopeExtent {
    /// The `PushHandler` that installs the scope.
    pub(crate) push_ip: usize,
    /// Where a throw transfers to: the catch entry, or the finally entry for a
    /// cleanup-only scope.
    pub(crate) handler_ip: usize,
    /// The scope's `finally` entry, when it has one.
    pub(crate) finally_ip: Option<usize>,
    /// Whether the scope catches, as opposed to only running cleanup.
    pub(crate) catches: bool,
    /// One past the last instruction the scope protects.
    pub(crate) end_ip: usize,
}

impl Chunk {
    /// Looks up the scope a durable handler record names. The handler target is
    /// the scope's identity: no two scopes share one, so a record that does not
    /// match exactly names no scope the compiler emitted.
    pub(crate) fn handler_scope(
        &self,
        handler_ip: usize,
        finally_ip: Option<usize>,
        catches: bool,
    ) -> Option<&HandlerScopeExtent> {
        let index = self
            .handler_scopes
            .binary_search_by_key(&handler_ip, |scope| scope.handler_ip)
            .ok()?;
        let scope = &self.handler_scopes[index];
        (scope.finally_ip == finally_ip && scope.catches == catches).then_some(scope)
    }
}

#[derive(Clone)]
pub(crate) struct CompiledFunction {
    pub(crate) entry_ip: usize,
    pub(crate) end_ip: usize,
    pub(crate) parameter_count: usize,
    pub(crate) parameter_model: ClosureParameterModel,
    pub(crate) capture_count: usize,
    pub(crate) self_slot: Option<usize>,
    pub(crate) parameter_slots: Box<[usize]>,
    pub(crate) capture_slots: Box<[usize]>,
    pub(crate) slot_names: Box<[Name]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClosureParameterModel {
    /// Lashlang closures retain the language's declared-count-is-exact rule.
    Exact,
    /// TypeScript closures follow ECMA call entry: absent fixed arguments are
    /// `undefined`, surplus arguments are ignored, and an optional final rest
    /// slot receives a fresh list. `required_count` is emitted by the lowerer
    /// for the declaration's pre-default prefix (and therefore Function.length
    /// semantics); it does not make missing arguments a call-time error.
    TypeScript {
        required_count: usize,
        accepts_rest: bool,
    },
}

#[derive(Clone)]
pub(crate) struct Name {
    pub(crate) symbol: Symbol,
    pub(crate) text: Arc<str>,
}

#[derive(Clone)]
pub(crate) struct CompiledFormatTemplate {
    pub(crate) parts: Box<[CompiledFormatPart]>,
    pub(crate) argc: usize,
    pub(crate) min_capacity: usize,
    pub(crate) error: Option<FormatError>,
    pub(crate) one_arg: Option<CompiledFormatOneArg>,
}

#[derive(Clone)]
pub(crate) enum CompiledFormatPart {
    Literal(Arc<str>),
    Arg(usize),
}

#[derive(Clone)]
pub(crate) struct CompiledFormatOneArg {
    pub(crate) prefix: Option<Arc<str>>,
    pub(crate) suffix: Option<Arc<str>>,
}

#[derive(Clone)]
pub(crate) struct CompiledAssignPath {
    pub(crate) steps: Box<[CompiledAssignPathStep]>,
    pub(crate) dynamic_index_count: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum CompiledAssignPathStep {
    Field(usize),
    Index,
}

#[derive(Clone)]
pub(crate) struct CompiledResourceOperationBatch {
    pub(crate) leaves: Box<[CompiledResourceOperationBatchLeaf]>,
    pub(crate) shape: CompiledAggregateAwaitShape,
    pub(crate) stack_value_count: usize,
    pub(crate) aggregate_unwrap: bool,
    /// Select the rejection this batch reports by the order its leaves
    /// *settled* rather than the order they were written.
    ///
    /// `Promise.all` is specified to reject with the first settled rejection,
    /// so the TypeScript lowering sets this. Lashlang's own aggregates select
    /// in input order and leave it clear, which is why the choice is recorded
    /// per batch at lowering instead of being inferred at run time from a
    /// dialect flag that answers some other question.
    pub(crate) first_settled_rejection: bool,
}

#[derive(Clone)]
pub(crate) struct CompiledResourceOperationBatchLeaf {
    pub(crate) operation: usize,
    pub(crate) argc: usize,
    pub(crate) receiver_stack_index: usize,
    pub(crate) unwrap: bool,
    pub(crate) site: Option<LashlangExecutionSite>,
    pub(crate) source_span: Option<Span>,
}

#[derive(Clone)]
pub(crate) enum CompiledAggregateAwaitShape {
    BatchLeaf(usize),
    Value(usize),
    Tuple(Box<[CompiledAggregateAwaitShape]>),
    List(Box<[CompiledAggregateAwaitShape]>),
    Record {
        keys: usize,
        values: Box<[CompiledAggregateAwaitShape]>,
    },
}

pub(crate) struct ResultWrapperNames {
    pub(crate) ok: Name,
    pub(crate) value: Name,
    pub(crate) error: Name,
}

pub(crate) fn transient_name(name: &str) -> Name {
    let symbol = intern_symbol(name);
    Name {
        symbol,
        text: symbol_name(symbol),
    }
}

pub(crate) fn result_wrapper_names() -> &'static ResultWrapperNames {
    static NAMES: OnceLock<ResultWrapperNames> = OnceLock::new();
    NAMES.get_or_init(|| ResultWrapperNames {
        ok: transient_name("ok"),
        value: transient_name("value"),
        error: transient_name("error"),
    })
}

#[derive(Clone, Copy)]
pub(crate) enum Instruction {
    PushConst(usize),
    PushNull,
    PushUndefined,
    PushBool(bool),
    PushNumber(f64),
    LoadName(usize),
    Duplicate,
    DeepCopy,
    StoreName(usize),
    StoreConst {
        slot: usize,
        constant: usize,
    },
    BuildTuple(usize),
    BuildList(usize),
    BuildHeapList(usize),
    ListAppend,
    BuildRecord(usize),
    BuildHeapRecord(usize),
    LoadField {
        slot: usize,
        field: usize,
    },
    LoadFieldUnwrap {
        slot: usize,
        field: usize,
    },
    Field(usize),
    Index,
    PathAssign {
        slot: usize,
        path: usize,
    },
    HeapPathAssign {
        slot: usize,
        path: usize,
    },
    ResultUnwrap,
    Unary(UnaryOp),
    Binary(BinaryOp),
    JavaScriptUnary(JavaScriptUnaryOp),
    JavaScriptBinary(JavaScriptBinaryOp),
    IsNullish,
    // Retained after measurement: large_data/loop_control/type_system_stress
    // regressed when these numeric slot ops were routed through generic stack
    // dispatch.
    SlotNumberBinary {
        slot: usize,
        op: BinaryOp,
        right: f64,
    },
    SlotNumberCompare {
        slot: usize,
        op: BinaryOp,
        right: f64,
    },
    SlotNumberBinaryCompare {
        slot: usize,
        binary_op: BinaryOp,
        binary_right: f64,
        compare_op: BinaryOp,
        compare_right: f64,
    },
    ToBool,
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfCompareFalse {
        op: BinaryOp,
        target: usize,
    },
    JumpIfSlotNumberCompareFalse {
        slot: usize,
        op: BinaryOp,
        right: f64,
        target: usize,
    },
    JumpIfSlotNumberBinaryCompareFalse {
        slot: usize,
        binary_op: BinaryOp,
        binary_right: f64,
        compare_op: BinaryOp,
        compare_right: f64,
        target: usize,
    },
    JumpIfTrue(usize),
    ResourceCall {
        operation: usize,
        argc: usize,
    },
    ResourceCallUnwrap {
        operation: usize,
        argc: usize,
    },
    ResourceOperationBatch(usize),
    StartProcess {
        process: usize,
        keys: usize,
    },
    AwaitHandle,
    SleepFor,
    SleepUntil,
    ProcessWaitSignal {
        name: usize,
    },
    ProcessSignalRun {
        name: usize,
    },
    AwaitHandleUnwrap,
    CancelHandle,
    Intrinsic(IntrinsicOp),
    MakeClosure {
        function: usize,
        captures: usize,
    },
    Call {
        argc: usize,
    },
    CallDynamic,
    Map,
    AsyncMap,
    Return,
    PushHandler {
        handler: usize,
        finally: Option<usize>,
        catches: bool,
    },
    PopHandler,
    EnterFinally {
        finally: usize,
        resume: usize,
    },
    EndFinally,
    /// Discards the pending completion of the `finally` body being left by an
    /// abrupt completion (`break` / `continue`), per ECMA-262 completion
    /// replacement.
    AbandonFinally,
    /// Replaces the pending completion while preserving the return value that
    /// was evaluated before the `finally` body was left.
    AbandonFinallyKeepValue,
    Throw,
    AddAssign(usize),
    // Retained after measurement: indexed_assignment/large_data regress when
    // numeric add-assign paths route through generic stack/path assignment.
    AddAssignNumber {
        slot: usize,
        right: f64,
    },
    AddAssignSlot {
        slot: usize,
        right: usize,
    },
    AddAssignIndexNumber {
        slot: usize,
        right: f64,
    },
    AddAssignIndexSlotNumber {
        slot: usize,
        index: usize,
        right: f64,
    },
    AppendAssign(usize),
    Print,
    ProcessYield,
    ProcessWake,
    Finish,
    ProcessFail,
    ObserveStep,
    Pop,
    BeginIter(usize),
    BeginRangeIter {
        binding: usize,
        argc: usize,
    },
    IterNext {
        jump_to: usize,
    },
    DeepCopyLoopBinding(usize),
    EndIter,
    ResolveTypeRef(usize),
    WrapTypeLiteral,
    WrapHostDescriptor(usize),
}

#[derive(Clone, Copy)]
pub(crate) enum JavaScriptUriCodec {
    EncodeComponent,
    DecodeComponent,
    EncodeUri,
    DecodeUri,
}

#[derive(Clone, Copy)]
pub(crate) enum IntrinsicOp {
    Len,
    Empty,
    Keys,
    Values,
    Contains,
    Find(usize),
    GrepText,
    StartsWith,
    EndsWith,
    Split,
    Join,
    JavaScriptSplit,
    JavaScriptJoin,
    JavaScriptStdlib(usize),
    JavaScriptHeapNew(usize),
    JavaScriptHeapInstanceOf,
    JavaScriptHeapDeleteMember,
    JavaScriptRegExp(usize),
    JavaScriptGlobalDelete,
    JavaScriptGlobalHas,
    JavaScriptGlobalSet,
    JavaScriptUriCodec(JavaScriptUriCodec),
    Trim,
    Slice,
    ToString,
    ToInt,
    ToFloat,
    JsonParse,
    Format(usize),
    Validate,
    Range(usize),
    CeilDiv,
    FloorDiv,
    Push,
    Sort,
    SortBy,
    Sum,
    Min,
    Max,
    Replace,
    Lower,
    Upper,
    Unique,
    Reverse,
    InvalidArity {
        name: usize,
        argc: usize,
    },
    Unknown {
        name: usize,
        argc: usize,
    },
    ValidateCompiled(usize),
    PushAssign(usize),
    FormatCompiled(usize),
    // Retained after measurement: projected_operations and formatting-heavy
    // surfaces lost more than the allowed gate without these direct paths.
    FormatCompiledSlotNumber {
        template: usize,
        slot: usize,
    },
    FormatCompiledSlotNumberBinary {
        template: usize,
        slot: usize,
        op: BinaryOp,
        right: f64,
    },
}

impl Instruction {
    pub(crate) fn profile_tag(self) -> InstructionProfileTag {
        match self {
            Instruction::PushConst(_)
            | Instruction::PushNull
            | Instruction::PushUndefined
            | Instruction::PushBool(_)
            | Instruction::PushNumber(_) => InstructionProfileTag::PushConst,
            Instruction::LoadName(_) | Instruction::Duplicate => InstructionProfileTag::LoadName,
            Instruction::DeepCopy | Instruction::DeepCopyLoopBinding(_) => {
                InstructionProfileTag::StoreName
            }
            Instruction::StoreName(_)
            | Instruction::StoreConst { .. }
            | Instruction::PathAssign { .. }
            | Instruction::HeapPathAssign { .. } => InstructionProfileTag::StoreName,
            Instruction::BuildTuple(_) => InstructionProfileTag::BuildTuple,
            Instruction::BuildList(_) | Instruction::BuildHeapList(_) => {
                InstructionProfileTag::BuildList
            }
            Instruction::ListAppend => InstructionProfileTag::AppendAssign,
            Instruction::BuildRecord(_) | Instruction::BuildHeapRecord(_) => {
                InstructionProfileTag::BuildRecord
            }
            Instruction::LoadField { .. } | Instruction::Field(_) => InstructionProfileTag::Field,
            Instruction::Index => InstructionProfileTag::Index,
            Instruction::ResultUnwrap | Instruction::LoadFieldUnwrap { .. } => {
                InstructionProfileTag::ResultUnwrap
            }
            Instruction::Unary(_) | Instruction::JavaScriptUnary(_) => InstructionProfileTag::Unary,
            Instruction::Binary(_)
            | Instruction::JavaScriptBinary(_)
            | Instruction::SlotNumberBinary { .. }
            | Instruction::SlotNumberCompare { .. }
            | Instruction::SlotNumberBinaryCompare { .. } => InstructionProfileTag::Binary,
            Instruction::ToBool | Instruction::IsNullish => InstructionProfileTag::ToBool,
            Instruction::Jump(_) => InstructionProfileTag::Jump,
            Instruction::JumpIfFalse(_)
            | Instruction::JumpIfCompareFalse { .. }
            | Instruction::JumpIfSlotNumberCompareFalse { .. }
            | Instruction::JumpIfSlotNumberBinaryCompareFalse { .. } => {
                InstructionProfileTag::JumpIfFalse
            }
            Instruction::JumpIfTrue(_) => InstructionProfileTag::JumpIfTrue,
            Instruction::ResourceCall { .. } | Instruction::ResourceCallUnwrap { .. } => {
                InstructionProfileTag::ResourceCall
            }
            Instruction::ResourceOperationBatch(_) => InstructionProfileTag::ResourceCall,
            Instruction::StartProcess { .. } => InstructionProfileTag::StartProcess,
            Instruction::AwaitHandle
            | Instruction::AwaitHandleUnwrap
            | Instruction::ProcessWaitSignal { .. } => InstructionProfileTag::AwaitHandle,
            Instruction::SleepFor | Instruction::SleepUntil => InstructionProfileTag::Sleep,
            Instruction::ProcessSignalRun { .. } => InstructionProfileTag::SessionProcessAdmin,
            Instruction::CancelHandle => InstructionProfileTag::CancelHandle,
            Instruction::Intrinsic(_) => InstructionProfileTag::Intrinsic,
            Instruction::MakeClosure { .. } => InstructionProfileTag::MakeClosure,
            Instruction::Call { .. } | Instruction::CallDynamic => InstructionProfileTag::Call,
            Instruction::Map | Instruction::AsyncMap => InstructionProfileTag::Callback,
            Instruction::Return => InstructionProfileTag::Return,
            Instruction::PushHandler { .. }
            | Instruction::PopHandler
            | Instruction::EnterFinally { .. }
            | Instruction::EndFinally
            | Instruction::AbandonFinally
            | Instruction::AbandonFinallyKeepValue
            | Instruction::Throw => InstructionProfileTag::Exception,
            Instruction::AddAssign(_)
            | Instruction::AddAssignNumber { .. }
            | Instruction::AddAssignSlot { .. }
            | Instruction::AddAssignIndexNumber { .. }
            | Instruction::AddAssignIndexSlotNumber { .. } => InstructionProfileTag::AddAssign,
            Instruction::AppendAssign(_) => InstructionProfileTag::AppendAssign,
            Instruction::Print => InstructionProfileTag::Print,
            Instruction::Finish => InstructionProfileTag::Finish,
            Instruction::ProcessYield | Instruction::ProcessWake | Instruction::ProcessFail => {
                InstructionProfileTag::SessionProcessAdmin
            }
            Instruction::ObserveStep => InstructionProfileTag::ObserveStep,
            Instruction::Pop => InstructionProfileTag::Pop,
            Instruction::BeginIter(_) | Instruction::BeginRangeIter { .. } => {
                InstructionProfileTag::BeginIter
            }
            Instruction::IterNext { .. } => InstructionProfileTag::IterNext,
            Instruction::EndIter => InstructionProfileTag::EndIter,
            Instruction::ResolveTypeRef(_) => InstructionProfileTag::ResolveTypeRef,
            Instruction::WrapTypeLiteral | Instruction::WrapHostDescriptor(_) => {
                InstructionProfileTag::WrapTypeLiteral
            }
        }
    }
}

impl IntrinsicOp {
    pub(crate) fn fixed_argc(self) -> Option<usize> {
        Some(match self {
            IntrinsicOp::Len
            | IntrinsicOp::Empty
            | IntrinsicOp::Keys
            | IntrinsicOp::Values
            | IntrinsicOp::Trim
            | IntrinsicOp::ToString
            | IntrinsicOp::ToInt
            | IntrinsicOp::ToFloat
            | IntrinsicOp::JsonParse
            | IntrinsicOp::Sort
            | IntrinsicOp::Sum
            | IntrinsicOp::Min
            | IntrinsicOp::Max
            | IntrinsicOp::Lower
            | IntrinsicOp::Upper
            | IntrinsicOp::Unique
            | IntrinsicOp::Reverse
            | IntrinsicOp::ValidateCompiled(_)
            | IntrinsicOp::PushAssign(_)
            | IntrinsicOp::JavaScriptGlobalDelete
            | IntrinsicOp::JavaScriptGlobalHas
            | IntrinsicOp::JavaScriptUriCodec(_) => 1,
            IntrinsicOp::Contains
            | IntrinsicOp::GrepText
            | IntrinsicOp::StartsWith
            | IntrinsicOp::EndsWith
            | IntrinsicOp::Split
            | IntrinsicOp::Join
            | IntrinsicOp::JavaScriptSplit
            | IntrinsicOp::JavaScriptJoin
            | IntrinsicOp::JavaScriptHeapInstanceOf
            | IntrinsicOp::JavaScriptGlobalSet
            | IntrinsicOp::JavaScriptHeapDeleteMember
            | IntrinsicOp::Validate
            | IntrinsicOp::CeilDiv
            | IntrinsicOp::FloorDiv
            | IntrinsicOp::Push
            | IntrinsicOp::SortBy => 2,
            IntrinsicOp::Slice | IntrinsicOp::Replace => 3,
            IntrinsicOp::Find(argc)
            | IntrinsicOp::Format(argc)
            | IntrinsicOp::Range(argc)
            | IntrinsicOp::JavaScriptStdlib(argc)
            | IntrinsicOp::JavaScriptHeapNew(argc)
            | IntrinsicOp::JavaScriptRegExp(argc)
            | IntrinsicOp::InvalidArity { argc, .. }
            | IntrinsicOp::Unknown { argc, .. } => argc,
            IntrinsicOp::FormatCompiled(_)
            | IntrinsicOp::FormatCompiledSlotNumber { .. }
            | IntrinsicOp::FormatCompiledSlotNumberBinary { .. } => return None,
        })
    }

    pub(crate) fn profile_tag(self) -> BuiltinProfileTag {
        match self {
            IntrinsicOp::Len => BuiltinProfileTag::Len,
            IntrinsicOp::Empty => BuiltinProfileTag::Empty,
            IntrinsicOp::Keys => BuiltinProfileTag::Keys,
            IntrinsicOp::Values => BuiltinProfileTag::Values,
            IntrinsicOp::Contains => BuiltinProfileTag::Contains,
            IntrinsicOp::Find(_) => BuiltinProfileTag::Find,
            IntrinsicOp::GrepText => BuiltinProfileTag::GrepText,
            IntrinsicOp::StartsWith => BuiltinProfileTag::StartsWith,
            IntrinsicOp::EndsWith => BuiltinProfileTag::EndsWith,
            IntrinsicOp::Split => BuiltinProfileTag::Split,
            IntrinsicOp::Join => BuiltinProfileTag::Join,
            IntrinsicOp::JavaScriptSplit => BuiltinProfileTag::Split,
            IntrinsicOp::JavaScriptJoin => BuiltinProfileTag::Join,
            IntrinsicOp::JavaScriptStdlib(_) => BuiltinProfileTag::TypeScriptStdlib,
            IntrinsicOp::JavaScriptHeapNew(_) => BuiltinProfileTag::TypeScriptStdlib,
            IntrinsicOp::JavaScriptHeapInstanceOf
            | IntrinsicOp::JavaScriptHeapDeleteMember
            | IntrinsicOp::JavaScriptRegExp(_)
            | IntrinsicOp::JavaScriptGlobalDelete
            | IntrinsicOp::JavaScriptGlobalHas
            | IntrinsicOp::JavaScriptGlobalSet
            | IntrinsicOp::JavaScriptUriCodec(_) => BuiltinProfileTag::TypeScriptStdlib,
            IntrinsicOp::Trim => BuiltinProfileTag::Trim,
            IntrinsicOp::Slice => BuiltinProfileTag::Slice,
            IntrinsicOp::ToString => BuiltinProfileTag::ToString,
            IntrinsicOp::ToInt => BuiltinProfileTag::ToInt,
            IntrinsicOp::ToFloat => BuiltinProfileTag::ToFloat,
            IntrinsicOp::JsonParse => BuiltinProfileTag::JsonParse,
            IntrinsicOp::Format(_)
            | IntrinsicOp::FormatCompiled(_)
            | IntrinsicOp::FormatCompiledSlotNumber { .. }
            | IntrinsicOp::FormatCompiledSlotNumberBinary { .. } => BuiltinProfileTag::Format,
            IntrinsicOp::Validate | IntrinsicOp::ValidateCompiled(_) => BuiltinProfileTag::Validate,
            IntrinsicOp::Range(_) => BuiltinProfileTag::Range,
            IntrinsicOp::CeilDiv => BuiltinProfileTag::CeilDiv,
            IntrinsicOp::FloorDiv => BuiltinProfileTag::FloorDiv,
            IntrinsicOp::Push | IntrinsicOp::PushAssign(_) => BuiltinProfileTag::Push,
            IntrinsicOp::Sort => BuiltinProfileTag::Sort,
            IntrinsicOp::SortBy => BuiltinProfileTag::SortBy,
            IntrinsicOp::Sum => BuiltinProfileTag::Sum,
            IntrinsicOp::Min => BuiltinProfileTag::Min,
            IntrinsicOp::Max => BuiltinProfileTag::Max,
            IntrinsicOp::Replace => BuiltinProfileTag::Replace,
            IntrinsicOp::Lower => BuiltinProfileTag::Lower,
            IntrinsicOp::Upper => BuiltinProfileTag::Upper,
            IntrinsicOp::Unique => BuiltinProfileTag::Unique,
            IntrinsicOp::Reverse => BuiltinProfileTag::Reverse,
            IntrinsicOp::InvalidArity { .. } | IntrinsicOp::Unknown { .. } => {
                BuiltinProfileTag::Unknown
            }
        }
    }
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum InstructionProfileTag {
    PushConst,
    LoadName,
    StoreName,
    BuildTuple,
    BuildList,
    BuildRecord,
    Field,
    Index,
    ResultUnwrap,
    Unary,
    Binary,
    ToBool,
    Jump,
    JumpIfFalse,
    JumpIfTrue,
    ResourceCall,
    StartProcess,
    AwaitHandle,
    CancelHandle,
    Intrinsic,
    AddAssign,
    AppendAssign,
    Print,
    Finish,
    Sleep,
    SessionProcessAdmin,
    ObserveStep,
    Pop,
    BeginIter,
    IterNext,
    EndIter,
    ResolveTypeRef,
    WrapTypeLiteral,
    MakeClosure,
    Call,
    Callback,
    Return,
    Exception,
}

const INSTRUCTION_PROFILE_COUNT: usize = InstructionProfileTag::Exception as usize + 1;

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum BuiltinProfileTag {
    Len,
    Empty,
    Keys,
    Values,
    Contains,
    Find,
    GrepText,
    StartsWith,
    EndsWith,
    Split,
    Join,
    Trim,
    Slice,
    ToString,
    ToInt,
    ToFloat,
    JsonParse,
    Format,
    Validate,
    Range,
    CeilDiv,
    FloorDiv,
    Push,
    Sort,
    SortBy,
    Sum,
    Min,
    Max,
    Replace,
    Lower,
    Upper,
    Unique,
    Reverse,
    TypeScriptStdlib,
    Unknown,
}

const BUILTIN_PROFILE_COUNT: usize = BuiltinProfileTag::Unknown as usize + 1;

pub(crate) struct ProfileAccumulator {
    pub(crate) instruction_counts: [u64; INSTRUCTION_PROFILE_COUNT],
    pub(crate) instruction_times: [u128; INSTRUCTION_PROFILE_COUNT],
    pub(crate) builtin_counts: [u64; BUILTIN_PROFILE_COUNT],
    pub(crate) builtin_times: [u128; BUILTIN_PROFILE_COUNT],
}

impl Default for ProfileAccumulator {
    fn default() -> Self {
        Self {
            instruction_counts: [0; INSTRUCTION_PROFILE_COUNT],
            instruction_times: [0; INSTRUCTION_PROFILE_COUNT],
            builtin_counts: [0; BUILTIN_PROFILE_COUNT],
            builtin_times: [0; BUILTIN_PROFILE_COUNT],
        }
    }
}

impl ProfileAccumulator {
    pub(crate) fn finish(self) -> ProfileReport {
        ProfileReport {
            instruction_stats: build_stats(
                &INSTRUCTION_PROFILE_NAMES,
                &self.instruction_counts,
                &self.instruction_times,
            ),
            builtin_stats: build_stats(
                &BUILTIN_PROFILE_NAMES,
                &self.builtin_counts,
                &self.builtin_times,
            ),
            compile_stats: CompileStats::default(),
        }
    }
}

const INSTRUCTION_PROFILE_NAMES: [&str; INSTRUCTION_PROFILE_COUNT] = [
    "push_const",
    "load_name",
    "store_name",
    "build_tuple",
    "build_list",
    "build_record",
    "field",
    "index",
    "result_unwrap",
    "unary",
    "binary",
    "to_bool",
    "jump",
    "jump_if_false",
    "jump_if_true",
    "resource_call",
    "start_process",
    "await_handle",
    "cancel_handle",
    "intrinsic",
    "add_assign",
    "append_assign",
    "print",
    "finish",
    "sleep",
    "processes",
    "observe_step",
    "pop",
    "begin_iter",
    "iter_next",
    "end_iter",
    "resolve_type_ref",
    "wrap_type_literal",
    "make_closure",
    "call",
    "callback",
    "return",
    "exception",
];

const BUILTIN_PROFILE_NAMES: [&str; BUILTIN_PROFILE_COUNT] = [
    "len",
    "empty",
    "keys",
    "values",
    "contains",
    "find",
    "grep_text",
    "starts_with",
    "ends_with",
    "split",
    "join",
    "trim",
    "slice",
    "to_string",
    "to_int",
    "to_float",
    "json_parse",
    "format",
    "validate",
    "range",
    "ceil_div",
    "floor_div",
    "push",
    "sort",
    "sort_by",
    "sum",
    "min",
    "max",
    "replace",
    "lower",
    "upper",
    "unique",
    "reverse",
    "typescript_stdlib",
    "unknown",
];

fn build_stats<const N: usize>(
    names: &[&'static str; N],
    counts: &[u64; N],
    times: &[u128; N],
) -> Vec<ProfileStat> {
    let mut stats = names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let count = counts[index];
            (count > 0).then_some(ProfileStat {
                name,
                count,
                total_ns: times[index],
            })
        })
        .collect::<Vec<_>>();
    stats.sort_by(|a, b| {
        b.total_ns
            .cmp(&a.total_ns)
            .then_with(|| b.count.cmp(&a.count))
    });
    stats
}

pub(crate) fn merge_stats(target: &mut Vec<ProfileStat>, source: &[ProfileStat]) {
    for stat in source {
        if let Some(existing) = target.iter_mut().find(|entry| entry.name == stat.name) {
            existing.count += stat.count;
            existing.total_ns += stat.total_ns;
        } else {
            target.push(stat.clone());
        }
    }
    target.sort_by(|a, b| {
        b.total_ns
            .cmp(&a.total_ns)
            .then_with(|| b.count.cmp(&a.count))
    });
}
