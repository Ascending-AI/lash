use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::super::{
    DateObject, ErrorKind, ErrorObject, ExecutableIdentity, ExecutionBound, HeapObject,
    HeapRestoreWire, MapObject, PersistedRoots, RegExpObject, SetObject,
};
use super::*;

mod types;
pub(crate) use types::VM_PARKED_AWAIT_SETTLED_LIMIT;
pub use types::{
    ContinuationError, PendingOperation, PendingOperationMap, VmFinallyCompletionContinuation,
    VmFinallyContinuation, VmHandlerContinuation, VmIteratorContinuation, VmIteratorCursor,
    VmLoopPhase, VmPendingErrorOriginContinuation, VmProfileContinuation, VmResumePoint,
    VmSuspendedOperation,
};

use super::exceptions::PendingErrorOrigin;

/// Version of the durable VM-continuation envelope a parked Lashlang segment
/// carries.
///
/// v8 carries the substrate-minted `EffectError`/`RuntimeError` error brands. A
/// heap error's `error_kind` serializes by name here too, so a v7 reader fails
/// to decode the new names instead of reporting a version boundary.
///
/// v10 preserves structured tool-failure classification inside a pending
/// runtime error's serialized execution-host source.
///
/// v11 removes the redundant all-false projected-slot vectors. Durable
/// continuations cannot carry projected bindings, so restore reconstructs the
/// in-memory vectors from the slot counts.
///
/// v14 follows the single-language cutover (ADR 0096). The instruction set
/// loses the deep-copy instructions the retired surface compiled to, so a
/// parked continuation's instruction pointer and frame stack address a stream
/// this build cannot reproduce; and `reference_semantics` stops being a
/// cross-check against the program's dialect and becomes only what it always
/// described on the wire, whether this heap is a shared graph or a forest.
///
/// v15 counts aggregates in `occurrence_counters`. The map's shape is
/// unchanged, which is exactly why this is a version rather than a decode
/// failure: a v14 continuation deserializes cleanly and then resumes with no
/// count for the aggregates its own earlier segment already ran, so every
/// batch it re-derives on replay carries an ordinal the journal never saw and
/// misses the effect it is supposed to reuse. A counter's contents are part of
/// this envelope's meaning even when its type is not.
///
/// v17 carries the replay key with a pending execution-host tool failure so a
/// resumed segment retains the recorded failure's observation provenance.
///
/// v18 carries pending timers in the pending-request map — an unawaited
/// `sleep(ms)` is a pending operation like a tool call (ADR 0099 §11) — and the
/// aggregate refusals `ResourceBatchReply`, `AggregateAwaitUnsettled` and
/// `AggregateHostControl` (which replaces the catchable `ResourceBatchFailed`)
/// in its runtime-error vocabulary. A v17 continuation's map could never hold
/// a timer, but the refusal a parked v17 VM carries names a retired variant,
/// so the boundary is a version rather than a decode failure.
///
/// v19 (FIG-3571) resumes over bytecode v20: its chunks carry private slots and
/// its occurrence counters are keyed by carrier node ids. A v18 continuation
/// decodes cleanly but its counters and slots name a retired node vocabulary,
/// so it is refused rather than resumed.
///
/// v20 (FIG-3625) carries live loop cursors: a `for...of` over an array or a
/// `URLSearchParams` is a reference and an index, and one over a `Map` or a
/// `Set` holds the collection its pending tail follows. A v19 cursor is a
/// snapshot the resumed loop would walk as if it were live, so it is refused.
///
/// v21 (FIG-3657) carries an error's own `message` as `Option<String>`: absent
/// stays absent and an explicitly empty message stays empty, where v20 encoded
/// both as `""`. A v20 continuation decodes cleanly but would resurrect an
/// error's `new Error('')` with no own `message`, so it is refused rather than
/// resumed.
///
/// v22 (FIG-3655) writes a closure's ECMA `name`/`length` own-property slots.
/// A v21 wire's closures would decode under the old shape and restore without
/// them — `f.name` answering `undefined` where the live run reported a name —
/// so the boundary is a version rather than a decode failure.
///
/// v23 (FIG-3672) resumes under a changed intrinsic fuel schedule:
/// `JSON.parse` and `JSON.stringify` charge one instruction per byte of text
/// to the instruction budget. A continuation's `instructions_executed` and the
/// segment it resumes are metered on one schedule, so a v22 continuation
/// resumed here could exhaust its budget at an instruction the recorded run
/// passed. The wire shape is unchanged; the meaning of the meter is not, so an
/// older continuation is refused typed before any effect rather than resumed.
///
/// v24 (FIG-3700, FIG-3701) binds call receivers and built-in method values:
/// a function that reads `this` lays out a receiver slot in its frame, member
/// calls return to `CallMethod` sites, a built-in method value
/// (`'x'.includes`) is a `builtin_function` heap object named by prototype and
/// `name`, and a pending error may be `IncompatibleReceiver`. A v23
/// continuation was laid out without them and its reader meets an unknown
/// kind, so it is refused.
///
/// v25 (FIG-3652) extends the serialized error vocabulary: a pending error
/// may be `GuestCoercionPending`, the internal marker a coercing instruction
/// raises so the VM can run the object's `valueOf`/`toString` hook and rerun
/// the instruction. A v24 continuation's reader meets the unknown variant, so
/// it is refused.
///
/// v26 (FIG-3656) renames the `builtin_function` object's `prototype` field to
/// `owner`: built-in values now include constructors, namespaces, and static
/// methods whose scope is not a prototype, so the field names the owning
/// scope. A v25 reader rejects the unknown field, so the bump is what makes
/// its refusal a version boundary.
///
/// v27 (FIG-3707) carries binding cells: a captured binding that something
/// assigns lives in a `cell` heap object its frame and every closure over it
/// reference, and the error enum gains `NotABindingCell` (an uncatchable
/// lowering defect, never a pending guest error). A v26 continuation's reader
/// meets an unknown kind, so it is refused.
///
/// v28 (FIG-3571) binds a continuation to the executable that parked it: the
/// wire carries the program's [`ExecutableIdentity`], and resume refuses any
/// other program, direct public resume included. A v27 continuation names no
/// executable, so it is refused rather than resumed against whatever program
/// the caller supplies.
///
/// v29 (FIG-3787) carries the callback driver's receiver and its wider
/// completion vocabulary: `this_arg` is the callback's `this`, and the
/// element-keyed completions (`every`, `some`, `filter`, `find`,
/// `findIndex`, `map`, `flatMap`, `reduce`) plus the comparator `sort`'s
/// in-flight ordering are variants a v28 wire never wrote. A v28 reader
/// meets the missing field and unknown kinds, so it is refused.
///
/// Re-exported by the facade's `formats` manifest so a host can read it before
/// wiring a store.
///
/// version_guard(
///     shapes(
///         path = "crates/lashlang/src/runtime/vm/continuation.rs",
///         path = "crates/lashlang/src/runtime/vm/continuation/types.rs",
///         cover(VmContinuation, PendingOperation, HeapWire, HeapObjectWire, ValueWire, VmHandlerContinuation),
///     ),
///     shapes(
///         path = "crates/lashlang/src/runtime/projected_wire.rs",
///         cover(CanonicalProjectedValue),
///     ),
///     shapes(path = "crates/lashlang/src/runtime/error.rs", cover(RuntimeError)),
///     shapes(
///         path = "crates/lashlang/src/runtime/heap.rs",
///         path = "crates/lashlang/src/runtime/heap/*.rs", cover(ErrorKind, HeapId),
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "drain"
/// format_manifest = "VmContinuation"
pub const VM_CONTINUATION_FORMAT_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the continuation format, so a
/// continuation it parks is one N cannot decode: it keeps N+1's deployment
/// and routes to N+1's generation (ADR 0115 §3.5).
#[cfg(feature = "synthetic-next")]
/// version_surface = "drain"
/// format_manifest = "VmContinuation"
pub const VM_CONTINUATION_FORMAT_VERSION: u32 = 2;

