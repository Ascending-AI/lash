use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{ModuleRef, ProcessRef};
use thiserror::Error;

use super::ExecutionHostError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ExecutionHostToolFailure {
    pub(super) class: lash_sansio::ToolFailureClass,
    pub(super) code: String,
    pub(super) source: lash_sansio::ToolFailureSource,
    pub(super) suggested_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cause: Option<Box<lash_sansio::ToolFailureCause>>,
    pub(super) replay_key: String,
}

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

/// One live pending operation at cell completion, in deterministic handle order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnawaitedToolCall {
    pub call_path: String,
    pub span: Option<crate::Span>,
}

fn pending_tool_details(pending: &[UnawaitedToolCall]) -> String {
    let mut details = String::new();
    for call in pending {
        details.push_str(&format!("\n- {}", call.call_path));
        if let Some(span) = call.span {
            details.push_str(&format!(" at bytes {}..{}", span.start, span.end));
        }
    }
    details
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
    /// A built-in method was called on a receiver its prototype rejects, as
    /// a detached `const f = 'x'.includes; f()` is. ECMA throws a TypeError;
    /// the message is node's.
    #[error("{message}")]
    IncompatibleReceiver { message: String },
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
    /// A binding-cell operation met something that is not a binding cell: the
    /// front end read or wrote a captured binding without the cell it
    /// declared for it. A lowering defect, never a guest program's fault.
    #[error(
        "binding cell operation on a {actual}: a captured binding was lowered without its cell"
    )]
    NotABindingCell { actual: String },
    /// ECMA-262 exotic objects have no detached host-facing value shape.
    #[error("ECMA-262 {kind} values cannot cross a lashlang host boundary")]
    BuiltinObjectAtHostBoundary { kind: String },
    /// Effects from callbacks require a resumable builtin protocol not yet present.
    #[error("effects are not supported inside builtin callbacks")]
    EffectInBuiltinCallback,
    /// A coercion reached an object's own `valueOf`/`toString` (FIG-3652).
    /// Internal to one instruction: the VM runs the hook through its call
    /// path and reruns the instruction, so this reaches neither a guest
    /// handler nor a host.
    #[error("a guest ToPrimitive hook must run before this instruction completes")]
    GuestCoercionPending,
    /// Active VM execution exceeded its explicit instruction budget.
    #[error("lashlang instruction budget of {limit} instructions exceeded")]
    InstructionBudgetExceeded { limit: u64 },
    /// RegExp execution consumed its deterministic bytecode/backtrack budget.
    #[error("regular expression execution budget of {limit} steps exceeded")]
    RegExpBudgetExceeded { limit: u64 },
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
    ///
    /// Durable state and the host boundary both carry value *trees*: a
    /// snapshot has no way to name "the object two levels up". A cycle is
    /// therefore usable inside a cell — `JSON.stringify` even reports it the
    /// way ECMA does — but a durable binding that still holds one when the
    /// cell ends cannot be written down, and that is what this says.
    #[error(
        "TS_CYCLIC_VALUE_UNSUPPORTED: lashlang heap value contains a cycle through object {id}; \
         durable state and host boundaries carry value trees, so break the cycle before the \
         cell ends (hold a key or index instead of the parent object)"
    )]
    CyclicHostValue { id: u64 },
    /// A value tree is nested deeper than a durable boundary will ever accept.
    ///
    /// The ceiling is the persisted-graph one (`MAX_SNAPSHOT_VALUE_DEPTH`), so
    /// a value that cannot be snapshotted also cannot be exported or coerced.
    /// Enforcing it on the runtime walks is what keeps those walks — which are
    /// recursive — inside the product stack budget instead of aborting the
    /// host process on a value the durable boundary would have refused anyway.
    #[error("lashlang value nesting depth limit of {limit} levels exceeded")]
    ValueDepthLimitExceeded { limit: usize },
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
    ArrayNonIndexPropertyUnsupported { key: String },
    /// A TypeScript pending tool promise violated its lifetime contract.
    #[error("TS_PENDING_TOOL: {problem}{}", pending_tool_details(.pending))]
    PendingTool {
        problem: String,
        pending: Vec<UnawaitedToolCall>,
    },
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
    /// A projection whose type no registered provider answers was read
    /// (ADR 0132 §9).
    #[error("projected value `{name}` cannot be read: {refusal}")]
    ProjectionRefused {
        name: String,
        refusal: super::ProjectionRefusal,
    },
    /// A projection's provider failed to answer a read.
    #[error("projected value `{name}` could not be read: {source}")]
    ProjectionReadFailed {
        name: String,
        source: super::ProjectionError,
    },
    /// A projected host descriptor was asked a read it does not answer
    /// (FIG-2863). Distinct from a descriptor answering "there is no value".
    #[error("projected host descriptor `{name}` ({type_name}) does not answer `{request}`")]
    ProjectedReadUnsupported {
        name: String,
        type_name: String,
        request: String,
    },
    /// `validate` received a second argument that is not a type literal.
    #[error("`validate` requires a Type literal as the second argument")]
    ValidateTypeLiteralRequired,
    /// A referenced binding is not a Lashlang type value.
    #[error("`{name}` is not a Type value (missing `$lash_type`)")]
    NotTypeValue { name: String },

    /// The `?` operator unwrapped a failed tool result.
    #[error("`?` unwrapped failed tool result: {message}")]
    UnwrappedToolResultFailed { message: String },
    /// The `?` operator unwrapped a failed host tool result with typed failure metadata.
    #[error("`?` unwrapped failed tool result: {source}")]
    UnwrappedHostToolResultFailed { source: ExecutionHostError },
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
    /// The host answered an aggregate on its host-control channel: an
    /// infrastructure failure or a host stop, never a leaf's rejection
    /// (ADR 0099 §10 L3). Leaf rejections arrive inside the reply algebra, so
    /// no guest `catch` may see this one — a caught infrastructure failure
    /// would let the cell commit a value a redrive answers differently.
    #[error("aggregate await stopped on the host-control channel: {source}")]
    AggregateHostControl { source: ExecutionHostError },
    /// A resource-operation batch returned the wrong number of results.
    #[error("resource operation batch returned {actual} results for {expected} operations")]
    ResourceBatchResultCount { actual: usize, expected: usize },
    /// A resource-operation batch reply does not fit the aggregate that asked
    /// for it — a selected leaf out of range, a reply shape the consumer mode
    /// cannot produce. Refused rather than repaired (ADR 0099 §10 L2).
    #[error("resource operation batch reply does not fit its aggregate: {problem}")]
    ResourceBatchReply { problem: String },
    /// An awaited aggregate that nothing can ever settle — `Promise.race([])`.
    /// ECMA-262 returns a promise that never settles; the host ends the
    /// execution with this typed terminal rather than parking it forever,
    /// the analogue of Node exiting on an unsettled top-level await
    /// (ADR 0099 §11 clause 5). Not catchable: the program's semantics are
    /// ECMA's, and it is the host's lifetime that ends.
    #[error("{aggregate} can never settle: the aggregate had no members to settle")]
    AggregateAwaitUnsettled { aggregate: String },
    /// `await` was applied to a value that is not a process handle.
    #[error("`await` expects a process handle but found {found}; the value is already resolved")]
    AwaitExpectsHandle { found: String },
    /// An awaited list comprehension left something other than call tuples for its batch.
    #[error("resource operation list batch found malformed call entries")]
    ResourceListBatchMalformed,
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
    /// An operation ECMA-262 specifies to throw one of its native errors.
    ///
    /// This is the typed form of "the operation throws a `TypeError`" for a
    /// site that has no heap to allocate the error object on. The VM's error
    /// routing turns it into exactly that thrown object before any guest
    /// handler, finally block or host sees it (see [`RuntimeError::ecma_error`]),
    /// so it never reaches a guest as a `RuntimeError` brand. For the same
    /// reason it is never a suspended `finally`'s pending origin, and it has no
    /// wire form: the continuation's error vocabulary is unchanged by it.
    #[error("{class}: {message}")]
    #[serde(skip)]
    EcmaThrow {
        class: EcmaErrorClass,
        message: String,
    },
}

