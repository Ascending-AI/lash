use super::*;
use serde::Deserializer;

impl<'de> Deserialize<'de> for VmContinuation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            format_version: u32,
            instruction_pointer: usize,
            active_function: Option<u32>,
            #[serde(deserialize_with = "continuation_serde::deserialize_values")]
            operand_stack: Vec<Value>,
            #[serde(deserialize_with = "continuation_serde::deserialize_optional_value")]
            last_value: Option<Value>,
            #[serde(deserialize_with = "continuation_serde::deserialize_slots")]
            slots: Vec<Option<Value>>,
            projected_slots: Vec<bool>,
            #[serde(deserialize_with = "continuation_serde::deserialize_record")]
            globals: Record,
            iterator_stack: Vec<VmIteratorContinuation>,
            frame_stack: Vec<VmFrameContinuation>,
            handler_stack: Vec<VmHandlerContinuation>,
            finally_stack: Vec<VmFinallyContinuation>,
            occurrence_counters: std::collections::BTreeMap<String, u64>,
            mode: ExecutionMode,
            profile: Option<VmProfileContinuation>,
            pending_error_span: Option<Span>,
            instructions_executed: u64,
            active_execution_elapsed: std::time::Duration,
            #[serde(deserialize_with = "continuation_serde::deserialize_heap")]
            heap: VmHeapContinuation,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.format_version != VM_CONTINUATION_FORMAT_VERSION {
            return Err(serde::de::Error::custom(format!(
                "continuation format version {} is incompatible with version {}",
                wire.format_version, VM_CONTINUATION_FORMAT_VERSION
            )));
        }
        let continuation = Self {
            format_version: wire.format_version,
            instruction_pointer: wire.instruction_pointer,
            active_function: wire.active_function,
            operand_stack: wire.operand_stack,
            last_value: wire.last_value,
            slots: wire.slots,
            projected_slots: wire.projected_slots,
            globals: wire.globals,
            iterator_stack: wire.iterator_stack,
            frame_stack: wire.frame_stack,
            handler_stack: wire.handler_stack,
            finally_stack: wire.finally_stack,
            occurrence_counters: wire.occurrence_counters,
            mode: wire.mode,
            profile: wire.profile,
            pending_error_span: wire.pending_error_span,
            instructions_executed: wire.instructions_executed,
            active_execution_elapsed: wire.active_execution_elapsed,
            heap: wire.heap,
        };
        validate_continuation(&continuation).map_err(serde::de::Error::custom)?;
        Ok(continuation)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmHandlerContinuation {
    pub handler_instruction_pointer: usize,
    pub finally_instruction_pointer: Option<usize>,
    pub catches: bool,
    pub frame_depth: usize,
    pub frame_function: Option<u32>,
    pub operand_stack_depth: usize,
    pub iterator_stack_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmFinallyContinuation {
    pub completion: VmFinallyCompletionContinuation,
    pub handler_stack_depth: usize,
    pub frame_depth: usize,
    pub frame_function: Option<u32>,
    pub operand_stack_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VmFinallyCompletionContinuation {
    Normal {
        resume_instruction_pointer: usize,
    },
    Throw {
        #[serde(
            serialize_with = "continuation_serde::serialize_value",
            deserialize_with = "continuation_serde::deserialize_value"
        )]
        value: Value,
        /// The typed runtime failure this throw was raised from, present only
        /// when the VM routed a `RuntimeError` rather than an explicit
        /// `throw`. It is what the trap re-raises if the cleanup chain ends
        /// with no catch, so it is carried durably rather than rebuilt.
        origin: Option<VmPendingErrorOriginContinuation>,
    },
}

