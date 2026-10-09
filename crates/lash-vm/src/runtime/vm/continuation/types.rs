use super::*;
use lash_sansio::handle::HandleId;

/// The suspended execution's live tool requests, keyed by the handle the cell
/// holds (ADR 0095).
///
/// A consumed request stays in the map as `None` rather than leaving it: the
/// entry is what tells a handle awaited twice from a handle this execution
/// never minted, and the two get different repair text. Keying by handle makes
/// the serialized order a function of the handle ids alone, so two runs of the
/// same program from the same state still produce byte-identical
/// continuations.
pub type PendingOperationMap = std::collections::BTreeMap<HandleId, Option<PendingOperation>>;

/// Which run of its instruction's execution site a pending operation is:
/// the site's occurrence and the loops that enclosed it when its handle was
/// minted, outermost first. Wherever the handle is awaited, its dispatch,
/// wait, reissue and completion report this and not the context of the
/// `await` that consumes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingOccurrence {
    pub occurrence: u64,
    pub loops: Vec<lash_sansio::WorkflowLoopFrame>,
}

/// Captured operands of the pending instruction at `site`, and the
/// occurrence of that instruction's execution site the handle was minted as
/// (`None` when the instruction has no execution site).
/// Operation identity and argument count belong to that instruction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PendingOperation {
    Tool {
        site: usize,
        occurrence: Option<PendingOccurrence>,
        #[serde(
            serialize_with = "continuation_serde::serialize_value",
            deserialize_with = "continuation_serde::deserialize_value"
        )]
        receiver: Value,
        #[serde(
            serialize_with = "continuation_serde::serialize_values",
            deserialize_with = "continuation_serde::deserialize_values"
        )]
        args: Vec<Value>,
    },
    Timer {
        site: usize,
        occurrence: Option<PendingOccurrence>,
        #[serde(
            serialize_with = "continuation_serde::serialize_value",
            deserialize_with = "continuation_serde::deserialize_value"
        )]
        duration: Value,
    },
}

impl PendingOperation {
    pub(crate) fn site(&self) -> usize {
        match self {
            Self::Tool { site, .. } | Self::Timer { site, .. } => *site,
        }
    }

    pub(crate) fn occurrence(&self) -> Option<&PendingOccurrence> {
        match self {
            Self::Tool { occurrence, .. } | Self::Timer { occurrence, .. } => occurrence.as_ref(),
        }
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &Value> {
        let (first, rest) = match self {
            Self::Tool { receiver, args, .. } => (receiver, args.as_slice()),
            Self::Timer { duration, .. } => (duration, &[][..]),
        };
        std::iter::once(first).chain(rest)
    }
}

impl VmContinuation {
    /// The continuation's wire bytes: what a worker hands its parent as
    /// opaque VM state.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ContinuationError> {
        serde_json::to_vec(self).map_err(|error| ContinuationError::Undecodable {
            reason: format!("continuation does not encode: {error}"),
        })
    }