/// The ECMA-262 native error classes a built-in operation can throw.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EcmaErrorClass {
    TypeError,
    RangeError,
    SyntaxError,
    ReferenceError,
    URIError,
}

impl EcmaErrorClass {
    pub fn name(self) -> &'static str {
        match self {
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::SyntaxError => "SyntaxError",
            Self::ReferenceError => "ReferenceError",
            Self::URIError => "URIError",
        }
    }
}

impl std::fmt::Display for EcmaErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl RuntimeError {
    /// Stable observation code for the explicit guest `fail` terminal.
    pub const PROCESS_FAILED_CODE: &'static str = "ProcessFailed";

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
            Self::IncompatibleReceiver { .. } => ErrorTaxonomy::Catchable,
            Self::FunctionArgumentCount { .. } => ErrorTaxonomy::Catchable,
            Self::UnknownFunction { .. } => ErrorTaxonomy::Catchable,
            Self::ClosureCaptureCountMismatch { .. } => ErrorTaxonomy::Catchable,
            Self::FunctionValueAtHostBoundary => ErrorTaxonomy::Catchable,
            // A lowering defect: a guest `try`/`catch` must never swallow it.
            Self::NotABindingCell { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::BuiltinObjectAtHostBoundary { .. } => ErrorTaxonomy::Catchable,
            Self::EffectInBuiltinCallback => ErrorTaxonomy::Catchable,
            // Never raised past its instruction; were it to escape, it is an
            // invariant break, not a guest failure.
            Self::GuestCoercionPending => ErrorTaxonomy::UncatchableTerminal,
            Self::InstructionBudgetExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::RegExpBudgetExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::MemoryLimitExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::HostCancelled => ErrorTaxonomy::UncatchableTerminal,
            Self::DanglingHeapReference { .. } => ErrorTaxonomy::Catchable,
            Self::HeapIdExhausted => ErrorTaxonomy::Catchable,
            Self::UnexportedHeapReference { .. } => ErrorTaxonomy::Catchable,
            Self::CyclicHostValue { .. } => ErrorTaxonomy::Catchable,
            // Classified like the snapshot boundary classifies it: a refusal of
            // the whole value, not a failure a guest handler can absorb and
            // continue past with the same over-deep value still in hand.
            Self::ValueDepthLimitExceeded { .. } => ErrorTaxonomy::UncatchableTerminal,
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
            Self::ArrayNonIndexPropertyUnsupported { .. } => ErrorTaxonomy::Catchable,
            Self::PendingTool { .. } => ErrorTaxonomy::Catchable,
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
            Self::ProjectionRefused { .. } => ErrorTaxonomy::Catchable,
            Self::ProjectionReadFailed { .. } => ErrorTaxonomy::Catchable,
            Self::ProjectedReadUnsupported { .. } => ErrorTaxonomy::Catchable,
            Self::ValidateTypeLiteralRequired => ErrorTaxonomy::Catchable,
            Self::NotTypeValue { .. } => ErrorTaxonomy::Catchable,
            Self::UnwrappedToolResultFailed { .. } => ErrorTaxonomy::EffectFailure,
            Self::UnwrappedHostToolResultFailed { .. } => ErrorTaxonomy::EffectFailure,
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
            Self::AggregateHostControl { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::ResourceBatchResultCount { .. } => ErrorTaxonomy::Catchable,
            Self::ResourceBatchReply { .. } => ErrorTaxonomy::Catchable,
            Self::AggregateAwaitUnsettled { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::AwaitExpectsHandle { .. } => ErrorTaxonomy::Catchable,
            Self::ResourceListBatchMalformed => ErrorTaxonomy::Catchable,
            Self::AggregateAwaitLeafOutOfRange => ErrorTaxonomy::Catchable,
            Self::AggregateAwaitValueOutOfRange => ErrorTaxonomy::Catchable,
            Self::InvalidAggregateAwaitRecordShape => ErrorTaxonomy::Catchable,
            Self::VmStackUnderflow => ErrorTaxonomy::Catchable,
            Self::MissingLoopState => ErrorTaxonomy::Catchable,
            Self::ContextDependentIntrinsicMisdispatch { .. } => ErrorTaxonomy::Catchable,
            Self::UncaughtException { .. } => ErrorTaxonomy::Catchable,
            Self::InvalidExceptionState { .. } => ErrorTaxonomy::UncatchableTerminal,
            Self::EcmaThrow { .. } => ErrorTaxonomy::Catchable,
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
            Self::IncompatibleReceiver { .. } => "IncompatibleReceiver",
            Self::FunctionArgumentCount { .. } => "FunctionArgumentCount",
            Self::UnknownFunction { .. } => "UnknownFunction",
            Self::ClosureCaptureCountMismatch { .. } => "ClosureCaptureCountMismatch",
            Self::FunctionValueAtHostBoundary => "FunctionValueAtHostBoundary",
            Self::NotABindingCell { .. } => "NotABindingCell",
            Self::BuiltinObjectAtHostBoundary { .. } => "BuiltinObjectAtHostBoundary",
            Self::EffectInBuiltinCallback => "EffectInBuiltinCallback",
            Self::GuestCoercionPending => "GuestCoercionPending",
            Self::InstructionBudgetExceeded { .. } => "InstructionBudgetExceeded",
            Self::RegExpBudgetExceeded { .. } => "RegExpBudgetExceeded",
            Self::MemoryLimitExceeded { .. } => "MemoryLimitExceeded",
            Self::HostCancelled => "HostCancelled",
            Self::DanglingHeapReference { .. } => "DanglingHeapReference",
            Self::HeapIdExhausted => "HeapIdExhausted",
            Self::UnexportedHeapReference { .. } => "UnexportedHeapReference",
            Self::CyclicHostValue { .. } => "CyclicHostValue",
            Self::ValueDepthLimitExceeded { .. } => "ValueDepthLimitExceeded",
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
            Self::ArrayNonIndexPropertyUnsupported { .. } => "ArrayNonIndexPropertyUnsupported",
            Self::PendingTool { .. } => "PendingTool",
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
            Self::ProjectionRefused { .. } => "ProjectionRefused",
            Self::ProjectionReadFailed { .. } => "ProjectionReadFailed",
            Self::ProjectedReadUnsupported { .. } => "ProjectedReadUnsupported",
            Self::ValidateTypeLiteralRequired => "ValidateTypeLiteralRequired",
            Self::NotTypeValue { .. } => "NotTypeValue",
            Self::UnwrappedToolResultFailed { .. } => "UnwrappedToolResultFailed",
            Self::UnwrappedHostToolResultFailed { .. } => "UnwrappedHostToolResultFailed",
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
            Self::AggregateHostControl { .. } => "AggregateHostControl",
            Self::ResourceBatchResultCount { .. } => "ResourceBatchResultCount",
            Self::ResourceBatchReply { .. } => "ResourceBatchReply",
            Self::AggregateAwaitUnsettled { .. } => "AggregateAwaitUnsettled",
            Self::AwaitExpectsHandle { .. } => "AwaitExpectsHandle",
            Self::ResourceListBatchMalformed => "ResourceListBatchMalformed",
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
            Self::EcmaThrow { .. } => "EcmaThrow",
        }
    }

    /// The ECMA-262 native error this failure is, when it is one: the class
    /// and message of the error object the guest receives in its place.
    ///
    /// This is the one mapping from a VM failure to an ECMA throw. A failure
    /// is an ECMA error exactly when the operation that raised it is one
    /// ECMA-262 specifies to throw — reading or writing a property of `null`
    /// or `undefined`, calling a value that has no `[[Call]]`, or any
    /// [`RuntimeError::EcmaThrow`] a built-in raised. The VM's error routing
    /// allocates that error object and throws it as the operation's own
    /// completion, so `catch`, `instanceof TypeError` and `error.name` answer
    /// as they do in Node, and an uncaught one ends the cell as an uncaught
    /// exception of that class. Every other failure has no ECMA counterpart —
    /// a host-boundary, tool, process or invariant failure — and keeps the
    /// substrate's `RuntimeError` brand.
    pub fn ecma_error(&self) -> Option<(EcmaErrorClass, String)> {
        let nullish = |actual: &str| matches!(actual, "null" | "undefined");
        Some(match self {
            Self::EcmaThrow { class, message } => (*class, message.clone()),
            Self::NonFunctionCall { actual } => (
                EcmaErrorClass::TypeError,
                format!("{actual} is not a function"),
            ),
            Self::CannotReadField { field, actual } if nullish(actual) => (
                EcmaErrorClass::TypeError,
                format!("Cannot read properties of {actual} (reading '{field}')"),
            ),
            Self::CannotIndex { actual } if nullish(actual) => (
                EcmaErrorClass::TypeError,
                format!("Cannot read properties of {actual}"),
            ),
            Self::CannotAssignField { field, actual }
            | Self::CannotAssignThroughField { field, actual }
                if nullish(actual) =>
            {
                (
                    EcmaErrorClass::TypeError,
                    format!("Cannot set properties of {actual} (setting '{field}')"),
                )
            }
            Self::CannotAssignIndex { actual } | Self::CannotAssignThroughIndex { actual }
                if nullish(actual) =>
            {
                (
                    EcmaErrorClass::TypeError,
                    format!("Cannot set properties of {actual}"),
                )
            }
            _ => return None,
        })
    }

    /// An ECMA-262 `TypeError` thrown by the failing operation.
    pub(crate) fn type_error(message: impl Into<String>) -> Self {
        Self::EcmaThrow {
            class: EcmaErrorClass::TypeError,
            message: message.into(),
        }
    }

    /// An ECMA-262 `RangeError` thrown by the failing operation.
    pub(crate) fn range_error(message: impl Into<String>) -> Self {
        Self::EcmaThrow {
            class: EcmaErrorClass::RangeError,
            message: message.into(),
        }
    }

    /// An ECMA-262 `SyntaxError` thrown by the failing operation.
    pub(crate) fn syntax_error(message: impl Into<String>) -> Self {
        Self::EcmaThrow {
            class: EcmaErrorClass::SyntaxError,
            message: message.into(),
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
#[non_exhaustive]
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
                | Self::RegExpBudgetExceeded { .. }
                | Self::MemoryLimitExceeded { .. }
                | Self::FrameDepthExceeded { .. }
        )
    }

    pub(crate) fn execution_host_error(&self) -> Option<&ExecutionHostError> {
        match self {
            Self::UnwrappedHostToolResultFailed { source }
            | Self::UnwrappedModuleOperationFailed { source }
            | Self::ProcessStartFailed { source }
            | Self::SleepFailed { source }
            | Self::WaitSignalFailed { source }
            | Self::SignalRunFailed { source }
            | Self::CancelFailed { source }
            | Self::ProcessEventFailed { source }
            | Self::PrintFailed { source }
            | Self::FinishFailed { source }
            | Self::FailFailed { source }
            | Self::AggregateHostControl { source } => Some(source),
            _ => None,
        }
    }
}