/// The execution identity pending-tool handles carry.
///
/// Distinctness is what matters, not secrecy: a handle from one cell must
/// never match the nonce of the next cell on the same session, and a
/// hand-written `{__handle__: "lash", id: "t.0…0.0"}` must not match anything. The
/// nonce is a mixed function of the session heap's allocation counter at
/// execution start — a value that only grows across a session's cells — rather
/// than a random draw, so two runs of the same program from the same state
/// produce byte-identical continuations (the cross-process determinism probes
/// compare them). A durable park carries the nonce inside the continuation, so
/// a resumed execution still recognises the handles it minted.
pub(super) fn mint_execution_nonce(seed: u64) -> u64 {
    // SplitMix64 finaliser: a bijection over the seed, so distinct seeds never
    // share a nonce and the spelled value does not read as a counter.
    let mut nonce = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    nonce = (nonce ^ (nonce >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    nonce = (nonce ^ (nonce >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    nonce ^ (nonce >> 31)
}

#[derive(Clone, Debug, PartialEq)]
pub enum VmRunOutcome {
    EffectCompleted,
    Complete(ExecutionOutcome),
    /// The host handed the process's pending operation to a successor
    /// segment ([`AbilityOutcome::HandedOver`](crate::AbilityOutcome::HandedOver)).
    /// The VM stands on the wait instruction, which has not completed:
    /// [`Vm::suspend`] captures a continuation that issues the wait again.
    HandedOver,
}

#[cfg(test)]
#[derive(Default)]
pub(super) enum TestSuspension {
    #[default]
    Disabled,
    AfterInstructions(usize),
    AfterEffects(usize),
}

#[cfg(test)]
impl TestSuspension {
    pub(super) fn should_suspend(&mut self, completed_effect: bool) -> bool {
        let remaining = match self {
            Self::Disabled => return false,
            Self::AfterInstructions(remaining) => remaining,
            Self::AfterEffects(remaining) if completed_effect => remaining,
            Self::AfterEffects(_) => return false,
        };
        *remaining = remaining.saturating_sub(1);
        *remaining == 0
    }
}

/// A complete, code-independent snapshot of a suspended bytecode VM.
///
/// The compiled program is intentionally not embedded: callers must supply the
/// same content-addressed program to [`Vm::resume_from`]. Derived validation
/// plans are rebuilt lazily after restore.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VmContinuation {
    pub format_version: u32,
    /// The executable identity of the program that parked this continuation
    /// ([`CompiledProgram::executable_identity`]); [`Vm::resume_from`] refuses
    /// any other program.
    pub executable: ExecutableIdentity,
    pub reference_semantics: bool,
    pub instruction_pointer: usize,
    pub active_function: Option<u32>,
    #[serde(
        serialize_with = "continuation_serde::serialize_values",
        deserialize_with = "continuation_serde::deserialize_values"
    )]
    pub operand_stack: Vec<Value>,
    pub pending_tools: PendingOperationMap,
    /// The suspended execution's identity; every pending-tool handle it minted
    /// carries it, and the resumed VM keeps accepting exactly those handles.
    pub execution_nonce: u64,
    #[serde(
        serialize_with = "continuation_serde::serialize_optional_value",
        deserialize_with = "continuation_serde::deserialize_optional_value"
    )]
    pub last_value: Option<Value>,
    #[serde(
        serialize_with = "continuation_serde::serialize_slots",
        deserialize_with = "continuation_serde::deserialize_slots"
    )]
    pub slots: Vec<Option<Value>>,
    #[serde(
        serialize_with = "continuation_serde::serialize_record",
        deserialize_with = "continuation_serde::deserialize_record"
    )]
    pub globals: Record,
    pub iterator_stack: Vec<VmIteratorContinuation>,
    pub(crate) frame_stack: Vec<VmFrameContinuation>,
    pub handler_stack: Vec<VmHandlerContinuation>,
    pub finally_stack: Vec<VmFinallyContinuation>,
    pub occurrence_counters: std::collections::BTreeMap<String, u64>,
    pub mode: ExecutionMode,
    pub profile: Option<VmProfileContinuation>,
    pub pending_error_span: Option<Span>,
    pub instructions_executed: u64,
    #[serde(
        serialize_with = "continuation_serde::serialize_heap",
        deserialize_with = "continuation_serde::deserialize_heap"
    )]
    pub heap: VmHeapContinuation,
    /// Where the continuation resumes: the explicit suspended-operation and
    /// resume discriminant a worker and its parent agree on (FIG-4158).
    pub resume: VmResumePoint,
    /// A foreground run parked on an operation it awaits carries the rest of
    /// its session state with it (FIG-4159): the names of the globals earlier
    /// cells dropped because their value reached a function. A process body
    /// has none, and its bytes carry no field.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub expired_functions: std::collections::BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmFrameContinuation {
    pub return_instruction_pointer: usize,
    pub function: Option<u32>,
    pub operand_stack_base: usize,
    #[serde(
        serialize_with = "continuation_serde::serialize_slots",
        deserialize_with = "continuation_serde::deserialize_slots"
    )]
    pub slots: Vec<Option<Value>>,
    #[serde(
        serialize_with = "continuation_serde::serialize_record",
        deserialize_with = "continuation_serde::deserialize_record"
    )]
    pub globals: Record,
    pub iterator_stack: Vec<VmIteratorContinuation>,
    pub return_target: VmFrameReturnContinuation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum VmFrameReturnContinuation {
    Direct,
    /// Boxed so the common `Direct` frame stays pointer-sized.
    Callback(Box<VmCallbackContinuation>),
}

