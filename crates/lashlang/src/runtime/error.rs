use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{ModuleRef, ProcessRef};
use thiserror::Error;

use super::ExecutionHostError;

/// A failure while interpolating arguments into a format template.
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormatError {
    /// The template contains an opening brace without a matching closing brace.
    #[error("unmatched `{{` in format string")]
    UnmatchedOpenBrace,
    /// The template contains a closing brace without a matching opening brace.
    #[error("unmatched `}}` in format string")]
    UnmatchedCloseBrace,
    /// The template contains a placeholder that is neither empty nor an index.
    #[error("invalid format placeholder")]
    InvalidPlaceholder,
    /// The template combines automatic and explicitly indexed placeholders.
    #[error("can't mix `{{}}` and indexed format placeholders")]
    MixedPlaceholderKinds,
    /// The template contains a slot that is not a valid argument index.
    #[error("bad format slot `{slot}`")]
    InvalidSlot { slot: String },
    /// The template refers to an argument index outside the supplied arguments.
    #[error("format slot `{slot}` is out of range")]
    SlotOutOfRange { slot: String },
    /// A supplied format argument is not referenced by the template.
    #[error("format argument `{index}` is unused")]
    UnusedArgument { index: usize },
}

/// A typed failure raised while executing compiled Lashlang code.
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Serialize, Deserialize)]
pub enum RuntimeError {
    /// Guest recursion exceeded the configured VM frame-depth limit.
    #[error("lashlang frame depth limit of {limit} frames was exceeded")]
    FrameDepthExceeded { limit: u64 },
    /// A closure's stable function-table index cannot fit the durable wire.
    #[error("lashlang function table exceeds the durable function index space")]
    FunctionIndexOverflow,
    /// A value used as a function was not a closure.
    #[error("attempted to call a non-function {actual}")]
    NonFunctionCall { actual: String },
    /// A closure was called with the wrong number of arguments.
    #[error("function takes {expected} arg(s), got {actual}")]
    FunctionArgumentCount { expected: usize, actual: usize },
    /// Closure metadata did not match the compiled function table.
    #[error("closure function index {index} is not present in the compiled program")]
    UnknownFunction { index: u32 },
    /// A closure's captures do not match its compiled function definition.
    #[error("closure function {index} requires {expected} capture(s), found {actual}")]
    ClosureCaptureCountMismatch {
        index: u32,
        expected: usize,
        actual: usize,
    },
    /// Function values cannot cross host-facing value boundaries.
    #[error("function values cannot cross a lashlang host boundary")]
    FunctionValueAtHostBoundary,
    /// JavaScript exotic objects have no detached host-facing value shape.
    #[error("JavaScript {kind} values cannot cross a lashlang host boundary")]
    JavaScriptExoticAtHostBoundary { kind: String },
    /// Effects from callbacks require a resumable builtin protocol not yet present.
    #[error("effects are not supported inside builtin callbacks")]
    EffectInBuiltinCallback,
    /// Active VM execution exceeded its explicit instruction budget.
    #[error("lashlang instruction budget of {limit} instructions exceeded")]
    InstructionBudgetExceeded { limit: u64 },
    /// Active VM execution exceeded its explicit deadline.
    #[error("lashlang execution deadline of {limit_ms}ms exceeded")]
    ExecutionDeadlineExceeded { limit_ms: u128 },
    /// Logical heap usage exceeded the versioned memory schedule limit.
    #[error(
        "lashlang logical memory limit of {limit} bytes exceeded (allocation would reach {attempted} bytes)"
    )]
    MemoryLimitExceeded { limit: u64, attempted: u64 },
    /// The host cancelled this VM execution. Cancellation is an execution
    /// terminal and is never presented to guest handlers.
    #[error("lashlang execution was cancelled by the host")]
    HostCancelled,
    /// A heap reference named an object that has already been swept.
    #[error("dangling lashlang heap reference {id}")]
    DanglingHeapReference { id: u64 },
    /// The deterministic allocation identity counter was exhausted.
    #[error("lashlang heap allocation identity space exhausted")]
    HeapIdExhausted,
    /// A heap reference reached a boundary that requires an exported value.
    ///
    /// The instruction heap plan is what keeps references off these paths. This
    /// error is the backstop for an opcode that forgets to declare what it
    /// reads: the cell fails, the process lives.
    #[error("lashlang heap reference {id} reached {context} before it was exported")]
    UnexportedHeapReference { id: u64, context: Cow<'static, str> },
    /// A host boundary cannot represent cyclic heap values.
    #[error("lashlang heap value contains a cycle through object {id}")]
    CyclicHostValue { id: u64 },
    /// Execution referenced a binding that is not defined.
    #[error("unknown name `{name}`")]
    UndefinedVariable { name: String },
    /// A `for` loop received a value that is not iterable.
    #[error("`for` expects a list or tuple")]
    NonListIteration,
    /// A process-administration keyword was used outside a process body.
    #[error("`{keyword}` can only be used inside a process body")]
    SessionProcessAdminOutsideProcess { keyword: Cow<'static, str> },
    /// A foreground-only control keyword was used inside a process body.
    #[error("`{keyword}` can't be used inside a process body")]
    ForegroundControlInsideProcess { keyword: Cow<'static, str> },
    /// Execution referenced a builtin that is not defined.
    #[error("unknown builtin `{name}`")]
    UnknownBuiltin { name: String },

    /// Field access targeted a value that does not expose the requested field.
    #[error("can't read `.{field}` from {actual}")]
    CannotReadField { field: String, actual: String },
    /// The `?` operator received a value that is not a tool-result wrapper.
    #[error("`?` expected a tool result wrapper, got {actual}")]
    ToolResultExpected { actual: String },
    /// A successful tool-result wrapper did not contain a `value` field.
    #[error("`?` found a successful tool result wrapper missing `value`")]
    ToolResultMissingValue,
    /// A tool-result wrapper did not contain a boolean `ok` field.
    #[error("`?` expected a tool result wrapper with boolean `ok`")]
    ToolResultInvalidOk,
    /// Indexing targeted a value that does not support indexing.
    #[error("can't index {actual}")]
    CannotIndex { actual: String },
    /// Assignment targeted an image field, but image fields are immutable.
    #[error("can't assign image fields; images are immutable")]
    ImmutableImageFields,
    /// Assignment traversed an image field, but image fields are immutable.
    #[error("can't assign through image fields; images are immutable")]
    ImmutableImageFieldsThrough,
    /// Assignment targeted a tuple index, but tuple indexes are immutable.
    #[error("can't assign tuple indexes; tuples are immutable")]
    ImmutableTupleIndexes,
    /// Assignment traversed a tuple index, but tuple indexes are immutable.
    #[error("can't assign through tuple indexes; tuples are immutable")]
    ImmutableTupleIndexesThrough,
    /// Field assignment targeted a value that does not support it.
    #[error("can't assign `.{field}` on {actual}")]
    CannotAssignField { field: String, actual: String },
    /// Nested field assignment traversed a value that does not support it.
    #[error("can't assign through `.{field}` on {actual}")]
    CannotAssignThroughField { field: String, actual: String },
    /// Index assignment targeted a value that does not support it.
    #[error("can't assign index on {actual}")]
    CannotAssignIndex { actual: String },
    /// Nested index assignment traversed a value that does not support it.
    #[error("can't assign through index on {actual}")]
    CannotAssignThroughIndex { actual: String },
    /// List assignment used an index that is not an integer.
    #[error("list assignment index must be an integer")]
    InvalidListAssignmentIndex,
    /// TypeScript array assignment targeted a property the v1 heap cannot store.
    #[error(
        "TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED: array property `{key}` is not representable in the v1 heap"
    )]
    TypeScriptArrayNonIndexPropertyUnsupported { key: String },
    /// A builtin received the wrong number of arguments.
    #[error("`{name}` takes {expected} arg(s), got {actual}")]
    InvalidArgumentCount {
        name: String,
        expected: String,
        actual: usize,
    },
    /// `empty` received a value outside its supported types.
    #[error("`empty` requires a string, tuple, list, record, or null")]
    EmptyUnsupported,
    /// `keys` received a value outside its supported types.
    #[error("`keys` requires a record or null")]
    KeysUnsupported,
    /// `values` received a value outside its supported types.
    #[error("`values` requires a record or null")]
    ValuesUnsupported,
    /// `slice` received a value outside its supported types.
    #[error("`slice` requires a string, tuple, or list")]
    SliceUnsupported,
    /// `format` was called without a template argument.
    #[error("`format` requires at least a template string")]
    FormatTemplateMissing,
    /// `format` received a template argument that is not text.
    #[error("`format` template must be a string, got {actual}")]
    FormatTemplateInvalid { actual: String },
    /// `len` received a value outside its supported types.
    #[error("`len` requires a string, tuple, list, record, or null; use `.size` for images")]
    LenUnsupported,
    /// `contains` received an unsupported haystack and needle pair.
    #[error(
        "`contains` requires a string/string, tuple/value, list/value, record/key, or null/value pair"
    )]
    ContainsUnsupported,
    /// The `in` operator received an unsupported haystack and needle pair.
    #[error(
        "`in` requires a string/string, tuple/value, list/value, record/key, or null/value pair"
    )]
    InUnsupported,
    /// `join` received a first argument that is neither a tuple nor a list.
    #[error("`join` requires a tuple or list as the first argument")]
    JoinUnsupported,
    /// `push` received a first argument that is not a list.
    #[error("`push` requires a list as the first argument")]
    PushUnsupported,
    /// A shaping builtin received a value that is not a list or tuple.
    #[error("`{builtin}` requires a list or tuple, got {actual}")]
    ShapingListRequired {
        builtin: Cow<'static, str>,
        actual: String,
    },
    /// A text-shaping builtin received a non-text argument.
    #[error("`{builtin}` {argument} must be text, got {actual}")]
    ShapingTextRequired {
        builtin: Cow<'static, str>,
        argument: Cow<'static, str>,
        actual: String,
    },
    /// A numeric aggregation encountered a non-number list element.
    #[error("`{builtin}` item {index} must be a number, got {actual}")]
    ShapingNumberRequired {
        builtin: Cow<'static, str>,
        index: usize,
        actual: String,
    },
    /// An ordering builtin encountered a value that cannot share one ordering.
    #[error("`{builtin}` item {index} ({actual}) is not comparable with item 0 ({reference})")]
    ShapingComparableRequired {
        builtin: Cow<'static, str>,
        index: usize,
        reference: String,
        actual: String,
    },
    /// An extrema builtin received an empty list.
    #[error("`{builtin}` requires a non-empty list")]
    ShapingEmptyList { builtin: Cow<'static, str> },
    /// `sort_by` received an item that was not a record.
    #[error("`sort_by` item {index} must be a record, got {actual}")]
    SortByRecordRequired { index: usize, actual: String },
    /// `sort_by` received an empty field path.
    #[error("`sort_by` field path must not be empty")]
    SortByEmptyPath,
    /// `sort_by` could not resolve its field path on a list item.
    #[error("`sort_by` item {index} is missing field path `{path}`")]
    SortByMissingPath { path: String, index: usize },
    /// `range` received a bound that is not a finite integer.
    #[error("`range` bounds must be finite integers")]
    InvalidRangeBound,
    /// `range` received a bound of an unsupported value type.
    #[error("`range` bounds must be finite integers, got {actual}")]
    InvalidRangeBoundType { actual: String },
    /// Integer division received an argument that is not a finite integer.
    #[error("`{builtin}` {argument} must be a finite integer")]
    InvalidIntegerDivisionArgument {
        builtin: Cow<'static, str>,
        argument: Cow<'static, str>,
    },
    /// Integer division received an argument of an unsupported value type.
    #[error("`{builtin}` {argument} must be a finite integer, got {actual}")]
    InvalidIntegerDivisionArgumentType {
        builtin: Cow<'static, str>,
        argument: Cow<'static, str>,
        actual: String,
    },
    /// A numeric operation received a value that is not numeric.
    #[error("expected a number")]
    ExpectedNumber,
    /// A numeric operation received a value of an unsupported type.
    #[error("expected a number, got {actual}")]
    ExpectedNumberType { actual: String },
    /// A text operation received a value of an unsupported type.
    #[error("expected text, got {actual}")]
    ExpectedText { actual: String },
    /// An index was not an integer.
    #[error("index must be an integer")]
    InvalidIndex,
    /// A character index was not a non-negative integer.
    #[error("`{builtin}` {argument} must be a non-negative integer")]
    InvalidCharacterIndex {
        builtin: Cow<'static, str>,
        argument: Cow<'static, str>,
    },
    /// Concatenation mixed list and tuple values.
    #[error("can't concatenate list and tuple")]
    IncompatibleSequenceConcatenation,
    /// Assignment targeted a projected binding that is read-only.
    #[error("`{name}` is a read-only projected binding")]
    ReadOnlyProjectedBinding { name: String },
    /// `validate` received a second argument that is not a type literal.
    #[error("`validate` requires a Type literal as the second argument")]
    ValidateTypeLiteralRequired,
    /// A referenced binding is not a Lashlang type value.
    #[error("`{name}` is not a Type value (missing `$lash_type`)")]
    NotTypeValue { name: String },

    /// The `?` operator unwrapped a failed tool result.
    #[error("`?` unwrapped failed tool result: {message}")]
    UnwrappedToolResultFailed { message: String },
    /// The `?` operator unwrapped a failed module operation.
    #[error("`?` unwrapped failed module operation: {source}")]
    UnwrappedModuleOperationFailed { source: ExecutionHostError },
    /// Assignment bytecode omitted its required index operand.
    #[error("missing assignment index")]
    MissingAssignmentIndex,
    /// Nested assignment traversed a missing record field.
    #[error("can't assign through missing field `.{field}`")]
    MissingAssignmentField { field: String },
    /// Nested assignment traversed a missing record key.
    #[error("can't assign through missing key `{key}`")]
    MissingAssignmentKey { key: String },
    /// List assignment addressed an index outside the list.
    #[error("list assignment index out of bounds")]
    ListAssignmentIndexOutOfBounds,
    /// JSON decoding failed for the supplied value.
    #[error("invalid json: {detail}")]
    InvalidJson { detail: String },
    /// `grep_text` received an empty needle.
    #[error("`grep_text` needle must not be empty")]
    EmptyGrepNeedle,
    /// Formatting a template and its arguments failed.
    #[error(transparent)]
    Format(FormatError),
    /// `range` received a zero step.
    #[error("`range` step must not be 0")]
    ZeroRangeStep,
    /// `range` would allocate more items than the configured limit.
    #[error("`range` would create more than {limit} items")]
    RangeTooLarge { limit: i128 },
    /// Integer division received a zero divisor.
    #[error("`{builtin}` divisor must not be 0")]
    IntegerDivisionByZero { builtin: Cow<'static, str> },
    /// Process execution referenced an unknown process name.
    #[error("unknown process `{name}`")]
    UnknownProcess { name: String },
    /// A linked module does not export the requested process name.
    #[error("linked module does not export process `{name}`")]
    ProcessNotExported { name: String },
    /// A module artifact does not export the requested process reference.
    #[error("module artifact `{module_ref}` does not export process ref {process_ref:?}")]
    ProcessRefNotExported {
        module_ref: ModuleRef,
        process_ref: ProcessRef,
    },
    /// A module artifact is missing the requested process name.
    #[error("module artifact `{module_ref}` is missing process `{name}`")]
    ArtifactProcessMissing { module_ref: ModuleRef, name: String },
    /// Runtime value validation failed.
    #[error("validation failed: {reason}")]
    ValidationFailed { reason: String },
    /// Process start was attempted without a deterministic execution site.
    #[error("`start` requires a deterministic lashlang execution site")]
    StartSiteMissing,
    /// Process start was attempted without a linked module artifact.
    #[error("`start` requires a linked lashlang module artifact")]
    LinkedArtifactMissing,
    /// The linked module does not export the process requested for start.
    #[error("linked lashlang module `{module_ref}` does not export process `{name}`")]
    LinkedProcessNotExported { module_ref: ModuleRef, name: String },
    /// Starting a process through the execution host failed.
    #[error("process start failed: {source}")]
    ProcessStartFailed { source: ExecutionHostError },
    /// Sleeping through the execution host failed.
    #[error("sleep failed: {source}")]
    SleepFailed { source: ExecutionHostError },
    /// Waiting for a process signal through the execution host failed.
    #[error("wait_signal failed: {source}")]
    WaitSignalFailed { source: ExecutionHostError },
    /// Sending a process signal through the execution host failed.
    #[error("signal_run failed: {source}")]
    SignalRunFailed { source: ExecutionHostError },
    /// Cancelling a process through the execution host failed.
    #[error("cancel failed: {source}")]
    CancelFailed { source: ExecutionHostError },
    /// Appending a process event through the execution host failed.
    #[error("process event failed: {source}")]
    ProcessEventFailed { source: ExecutionHostError },
    /// Printing through the execution host failed.
    #[error("print failed: {source}")]
    PrintFailed { source: ExecutionHostError },
    /// Finishing a process through the execution host failed.
    #[error("finish failed: {source}")]
    FinishFailed { source: ExecutionHostError },
    /// Failing a process through the execution host failed.
    #[error("fail failed: {source}")]
    FailFailed { source: ExecutionHostError },
    /// A resource-operation batch referenced a missing receiver.
    #[error("resource operation batch receiver index out of range")]
    ResourceBatchReceiverOutOfRange,
    /// A resource-operation batch referenced a missing argument.
    #[error("resource operation batch argument index out of range")]
    ResourceBatchArgumentOutOfRange,
    /// A resource-operation batch returned a result with an invalid shape.
    #[error("resource operation batch returned invalid result")]
    InvalidResourceBatchResult,
    /// Executing a resource-operation batch through the host failed.
    #[error("resource operation batch failed: {source}")]
    ResourceBatchFailed { source: ExecutionHostError },
    /// A resource-operation batch returned the wrong number of results.
    #[error("resource operation batch returned {actual} results for {expected} operations")]
    ResourceBatchResultCount { actual: usize, expected: usize },
    /// A resource-operation batch reported a settlement order that is not an
    /// ordering of its own results.
    #[error("resource operation batch settlement order is unusable: {problem}")]
    ResourceBatchSettlementOrder { problem: String },
    /// Aggregate-await bytecode referenced a missing leaf.
    #[error("aggregate await leaf index out of range")]
    AggregateAwaitLeafOutOfRange,
    /// Aggregate-await bytecode referenced a missing value.
    #[error("aggregate await value index out of range")]
    AggregateAwaitValueOutOfRange,
    /// Aggregate-await bytecode received an invalid record shape.
    #[error("aggregate await record shape is invalid")]
    InvalidAggregateAwaitRecordShape,
    /// Execution attempted to pop a value from an empty VM stack.
    #[error("vm stack underflow")]
    VmStackUnderflow,
    /// Loop bytecode executed without its required loop state.
    #[error("missing loop state")]
    MissingLoopState,
    /// A context-dependent intrinsic reached a generic path without an explicit arm.
    #[error("context-dependent intrinsic reached generic {context}")]
    ContextDependentIntrinsicMisdispatch { context: Cow<'static, str> },
    /// An explicitly thrown value escaped every handler.
    #[error("uncaught lashlang exception: {value}")]
    UncaughtException { value: super::Value },
    /// Bytecode violated the structured handler/finally stack discipline.
    #[error("invalid lashlang exception state: {reason}")]
    InvalidExceptionState { reason: Cow<'static, str> },
}

impl RuntimeError {
    /// Terminals which are structurally forbidden from consulting guest
    /// handlers. This is intentionally the only taxonomy classification used
    /// by the VM's error exit.
    /// Which taxonomy row this error belongs to.
    ///
    /// The match is exhaustive on purpose: the taxonomy decides whether an
    /// error bypasses guest handlers and whether the host classifies it as an
    /// effect failure, and a new variant that silently defaulted to "catchable
    /// runtime error" is exactly the hand-maintained-parallel-list defect this
    /// layer exists to avoid. Adding a variant fails to compile until it
    /// declares its class here.
    pub fn taxonomy(&self) -> ErrorTaxonomy {
        match self {
            Self::FrameDepthExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::FunctionIndexOverflow => ErrorTaxonomy::Catchable,
            Self::NonFunctionCall { .. } => ErrorTaxonomy::Catchable,
            Self::FunctionArgumentCount { .. } => ErrorTaxonomy::Catchable,
            Self::UnknownFunction { .. } => ErrorTaxonomy::Catchable,
            Self::ClosureCaptureCountMismatch { .. } => ErrorTaxonomy::Catchable,
            Self::FunctionValueAtHostBoundary => ErrorTaxonomy::Catchable,
            Self::JavaScriptExoticAtHostBoundary { .. } => ErrorTaxonomy::Catchable,
            Self::EffectInBuiltinCallback => ErrorTaxonomy::Catchable,
            Self::InstructionBudgetExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::ExecutionDeadlineExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::MemoryLimitExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::HostCancelled => ErrorTaxonomy::UncatchableTerminal,
            Self::DanglingHeapReference { .. } => ErrorTaxonomy::Catchable,
            Self::HeapIdExhausted => ErrorTaxonomy::Catchable,
            Self::UnexportedHeapReference { .. } => ErrorTaxonomy::Catchable,
            Self::CyclicHostValue { .. } => ErrorTaxonomy::Catchable,
            Self::UndefinedVariable { .. } => ErrorTaxonomy::Catchable,
            Self::NonListIteration => ErrorTaxonomy::Catchable,
            Self::SessionProcessAdminOutsideProcess { .. } => ErrorTaxonomy::Catchable,
            Self::ForegroundControlInsideProcess { .. } => ErrorTaxonomy::Catchable,
            Self::UnknownBuiltin { .. } => ErrorTaxonomy::Catchable,
            Self::CannotReadField { .. } => ErrorTaxonomy::Catchable,
            Self::ToolResultExpected { .. } => ErrorTaxonomy::Catchable,
            Self::ToolResultMissingValue => ErrorTaxonomy::Catchable,
            Self::ToolResultInvalidOk => ErrorTaxonomy::Catchable,
            Self::CannotIndex { .. } => ErrorTaxonomy::Catchable,
            Self::ImmutableImageFields => ErrorTaxonomy::Catchable,
            Self::ImmutableImageFieldsThrough => ErrorTaxonomy::Catchable,
            Self::ImmutableTupleIndexes => ErrorTaxonomy::Catchable,
            Self::ImmutableTupleIndexesThrough => ErrorTaxonomy::Catchable,
            Self::CannotAssignField { .. } => ErrorTaxonomy::Catchable,
            Self::CannotAssignThroughField { .. } => ErrorTaxonomy::Catchable,
            Self::CannotAssignIndex { .. } => ErrorTaxonomy::Catchable,
            Self::CannotAssignThroughIndex { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidListAssignmentIndex => ErrorTaxonomy::Catchable,
            Self::TypeScriptArrayNonIndexPropertyUnsupported { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidArgumentCount { .. } => ErrorTaxonomy::Catchable,
            Self::EmptyUnsupported => ErrorTaxonomy::Catchable,
            Self::KeysUnsupported => ErrorTaxonomy::Catchable,
            Self::ValuesUnsupported => ErrorTaxonomy::Catchable,
            Self::SliceUnsupported => ErrorTaxonomy::Catchable,
            Self::FormatTemplateMissing => ErrorTaxonomy::Catchable,
            Self::FormatTemplateInvalid { .. } => ErrorTaxonomy::Catchable,
            Self::LenUnsupported => ErrorTaxonomy::Catchable,
            Self::ContainsUnsupported => ErrorTaxonomy::Catchable,
            Self::InUnsupported => ErrorTaxonomy::Catchable,
            Self::JoinUnsupported => ErrorTaxonomy::Catchable,
            Self::PushUnsupported => ErrorTaxonomy::Catchable,
            Self::ShapingListRequired { .. } => ErrorTaxonomy::Catchable,
            Self::ShapingTextRequired { .. } => ErrorTaxonomy::Catchable,
            Self::ShapingNumberRequired { .. } => ErrorTaxonomy::Catchable,
            Self::ShapingComparableRequired { .. } => ErrorTaxonomy::Catchable,
            Self::ShapingEmptyList { .. } => ErrorTaxonomy::Catchable,
            Self::SortByRecordRequired { .. } => ErrorTaxonomy::Catchable,
            Self::SortByEmptyPath => ErrorTaxonomy::Catchable,
            Self::SortByMissingPath { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidRangeBound => ErrorTaxonomy::Catchable,
            Self::InvalidRangeBoundType { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidIntegerDivisionArgument { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidIntegerDivisionArgumentType { .. } => ErrorTaxonomy::Catchable,
            Self::ExpectedNumber => ErrorTaxonomy::Catchable,
            Self::ExpectedNumberType { .. } => ErrorTaxonomy::Catchable,
            Self::ExpectedText { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidIndex => ErrorTaxonomy::Catchable,
            Self::InvalidCharacterIndex { .. } => ErrorTaxonomy::Catchable,
            Self::IncompatibleSequenceConcatenation => ErrorTaxonomy::Catchable,
            Self::ReadOnlyProjectedBinding { .. } => ErrorTaxonomy::Catchable,
            Self::ValidateTypeLiteralRequired => ErrorTaxonomy::Catchable,
            Self::NotTypeValue { .. } => ErrorTaxonomy::Catchable,
            Self::UnwrappedToolResultFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::UnwrappedModuleOperationFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::MissingAssignmentIndex => ErrorTaxonomy::Catchable,
            Self::MissingAssignmentField { .. } => ErrorTaxonomy::Catchable,
            Self::MissingAssignmentKey { .. } => ErrorTaxonomy::Catchable,
            Self::ListAssignmentIndexOutOfBounds => ErrorTaxonomy::Catchable,
            Self::InvalidJson { .. } => ErrorTaxonomy::Catchable,
            Self::EmptyGrepNeedle => ErrorTaxonomy::Catchable,
            Self::Format(_) => ErrorTaxonomy::Catchable,
            Self::ZeroRangeStep => ErrorTaxonomy::Catchable,
            Self::RangeTooLarge { .. } => ErrorTaxonomy::Catchable,
            Self::IntegerDivisionByZero { .. } => ErrorTaxonomy::Catchable,
            Self::UnknownProcess { .. } => ErrorTaxonomy::Catchable,
            Self::ProcessNotExported { .. } => ErrorTaxonomy::Catchable,
            Self::ProcessRefNotExported { .. } => ErrorTaxonomy::Catchable,
            Self::ArtifactProcessMissing { .. } => ErrorTaxonomy::Catchable,
            Self::ValidationFailed { .. } => ErrorTaxonomy::Catchable,
            Self::StartSiteMissing => ErrorTaxonomy::Catchable,
            Self::LinkedArtifactMissing => ErrorTaxonomy::Catchable,
            Self::LinkedProcessNotExported { .. } => ErrorTaxonomy::Catchable,
            Self::ProcessStartFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::SleepFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::WaitSignalFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::SignalRunFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::CancelFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::ProcessEventFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::PrintFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::FinishFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::FailFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::ResourceBatchReceiverOutOfRange => ErrorTaxonomy::Catchable,
            Self::ResourceBatchArgumentOutOfRange => ErrorTaxonomy::Catchable,
            Self::InvalidResourceBatchResult => ErrorTaxonomy::Catchable,
            Self::ResourceBatchFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::ResourceBatchResultCount { .. } => ErrorTaxonomy::Catchable,
            Self::ResourceBatchSettlementOrder { .. } => ErrorTaxonomy::Catchable,
            Self::AggregateAwaitLeafOutOfRange => ErrorTaxonomy::Catchable,
            Self::AggregateAwaitValueOutOfRange => ErrorTaxonomy::Catchable,
            Self::InvalidAggregateAwaitRecordShape => ErrorTaxonomy::Catchable,
            Self::VmStackUnderflow => ErrorTaxonomy::Catchable,
            Self::MissingLoopState => ErrorTaxonomy::Catchable,
            Self::ContextDependentIntrinsicMisdispatch { .. } => ErrorTaxonomy::Catchable,
            Self::UncaughtException { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidExceptionState { .. } => ErrorTaxonomy::UncatchableTerminal,
        }
    }

    /// Stable guest-visible identity for a catchable runtime failure.
    ///
    /// Guest code branches on this string, so it is an explicit table rather
    /// than anything derived from Rust identifiers: renaming a variant must be
    /// a deliberate change to the guest contract, not a side effect. The
    /// pinning test in this module enumerates every code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::FrameDepthExceeded { .. } => "FrameDepthExceeded",
            Self::FunctionIndexOverflow => "FunctionIndexOverflow",
            Self::NonFunctionCall { .. } => "NonFunctionCall",
            Self::FunctionArgumentCount { .. } => "FunctionArgumentCount",
            Self::UnknownFunction { .. } => "UnknownFunction",
            Self::ClosureCaptureCountMismatch { .. } => "ClosureCaptureCountMismatch",
            Self::FunctionValueAtHostBoundary => "FunctionValueAtHostBoundary",
            Self::JavaScriptExoticAtHostBoundary { .. } => "JavaScriptExoticAtHostBoundary",
            Self::EffectInBuiltinCallback => "EffectInBuiltinCallback",
            Self::InstructionBudgetExceeded { .. } => "InstructionBudgetExceeded",
            Self::ExecutionDeadlineExceeded { .. } => "ExecutionDeadlineExceeded",
            Self::MemoryLimitExceeded { .. } => "MemoryLimitExceeded",
            Self::HostCancelled => "HostCancelled",
            Self::DanglingHeapReference { .. } => "DanglingHeapReference",
            Self::HeapIdExhausted => "HeapIdExhausted",
            Self::UnexportedHeapReference { .. } => "UnexportedHeapReference",
            Self::CyclicHostValue { .. } => "CyclicHostValue",
            Self::UndefinedVariable { .. } => "UndefinedVariable",
            Self::NonListIteration => "NonListIteration",
            Self::SessionProcessAdminOutsideProcess { .. } => "SessionProcessAdminOutsideProcess",
            Self::ForegroundControlInsideProcess { .. } => "ForegroundControlInsideProcess",
            Self::UnknownBuiltin { .. } => "UnknownBuiltin",
            Self::CannotReadField { .. } => "CannotReadField",
            Self::ToolResultExpected { .. } => "ToolResultExpected",
            Self::ToolResultMissingValue => "ToolResultMissingValue",
            Self::ToolResultInvalidOk => "ToolResultInvalidOk",
            Self::CannotIndex { .. } => "CannotIndex",
            Self::ImmutableImageFields => "ImmutableImageFields",
            Self::ImmutableImageFieldsThrough => "ImmutableImageFieldsThrough",
            Self::ImmutableTupleIndexes => "ImmutableTupleIndexes",
            Self::ImmutableTupleIndexesThrough => "ImmutableTupleIndexesThrough",
            Self::CannotAssignField { .. } => "CannotAssignField",
            Self::CannotAssignThroughField { .. } => "CannotAssignThroughField",
            Self::CannotAssignIndex { .. } => "CannotAssignIndex",
            Self::CannotAssignThroughIndex { .. } => "CannotAssignThroughIndex",
            Self::InvalidListAssignmentIndex => "InvalidListAssignmentIndex",
            Self::TypeScriptArrayNonIndexPropertyUnsupported { .. } => {
                "TypeScriptArrayNonIndexPropertyUnsupported"
            }
            Self::InvalidArgumentCount { .. } => "InvalidArgumentCount",
            Self::EmptyUnsupported => "EmptyUnsupported",
            Self::KeysUnsupported => "KeysUnsupported",
            Self::ValuesUnsupported => "ValuesUnsupported",
            Self::SliceUnsupported => "SliceUnsupported",
            Self::FormatTemplateMissing => "FormatTemplateMissing",
            Self::FormatTemplateInvalid { .. } => "FormatTemplateInvalid",
            Self::LenUnsupported => "LenUnsupported",
            Self::ContainsUnsupported => "ContainsUnsupported",
            Self::InUnsupported => "InUnsupported",
            Self::JoinUnsupported => "JoinUnsupported",
            Self::PushUnsupported => "PushUnsupported",
            Self::ShapingListRequired { .. } => "ShapingListRequired",
            Self::ShapingTextRequired { .. } => "ShapingTextRequired",
            Self::ShapingNumberRequired { .. } => "ShapingNumberRequired",
            Self::ShapingComparableRequired { .. } => "ShapingComparableRequired",
            Self::ShapingEmptyList { .. } => "ShapingEmptyList",
            Self::SortByRecordRequired { .. } => "SortByRecordRequired",
            Self::SortByEmptyPath => "SortByEmptyPath",
            Self::SortByMissingPath { .. } => "SortByMissingPath",
            Self::InvalidRangeBound => "InvalidRangeBound",
            Self::InvalidRangeBoundType { .. } => "InvalidRangeBoundType",
            Self::InvalidIntegerDivisionArgument { .. } => "InvalidIntegerDivisionArgument",
            Self::InvalidIntegerDivisionArgumentType { .. } => "InvalidIntegerDivisionArgumentType",
            Self::ExpectedNumber => "ExpectedNumber",
            Self::ExpectedNumberType { .. } => "ExpectedNumberType",
            Self::ExpectedText { .. } => "ExpectedText",
            Self::InvalidIndex => "InvalidIndex",
            Self::InvalidCharacterIndex { .. } => "InvalidCharacterIndex",
            Self::IncompatibleSequenceConcatenation => "IncompatibleSequenceConcatenation",
            Self::ReadOnlyProjectedBinding { .. } => "ReadOnlyProjectedBinding",
            Self::ValidateTypeLiteralRequired => "ValidateTypeLiteralRequired",
            Self::NotTypeValue { .. } => "NotTypeValue",
            Self::UnwrappedToolResultFailed { .. } => "UnwrappedToolResultFailed",
            Self::UnwrappedModuleOperationFailed { .. } => "UnwrappedModuleOperationFailed",
            Self::MissingAssignmentIndex => "MissingAssignmentIndex",
            Self::MissingAssignmentField { .. } => "MissingAssignmentField",
            Self::MissingAssignmentKey { .. } => "MissingAssignmentKey",
            Self::ListAssignmentIndexOutOfBounds => "ListAssignmentIndexOutOfBounds",
            Self::InvalidJson { .. } => "InvalidJson",
            Self::EmptyGrepNeedle => "EmptyGrepNeedle",
            Self::Format(_) => "Format",
            Self::ZeroRangeStep => "ZeroRangeStep",
            Self::RangeTooLarge { .. } => "RangeTooLarge",
            Self::IntegerDivisionByZero { .. } => "IntegerDivisionByZero",
            Self::UnknownProcess { .. } => "UnknownProcess",
            Self::ProcessNotExported { .. } => "ProcessNotExported",
            Self::ProcessRefNotExported { .. } => "ProcessRefNotExported",
            Self::ArtifactProcessMissing { .. } => "ArtifactProcessMissing",
            Self::ValidationFailed { .. } => "ValidationFailed",
            Self::StartSiteMissing => "StartSiteMissing",
            Self::LinkedArtifactMissing => "LinkedArtifactMissing",
            Self::LinkedProcessNotExported { .. } => "LinkedProcessNotExported",
            Self::ProcessStartFailed { .. } => "ProcessStartFailed",
            Self::SleepFailed { .. } => "SleepFailed",
            Self::WaitSignalFailed { .. } => "WaitSignalFailed",
            Self::SignalRunFailed { .. } => "SignalRunFailed",
            Self::CancelFailed { .. } => "CancelFailed",
            Self::ProcessEventFailed { .. } => "ProcessEventFailed",
            Self::PrintFailed { .. } => "PrintFailed",
            Self::FinishFailed { .. } => "FinishFailed",
            Self::FailFailed { .. } => "FailFailed",
            Self::ResourceBatchReceiverOutOfRange => "ResourceBatchReceiverOutOfRange",
            Self::ResourceBatchArgumentOutOfRange => "ResourceBatchArgumentOutOfRange",
            Self::InvalidResourceBatchResult => "InvalidResourceBatchResult",
            Self::ResourceBatchFailed { .. } => "ResourceBatchFailed",
            Self::ResourceBatchResultCount { .. } => "ResourceBatchResultCount",
            Self::ResourceBatchSettlementOrder { .. } => "ResourceBatchSettlementOrder",
            Self::AggregateAwaitLeafOutOfRange => "AggregateAwaitLeafOutOfRange",
            Self::AggregateAwaitValueOutOfRange => "AggregateAwaitValueOutOfRange",
            Self::InvalidAggregateAwaitRecordShape => "InvalidAggregateAwaitRecordShape",
            Self::VmStackUnderflow => "VmStackUnderflow",
            Self::MissingLoopState => "MissingLoopState",
            Self::ContextDependentIntrinsicMisdispatch { .. } => {
                "ContextDependentIntrinsicMisdispatch"
            }
            Self::UncaughtException { .. } => "UncaughtException",
            Self::InvalidExceptionState { .. } => "InvalidExceptionState",
        }
    }

    pub(crate) fn is_uncatchable_terminal(&self) -> bool {
        matches!(self.taxonomy(), ErrorTaxonomy::UncatchableTerminal)
    }

    pub(crate) fn is_effect_failure(&self) -> bool {
        matches!(self.taxonomy(), ErrorTaxonomy::EffectFailure)
    }
}

/// How the VM and its host must treat a [`RuntimeError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorTaxonomy {
    /// A host, execution-bound or internal-invariant terminal: it ends
    /// execution and bypasses every guest handler. `InvalidExceptionState`
    /// belongs here because it is raised *by* the exception machinery, and
    /// routing it back through a handler stack that has already been shown
    /// inconsistent is the one place a catchable classification cannot be
    /// defended.
    UncatchableTerminal,
    /// A failure raised by a host effect. Catchable, and reported to the guest
    /// as an `EffectError`.
    EffectFailure,
    /// An ordinary catchable runtime failure.
    Catchable,
}

impl RuntimeError {
    pub fn is_execution_bound_exhausted(&self) -> bool {
        matches!(
            self,
            Self::InstructionBudgetExceeded { .. }
                | Self::ExecutionDeadlineExceeded { .. }
                | Self::MemoryLimitExceeded { .. }
                | Self::FrameDepthExceeded { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_error() -> ExecutionHostError {
        ExecutionHostError::new("host error")
    }

    #[test]
    fn every_runtime_error_display_is_exact() {
        let errors = vec![
            RuntimeError::FrameDepthExceeded { limit: 32 },
            RuntimeError::FunctionIndexOverflow,
            RuntimeError::NonFunctionCall {
                actual: "number".into(),
            },
            RuntimeError::FunctionArgumentCount {
                expected: 1,
                actual: 2,
            },
            RuntimeError::UnknownFunction { index: 7 },
            RuntimeError::ClosureCaptureCountMismatch {
                index: 7,
                expected: 1,
                actual: 2,
            },
            RuntimeError::FunctionValueAtHostBoundary,
            RuntimeError::JavaScriptExoticAtHostBoundary { kind: "Map".into() },
            RuntimeError::EffectInBuiltinCallback,
            RuntimeError::InstructionBudgetExceeded { limit: 10 },
            RuntimeError::ExecutionDeadlineExceeded { limit_ms: 20 },
            RuntimeError::UndefinedVariable {
                name: "name".into(),
            },
            RuntimeError::NonListIteration,
            RuntimeError::SessionProcessAdminOutsideProcess {
                keyword: "finish".into(),
            },
            RuntimeError::ForegroundControlInsideProcess {
                keyword: "print".into(),
            },
            RuntimeError::UnknownBuiltin {
                name: "builtin".into(),
            },
            RuntimeError::CannotReadField {
                field: "field".into(),
                actual: "value".into(),
            },
            RuntimeError::ToolResultExpected {
                actual: "value".into(),
            },
            RuntimeError::ToolResultMissingValue,
            RuntimeError::ToolResultInvalidOk,
            RuntimeError::CannotIndex {
                actual: "value".into(),
            },
            RuntimeError::ImmutableImageFields,
            RuntimeError::ImmutableImageFieldsThrough,
            RuntimeError::ImmutableTupleIndexes,
            RuntimeError::ImmutableTupleIndexesThrough,
            RuntimeError::CannotAssignField {
                field: "field".into(),
                actual: "value".into(),
            },
            RuntimeError::CannotAssignThroughField {
                field: "field".into(),
                actual: "value".into(),
            },
            RuntimeError::CannotAssignIndex {
                actual: "value".into(),
            },
            RuntimeError::CannotAssignThroughIndex {
                actual: "value".into(),
            },
            RuntimeError::InvalidListAssignmentIndex,
            RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported { key: "-1".into() },
            RuntimeError::InvalidArgumentCount {
                name: "call".into(),
                expected: "one or two".into(),
                actual: 3,
            },
            RuntimeError::EmptyUnsupported,
            RuntimeError::KeysUnsupported,
            RuntimeError::ValuesUnsupported,
            RuntimeError::SliceUnsupported,
            RuntimeError::FormatTemplateMissing,
            RuntimeError::FormatTemplateInvalid {
                actual: "int".into(),
            },
            RuntimeError::LenUnsupported,
            RuntimeError::ContainsUnsupported,
            RuntimeError::InUnsupported,
            RuntimeError::JoinUnsupported,
            RuntimeError::PushUnsupported,
            RuntimeError::ShapingListRequired {
                builtin: "sort".into(),
                actual: "text".into(),
            },
            RuntimeError::ShapingTextRequired {
                builtin: "replace".into(),
                argument: "needle".into(),
                actual: "int".into(),
            },
            RuntimeError::ShapingNumberRequired {
                builtin: "sum".into(),
                index: 1,
                actual: "text".into(),
            },
            RuntimeError::ShapingComparableRequired {
                builtin: "sort".into(),
                index: 1,
                reference: "number".into(),
                actual: "string".into(),
            },
            RuntimeError::ShapingEmptyList {
                builtin: "min".into(),
            },
            RuntimeError::SortByRecordRequired {
                index: 2,
                actual: "number".into(),
            },
            RuntimeError::SortByEmptyPath,
            RuntimeError::SortByMissingPath {
                path: "profile.score".into(),
                index: 2,
            },
            RuntimeError::InvalidRangeBound,
            RuntimeError::InvalidRangeBoundType {
                actual: "text".into(),
            },
            RuntimeError::InvalidIntegerDivisionArgument {
                builtin: "floor_div".into(),
                argument: "dividend".into(),
            },
            RuntimeError::InvalidIntegerDivisionArgumentType {
                builtin: "floor_div".into(),
                argument: "divisor".into(),
                actual: "text".into(),
            },
            RuntimeError::ExpectedNumber,
            RuntimeError::ExpectedNumberType {
                actual: "text".into(),
            },
            RuntimeError::ExpectedText {
                actual: "int".into(),
            },
            RuntimeError::InvalidIndex,
            RuntimeError::InvalidCharacterIndex {
                builtin: "char_at".into(),
                argument: "index".into(),
            },
            RuntimeError::IncompatibleSequenceConcatenation,
            RuntimeError::ReadOnlyProjectedBinding {
                name: "binding".into(),
            },
            RuntimeError::ValidateTypeLiteralRequired,
            RuntimeError::NotTypeValue {
                name: "value".into(),
            },
            RuntimeError::UnwrappedToolResultFailed {
                message: "tool error".into(),
            },
            RuntimeError::UnwrappedModuleOperationFailed {
                source: host_error(),
            },
            RuntimeError::MissingAssignmentIndex,
            RuntimeError::MissingAssignmentField {
                field: "field".into(),
            },
            RuntimeError::MissingAssignmentKey { key: "key".into() },
            RuntimeError::ListAssignmentIndexOutOfBounds,
            RuntimeError::InvalidJson {
                detail: "bad value".into(),
            },
            RuntimeError::EmptyGrepNeedle,
            RuntimeError::Format(FormatError::UnmatchedOpenBrace),
            RuntimeError::Format(FormatError::UnmatchedCloseBrace),
            RuntimeError::Format(FormatError::InvalidPlaceholder),
            RuntimeError::Format(FormatError::MixedPlaceholderKinds),
            RuntimeError::Format(FormatError::InvalidSlot { slot: "x".into() }),
            RuntimeError::Format(FormatError::SlotOutOfRange { slot: "7".into() }),
            RuntimeError::Format(FormatError::UnusedArgument { index: 2 }),
            RuntimeError::ZeroRangeStep,
            RuntimeError::RangeTooLarge { limit: 100 },
            RuntimeError::IntegerDivisionByZero {
                builtin: "floor_div".into(),
            },
            RuntimeError::UnknownProcess {
                name: "process".into(),
            },
            RuntimeError::ProcessNotExported {
                name: "process".into(),
            },
            RuntimeError::ProcessRefNotExported {
                module_ref: ModuleRef::default(),
                process_ref: ProcessRef::default(),
            },
            RuntimeError::ArtifactProcessMissing {
                module_ref: ModuleRef::default(),
                name: "process".into(),
            },
            RuntimeError::ValidationFailed {
                reason: "reason".into(),
            },
            RuntimeError::StartSiteMissing,
            RuntimeError::LinkedArtifactMissing,
            RuntimeError::LinkedProcessNotExported {
                module_ref: ModuleRef::default(),
                name: "process".into(),
            },
            RuntimeError::ProcessStartFailed {
                source: host_error(),
            },
            RuntimeError::SleepFailed {
                source: host_error(),
            },
            RuntimeError::WaitSignalFailed {
                source: host_error(),
            },
            RuntimeError::SignalRunFailed {
                source: host_error(),
            },
            RuntimeError::CancelFailed {
                source: host_error(),
            },
            RuntimeError::ProcessEventFailed {
                source: host_error(),
            },
            RuntimeError::PrintFailed {
                source: host_error(),
            },
            RuntimeError::FinishFailed {
                source: host_error(),
            },
            RuntimeError::FailFailed {
                source: host_error(),
            },
            RuntimeError::ResourceBatchReceiverOutOfRange,
            RuntimeError::ResourceBatchArgumentOutOfRange,
            RuntimeError::InvalidResourceBatchResult,
            RuntimeError::ResourceBatchFailed {
                source: host_error(),
            },
            RuntimeError::ResourceBatchResultCount {
                actual: 2,
                expected: 3,
            },
            RuntimeError::ResourceBatchSettlementOrder {
                problem: "settled position 5 is out of range for 3 results".to_string(),
            },
            RuntimeError::AggregateAwaitLeafOutOfRange,
            RuntimeError::AggregateAwaitValueOutOfRange,
            RuntimeError::InvalidAggregateAwaitRecordShape,
            RuntimeError::VmStackUnderflow,
            RuntimeError::MissingLoopState,
            RuntimeError::ContextDependentIntrinsicMisdispatch {
                context: "heap planning".into(),
            },
            RuntimeError::MemoryLimitExceeded {
                limit: 64,
                attempted: 128,
            },
            RuntimeError::HostCancelled,
            RuntimeError::DanglingHeapReference { id: 7 },
            RuntimeError::HeapIdExhausted,
            RuntimeError::UnexportedHeapReference {
                id: 7,
                context: "string formatting".into(),
            },
            RuntimeError::CyclicHostValue { id: 7 },
            RuntimeError::UncaughtException {
                value: crate::Value::Null,
            },
            RuntimeError::InvalidExceptionState {
                reason: "test".into(),
            },
        ];

        for error in &errors {
            assert_eq!(error.code(), expected_code(error), "{error:?}");
        }
        // The exhaustive match in `expected_code` forces a new variant to
        // declare a code, but nothing forces it into `errors`, so a variant
        // could be classified and never instantiated. Counting distinct codes
        // against the arm count closes that: a variant that is never built here
        // fails this assertion.
        let mut observed: Vec<&'static str> = errors.iter().map(RuntimeError::code).collect();
        observed.sort_unstable();
        observed.dedup();
        assert_eq!(
            observed.len(),
            RUNTIME_ERROR_VARIANT_COUNT,
            "every RuntimeError variant must be constructed and asserted here; \
             missing: {:?}",
            RUNTIME_ERROR_CODES
                .iter()
                .filter(|code| !observed.contains(code))
                .collect::<Vec<_>>()
        );

        for error in errors {
            let expected = match &error {
                RuntimeError::FrameDepthExceeded { .. } => {
                    "lashlang frame depth limit of 32 frames was exceeded"
                }
                RuntimeError::FunctionIndexOverflow => {
                    "lashlang function table exceeds the durable function index space"
                }
                RuntimeError::NonFunctionCall { .. } => "attempted to call a non-function number",
                RuntimeError::FunctionArgumentCount { .. } => "function takes 1 arg(s), got 2",
                RuntimeError::UnknownFunction { .. } => {
                    "closure function index 7 is not present in the compiled program"
                }
                RuntimeError::ClosureCaptureCountMismatch { .. } => {
                    "closure function 7 requires 1 capture(s), found 2"
                }
                RuntimeError::FunctionValueAtHostBoundary => {
                    "function values cannot cross a lashlang host boundary"
                }
                RuntimeError::JavaScriptExoticAtHostBoundary { .. } => {
                    "JavaScript Map values cannot cross a lashlang host boundary"
                }
                RuntimeError::EffectInBuiltinCallback => {
                    "effects are not supported inside builtin callbacks"
                }
                RuntimeError::InstructionBudgetExceeded { .. } => {
                    "lashlang instruction budget of 10 instructions exceeded"
                }
                RuntimeError::ExecutionDeadlineExceeded { .. } => {
                    "lashlang execution deadline of 20ms exceeded"
                }
                RuntimeError::MemoryLimitExceeded { .. } => {
                    "lashlang logical memory limit of 64 bytes exceeded (allocation would reach 128 bytes)"
                }
                RuntimeError::HostCancelled => "lashlang execution was cancelled by the host",
                RuntimeError::DanglingHeapReference { .. } => "dangling lashlang heap reference 7",
                RuntimeError::HeapIdExhausted => {
                    "lashlang heap allocation identity space exhausted"
                }
                RuntimeError::UnexportedHeapReference { .. } => {
                    "lashlang heap reference 7 reached string formatting before it was exported"
                }
                RuntimeError::CyclicHostValue { .. } => {
                    "lashlang heap value contains a cycle through object 7"
                }
                RuntimeError::UndefinedVariable { .. } => "unknown name `name`",
                RuntimeError::NonListIteration => "`for` expects a list or tuple",
                RuntimeError::SessionProcessAdminOutsideProcess { .. } => {
                    "`finish` can only be used inside a process body"
                }
                RuntimeError::ForegroundControlInsideProcess { .. } => {
                    "`print` can't be used inside a process body"
                }
                RuntimeError::UnknownBuiltin { .. } => "unknown builtin `builtin`",
                RuntimeError::CannotReadField { .. } => "can't read `.field` from value",
                RuntimeError::ToolResultExpected { .. } => {
                    "`?` expected a tool result wrapper, got value"
                }
                RuntimeError::ToolResultMissingValue => {
                    "`?` found a successful tool result wrapper missing `value`"
                }
                RuntimeError::ToolResultInvalidOk => {
                    "`?` expected a tool result wrapper with boolean `ok`"
                }
                RuntimeError::CannotIndex { .. } => "can't index value",
                RuntimeError::ImmutableImageFields => {
                    "can't assign image fields; images are immutable"
                }
                RuntimeError::ImmutableImageFieldsThrough => {
                    "can't assign through image fields; images are immutable"
                }
                RuntimeError::ImmutableTupleIndexes => {
                    "can't assign tuple indexes; tuples are immutable"
                }
                RuntimeError::ImmutableTupleIndexesThrough => {
                    "can't assign through tuple indexes; tuples are immutable"
                }
                RuntimeError::CannotAssignField { .. } => "can't assign `.field` on value",
                RuntimeError::CannotAssignThroughField { .. } => {
                    "can't assign through `.field` on value"
                }
                RuntimeError::CannotAssignIndex { .. } => "can't assign index on value",
                RuntimeError::CannotAssignThroughIndex { .. } => {
                    "can't assign through index on value"
                }
                RuntimeError::InvalidListAssignmentIndex => {
                    "list assignment index must be an integer"
                }
                RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported { .. } => {
                    "TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED: array property `-1` is not representable in the v1 heap"
                }
                RuntimeError::InvalidArgumentCount { .. } => {
                    "`call` takes one or two arg(s), got 3"
                }
                RuntimeError::EmptyUnsupported => {
                    "`empty` requires a string, tuple, list, record, or null"
                }
                RuntimeError::KeysUnsupported => "`keys` requires a record or null",
                RuntimeError::ValuesUnsupported => "`values` requires a record or null",
                RuntimeError::SliceUnsupported => "`slice` requires a string, tuple, or list",
                RuntimeError::FormatTemplateMissing => {
                    "`format` requires at least a template string"
                }
                RuntimeError::FormatTemplateInvalid { .. } => {
                    "`format` template must be a string, got int"
                }
                RuntimeError::LenUnsupported => {
                    "`len` requires a string, tuple, list, record, or null; use `.size` for images"
                }
                RuntimeError::ContainsUnsupported => {
                    "`contains` requires a string/string, tuple/value, list/value, record/key, or null/value pair"
                }
                RuntimeError::InUnsupported => {
                    "`in` requires a string/string, tuple/value, list/value, record/key, or null/value pair"
                }
                RuntimeError::JoinUnsupported => {
                    "`join` requires a tuple or list as the first argument"
                }
                RuntimeError::PushUnsupported => "`push` requires a list as the first argument",
                RuntimeError::ShapingListRequired { .. } => {
                    "`sort` requires a list or tuple, got text"
                }
                RuntimeError::ShapingTextRequired { .. } => {
                    "`replace` needle must be text, got int"
                }
                RuntimeError::ShapingNumberRequired { .. } => {
                    "`sum` item 1 must be a number, got text"
                }
                RuntimeError::ShapingComparableRequired { .. } => {
                    "`sort` item 1 (string) is not comparable with item 0 (number)"
                }
                RuntimeError::ShapingEmptyList { .. } => "`min` requires a non-empty list",
                RuntimeError::SortByRecordRequired { .. } => {
                    "`sort_by` item 2 must be a record, got number"
                }
                RuntimeError::SortByEmptyPath => "`sort_by` field path must not be empty",
                RuntimeError::SortByMissingPath { .. } => {
                    "`sort_by` item 2 is missing field path `profile.score`"
                }
                RuntimeError::InvalidRangeBound => "`range` bounds must be finite integers",
                RuntimeError::InvalidRangeBoundType { .. } => {
                    "`range` bounds must be finite integers, got text"
                }
                RuntimeError::InvalidIntegerDivisionArgument { .. } => {
                    "`floor_div` dividend must be a finite integer"
                }
                RuntimeError::InvalidIntegerDivisionArgumentType { .. } => {
                    "`floor_div` divisor must be a finite integer, got text"
                }
                RuntimeError::ExpectedNumber => "expected a number",
                RuntimeError::ExpectedNumberType { .. } => "expected a number, got text",
                RuntimeError::ExpectedText { .. } => "expected text, got int",
                RuntimeError::InvalidIndex => "index must be an integer",
                RuntimeError::InvalidCharacterIndex { .. } => {
                    "`char_at` index must be a non-negative integer"
                }
                RuntimeError::IncompatibleSequenceConcatenation => {
                    "can't concatenate list and tuple"
                }
                RuntimeError::ReadOnlyProjectedBinding { .. } => {
                    "`binding` is a read-only projected binding"
                }
                RuntimeError::ValidateTypeLiteralRequired => {
                    "`validate` requires a Type literal as the second argument"
                }
                RuntimeError::NotTypeValue { .. } => {
                    "`value` is not a Type value (missing `$lash_type`)"
                }
                RuntimeError::UnwrappedToolResultFailed { .. } => {
                    "`?` unwrapped failed tool result: tool error"
                }
                RuntimeError::UnwrappedModuleOperationFailed { .. } => {
                    "`?` unwrapped failed module operation: host error"
                }
                RuntimeError::MissingAssignmentIndex => "missing assignment index",
                RuntimeError::MissingAssignmentField { .. } => {
                    "can't assign through missing field `.field`"
                }
                RuntimeError::MissingAssignmentKey { .. } => {
                    "can't assign through missing key `key`"
                }
                RuntimeError::ListAssignmentIndexOutOfBounds => {
                    "list assignment index out of bounds"
                }
                RuntimeError::InvalidJson { .. } => "invalid json: bad value",
                RuntimeError::EmptyGrepNeedle => "`grep_text` needle must not be empty",
                RuntimeError::Format(error) => match error {
                    FormatError::UnmatchedOpenBrace => "unmatched `{` in format string",
                    FormatError::UnmatchedCloseBrace => "unmatched `}` in format string",
                    FormatError::InvalidPlaceholder => "invalid format placeholder",
                    FormatError::MixedPlaceholderKinds => {
                        "can't mix `{}` and indexed format placeholders"
                    }
                    FormatError::InvalidSlot { .. } => "bad format slot `x`",
                    FormatError::SlotOutOfRange { .. } => "format slot `7` is out of range",
                    FormatError::UnusedArgument { .. } => "format argument `2` is unused",
                },
                RuntimeError::ZeroRangeStep => "`range` step must not be 0",
                RuntimeError::RangeTooLarge { .. } => "`range` would create more than 100 items",
                RuntimeError::IntegerDivisionByZero { .. } => "`floor_div` divisor must not be 0",
                RuntimeError::UnknownProcess { .. } => "unknown process `process`",
                RuntimeError::ProcessNotExported { .. } => {
                    "linked module does not export process `process`"
                }
                RuntimeError::ProcessRefNotExported { .. } => {
                    "module artifact `` does not export process ref ProcessRef { component: ContentHash(\"\"), pos: 0 }"
                }
                RuntimeError::ArtifactProcessMissing { .. } => {
                    "module artifact `` is missing process `process`"
                }
                RuntimeError::ValidationFailed { .. } => "validation failed: reason",
                RuntimeError::StartSiteMissing => {
                    "`start` requires a deterministic lashlang execution site"
                }
                RuntimeError::LinkedArtifactMissing => {
                    "`start` requires a linked lashlang module artifact"
                }
                RuntimeError::LinkedProcessNotExported { .. } => {
                    "linked lashlang module `` does not export process `process`"
                }
                RuntimeError::ProcessStartFailed { .. } => "process start failed: host error",
                RuntimeError::SleepFailed { .. } => "sleep failed: host error",
                RuntimeError::WaitSignalFailed { .. } => "wait_signal failed: host error",
                RuntimeError::SignalRunFailed { .. } => "signal_run failed: host error",
                RuntimeError::CancelFailed { .. } => "cancel failed: host error",
                RuntimeError::ProcessEventFailed { .. } => "process event failed: host error",
                RuntimeError::PrintFailed { .. } => "print failed: host error",
                RuntimeError::FinishFailed { .. } => "finish failed: host error",
                RuntimeError::FailFailed { .. } => "fail failed: host error",
                RuntimeError::ResourceBatchReceiverOutOfRange => {
                    "resource operation batch receiver index out of range"
                }
                RuntimeError::ResourceBatchArgumentOutOfRange => {
                    "resource operation batch argument index out of range"
                }
                RuntimeError::InvalidResourceBatchResult => {
                    "resource operation batch returned invalid result"
                }
                RuntimeError::ResourceBatchFailed { .. } => {
                    "resource operation batch failed: host error"
                }
                RuntimeError::ResourceBatchResultCount { .. } => {
                    "resource operation batch returned 2 results for 3 operations"
                }
                RuntimeError::ResourceBatchSettlementOrder { .. } => {
                    "resource operation batch settlement order is unusable: settled position 5 is out of range for 3 results"
                }
                RuntimeError::AggregateAwaitLeafOutOfRange => {
                    "aggregate await leaf index out of range"
                }
                RuntimeError::AggregateAwaitValueOutOfRange => {
                    "aggregate await value index out of range"
                }
                RuntimeError::InvalidAggregateAwaitRecordShape => {
                    "aggregate await record shape is invalid"
                }
                RuntimeError::VmStackUnderflow => "vm stack underflow",
                RuntimeError::MissingLoopState => "missing loop state",
                RuntimeError::ContextDependentIntrinsicMisdispatch { .. } => {
                    "context-dependent intrinsic reached generic heap planning"
                }
                RuntimeError::UncaughtException { .. } => "uncaught lashlang exception: null",
                RuntimeError::InvalidExceptionState { .. } => {
                    "invalid lashlang exception state: test"
                }
            };

            assert_eq!(error.to_string(), expected);
        }
    }

    /// Every guest-facing code, in declaration order. The list is the pin's
    /// completeness half: `expected_code` forces each variant to declare one,
    /// this forces each declared one to be exercised.
    const RUNTIME_ERROR_CODES: [&str; 113] = [
        "FrameDepthExceeded",
        "FunctionIndexOverflow",
        "NonFunctionCall",
        "FunctionArgumentCount",
        "UnknownFunction",
        "ClosureCaptureCountMismatch",
        "FunctionValueAtHostBoundary",
        "JavaScriptExoticAtHostBoundary",
        "EffectInBuiltinCallback",
        "InstructionBudgetExceeded",
        "ExecutionDeadlineExceeded",
        "MemoryLimitExceeded",
        "HostCancelled",
        "DanglingHeapReference",
        "HeapIdExhausted",
        "UnexportedHeapReference",
        "CyclicHostValue",
        "UndefinedVariable",
        "NonListIteration",
        "SessionProcessAdminOutsideProcess",
        "ForegroundControlInsideProcess",
        "UnknownBuiltin",
        "CannotReadField",
        "ToolResultExpected",
        "ToolResultMissingValue",
        "ToolResultInvalidOk",
        "CannotIndex",
        "ImmutableImageFields",
        "ImmutableImageFieldsThrough",
        "ImmutableTupleIndexes",
        "ImmutableTupleIndexesThrough",
        "CannotAssignField",
        "CannotAssignThroughField",
        "CannotAssignIndex",
        "CannotAssignThroughIndex",
        "InvalidListAssignmentIndex",
        "TypeScriptArrayNonIndexPropertyUnsupported",
        "InvalidArgumentCount",
        "EmptyUnsupported",
        "KeysUnsupported",
        "ValuesUnsupported",
        "SliceUnsupported",
        "FormatTemplateMissing",
        "FormatTemplateInvalid",
        "LenUnsupported",
        "ContainsUnsupported",
        "InUnsupported",
        "JoinUnsupported",
        "PushUnsupported",
        "ShapingListRequired",
        "ShapingTextRequired",
        "ShapingNumberRequired",
        "ShapingComparableRequired",
        "ShapingEmptyList",
        "SortByRecordRequired",
        "SortByEmptyPath",
        "SortByMissingPath",
        "InvalidRangeBound",
        "InvalidRangeBoundType",
        "InvalidIntegerDivisionArgument",
        "InvalidIntegerDivisionArgumentType",
        "ExpectedNumber",
        "ExpectedNumberType",
        "ExpectedText",
        "InvalidIndex",
        "InvalidCharacterIndex",
        "IncompatibleSequenceConcatenation",
        "ReadOnlyProjectedBinding",
        "ValidateTypeLiteralRequired",
        "NotTypeValue",
        "UnwrappedToolResultFailed",
        "UnwrappedModuleOperationFailed",
        "MissingAssignmentIndex",
        "MissingAssignmentField",
        "MissingAssignmentKey",
        "ListAssignmentIndexOutOfBounds",
        "InvalidJson",
        "EmptyGrepNeedle",
        "Format",
        "ZeroRangeStep",
        "RangeTooLarge",
        "IntegerDivisionByZero",
        "UnknownProcess",
        "ProcessNotExported",
        "ProcessRefNotExported",
        "ArtifactProcessMissing",
        "ValidationFailed",
        "StartSiteMissing",
        "LinkedArtifactMissing",
        "LinkedProcessNotExported",
        "ProcessStartFailed",
        "SleepFailed",
        "WaitSignalFailed",
        "SignalRunFailed",
        "CancelFailed",
        "ProcessEventFailed",
        "PrintFailed",
        "FinishFailed",
        "FailFailed",
        "ResourceBatchReceiverOutOfRange",
        "ResourceBatchArgumentOutOfRange",
        "InvalidResourceBatchResult",
        "ResourceBatchFailed",
        "ResourceBatchResultCount",
        "ResourceBatchSettlementOrder",
        "AggregateAwaitLeafOutOfRange",
        "AggregateAwaitValueOutOfRange",
        "InvalidAggregateAwaitRecordShape",
        "VmStackUnderflow",
        "MissingLoopState",
        "ContextDependentIntrinsicMisdispatch",
        "UncaughtException",
        "InvalidExceptionState",
    ];

    const RUNTIME_ERROR_VARIANT_COUNT: usize = RUNTIME_ERROR_CODES.len();

    /// Guest-facing codes are a durable contract, so they are pinned here
    /// exhaustively: renaming a variant breaks this match and forces the
    /// author to decide, deliberately, whether the code the guest branches on
    /// changes with it.
    fn expected_code(error: &RuntimeError) -> &'static str {
        match error {
            RuntimeError::FrameDepthExceeded { .. } => "FrameDepthExceeded",
            RuntimeError::FunctionIndexOverflow => "FunctionIndexOverflow",
            RuntimeError::NonFunctionCall { .. } => "NonFunctionCall",
            RuntimeError::FunctionArgumentCount { .. } => "FunctionArgumentCount",
            RuntimeError::UnknownFunction { .. } => "UnknownFunction",
            RuntimeError::ClosureCaptureCountMismatch { .. } => "ClosureCaptureCountMismatch",
            RuntimeError::FunctionValueAtHostBoundary => "FunctionValueAtHostBoundary",
            RuntimeError::JavaScriptExoticAtHostBoundary { .. } => "JavaScriptExoticAtHostBoundary",
            RuntimeError::EffectInBuiltinCallback => "EffectInBuiltinCallback",
            RuntimeError::InstructionBudgetExceeded { .. } => "InstructionBudgetExceeded",
            RuntimeError::ExecutionDeadlineExceeded { .. } => "ExecutionDeadlineExceeded",
            RuntimeError::MemoryLimitExceeded { .. } => "MemoryLimitExceeded",
            RuntimeError::HostCancelled => "HostCancelled",
            RuntimeError::DanglingHeapReference { .. } => "DanglingHeapReference",
            RuntimeError::HeapIdExhausted => "HeapIdExhausted",
            RuntimeError::UnexportedHeapReference { .. } => "UnexportedHeapReference",
            RuntimeError::CyclicHostValue { .. } => "CyclicHostValue",
            RuntimeError::UndefinedVariable { .. } => "UndefinedVariable",
            RuntimeError::NonListIteration => "NonListIteration",
            RuntimeError::SessionProcessAdminOutsideProcess { .. } => {
                "SessionProcessAdminOutsideProcess"
            }
            RuntimeError::ForegroundControlInsideProcess { .. } => "ForegroundControlInsideProcess",
            RuntimeError::UnknownBuiltin { .. } => "UnknownBuiltin",
            RuntimeError::CannotReadField { .. } => "CannotReadField",
            RuntimeError::ToolResultExpected { .. } => "ToolResultExpected",
            RuntimeError::ToolResultMissingValue => "ToolResultMissingValue",
            RuntimeError::ToolResultInvalidOk => "ToolResultInvalidOk",
            RuntimeError::CannotIndex { .. } => "CannotIndex",
            RuntimeError::ImmutableImageFields => "ImmutableImageFields",
            RuntimeError::ImmutableImageFieldsThrough => "ImmutableImageFieldsThrough",
            RuntimeError::ImmutableTupleIndexes => "ImmutableTupleIndexes",
            RuntimeError::ImmutableTupleIndexesThrough => "ImmutableTupleIndexesThrough",
            RuntimeError::CannotAssignField { .. } => "CannotAssignField",
            RuntimeError::CannotAssignThroughField { .. } => "CannotAssignThroughField",
            RuntimeError::CannotAssignIndex { .. } => "CannotAssignIndex",
            RuntimeError::CannotAssignThroughIndex { .. } => "CannotAssignThroughIndex",
            RuntimeError::InvalidListAssignmentIndex => "InvalidListAssignmentIndex",
            RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported { .. } => {
                "TypeScriptArrayNonIndexPropertyUnsupported"
            }
            RuntimeError::InvalidArgumentCount { .. } => "InvalidArgumentCount",
            RuntimeError::EmptyUnsupported => "EmptyUnsupported",
            RuntimeError::KeysUnsupported => "KeysUnsupported",
            RuntimeError::ValuesUnsupported => "ValuesUnsupported",
            RuntimeError::SliceUnsupported => "SliceUnsupported",
            RuntimeError::FormatTemplateMissing => "FormatTemplateMissing",
            RuntimeError::FormatTemplateInvalid { .. } => "FormatTemplateInvalid",
            RuntimeError::LenUnsupported => "LenUnsupported",
            RuntimeError::ContainsUnsupported => "ContainsUnsupported",
            RuntimeError::InUnsupported => "InUnsupported",
            RuntimeError::JoinUnsupported => "JoinUnsupported",
            RuntimeError::PushUnsupported => "PushUnsupported",
            RuntimeError::ShapingListRequired { .. } => "ShapingListRequired",
            RuntimeError::ShapingTextRequired { .. } => "ShapingTextRequired",
            RuntimeError::ShapingNumberRequired { .. } => "ShapingNumberRequired",
            RuntimeError::ShapingComparableRequired { .. } => "ShapingComparableRequired",
            RuntimeError::ShapingEmptyList { .. } => "ShapingEmptyList",
            RuntimeError::SortByRecordRequired { .. } => "SortByRecordRequired",
            RuntimeError::SortByEmptyPath => "SortByEmptyPath",
            RuntimeError::SortByMissingPath { .. } => "SortByMissingPath",
            RuntimeError::InvalidRangeBound => "InvalidRangeBound",
            RuntimeError::InvalidRangeBoundType { .. } => "InvalidRangeBoundType",
            RuntimeError::InvalidIntegerDivisionArgument { .. } => "InvalidIntegerDivisionArgument",
            RuntimeError::InvalidIntegerDivisionArgumentType { .. } => {
                "InvalidIntegerDivisionArgumentType"
            }
            RuntimeError::ExpectedNumber => "ExpectedNumber",
            RuntimeError::ExpectedNumberType { .. } => "ExpectedNumberType",
            RuntimeError::ExpectedText { .. } => "ExpectedText",
            RuntimeError::InvalidIndex => "InvalidIndex",
            RuntimeError::InvalidCharacterIndex { .. } => "InvalidCharacterIndex",
            RuntimeError::IncompatibleSequenceConcatenation => "IncompatibleSequenceConcatenation",
            RuntimeError::ReadOnlyProjectedBinding { .. } => "ReadOnlyProjectedBinding",
            RuntimeError::ValidateTypeLiteralRequired => "ValidateTypeLiteralRequired",
            RuntimeError::NotTypeValue { .. } => "NotTypeValue",
            RuntimeError::UnwrappedToolResultFailed { .. } => "UnwrappedToolResultFailed",
            RuntimeError::UnwrappedModuleOperationFailed { .. } => "UnwrappedModuleOperationFailed",
            RuntimeError::MissingAssignmentIndex => "MissingAssignmentIndex",
            RuntimeError::MissingAssignmentField { .. } => "MissingAssignmentField",
            RuntimeError::MissingAssignmentKey { .. } => "MissingAssignmentKey",
            RuntimeError::ListAssignmentIndexOutOfBounds => "ListAssignmentIndexOutOfBounds",
            RuntimeError::InvalidJson { .. } => "InvalidJson",
            RuntimeError::EmptyGrepNeedle => "EmptyGrepNeedle",
            RuntimeError::Format(_) => "Format",
            RuntimeError::ZeroRangeStep => "ZeroRangeStep",
            RuntimeError::RangeTooLarge { .. } => "RangeTooLarge",
            RuntimeError::IntegerDivisionByZero { .. } => "IntegerDivisionByZero",
            RuntimeError::UnknownProcess { .. } => "UnknownProcess",
            RuntimeError::ProcessNotExported { .. } => "ProcessNotExported",
            RuntimeError::ProcessRefNotExported { .. } => "ProcessRefNotExported",
            RuntimeError::ArtifactProcessMissing { .. } => "ArtifactProcessMissing",
            RuntimeError::ValidationFailed { .. } => "ValidationFailed",
            RuntimeError::StartSiteMissing => "StartSiteMissing",
            RuntimeError::LinkedArtifactMissing => "LinkedArtifactMissing",
            RuntimeError::LinkedProcessNotExported { .. } => "LinkedProcessNotExported",
            RuntimeError::ProcessStartFailed { .. } => "ProcessStartFailed",
            RuntimeError::SleepFailed { .. } => "SleepFailed",
            RuntimeError::WaitSignalFailed { .. } => "WaitSignalFailed",
            RuntimeError::SignalRunFailed { .. } => "SignalRunFailed",
            RuntimeError::CancelFailed { .. } => "CancelFailed",
            RuntimeError::ProcessEventFailed { .. } => "ProcessEventFailed",
            RuntimeError::PrintFailed { .. } => "PrintFailed",
            RuntimeError::FinishFailed { .. } => "FinishFailed",
            RuntimeError::FailFailed { .. } => "FailFailed",
            RuntimeError::ResourceBatchReceiverOutOfRange => "ResourceBatchReceiverOutOfRange",
            RuntimeError::ResourceBatchArgumentOutOfRange => "ResourceBatchArgumentOutOfRange",
            RuntimeError::InvalidResourceBatchResult => "InvalidResourceBatchResult",
            RuntimeError::ResourceBatchFailed { .. } => "ResourceBatchFailed",
            RuntimeError::ResourceBatchResultCount { .. } => "ResourceBatchResultCount",
            RuntimeError::ResourceBatchSettlementOrder { .. } => "ResourceBatchSettlementOrder",
            RuntimeError::AggregateAwaitLeafOutOfRange => "AggregateAwaitLeafOutOfRange",
            RuntimeError::AggregateAwaitValueOutOfRange => "AggregateAwaitValueOutOfRange",
            RuntimeError::InvalidAggregateAwaitRecordShape => "InvalidAggregateAwaitRecordShape",
            RuntimeError::VmStackUnderflow => "VmStackUnderflow",
            RuntimeError::MissingLoopState => "MissingLoopState",
            RuntimeError::ContextDependentIntrinsicMisdispatch { .. } => {
                "ContextDependentIntrinsicMisdispatch"
            }
            RuntimeError::UncaughtException { .. } => "UncaughtException",
            RuntimeError::InvalidExceptionState { .. } => "InvalidExceptionState",
        }
    }
}
