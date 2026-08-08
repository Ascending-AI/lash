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
use crate::ast::{BinaryOp, UnaryOp};
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
    PushBool(bool),
    PushNumber(f64),
    LoadName(usize),
    StoreName(usize),
    StoreConst {
        slot: usize,
        constant: usize,
    },
    BuildTuple(usize),
    BuildList(usize),
    ListAppend,
    BuildRecord(usize),
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
    ResultUnwrap,
    Unary(UnaryOp),
    Binary(BinaryOp),
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
    EndIter,
    ResolveTypeRef(usize),
    WrapTypeLiteral,
    WrapHostDescriptor(usize),
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
            | Instruction::PushBool(_)
            | Instruction::PushNumber(_) => InstructionProfileTag::PushConst,
            Instruction::LoadName(_) => InstructionProfileTag::LoadName,
            Instruction::StoreName(_)
            | Instruction::StoreConst { .. }
            | Instruction::PathAssign { .. } => InstructionProfileTag::StoreName,
            Instruction::BuildTuple(_) => InstructionProfileTag::BuildTuple,
            Instruction::BuildList(_) => InstructionProfileTag::BuildList,
            Instruction::ListAppend => InstructionProfileTag::AppendAssign,
            Instruction::BuildRecord(_) => InstructionProfileTag::BuildRecord,
            Instruction::LoadField { .. } | Instruction::Field(_) => InstructionProfileTag::Field,
            Instruction::Index => InstructionProfileTag::Index,
            Instruction::ResultUnwrap | Instruction::LoadFieldUnwrap { .. } => {
                InstructionProfileTag::ResultUnwrap
            }
            Instruction::Unary(_) => InstructionProfileTag::Unary,
            Instruction::Binary(_)
            | Instruction::SlotNumberBinary { .. }
            | Instruction::SlotNumberCompare { .. }
            | Instruction::SlotNumberBinaryCompare { .. } => InstructionProfileTag::Binary,
            Instruction::ToBool => InstructionProfileTag::ToBool,
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
    pub(crate) fn argc(self) -> usize {
        match self {
            IntrinsicOp::Len
            | IntrinsicOp::Empty
            | IntrinsicOp::Keys
            | IntrinsicOp::Values
            | IntrinsicOp::Trim
            | IntrinsicOp::ToString
            | IntrinsicOp::ToInt
            | IntrinsicOp::ToFloat
            | IntrinsicOp::JsonParse
            | IntrinsicOp::ValidateCompiled(_)
            | IntrinsicOp::PushAssign(_) => 1,
            IntrinsicOp::Contains
            | IntrinsicOp::GrepText
            | IntrinsicOp::StartsWith
            | IntrinsicOp::EndsWith
            | IntrinsicOp::Split
            | IntrinsicOp::Join
            | IntrinsicOp::Validate
            | IntrinsicOp::CeilDiv
            | IntrinsicOp::FloorDiv
            | IntrinsicOp::Push => 2,
            IntrinsicOp::Slice => 3,
            IntrinsicOp::Find(argc)
            | IntrinsicOp::Format(argc)
            | IntrinsicOp::Range(argc)
            | IntrinsicOp::InvalidArity { argc, .. }
            | IntrinsicOp::Unknown { argc, .. }
            | IntrinsicOp::FormatCompiled(argc) => argc,
            IntrinsicOp::FormatCompiledSlotNumber { .. }
            | IntrinsicOp::FormatCompiledSlotNumberBinary { .. } => 0,
        }
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
}

const INSTRUCTION_PROFILE_COUNT: usize = InstructionProfileTag::WrapTypeLiteral as usize + 1;

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