/// The `callback` return target's wire payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct VmCallbackContinuation {
    #[serde(
        serialize_with = "continuation_serde::serialize_value",
        deserialize_with = "continuation_serde::deserialize_value"
    )]
    pub(crate) function: Value,
    #[serde(
        serialize_with = "continuation_serde::serialize_value",
        deserialize_with = "continuation_serde::deserialize_value"
    )]
    pub(crate) this_arg: Value,
    #[serde(
        serialize_with = "continuation_serde::serialize_values",
        deserialize_with = "continuation_serde::deserialize_values"
    )]
    pub(crate) calls: Vec<Value>,
    pub(crate) next_index: usize,
    #[serde(
        serialize_with = "continuation_serde::serialize_values",
        deserialize_with = "continuation_serde::deserialize_values"
    )]
    pub(crate) results: Vec<Value>,
    pub(crate) completion: VmCallbackCompletion,
    pub(crate) allow_effects: bool,
    #[serde(default)]
    pub(crate) live_url_search_params: bool,
    /// The lazy array-like index walk (FIG-3787) — `None` for the
    /// materialized `calls` queue the collection drivers use.
    #[serde(default)]
    pub(crate) array_like: Option<VmArrayLikeWalk>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmCallbackCompletion {
    Collect,
    Discard,
    Every,
    Some,
    Filter,
    Find,
    FindIndex,
    Map {
        length: u64,
    },
    FlatMap,
    Reduce {
        #[serde(
            serialize_with = "continuation_serde::serialize_value",
            deserialize_with = "continuation_serde::deserialize_value"
        )]
        accumulator: Value,
    },
    Sort(VmSortState),
}