/// A pending runtime failure travelling through a cleanup chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmPendingErrorOriginContinuation {
    pub error: RuntimeError,
    pub instruction_pointer: usize,
    pub span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmIteratorContinuation {
    pub cursor: VmIteratorCursor,
    pub binding_slot: usize,
    #[serde(
        serialize_with = "continuation_serde::serialize_optional_value",
        deserialize_with = "continuation_serde::deserialize_optional_value"
    )]
    pub restore_value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VmIteratorCursor {
    List {
        #[serde(
            serialize_with = "continuation_serde::serialize_values",
            deserialize_with = "continuation_serde::deserialize_values"
        )]
        values: Vec<Value>,
        next_index: usize,
    },
    Range {
        next: i64,
        end: i64,
        step: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VmProfileContinuation {
    pub instruction_counts: Vec<u64>,
    pub instruction_times: Vec<u128>,
    pub builtin_counts: Vec<u64>,
    pub builtin_times: Vec<u128>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ContinuationError {
    #[error("continuation format version {found} is incompatible with version {expected}")]
    FormatVersionMismatch { expected: u32, found: u32 },
    #[error("continuation function index exceeds the durable u32 index space")]
    FunctionIndexOverflow,
    #[error("continuation closure function index {index} is not present in the compiled program")]
    UnknownFunction { index: u32 },
    #[error("continuation closure function {index} requires {expected} capture(s), found {actual}")]
    ClosureCaptureCountMismatch {
        index: u32,
        expected: usize,
        actual: usize,
    },
    #[error("cannot capture VM continuation: `{variant}` value at {location} is not serializable")]
    UnserializableValue {
        location: String,
        variant: &'static str,
    },
    #[error(
        "continuation {location} instruction pointer {instruction_pointer} is outside {owner} code range {range_start}..{range_end}"
    )]
    InstructionPointerOutsideCodeRange {
        location: String,
        instruction_pointer: usize,
        owner: String,
        range_start: usize,
        range_end: usize,
    },
    #[error(
        "continuation frame {frame} return instruction pointer {instruction_pointer} is not immediately after a call site"
    )]
    InvalidReturnSite {
        frame: usize,
        instruction_pointer: usize,
    },
    #[error("continuation with an active function must have a root-owned bottom frame")]
    MissingRootFrame,
    #[error("continuation has {actual} slots but program requires {expected}")]
    SlotCountMismatch { expected: usize, actual: usize },
    #[error(
        "continuation iterator {iterator} binds slot {binding_slot}, but only {slot_count} slots exist"
    )]
    IteratorBindingOutOfBounds {
        iterator: usize,
        binding_slot: usize,
        slot_count: usize,
    },
    #[error(
        "continuation frame {frame} iterator {iterator} binds slot {binding_slot}, but only {slot_count} slots exist"
    )]
    FrameIteratorBindingOutOfBounds {
        frame: usize,
        iterator: usize,
        binding_slot: usize,
        slot_count: usize,
    },
    #[error("continuation iterator {iterator} has a zero range step")]
    ZeroRangeStep { iterator: usize },
    #[error("continuation frame {frame} iterator {iterator} has a zero range step")]
    FrameZeroRangeStep { frame: usize, iterator: usize },
    #[error(
        "continuation handler {handler} frame depth {frame_depth} exceeds frame stack depth {frame_count}"
    )]
    HandlerFrameDepthOutOfBounds {
        handler: usize,
        frame_depth: usize,
        frame_count: usize,
    },
    #[error(
        "continuation handler {handler} frame identity does not match frame depth {frame_depth}"
    )]
    HandlerFrameIdentityMismatch { handler: usize, frame_depth: usize },
    #[error(
        "continuation handler {handler} operand stack depth {stack_depth} exceeds stack size {stack_size}"
    )]
    HandlerStackDepthOutOfBounds {
        handler: usize,
        stack_depth: usize,
        stack_size: usize,
    },
    #[error(
        "continuation handler {handler} iterator stack depth {iterator_depth} exceeds owner size {iterator_count}"
    )]
    HandlerIteratorDepthOutOfBounds {
        handler: usize,
        iterator_depth: usize,
        iterator_count: usize,
    },
    #[error("continuation finally {finally} frame identity is invalid")]
    FinallyFrameIdentityMismatch { finally: usize },
    #[error(
        "continuation finally {finally} handler depth {handler_depth} exceeds handler stack size {handler_count}"
    )]
    FinallyHandlerDepthOutOfBounds {
        finally: usize,
        handler_depth: usize,
        handler_count: usize,
    },
    #[error(
        "continuation finally {finally} operand stack depth {stack_depth} exceeds stack size {stack_size}"
    )]
    FinallyStackDepthOutOfBounds {
        finally: usize,
        stack_depth: usize,
        stack_size: usize,
    },
    #[error(
        "continuation handler {handler} names no exception scope in the compiled program (handler {handler_instruction_pointer}, finally {finally_instruction_pointer:?}, catches {catches})"
    )]
    HandlerScopeUnknown {
        handler: usize,
        handler_instruction_pointer: usize,
        finally_instruction_pointer: Option<usize>,
        catches: bool,
    },
    #[error(
        "continuation handler {handler} is not live at instruction {anchor}: its scope covers ({push_ip}, {end_ip}]"
    )]
    HandlerScopeNotLive {
        handler: usize,
        anchor: usize,
        push_ip: usize,
        end_ip: usize,
    },
    #[error("continuation handler {handler} is not nested inside handler {outer}: {reason}")]
    HandlerNestingNotMonotonic {
        handler: usize,
        outer: usize,
        reason: &'static str,
    },
    #[error("continuation finally {finally} is not nested inside finally {outer}: {reason}")]
    FinallyNestingNotMonotonic {
        finally: usize,
        outer: usize,
        reason: &'static str,
    },
    #[error("continuation profile shape is incompatible with this VM")]
    ProfileShapeMismatch,
    #[error("lashlang instruction budget of {limit} instructions was already exceeded")]
    InstructionBudgetExceeded { limit: u64 },
    #[error("lashlang frame depth limit of {limit} frames was already exceeded")]
    FrameDepthExceeded { limit: u64 },
    #[error("lashlang active-execution deadline of {limit_ms}ms was already exceeded")]
    ExecutionDeadlineExceeded { limit_ms: u128 },
    #[error(
        "lashlang logical memory limit of {limit} bytes was already exceeded by {live} live bytes"
    )]
    MemoryLimitExceeded { limit: u64, live: u64 },
}

impl ContinuationError {
    pub fn is_execution_bound_exhausted(&self) -> bool {
        matches!(
            self,
            Self::InstructionBudgetExceeded { .. }
                | Self::FrameDepthExceeded { .. }
                | Self::ExecutionDeadlineExceeded { .. }
                | Self::MemoryLimitExceeded { .. }
        )
    }
}