    /// The semantic decode: it restores guest values and validates every
    /// regular expression the heap holds, so it runs only on the worker side
    /// ([`crate::VmInstance::open_continuation`]). There is deliberately no
    /// `Deserialize` implementation a parent could reach through a serde
    /// envelope.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ContinuationError> {
        let undecodable = |reason: String| ContinuationError::Undecodable { reason };
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| undecodable(error.to_string()))?;
        if let Some(version_val) = raw.get("format_version") {
            if let Some(version) = version_val.as_u64() {
                if !u32::try_from(version)
                    .is_ok_and(|version| version == VM_CONTINUATION_FORMAT_VERSION)
                {
                    return Err(match u32::try_from(version) {
                        Ok(found) => ContinuationError::FormatVersionMismatch {
                            expected: VM_CONTINUATION_FORMAT_VERSION,
                            found,
                        },
                        Err(_) => undecodable(format!(
                            "continuation format version {version} is incompatible with version {VM_CONTINUATION_FORMAT_VERSION}"
                        )),
                    });
                }
            } else if version_val.as_i64().is_some() {
                return Err(undecodable(format!(
                    "continuation format version {version_val} is incompatible with version {VM_CONTINUATION_FORMAT_VERSION}"
                )));
            }
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            // Checked against this build's format before decoding guest state.
            format_version: u32,
            executable: ExecutableIdentity,
            reference_semantics: bool,
            instruction_pointer: usize,
            active_function: Option<u32>,
            #[serde(deserialize_with = "continuation_serde::deserialize_values")]
            operand_stack: Vec<Value>,
            pending_tools: super::PendingOperationMap,
            execution_nonce: u64,
            #[serde(deserialize_with = "continuation_serde::deserialize_optional_value")]
            last_value: Option<Value>,
            #[serde(deserialize_with = "continuation_serde::deserialize_slots")]
            slots: Vec<Option<Value>>,
            #[serde(deserialize_with = "continuation_serde::deserialize_record")]
            globals: Record,
            iterator_stack: Vec<VmIteratorContinuation>,
            frame_stack: Vec<VmFrameContinuation>,
            handler_stack: Vec<VmHandlerContinuation>,
            finally_stack: Vec<VmFinallyContinuation>,
            occurrence_counters: Vec<VmSiteOccurrenceCounter>,
            loop_stack: Vec<VmLoopContinuation>,
            loop_activations: u64,
            mode: ExecutionMode,
            profile: Option<VmProfileContinuation>,
            pending_error_span: Option<Span>,
            instructions_executed: u64,
            #[serde(deserialize_with = "continuation_serde::deserialize_heap")]
            heap: VmHeapContinuation,
            resume: VmResumePoint,
            #[serde(default)]
            expired_functions: std::collections::BTreeSet<String>,
        }

        let wire = Wire::deserialize(raw).map_err(|error| undecodable(error.to_string()))?;
        let continuation = Self {
            format_version: wire.format_version,
            executable: wire.executable,
            reference_semantics: wire.reference_semantics,
            instruction_pointer: wire.instruction_pointer,
            active_function: wire.active_function,
            operand_stack: wire.operand_stack,
            pending_tools: wire.pending_tools,
            execution_nonce: wire.execution_nonce,
            last_value: wire.last_value,
            slots: wire.slots,
            globals: wire.globals,
            iterator_stack: wire.iterator_stack,
            frame_stack: wire.frame_stack,
            handler_stack: wire.handler_stack,
            finally_stack: wire.finally_stack,
            occurrence_counters: wire.occurrence_counters,
            loop_stack: wire.loop_stack,
            loop_activations: wire.loop_activations,
            mode: wire.mode,
            profile: wire.profile,
            pending_error_span: wire.pending_error_span,
            instructions_executed: wire.instructions_executed,
            heap: wire.heap,
            resume: wire.resume,
            expired_functions: wire.expired_functions,
        };
        validate_continuation(&continuation)?;
        Ok(continuation)
    }
}

/// The crate's own unit tests read continuations through serde envelopes;
/// this delegates to the one semantic decode. It exists only in this crate's
/// test build: no dependent sees a `Deserialize` for a continuation.
#[cfg(test)]
impl<'de> Deserialize<'de> for VmContinuation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let bytes = serde_json::to_vec(&raw).map_err(serde::de::Error::custom)?;
        Self::decode(&bytes).map_err(|error| match error {
            ContinuationError::Undecodable { reason } => serde::de::Error::custom(reason),
            other => serde::de::Error::custom(other),
        })
    }
}

/// How many settled leaf results an await parked mid-aggregate may carry in
/// its continuation ([`VmSuspendedOperation::Await`]). A run parked past it
/// cannot be captured: it declines the park and its host answers the pending
/// await in place.
pub(crate) const VM_PARKED_AWAIT_SETTLED_LIMIT: usize = 1024;

/// Where a parked continuation resumes, stated explicitly rather than
/// implied by its instruction pointer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VmResumePoint {
    /// Parked between instructions, after an effect completed: resuming runs
    /// the instruction at the instruction pointer.
    NextInstruction,
    /// Parked on an operation that did not complete: the instruction pointer
    /// stands on it, and resuming issues exactly this operation again.
    ///
    /// `loop_phase` is the dispatch loop's cooperative-yield phase at the
    /// park, when the run was a whole-run (foreground) loop that would have
    /// carried on past the operation: the resumed loop picks it up, so its
    /// cancel checkpoints fall where an unparked run's do. A run that stops
    /// after every effect (a process segment) restarts its phase after each
    /// one anyway, and carries none.
    ReissueOperation {
        operation: VmSuspendedOperation,
        loop_phase: Option<VmLoopPhase>,
    },
}

/// The operation a continuation parked on without completing.
///
/// Under the pre-1.0 version freeze (FIG-3846) this vocabulary grows in place
/// within `VM_CONTINUATION_FORMAT_VERSION` 29: a reader meets an unknown
/// variant and refuses the continuation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VmSuspendedOperation {
    /// A resource operation the run parked awaiting (FIG-4159): its host kept
    /// the admitted operation open and asked the run to park on it, so the
    /// worker holding the run could be released. Resuming issues the same
    /// operation again, and the host answers it with the outcome it held.
    ResourceOperation { operation: String },
    /// A sleep the run parked awaiting, the same way.
    Sleep,
    /// A resource-operation batch (one aggregate await over pending
    /// operations) the run parked awaiting, the same way: the operands and
    /// the pending-request entries the batch consumes are restored, so the
    /// continuation issues the same batch again (FIG-4275).
    ResourceOperationBatch,
    /// An await of a process handle, or of a tuple, list or record of them,
    /// the run parked awaiting on its next pending handle (FIG-4275). The
    /// awaited value stands on the operand stack; when `settled` is not zero,
    /// a list of the `settled` leaf results the run had already received, in
    /// traversal order, stands above it. Resuming walks the same value again,
    /// takes those results for its first `settled` handles without asking the
    /// host, and issues the await of the handle it parked on again. At most
    /// [`VM_PARKED_AWAIT_SETTLED_LIMIT`] results ride in a continuation.
    Await { settled: usize },
}