/// The durable shape of a lazy array-like index walk (FIG-3787) — the
/// pending-call source a callback driver carries instead of a materialized
/// tuple queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct VmArrayLikeWalk {
    #[serde(
        serialize_with = "continuation_serde::serialize_value",
        deserialize_with = "continuation_serde::deserialize_value"
    )]
    pub receiver: Value,
    pub next: u64,
    pub length: u64,
    pub descending: bool,
    pub gated: bool,
    #[serde(default)]
    pub omit_receiver: bool,
}

fn callback_completion_continuation(completion: &CallbackCompletion) -> VmCallbackCompletion {
    match completion {
        CallbackCompletion::Collect => VmCallbackCompletion::Collect,
        CallbackCompletion::Discard => VmCallbackCompletion::Discard,
        CallbackCompletion::Every => VmCallbackCompletion::Every,
        CallbackCompletion::Some => VmCallbackCompletion::Some,
        CallbackCompletion::Filter => VmCallbackCompletion::Filter,
        CallbackCompletion::Find => VmCallbackCompletion::Find,
        CallbackCompletion::FindIndex => VmCallbackCompletion::FindIndex,
        CallbackCompletion::Map { length } => VmCallbackCompletion::Map { length: *length },
        CallbackCompletion::FlatMap => VmCallbackCompletion::FlatMap,
        CallbackCompletion::Reduce { accumulator } => VmCallbackCompletion::Reduce {
            accumulator: accumulator.clone(),
        },
        CallbackCompletion::Sort(state) => VmCallbackCompletion::Sort(VmSortState {
            pending: state.pending.clone(),
            sorted: state.sorted.clone(),
            current: state.current.clone(),
            probe: state.probe,
            lo: state.lo,
            hi: state.hi,
            undefined_count: state.undefined_count,
            receiver: state.receiver.clone(),
            length: state.length,
            in_place: state.in_place,
        }),
    }
}

fn callback_completion_from_continuation(completion: VmCallbackCompletion) -> CallbackCompletion {
    match completion {
        VmCallbackCompletion::Collect => CallbackCompletion::Collect,
        VmCallbackCompletion::Discard => CallbackCompletion::Discard,
        VmCallbackCompletion::Every => CallbackCompletion::Every,
        VmCallbackCompletion::Some => CallbackCompletion::Some,
        VmCallbackCompletion::Filter => CallbackCompletion::Filter,
        VmCallbackCompletion::Find => CallbackCompletion::Find,
        VmCallbackCompletion::FindIndex => CallbackCompletion::FindIndex,
        VmCallbackCompletion::Map { length } => CallbackCompletion::Map { length },
        VmCallbackCompletion::FlatMap => CallbackCompletion::FlatMap,
        VmCallbackCompletion::Reduce { accumulator } => CallbackCompletion::Reduce { accumulator },
        VmCallbackCompletion::Sort(state) => CallbackCompletion::Sort(SortState {
            pending: state.pending,
            sorted: state.sorted,
            current: state.current,
            probe: state.probe,
            lo: state.lo,
            hi: state.hi,
            undefined_count: state.undefined_count,
            receiver: state.receiver,
            length: state.length,
            in_place: state.in_place,
        }),
    }
}

/// The wire form of a comparator `sort`'s in-flight ordering
/// ([`SortState`]): every field a resume needs, held as plain data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct VmSortState {
    #[serde(
        serialize_with = "continuation_serde::serialize_values",
        deserialize_with = "continuation_serde::deserialize_values"
    )]
    pub pending: Vec<Value>,
    #[serde(
        serialize_with = "continuation_serde::serialize_values",
        deserialize_with = "continuation_serde::deserialize_values"
    )]
    pub sorted: Vec<Value>,
    #[serde(
        serialize_with = "continuation_serde::serialize_value",
        deserialize_with = "continuation_serde::deserialize_value"
    )]
    pub current: Value,
    pub probe: usize,
    pub lo: usize,
    pub hi: usize,
    pub undefined_count: u64,
    #[serde(
        serialize_with = "continuation_serde::serialize_value",
        deserialize_with = "continuation_serde::deserialize_value"
    )]
    pub receiver: Value,
    pub length: u64,
    pub in_place: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VmHeapContinuation {
    heap: Heap,
}

impl VmHeapContinuation {
    fn new(heap: Heap) -> Self {
        Self { heap }
    }

    fn into_heap(self) -> Heap {
        self.heap
    }

    pub fn allocation_counter(&self) -> u64 {
        self.heap.allocations()
    }

    pub fn live_logical_bytes(&self) -> u64 {
        self.heap.live_logical_bytes()
    }

    #[cfg(test)]
    pub(crate) fn live_object_count(&self) -> usize {
        self.heap.objects_in_id_order().count()
    }

    /// Objects allocated and swept since the VM started: the heap stores only
    /// the live entries, so what a collection removed is the gap between the
    /// allocation counter and what remains.
    #[cfg(test)]
    pub(crate) fn swept_object_count(&self) -> u64 {
        self.heap.allocations() - self.heap.objects_in_id_order().count() as u64
    }

    pub fn materialize(&self, value: &Value) -> Result<Value, ContinuationError> {
        self.heap
            .export(value)
            .map_err(|_| ContinuationError::UnserializableValue {
                location: "continuation heap".to_string(),
                variant: "invalid heap reference",
            })
    }
}

impl VmContinuation {
    /// Number of parked caller frames in this suspended VM.
    pub fn frame_depth(&self) -> usize {
        self.frame_stack.len()
    }
}

impl Default for VmHeapContinuation {
    fn default() -> Self {
        Self::new(Heap::default())
    }
}

mod continuation_serde {
    use super::*;
    use crate::HeapId;
    use crate::runtime::CANONICAL_NAN_BITS;
    use crate::runtime::heap::{UrlObject, UrlSearchParamsObject};
    use crate::runtime::projected_wire::CanonicalProjectedValue;

    /// version_surface = "coexist"
    /// version_guard(items(number_to_wire, number_from_wire), roots(NumberWire))
    const NUMBER_WIRE_VERSION: u32 = 1;

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
    enum OptionalValueWire {
        Unset,
        Set(ValueWire),
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", content = "value", rename_all = "snake_case")]
    enum ValueWire {
        Null,
        Undefined,
        Bool(bool),
        Number(NumberWire),
        String(String),
        Image(super::ImageValue),
        Resource(super::ResourceHandle),
        Ref(HeapId),
        Tuple(Vec<ValueWire>),
        List(Vec<ValueWire>),
        Record(Vec<(String, ValueWire)>),
        /// The same canonical shape the `State` snapshot writes, so a value one
        /// durable writer accepts the other accepts too (FIG-2865).
        Projected(CanonicalProjectedValue<ValueWire>),
    }

    #[derive(Serialize, Deserialize)]
    struct NumberWire {
        version: u32,
        bits: u64,
    }

    fn number_to_wire(value: f64) -> NumberWire {
        NumberWire {
            version: NUMBER_WIRE_VERSION,
            bits: if value.is_nan() {
                CANONICAL_NAN_BITS
            } else {
                value.to_bits()
            },
        }
    }

    fn number_from_wire(value: NumberWire) -> Result<f64, &'static str> {
        if value.version != NUMBER_WIRE_VERSION {
            return Err("unsupported continuation number wire version");
        }
        let number = f64::from_bits(value.bits);
        Ok(if number.is_nan() {
            f64::from_bits(CANONICAL_NAN_BITS)
        } else {
            number
        })
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HeapWire {
        #[serde(flatten)]
        header: crate::runtime::heap::HeapHeaderWire,
        objects: Vec<HeapEntryWire>,
    }

    #[derive(Serialize, Deserialize)]
    struct HeapEntryWire {
        id: HeapId,
        object: HeapObjectWire,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum HeapObjectWire {
        Tuple {
            items: Vec<ValueWire>,
        },
        List {
            items: Vec<ValueWire>,
            holes: Vec<usize>,
        },
        Record {
            fields: Vec<(String, ValueWire)>,
        },
        Closure {
            function: u32,
            captures: Vec<ValueWire>,
            name: Option<ValueWire>,
            length: Option<ValueWire>,
        },
        /// A built-in value, named by its owner scope and its ECMA `name`
        /// rather than by a table position, so the bytes do not move when
        /// the table is reordered.
        BuiltinFunction {
            owner: String,
            name: String,
        },
        RegExp {
            pattern: String,
            flags: String,
            last_index: ValueWire,
        },
        RegExpMatch {
            items: Vec<ValueWire>,
            index: ValueWire,
            input: ValueWire,
            groups: ValueWire,
        },
        Map {
            entries: Vec<(ValueWire, ValueWire)>,
        },
        Set {
            values: Vec<ValueWire>,
        },
        Date {
            milliseconds: NumberWire,
        },
        Error {
            error_kind: ErrorKind,
            message: Option<String>,
            cause: Option<ValueWire>,
            errors: Option<ValueWire>,
        },
        Url {
            href: String,
            search_params: ValueWire,
        },
        UrlSearchParams {
            entries: Vec<(String, String)>,
        },
        /// A binding cell (FIG-3707): the one storage location of a captured
        /// binding that something assigns, shared by the frame that owns it
        /// and every closure over it through their references to this id.
        Cell {
            value: ValueWire,
        },
    }

    fn value_to_wire(value: &Value) -> Result<ValueWire, &'static str> {
        Ok(match value {
            Value::Null => ValueWire::Null,
            Value::Undefined => ValueWire::Undefined,
            Value::Bool(value) => ValueWire::Bool(*value),
            Value::Number(value) => ValueWire::Number(number_to_wire(*value)),
            Value::String(value) => ValueWire::String(value.to_string()),
            Value::Image(value) => ValueWire::Image((**value).clone()),
            Value::Resource(value) => ValueWire::Resource(value.clone()),
            Value::Ref(value) => ValueWire::Ref(*value),
            Value::Tuple(values) => {
                ValueWire::Tuple(values.iter().map(value_to_wire).collect::<Result<_, _>>()?)
            }
            Value::List(values) => {
                ValueWire::List(values.iter().map(value_to_wire).collect::<Result<_, _>>()?)
            }
            Value::Record(record) => ValueWire::Record(record_to_wire(record)?),
            // One leaf wherever it sits; a scalar projection's value is host
            // data and holds no heap reference (ADR 0132 §9).
            Value::Projected(projected) => ValueWire::Projected(
                CanonicalProjectedValue::from_projected(projected, |value| {
                    if value_holds_reference(value) {
                        return Err(PROJECTED_SCALAR_REFERENCE);
                    }
                    value_to_wire(value)
                })?,
            ),
        })
    }