/// Where a whole-run dispatch loop stood in its cooperative-yield schedule
/// when it parked on an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmLoopPhase {
    /// Dispatches left before the loop's next cooperative yield.
    pub yield_budget: u64,
    /// The last cancel checkpoint the loop announced.
    pub announced_checkpoint: u64,
}

/// How many times one execution site ran before the park.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSiteOccurrenceCounter {
    pub site: lash_sansio::WorkflowSiteRef,
    pub count: u64,
}

/// One loop a parked run is inside: its site and activation, how far its
/// checks and body iterations have counted, and the call-frame and handler
/// depths it was entered under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmLoopContinuation {
    pub site: lash_sansio::WorkflowSiteRef,
    pub activation: u64,
    pub checks: u64,
    pub iterations: u64,
    pub checking: bool,
    pub call_depth: usize,
    pub handler_depth: usize,
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
    /// A snapshot, or the pending keys of a live `Map` or values of a live
    /// `Set` (`collection`), whose mutations keep the tail current.
    List {
        #[serde(
            serialize_with = "continuation_serde::serialize_values",
            deserialize_with = "continuation_serde::deserialize_values"
        )]
        values: Vec<Value>,
        next_index: usize,
        #[serde(
            serialize_with = "continuation_serde::serialize_optional_value",
            deserialize_with = "continuation_serde::deserialize_optional_value"
        )]
        collection: Option<Value>,
    },
    /// An array or a `URLSearchParams`, read at `next_index` on every step.
    Live {
        #[serde(
            serialize_with = "continuation_serde::serialize_value",
            deserialize_with = "continuation_serde::deserialize_value"
        )]
        source: Value,
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
#[non_exhaustive]
pub enum ContinuationError {
    #[error("continuation format version {found} is incompatible with version {expected}")]
    FormatVersionMismatch { expected: u32, found: u32 },
    #[error("continuation bytes do not decode: {reason}")]
    Undecodable { reason: String },
    #[error(
        "an await parked with {settled} settled handle results, over the {limit}-result continuation bound"
    )]
    ParkedAwaitTooLarge { settled: usize, limit: usize },
    #[error(
        "continuation resumes by re-issuing {operation}, but instruction {instruction_pointer} does not issue it"
    )]
    ResumePointMismatch {
        instruction_pointer: usize,
        operation: String,
    },
    #[error(
        "continuation was parked by executable `{found}`, not by this program's executable `{expected}`"
    )]
    ExecutableMismatch {
        expected: ExecutableIdentity,
        found: ExecutableIdentity,
    },
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
    #[error("heapless snapshot contains a heap reference at `{location}`")]
    HeaplessSnapshotContainsReference { location: String },
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
    #[error("continuation loop {index} is not a loop the parked run can be inside: {reason}")]
    InvalidLoopContext { index: usize, reason: &'static str },
    #[error("continuation counts execution site {site:?} more than once or at zero")]
    InvalidOccurrenceCounter { site: lash_sansio::WorkflowSiteRef },
    #[error(
        "continuation pending operation {handle} is not an occurrence the parked run can have minted: {reason}"
    )]
    InvalidPendingOccurrence {
        handle: String,
        reason: &'static str,
    },
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
    #[error(
        "continuation handler chain for frame {frame_depth} is not the chain the compiled program installs at instruction {anchor} (expected digest {expected:#018x}, found {found:#018x})"
    )]
    HandlerChainMismatch {
        frame_depth: usize,
        anchor: usize,
        expected: u64,
        found: u64,
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
    #[error("lash_vm instruction budget of {limit} instructions was already exceeded")]
    InstructionBudgetExceeded { limit: u64 },
    #[error("lash_vm frame depth limit of {limit} frames was already exceeded")]
    FrameDepthExceeded { limit: u64 },
    #[error(
        "lash_vm logical memory limit of {limit} bytes was already exceeded by {live} live bytes"
    )]
    MemoryLimitExceeded { limit: u64, live: u64 },
}

impl ContinuationError {
    pub fn is_execution_bound_exhausted(&self) -> bool {
        matches!(
            self,
            Self::InstructionBudgetExceeded { .. }
                | Self::FrameDepthExceeded { .. }
                | Self::MemoryLimitExceeded { .. }
        )
    }
}