    fn value_from_wire(value: ValueWire) -> Result<Value, &'static str> {
        Ok(match value {
            ValueWire::Null => Value::Null,
            ValueWire::Undefined => Value::Undefined,
            ValueWire::Bool(value) => Value::Bool(value),
            ValueWire::Number(value) => Value::Number(number_from_wire(value)?),
            ValueWire::String(value) => Value::String(value.into()),
            ValueWire::Image(value) => Value::Image(Box::new(value)),
            ValueWire::Resource(value) => Value::Resource(value),
            ValueWire::Ref(value) => Value::Ref(value),
            ValueWire::Tuple(values) => Value::Tuple(
                values
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            ValueWire::List(values) => Value::List(
                values
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            ValueWire::Record(entries) => Value::Record(Arc::new(record_from_wire(entries)?)),
            ValueWire::Projected(projected) => {
                Value::Projected(projected.into_projected(|value| {
                    let value = value_from_wire(value)?;
                    if value_holds_reference(&value) {
                        return Err(PROJECTED_SCALAR_REFERENCE);
                    }
                    Ok(value)
                })?)
            }
        })
    }

    const PROJECTED_SCALAR_REFERENCE: &str =
        "a scalar projection's value must hold no heap reference";

    /// Whether `value` reaches a heap reference, a scalar projection's own
    /// value included.
    fn value_holds_reference(value: &Value) -> bool {
        match value {
            Value::Ref(_) => true,
            Value::Tuple(values) | Value::List(values) => values.iter().any(value_holds_reference),
            Value::Record(record) => record.values().any(value_holds_reference),
            Value::Projected(projected) => {
                projected.scalar_value().is_some_and(value_holds_reference)
            }
            _ => false,
        }
    }

    fn record_to_wire(record: &Record) -> Result<Vec<(String, ValueWire)>, &'static str> {
        record
            .iter()
            .map(|(key, value)| Ok((key.to_string(), value_to_wire(value)?)))
            .collect()
    }

    fn record_from_wire(entries: Vec<(String, ValueWire)>) -> Result<Record, &'static str> {
        let mut record = record_with_capacity(entries.len());
        let mut names = std::collections::BTreeSet::new();
        for (key, value) in entries {
            if !names.insert(key.clone()) {
                return Err("continuation record keys must be unique");
            }
            crate::runtime::access::ensure_no_prototype_chain_wire_key(&key)?;
            record.insert(key, value_from_wire(value)?);
        }
        Ok(record)
    }

    fn object_to_wire(object: &HeapObject) -> Result<HeapObjectWire, &'static str> {
        Ok(match object {
            HeapObject::Tuple(values) => HeapObjectWire::Tuple {
                items: values.iter().map(value_to_wire).collect::<Result<_, _>>()?,
            },
            HeapObject::List {
                items: values,
                holes,
            } => HeapObjectWire::List {
                items: values.iter().map(value_to_wire).collect::<Result<_, _>>()?,
                holes: holes.iter().copied().collect(),
            },
            HeapObject::Record(record) => HeapObjectWire::Record {
                fields: record_to_wire(record)?,
            },
            HeapObject::Closure {
                function,
                captures,
                name,
                length,
            } => HeapObjectWire::Closure {
                function: *function,
                captures: captures
                    .iter()
                    .map(value_to_wire)
                    .collect::<Result<_, _>>()?,
                name: name.as_ref().map(value_to_wire).transpose()?,
                length: length.as_ref().map(value_to_wire).transpose()?,
            },
            HeapObject::BuiltinFunction(function) => HeapObjectWire::BuiltinFunction {
                owner: function.owner().name().to_string(),
                name: function.name().to_string(),
            },
            HeapObject::RegExp(regexp) => HeapObjectWire::RegExp {
                pattern: regexp.pattern.clone(),
                flags: regexp.flags.clone(),
                last_index: value_to_wire(&regexp.last_index)?,
            },
            HeapObject::RegExpMatch(result) => HeapObjectWire::RegExpMatch {
                items: result
                    .items
                    .iter()
                    .map(value_to_wire)
                    .collect::<Result<_, _>>()?,
                index: value_to_wire(&result.index)?,
                input: value_to_wire(&result.input)?,
                groups: value_to_wire(&result.groups)?,
            },
            HeapObject::Map(map) => HeapObjectWire::Map {
                entries: map
                    .entries
                    .iter()
                    .map(|(key, value)| Ok((value_to_wire(key)?, value_to_wire(value)?)))
                    .collect::<Result<_, &'static str>>()?,
            },
            HeapObject::Set(set) => HeapObjectWire::Set {
                values: set
                    .values
                    .iter()
                    .map(value_to_wire)
                    .collect::<Result<_, _>>()?,
            },
            HeapObject::Date(date) => HeapObjectWire::Date {
                milliseconds: number_to_wire(date.milliseconds),
            },
            HeapObject::Error(error) => HeapObjectWire::Error {
                error_kind: error.kind,
                message: error.message.clone(),
                cause: error.cause.as_ref().map(value_to_wire).transpose()?,
                errors: error.errors.as_ref().map(value_to_wire).transpose()?,
            },
            HeapObject::Url(url) => HeapObjectWire::Url {
                href: url.href.clone(),
                search_params: value_to_wire(&url.search_params)?,
            },
            HeapObject::UrlSearchParams(params) => HeapObjectWire::UrlSearchParams {
                entries: params.entries.clone(),
            },
            HeapObject::Cell(value) => HeapObjectWire::Cell {
                value: value_to_wire(value)?,
            },
        })
    }

    fn object_from_wire(object: HeapObjectWire) -> Result<HeapObject, &'static str> {
        Ok(match object {
            HeapObjectWire::Tuple { items } => HeapObject::Tuple(
                items
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
            ),
            HeapObjectWire::List { items, holes } => HeapObject::sparse_list(
                items
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
                holes,
            )?,
            HeapObjectWire::Record { fields } => {
                HeapObject::Record(Box::new(record_from_wire(fields)?))
            }
            HeapObjectWire::Closure {
                function,
                captures,
                name,
                length,
            } => HeapObject::Closure {
                function,
                captures: captures
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
                name: name.map(value_from_wire).transpose()?,
                length: length.map(value_from_wire).transpose()?,
            },
            HeapObjectWire::BuiltinFunction { owner, name } => HeapObject::BuiltinFunction(
                crate::runtime::heap::BuiltinFunction::named_scoped(&owner, &name)
                    .ok_or("unknown built-in function")?,
            ),
            HeapObjectWire::RegExp {
                pattern,
                flags,
                last_index,
            } => {
                crate::runtime::validate_regexp(&pattern, &flags)
                    .map_err(|_| "RegExp pattern or flags violate TypeScript bounds")?;
                HeapObject::RegExp(RegExpObject {
                    pattern,
                    flags,
                    last_index: value_from_wire(last_index)?,
                    compiled_program: None,
                })
            }
            HeapObjectWire::RegExpMatch {
                items,
                index,
                input,
                groups,
            } => HeapObject::RegExpMatch(RegExpMatchObject {
                items: items
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
                index: value_from_wire(index)?,
                input: value_from_wire(input)?,
                groups: value_from_wire(groups)?,
            }),
            HeapObjectWire::Map { entries } => HeapObject::Map(MapObject {
                entries: entries
                    .into_iter()
                    .map(|(key, value)| Ok((value_from_wire(key)?, value_from_wire(value)?)))
                    .collect::<Result<_, &'static str>>()?,
            }),
            HeapObjectWire::Set { values } => HeapObject::Set(SetObject {
                values: values
                    .into_iter()
                    .map(value_from_wire)
                    .collect::<Result<_, _>>()?,
            }),
            HeapObjectWire::Date { milliseconds } => HeapObject::Date(DateObject {
                milliseconds: number_from_wire(milliseconds)?,
            }),
            HeapObjectWire::Error {
                error_kind,
                message,
                cause,
                errors,
            } => HeapObject::Error(ErrorObject {
                kind: error_kind,
                message,
                cause: cause.map(value_from_wire).transpose()?,
                errors: errors.map(value_from_wire).transpose()?,
            }),
            HeapObjectWire::Url {
                href,
                search_params,
            } => HeapObject::Url(UrlObject {
                href,
                search_params: value_from_wire(search_params)?,
            }),
            HeapObjectWire::UrlSearchParams { entries } => {
                HeapObject::UrlSearchParams(UrlSearchParamsObject { entries })
            }
            HeapObjectWire::Cell { value } => HeapObject::Cell(value_from_wire(value)?),
        })
    }

    pub(super) fn serialize_value<S>(value: &Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value_to_wire(value)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_value<'de, D>(deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        ValueWire::deserialize(deserializer)
            .and_then(|value| value_from_wire(value).map_err(serde::de::Error::custom))
    }

    pub(super) fn serialize_heap<S>(
        continuation: &VmHeapContinuation,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let heap = &continuation.heap;
        let objects = heap
            .objects_in_id_order()
            .map(|(id, object)| {
                Ok(HeapEntryWire {
                    id,
                    object: object_to_wire(object)?,
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()
            .map_err(serde::ser::Error::custom)?;
        HeapWire {
            header: crate::runtime::heap::HeapHeaderWire {
                allocation_counter: heap.allocations(),
            },
            objects,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize_heap<'de, D>(deserializer: D) -> Result<VmHeapContinuation, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = HeapWire::deserialize(deserializer)?;
        let objects = wire
            .objects
            .into_iter()
            .map(|entry| object_from_wire(entry.object).map(|object| (entry.id, object)))
            .collect::<Result<_, _>>()
            .map_err(serde::de::Error::custom)?;
        let heap = Heap::from_wire(
            HeapRestoreWire {
                header: wire.header,
                objects,
            },
            &[],
        )
        .map_err(serde::de::Error::custom)?;
        Ok(VmHeapContinuation::new(heap))
    }

    fn optional_to_wire(value: &Option<Value>) -> Result<OptionalValueWire, &'static str> {
        match value {
            Some(value) => value_to_wire(value).map(OptionalValueWire::Set),
            None => Ok(OptionalValueWire::Unset),
        }
    }

    fn optional_from_wire(value: OptionalValueWire) -> Result<Option<Value>, &'static str> {
        Ok(match value {
            OptionalValueWire::Unset => None,
            OptionalValueWire::Set(value) => Some(value_from_wire(value)?),
        })
    }

    pub(super) fn serialize_values<S>(values: &[Value], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values
            .iter()
            .map(value_to_wire)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_values<'de, D>(deserializer: D) -> Result<Vec<Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<ValueWire>::deserialize(deserializer).and_then(|values| {
            values
                .into_iter()
                .map(value_from_wire)
                .collect::<Result<_, _>>()
                .map_err(serde::de::Error::custom)
        })
    }

    pub(super) fn serialize_optional_value<S>(
        value: &Option<Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        optional_to_wire(value)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_optional_value<'de, D>(
        deserializer: D,
    ) -> Result<Option<Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        OptionalValueWire::deserialize(deserializer)
            .and_then(|value| optional_from_wire(value).map_err(serde::de::Error::custom))
    }

    pub(super) fn serialize_slots<S>(
        slots: &[Option<Value>],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        slots
            .iter()
            .map(optional_to_wire)
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_slots<'de, D>(deserializer: D) -> Result<Vec<Option<Value>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<OptionalValueWire>::deserialize(deserializer).and_then(|slots| {
            slots
                .into_iter()
                .map(optional_from_wire)
                .collect::<Result<_, _>>()
                .map_err(serde::de::Error::custom)
        })
    }

    pub(super) fn serialize_record<S>(record: &Record, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value_to_wire(&Value::Record(Arc::new(record.clone())))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }

    pub(super) fn deserialize_record<'de, D>(deserializer: D) -> Result<Record, D::Error>
    where
        D: Deserializer<'de>,
    {
        ValueWire::deserialize(deserializer).and_then(|value| match value_from_wire(value) {
            Err(error) => Err(serde::de::Error::custom(error)),
            Ok(Value::Record(record)) => Ok((*record).clone()),
            Ok(_) => Err(serde::de::Error::custom("expected continuation record")),
        })
    }
}

/// Slots, globals and a parked loop binding all survive the boundary: whatever
/// they name is theirs, and two of them naming one object is the aliasing this
/// round exists to make unrepresentable. The operand stack, the last-value
/// register and an iterator's captured cursor are execution scratch — the VM
/// legitimately holds a value on the stack and in the slot it was just stored
/// into, and a cursor holds the elements it is handing out one at a time — so
/// they borrow without owning.
fn iterator_to_continuation(
    iterator: &IterState,
    location: &str,
) -> Result<VmIteratorContinuation, ContinuationError> {
    validate_optional_value(
        iterator.restore.previous.as_ref(),
        &format!("{location} restore value"),
    )?;
    let cursor = match &iterator.cursor {
        IterCursor::List {
            values,
            index,
            collection,
        } => {
            validate_values(values, &format!("{location} values"))?;
            validate_optional_value(collection.as_ref(), &format!("{location} collection"))?;
            VmIteratorCursor::List {
                values: values.iter().cloned().collect(),
                next_index: *index,
                collection: collection.clone(),
            }
        }
        IterCursor::Live { source, index } => {
            validate_optional_value(Some(source), &format!("{location} source"))?;
            VmIteratorCursor::Live {
                source: source.clone(),
                next_index: *index,
            }
        }
        IterCursor::Range { next, end, step } => VmIteratorCursor::Range {
            next: *next,
            end: *end,
            step: *step,
        },
    };
    Ok(VmIteratorContinuation {
        cursor,
        binding_slot: iterator.binding,
        restore_value: iterator.restore.previous.clone(),
    })
}

fn iterator_from_continuation(iterator: VmIteratorContinuation) -> IterState {
    IterState {
        cursor: match iterator.cursor {
            VmIteratorCursor::List {
                values,
                next_index,
                collection,
            } => IterCursor::List {
                values: values.into(),
                index: next_index,
                collection,
            },
            VmIteratorCursor::Live { source, next_index } => IterCursor::Live {
                source,
                index: next_index,
            },
            VmIteratorCursor::Range { next, end, step } => IterCursor::Range { next, end, step },
        },
        binding: iterator.binding_slot,
        restore: LoopRestore {
            previous: iterator.restore_value,
        },
        heapified: false,
    }
}

fn profile_from_continuation(
    profile: VmProfileContinuation,
) -> Result<ProfileAccumulator, ContinuationError> {
    Ok(ProfileAccumulator {
        instruction_counts: profile
            .instruction_counts
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
        instruction_times: profile
            .instruction_times
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
        builtin_counts: profile
            .builtin_counts
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
        builtin_times: profile
            .builtin_times
            .try_into()
            .map_err(|_| ContinuationError::ProfileShapeMismatch)?,
    })
}

mod program_validation;
mod structural_validation;
use program_validation::{
    validate_parked_await_bound, validate_program_continuation, validate_resume_point,
};
use structural_validation::{
    validate_continuation, validate_optional_value, validate_value, validate_values,
};

mod restore;

#[cfg(test)]
mod tests;

mod definition_refs;
