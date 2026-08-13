use std::sync::Arc;

use super::{
    CompiledProgram, ContinuationError, Heap, HeapId, HeapObject, HeapRestoreWire, ImageValue,
    PersistedRoots, ProjectedValue, Record, ResourceHandle, RuntimeError, Value,
    record_with_capacity,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod wire;
use wire::child_location;

mod canonical_messagepack;
pub use canonical_messagepack::{CanonicalMapOrder, validate_canonical_messagepack_structure};

const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;
pub const LASHLANG_SNAPSHOT_VERSION: u32 = 4;
pub(crate) const MAX_SNAPSHOT_VALUE_DEPTH: usize = 64;
// The raw-wire guard is secondary to the explicit value-depth guard below. A
// nested heap value advances through at most four MessagePack containers (the
// entry, tagged object, value map, and items/fields container); the root, heap,
// and projection wrappers account for the fixed allowance. Deriving this keeps it coupled to the
// value-domain limit if the encoding gains or loses a wrapper layer, while
// leaving the explicit value-depth check as the primary boundary rejection.
const MAX_FIXED_SNAPSHOT_WRAPPER_DEPTH: usize = 20;
const MESSAGEPACK_CONTAINERS_PER_VALUE_LEVEL: usize = 4;
#[doc(hidden)]
pub const CANONICAL_MESSAGEPACK_DEPTH_LIMIT: usize = MAX_FIXED_SNAPSHOT_WRAPPER_DEPTH
    + MESSAGEPACK_CONTAINERS_PER_VALUE_LEVEL * MAX_SNAPSHOT_VALUE_DEPTH;
const MAX_SNAPSHOT_MESSAGEPACK_DEPTH: usize = CANONICAL_MESSAGEPACK_DEPTH_LIMIT;

/// One operation in a [`State::patch_globals`] batch.
#[derive(Clone, Debug, PartialEq)]
pub enum GlobalPatch {
    /// Binds `name`, replacing any existing value.
    Insert { name: String, value: Value },
    /// Binds `name` only when it is currently unbound.
    SetDefault { name: String, value: Value },
    /// Unbinds `name` when it is bound.
    Remove { name: String },
}

/// What a committed [`State::patch_globals`] batch changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GlobalPatchOutcome {
    /// Names this batch bound or rebound, in batch order.
    pub inserted: Vec<String>,
    /// Names this batch unbound, in batch order.
    pub removed: Vec<String>,
    /// Defaults this batch skipped because the name was already bound.
    pub unchanged: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub(super) globals: Record,
    pub(super) runtime_globals: Record,
    pub(super) heap: Heap,
}

impl PartialEq for State {
    fn eq(&self, other: &Self) -> bool {
        self.globals == other.globals
            && self.runtime_globals == other.runtime_globals
            && self.heap == other.heap
    }
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn globals(&self) -> &Record {
        &self.globals
    }

    pub fn set_default(
        &mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<bool, RuntimeError> {
        let outcome = self.patch_globals([GlobalPatch::SetDefault {
            name: name.into(),
            value,
        }])?;
        Ok(!outcome.inserted.is_empty())
    }

    pub fn insert_global(
        &mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<Option<Value>, RuntimeError> {
        let name = name.into();
        let previous = self.globals.get(&name).cloned();
        self.patch_globals([GlobalPatch::Insert { name, value }])?;
        Ok(previous)
    }

    pub fn remove_global(&mut self, name: &str) -> Option<Value> {
        let previous = self.globals.get(name).cloned();
        self.patch_globals([GlobalPatch::Remove {
            name: name.to_string(),
        }])
        .expect("removing a global cannot exceed the heap bound");
        previous
    }

    /// Applies a batch of global patches as one transaction.
    ///
    /// The whole batch is staged against copies of the visible globals, the
    /// runtime roots, and the heap; nothing is published until every operation
    /// has succeeded. A rejected batch therefore leaves the state byte-identical
    /// rather than partially applied, and the caller's own bookkeeping can be
    /// committed together with it. The heap is cloned once and collected once
    /// per batch instead of once per key.
    pub fn patch_globals(
        &mut self,
        patch: impl IntoIterator<Item = GlobalPatch>,
    ) -> Result<GlobalPatchOutcome, RuntimeError> {
        let patch = patch.into_iter().collect::<Vec<_>>();
        if patch.is_empty() {
            return Ok(GlobalPatchOutcome::default());
        }
        let heap_backed = !self.runtime_globals.is_empty() || self.heap.has_runtime_state();
        let mut globals = self.globals.clone();
        let mut runtime_globals = self.runtime_globals.clone();
        let mut heap = self.heap.clone();
        let mut outcome = GlobalPatchOutcome::default();
        for operation in patch {
            match operation {
                GlobalPatch::SetDefault { name, value } if globals.get(&name).is_some() => {
                    outcome.unchanged.push(name);
                    let _ = value;
                }
                GlobalPatch::Insert { name, value } | GlobalPatch::SetDefault { name, value } => {
                    if heap_backed {
                        runtime_globals.remove(&name);
                        let runtime_value = heap.isolate_value(&value)?;
                        runtime_globals.insert(name.clone(), runtime_value);
                    }
                    globals.insert(name.clone(), value);
                    outcome.inserted.push(name);
                }
                GlobalPatch::Remove { name } => {
                    let existed = globals.remove(&name).is_some();
                    if heap_backed {
                        runtime_globals.remove(&name);
                    }
                    if existed {
                        outcome.removed.push(name);
                    }
                }
            }
        }
        if heap_backed {
            let roots = runtime_globals.values().cloned().collect::<Vec<_>>();
            heap.collect(roots.iter());
        }
        self.globals = globals;
        self.runtime_globals = runtime_globals;
        self.heap = heap;
        Ok(outcome)
    }

    /// Captures the state as a persistable snapshot.
    ///
    /// The captured heap is collected: a snapshot holds exactly the objects its
    /// roots reach, which is also exactly what the wire carries. Without this a
    /// snapshot taken straight after execution carries whatever garbage the last
    /// collection cycle had not reached yet, and `decode(encode(snapshot))`
    /// could not equal `snapshot`.
    pub fn snapshot(&self) -> Snapshot {
        let mut heap = self.heap.clone();
        let roots = self.runtime_globals.values().cloned().collect::<Vec<_>>();
        heap.collect(roots.iter());
        Snapshot {
            globals: self.globals.clone(),
            runtime_globals: self.runtime_globals.clone(),
            heap,
        }
    }

    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        Self {
            globals: snapshot.globals,
            runtime_globals: snapshot.runtime_globals,
            heap: snapshot.heap,
        }
    }

    pub(crate) fn validate_program(&self, program: &CompiledProgram) -> Result<(), RuntimeError> {
        self.heap.validate_closures(&program.chunk.functions)
    }

    pub(super) fn take_runtime(&mut self) -> (Record, Heap) {
        let globals = if self.runtime_globals.is_empty()
            && !self.heap.has_runtime_state()
            && !self.globals.is_empty()
        {
            std::mem::take(&mut self.globals)
        } else {
            std::mem::take(&mut self.runtime_globals)
        };
        (globals, std::mem::take(&mut self.heap))
    }

    pub(super) fn install_runtime(
        &mut self,
        runtime_globals: Record,
        mut heap: Heap,
    ) -> Result<(), RuntimeError> {
        let globals = materialize_runtime_globals(&runtime_globals, &mut heap)?;
        self.globals = globals;
        self.runtime_globals = runtime_globals;
        self.heap = heap;
        Ok(())
    }
}

pub(super) fn materialize_runtime_globals(
    runtime_globals: &Record,
    heap: &mut Heap,
) -> Result<Record, RuntimeError> {
    let mut globals = record_with_capacity(runtime_globals.len());
    for entry in runtime_globals.entries.iter() {
        match heap.export_for_instruction(&entry.value) {
            Ok(value) => {
                globals.insert_symbolized(entry.symbol, entry.name.clone(), value);
            }
            // Function values remain VM-private heap objects. A closure at any
            // depth omits the whole global rather than leaking a partial tree.
            Err(RuntimeError::FunctionValueAtHostBoundary) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(globals)
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    globals: Record,
    runtime_globals: Record,
    heap: Heap,
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.globals == other.globals
            && self.runtime_globals == other.runtime_globals
            && self.heap == other.heap
    }
}

impl Snapshot {
    pub fn new(globals: Record) -> Self {
        Self {
            globals,
            runtime_globals: Record::new(),
            heap: Heap::default(),
        }
    }

    pub fn globals(&self) -> &Record {
        &self.globals
    }

    pub fn into_globals(self) -> Record {
        self.globals
    }
    /// Encodes this snapshot as canonical, named-field MessagePack.
    ///
    /// Every byte sequence emitted here decodes and re-encodes identically.
    /// Accepted foreign wires have the same fixed-point property. The outer
    /// RLM envelope documents its single field-order exception separately.
    ///
    /// Snapshot equality does not imply byte equality for `-0.0` and `+0.0`:
    /// they compare equal under `PartialEq`, but preserve their distinct bits.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, ContinuationError> {
        let wire = CanonicalSnapshot::try_from(self)?;
        rmp_serde::to_vec_named(&wire).map_err(|_| ContinuationError::UnserializableValue {
            location: "snapshot".to_string(),
            variant: "canonical encoding",
        })
    }

    /// Decodes canonical snapshot MessagePack after enforcing Lashlang's own
    /// structural nesting bound and canonical wire representation in one raw
    /// byte pass, before serde deserialization.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, SnapshotDecodeError> {
        validate_snapshot_messagepack(bytes)?;
        let wire: CanonicalSnapshot = rmp_serde::from_slice(bytes)
            .map_err(|error| SnapshotDecodeError::InvalidEncoding(error.to_string()))?;
        if wire.version != LASHLANG_SNAPSHOT_VERSION {
            return Err(SnapshotDecodeError::VersionMismatch {
                expected: LASHLANG_SNAPSHOT_VERSION,
                found: wire.version,
            });
        }
        let snapshot: Self = wire.try_into()?;
        let canonical = snapshot
            .to_canonical_bytes()
            .map_err(|error| SnapshotDecodeError::InvalidEncoding(error.to_string()))?;
        if canonical.as_slice() != bytes {
            return Err(SnapshotDecodeError::NonCanonicalEncoding {
                location: "snapshot".to_string(),
                reason: "wire is not a byte-for-byte canonical fixed point".to_string(),
            });
        }
        Ok(snapshot)
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SnapshotDecodeError {
    #[error("snapshot version {found} is incompatible with version {expected}")]
    VersionMismatch { expected: u32, found: u32 },
    #[error("snapshot value exceeds the maximum nesting depth of {limit}")]
    ValueDepthLimitExceeded { limit: usize },
    #[error("snapshot exceeds the maximum MessagePack nesting depth of {limit}")]
    DepthLimitExceeded { limit: usize },
    #[error("non-canonical snapshot encoding at `{location}`: {reason}")]
    NonCanonicalEncoding { location: String, reason: String },
    #[error("invalid canonical snapshot encoding: {0}")]
    InvalidEncoding(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalSnapshot {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    globals: Option<Vec<CanonicalBinding>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    heap: Option<CanonicalHeap>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalHeap {
    next_id: u64,
    allocation_counter: u64,
    live_logical_bytes: u64,
    size_schedule_version: u32,
    roots: Vec<CanonicalBinding>,
    objects: Vec<CanonicalHeapEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalHeapEntry {
    id: HeapId,
    object: CanonicalHeapObject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanonicalHeapObject {
    Tuple {
        items: Vec<CanonicalValue>,
    },
    List {
        items: Vec<CanonicalValue>,
    },
    Record {
        fields: Vec<CanonicalBinding>,
    },
    Closure {
        function: u32,
        captures: Vec<CanonicalValue>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalBinding {
    name: String,
    value: CanonicalValue,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanonicalValue {
    Null {},
    Undefined {},
    Bool { value: bool },
    Number { value: f64 },
    String { value: String },
    Image { value: ImageValue },
    Resource { value: ResourceHandle },
    Ref { value: HeapId },
    Tuple { items: Vec<CanonicalValue> },
    List { items: Vec<CanonicalValue> },
    Record { fields: Vec<CanonicalBinding> },
    Projected { value: CanonicalProjectedValue },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalProjectedValue {
    name: String,
    type_name: String,
    projection_ref: Option<CanonicalJsonValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanonicalJsonValue {
    Null {},
    Bool { value: bool },
    Number { value: serde_json::Number },
    String { value: String },
    Array { items: Vec<CanonicalJsonValue> },
    Object { fields: Vec<CanonicalJsonField> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalJsonField {
    name: String,
    value: CanonicalJsonValue,
}

impl TryFrom<&Snapshot> for CanonicalSnapshot {
    type Error = ContinuationError;

    fn try_from(snapshot: &Snapshot) -> Result<Self, Self::Error> {
        let mut globals = snapshot.globals.iter().collect::<Vec<_>>();
        globals.sort_unstable_by_key(|(name, _)| *name);
        if snapshot.runtime_globals.is_empty() && !snapshot.heap.has_runtime_state() {
            return Ok(Self {
                version: LASHLANG_SNAPSHOT_VERSION,
                globals: Some(
                    globals
                        .into_iter()
                        .map(|(name, value)| {
                            let location = child_location("globals", name);
                            Ok(CanonicalBinding {
                                name: name.to_string(),
                                value: CanonicalValue::from_runtime(value, &location, 0)?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                ),
                heap: None,
            });
        }

        let mut heap = snapshot.heap.clone();
        let runtime_globals = snapshot.runtime_globals.clone();
        let root_values = runtime_globals.values().cloned().collect::<Vec<_>>();
        heap.collect(root_values.iter());
        // The writer checks the same forest invariant the reader enforces, in
        // release builds too. A violation then fails here, at the encode that
        // introduced it, rather than in another process at a later cold
        // restore — and it can never be written to durable storage at all.
        let mut forest_roots = PersistedRoots::default();
        forest_roots.durable_all(runtime_globals.iter());
        heap.validate_persisted_graph(&forest_roots)
            .map_err(|reason| ContinuationError::UnserializableValue {
                location: format!("snapshot heap: {reason}"),
                variant: "shared heap object",
            })?;
        drop(forest_roots);
        let mut roots = runtime_globals.iter().collect::<Vec<_>>();
        roots.sort_unstable_by_key(|(name, _)| *name);
        Ok(Self {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: None,
            heap: Some(CanonicalHeap {
                next_id: heap.next_id,
                allocation_counter: heap.allocations(),
                live_logical_bytes: heap.live_logical_bytes(),
                size_schedule_version: heap.schedule_version(),
                roots: roots
                    .into_iter()
                    .map(|(name, value)| {
                        Ok(CanonicalBinding {
                            name: name.to_string(),
                            value: CanonicalValue::from_runtime(
                                value,
                                &child_location("heap.roots", name),
                                0,
                            )?,
                        })
                    })
                    .collect::<Result<_, ContinuationError>>()?,
                objects: heap
                    .objects_in_id_order()
                    .map(|(id, object)| {
                        Ok(CanonicalHeapEntry {
                            id,
                            object: CanonicalHeapObject::from_runtime(object, id)?,
                        })
                    })
                    .collect::<Result<_, ContinuationError>>()?,
            }),
        })
    }
}

impl TryFrom<CanonicalSnapshot> for Snapshot {
    type Error = SnapshotDecodeError;

    fn try_from(snapshot: CanonicalSnapshot) -> Result<Self, Self::Error> {
        match (snapshot.globals, snapshot.heap) {
            (Some(globals), None) => Ok(Self {
                globals: bindings_into_record(globals, "globals")?,
                runtime_globals: Record::new(),
                heap: Heap::default(),
            }),
            (None, Some(heap_wire)) => {
                let CanonicalHeap {
                    next_id,
                    allocation_counter,
                    live_logical_bytes,
                    size_schedule_version,
                    roots,
                    objects,
                } = heap_wire;
                let runtime_globals = bindings_into_record(roots, "heap.roots")?;
                let objects = objects
                    .into_iter()
                    .map(|entry| entry.object.into_runtime().map(|object| (entry.id, object)))
                    .collect::<Result<_, _>>()?;
                let heap = Heap::from_wire(
                    HeapRestoreWire {
                        next_id,
                        allocation_counter,
                        live_logical_bytes,
                        size_schedule_version,
                        objects,
                    },
                    &runtime_globals.values().cloned().collect::<Vec<_>>(),
                )
                .map_err(SnapshotDecodeError::InvalidEncoding)?;
                let mut forest_roots = PersistedRoots::default();
                forest_roots.durable_all(runtime_globals.iter());
                heap.validate_persisted_graph(&forest_roots)
                    .map_err(SnapshotDecodeError::InvalidEncoding)?;
                // The heap form's depth lives in its chain of objects, not in
                // its MessagePack nesting, so the structural guard cannot see
                // it. Checking here — before anything materializes a root —
                // means an over-deep wire is refused rather than overflowing
                // the stack of whatever tries to read it.
                if heap.max_value_depth(&forest_roots) > MAX_SNAPSHOT_VALUE_DEPTH {
                    return Err(SnapshotDecodeError::ValueDepthLimitExceeded {
                        limit: MAX_SNAPSHOT_VALUE_DEPTH,
                    });
                }
                drop(forest_roots);
                let mut globals = Record::new();
                for (name, value) in runtime_globals.iter() {
                    match heap.export(value) {
                        Ok(value) => {
                            globals.insert(name.to_string(), value);
                        }
                        Err(RuntimeError::FunctionValueAtHostBoundary) => {}
                        Err(error) => {
                            return Err(SnapshotDecodeError::InvalidEncoding(error.to_string()));
                        }
                    }
                }
                Ok(Self {
                    globals,
                    runtime_globals,
                    heap,
                })
            }
            _ => Err(SnapshotDecodeError::InvalidEncoding(
                "snapshot must contain exactly one of globals or heap".to_string(),
            )),
        }
    }
}

fn bindings_into_record(
    bindings: Vec<CanonicalBinding>,
    location: &str,
) -> Result<Record, SnapshotDecodeError> {
    let mut previous: Option<&str> = None;
    let mut record = Record::new();
    for binding in &bindings {
        if previous.is_some_and(|prior| prior >= binding.name.as_str()) {
            return Err(SnapshotDecodeError::NonCanonicalEncoding {
                location: location.to_string(),
                reason: "binding names must be strictly sorted and unique".to_string(),
            });
        }
        previous = Some(binding.name.as_str());
    }
    for binding in bindings {
        record.insert(binding.name, binding.value.into_runtime()?);
    }
    Ok(record)
}

#[derive(Clone, Copy)]
enum BindingValueKind {
    RootRuntime,
    Runtime,
    Json,
}

enum ExpectedValue {
    Snapshot,
    Runtime,
    Json,
    Projected,
    Image,
    Resource,
    String,
    Bool,
    F64,
    JsonNumber,
    Unsigned {
        maximum: u64,
    },
    OptionalUnsigned {
        maximum: u64,
    },
    OptionalJson,
    Key(&'static str),
    RuntimeArray,
    JsonArray,
    Bindings(BindingValueKind),
    ArrayElements {
        remaining: usize,
        next_index: usize,
        kind: BindingValueKind,
    },
    BindingElements {
        remaining: usize,
        previous: Option<String>,
        kind: BindingValueKind,
    },
}

struct ValidationFrame {
    expected: ExpectedValue,
    location: String,
    depth: usize,
    value_depth: usize,
}

fn validate_canonical_messagepack(bytes: &[u8]) -> Result<(), SnapshotDecodeError> {
    let mut cursor = 0;
    let mut pending = vec![ValidationFrame {
        expected: ExpectedValue::Snapshot,
        location: "snapshot".to_string(),
        depth: 1,
        value_depth: 0,
    }];
    while let Some(frame) = pending.pop() {
        validate_expected(bytes, &mut cursor, frame, &mut pending)?;
    }
    if cursor != bytes.len() {
        return Err(invalid_messagepack("trailing bytes"));
    }
    Ok(())
}

const SNAPSHOT_FIELDS: &[&str] = &["version", "globals", "heap"];
const HEAP_FIELDS: &[&str] = &[
    "next_id",
    "allocation_counter",
    "live_logical_bytes",
    "size_schedule_version",
    "roots",
    "objects",
];
const BINDING_FIELDS: &[&str] = &["name", "value"];
const HEAP_ENTRY_FIELDS: &[&str] = &["id", "object"];
const TAGGED_VALUE_FIELDS: &[&str] = &["kind", "value", "items", "fields", "function", "captures"];

fn validate_snapshot_messagepack(bytes: &[u8]) -> Result<(), SnapshotDecodeError> {
    let globals_result = validate_snapshot_globals(bytes);
    if matches!(
        globals_result,
        Err(SnapshotDecodeError::ValueDepthLimitExceeded { .. })
            | Err(SnapshotDecodeError::NonCanonicalEncoding { .. })
    ) {
        return globals_result;
    }
    validate_canonical_messagepack_structure(
        bytes,
        "snapshot",
        MAX_SNAPSHOT_MESSAGEPACK_DEPTH,
        |location| match location {
            "snapshot" => CanonicalMapOrder::Declared(SNAPSHOT_FIELDS),
            "snapshot.heap" => CanonicalMapOrder::Declared(HEAP_FIELDS),
            _ if location.ends_with(".object") => CanonicalMapOrder::Declared(TAGGED_VALUE_FIELDS),
            _ if location.ends_with(".projection_ref") => CanonicalMapOrder::Unordered,
            _ if is_collection_entry(location, "globals")
                || is_collection_entry(location, "roots")
                || is_collection_entry(location, "fields") =>
            {
                CanonicalMapOrder::Declared(BINDING_FIELDS)
            }
            _ if is_collection_entry(location, "objects") => {
                CanonicalMapOrder::Declared(HEAP_ENTRY_FIELDS)
            }
            _ if location.ends_with(".value") => CanonicalMapOrder::Unordered,
            _ => CanonicalMapOrder::Unordered,
        },
        |location| {
            location == "snapshot"
                || location == "snapshot.heap"
                || location.ends_with(".object")
                || is_collection_entry(location, "globals")
                || is_collection_entry(location, "roots")
                || is_collection_entry(location, "objects")
                || is_collection_entry(location, "fields")
        },
    )?;
    globals_result
}

fn validate_snapshot_globals(bytes: &[u8]) -> Result<(), SnapshotDecodeError> {
    let mut cursor = 0;
    let fields = take_map_length(bytes, &mut cursor, "snapshot", "snapshot")?;
    if fields != 2 {
        return Err(non_canonical(
            "snapshot",
            "snapshot must contain exactly two fields",
        ));
    }
    expect_key(bytes, &mut cursor, "version", "snapshot")?;
    skip_messagepack_value(bytes, &mut cursor)?;
    let representation = take_canonical_string(bytes, &mut cursor, "snapshot")?;
    if representation == "heap" {
        // The heap form carries its values as a flat object table, so the
        // value-depth bound is enforced against the object graph after decode
        // rather than against the wire's nesting here.
        return Ok(());
    }
    if representation != "globals" {
        return Err(non_canonical(
            "snapshot",
            "snapshot representation must be globals or heap",
        ));
    }
    let globals_start = cursor;
    skip_messagepack_value(bytes, &mut cursor)?;
    let globals = &bytes[globals_start..cursor];
    let mut legacy_root = Vec::with_capacity(9 + globals.len());
    legacy_root.extend_from_slice(&[0x81, 0xa7]);
    legacy_root.extend_from_slice(b"globals");
    legacy_root.extend_from_slice(globals);
    validate_canonical_messagepack(&legacy_root)
}

fn skip_messagepack_value(bytes: &[u8], cursor: &mut usize) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    match marker {
        0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(()),
        0xcc | 0xd0 => skip_bytes(bytes, cursor, 1),
        0xcd | 0xd1 => skip_bytes(bytes, cursor, 2),
        0xce | 0xd2 | 0xca => skip_bytes(bytes, cursor, 4),
        0xcf | 0xd3 | 0xcb => skip_bytes(bytes, cursor, 8),
        0xa0..=0xbf => skip_bytes(bytes, cursor, usize::from(marker & 0x1f)),
        0xd9 | 0xc4 => {
            let length = usize::from(take_byte(bytes, cursor)?);
            skip_bytes(bytes, cursor, length)
        }
        0xda | 0xc5 => {
            let length = usize::from(take_u16(bytes, cursor)?);
            skip_bytes(bytes, cursor, length)
        }
        0xdb | 0xc6 => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            skip_bytes(bytes, cursor, length)
        }
        0x90..=0x9f => {
            for _ in 0..usize::from(marker & 0x0f) {
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xdc => {
            let length = usize::from(take_u16(bytes, cursor)?);
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xdd => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0x80..=0x8f => {
            for _ in 0..usize::from(marker & 0x0f) {
                skip_messagepack_value(bytes, cursor)?;
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xde => {
            let length = usize::from(take_u16(bytes, cursor)?);
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        0xdf => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            for _ in 0..length {
                skip_messagepack_value(bytes, cursor)?;
                skip_messagepack_value(bytes, cursor)?;
            }
            Ok(())
        }
        _ => Err(invalid_messagepack(&format!(
            "unsupported MessagePack marker 0x{marker:02x}"
        ))),
    }
}

fn is_collection_entry(location: &str, collection: &str) -> bool {
    let Some((_, suffix)) = location.rsplit_once(&format!(".{collection}")) else {
        return false;
    };
    suffix.starts_with('[') && suffix.ends_with(']') && !suffix.contains("].")
}

fn validate_expected(
    bytes: &[u8],
    cursor: &mut usize,
    frame: ValidationFrame,
    pending: &mut Vec<ValidationFrame>,
) -> Result<(), SnapshotDecodeError> {
    let ValidationFrame {
        expected,
        location,
        depth,
        value_depth,
    } = frame;
    match expected {
        ExpectedValue::Snapshot => {
            ensure_depth(depth)?;
            expect_struct_map(bytes, cursor, 1, &location, "snapshot")?;
            expect_key(bytes, cursor, "globals", &location)?;
            push(
                pending,
                ExpectedValue::Bindings(BindingValueKind::RootRuntime),
                "globals",
                depth + 1,
                value_depth,
            );
        }
        ExpectedValue::Runtime => {
            ensure_value_depth(value_depth)?;
            validate_runtime_value(bytes, cursor, &location, depth, value_depth, pending)?;
        }
        ExpectedValue::Json => {
            ensure_value_depth(value_depth)?;
            validate_json_value(bytes, cursor, &location, depth, value_depth, pending)?;
        }
        ExpectedValue::Projected => {
            ensure_depth(depth)?;
            expect_struct_map(bytes, cursor, 3, &location, "projected value")?;
            expect_key(bytes, cursor, "name", &location)?;
            push(
                pending,
                ExpectedValue::OptionalJson,
                format!("{location}.projection_ref"),
                depth + 1,
                value_depth + 1,
            );
            push_key(pending, "projection_ref", &location, depth + 1, value_depth);
            push(
                pending,
                ExpectedValue::String,
                format!("{location}.type_name"),
                depth + 1,
                value_depth,
            );
            push_key(pending, "type_name", &location, depth + 1, value_depth);
            push(
                pending,
                ExpectedValue::String,
                format!("{location}.name"),
                depth + 1,
                value_depth,
            );
        }
        ExpectedValue::Image => {
            validate_image(bytes, cursor, &location, depth, value_depth, pending)?;
        }
        ExpectedValue::Resource => {
            ensure_depth(depth)?;
            expect_struct_map(bytes, cursor, 2, &location, "resource handle")?;
            expect_key(bytes, cursor, "resource_type", &location)?;
            push(
                pending,
                ExpectedValue::String,
                format!("{location}.alias"),
                depth + 1,
                value_depth,
            );
            push_key(pending, "alias", &location, depth + 1, value_depth);
            push(
                pending,
                ExpectedValue::String,
                format!("{location}.resource_type"),
                depth + 1,
                value_depth,
            );
        }
        ExpectedValue::String => {
            take_canonical_string(bytes, cursor, &location)?;
        }
        ExpectedValue::Bool => match take_byte(bytes, cursor)? {
            0xc2 | 0xc3 => {}
            marker => return Err(unexpected_marker(&location, "a boolean", marker)),
        },
        ExpectedValue::F64 => validate_f64(bytes, cursor, &location)?,
        ExpectedValue::JsonNumber => validate_json_number(bytes, cursor, &location)?,
        ExpectedValue::Unsigned { maximum } => {
            validate_unsigned(bytes, cursor, &location, maximum)?;
        }
        ExpectedValue::OptionalUnsigned { maximum } => {
            if bytes.get(*cursor) == Some(&0xc0) {
                *cursor += 1;
            } else {
                validate_unsigned(bytes, cursor, &location, maximum)?;
            }
        }
        ExpectedValue::OptionalJson => {
            if bytes.get(*cursor) == Some(&0xc0) {
                *cursor += 1;
            } else {
                push(pending, ExpectedValue::Json, location, depth, value_depth);
            }
        }
        ExpectedValue::Key(key) => expect_key(bytes, cursor, key, &location)?,
        ExpectedValue::RuntimeArray => {
            ensure_depth(depth)?;
            let length = take_array_length(bytes, cursor, &location)?;
            push(
                pending,
                ExpectedValue::ArrayElements {
                    remaining: length,
                    next_index: 0,
                    kind: BindingValueKind::Runtime,
                },
                location,
                depth,
                value_depth,
            );
        }
        ExpectedValue::JsonArray => {
            ensure_depth(depth)?;
            let length = take_array_length(bytes, cursor, &location)?;
            push(
                pending,
                ExpectedValue::ArrayElements {
                    remaining: length,
                    next_index: 0,
                    kind: BindingValueKind::Json,
                },
                location,
                depth,
                value_depth,
            );
        }
        ExpectedValue::Bindings(kind) => {
            ensure_depth(depth)?;
            let length = take_array_length(bytes, cursor, &location)?;
            push(
                pending,
                ExpectedValue::BindingElements {
                    remaining: length,
                    previous: None,
                    kind,
                },
                location,
                depth,
                value_depth,
            );
        }
        ExpectedValue::ArrayElements {
            remaining,
            next_index,
            kind,
        } => {
            if remaining == 0 {
                return Ok(());
            }
            push(
                pending,
                ExpectedValue::ArrayElements {
                    remaining: remaining - 1,
                    next_index: next_index + 1,
                    kind,
                },
                location.clone(),
                depth,
                value_depth,
            );
            let expected = match kind {
                BindingValueKind::RootRuntime | BindingValueKind::Runtime => ExpectedValue::Runtime,
                BindingValueKind::Json => ExpectedValue::Json,
            };
            push(
                pending,
                expected,
                format!("{location}[{next_index}]"),
                depth + 1,
                value_depth + 1,
            );
        }
        ExpectedValue::BindingElements {
            remaining,
            previous,
            kind,
        } => {
            if remaining == 0 {
                return Ok(());
            }
            ensure_depth(depth)?;
            expect_struct_map(bytes, cursor, 2, &location, "dynamic map binding")?;
            expect_key(bytes, cursor, "name", &location)?;
            let name = take_canonical_string(bytes, cursor, &format!("{location}.name"))?;
            if previous.as_deref().is_some_and(|previous| previous >= name) {
                return Err(non_canonical(
                    &location,
                    "dynamic map keys must be strictly sorted and unique",
                ));
            }
            expect_key(bytes, cursor, "value", &location)?;
            let child = child_location(&location, name);
            push(
                pending,
                ExpectedValue::BindingElements {
                    remaining: remaining - 1,
                    previous: Some(name.to_string()),
                    kind,
                },
                location,
                depth,
                value_depth,
            );
            let (expected, child_value_depth) = match kind {
                BindingValueKind::RootRuntime => (ExpectedValue::Runtime, value_depth),
                BindingValueKind::Runtime => (ExpectedValue::Runtime, value_depth + 1),
                BindingValueKind::Json => (ExpectedValue::Json, value_depth + 1),
            };
            push(pending, expected, child, depth + 1, child_value_depth);
        }
    }
    Ok(())
}

fn validate_runtime_value(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    depth: usize,
    value_depth: usize,
    pending: &mut Vec<ValidationFrame>,
) -> Result<(), SnapshotDecodeError> {
    ensure_depth(depth)?;
    let fields = take_map_length(bytes, cursor, location, "runtime value")?;
    expect_key(bytes, cursor, "kind", location)?;
    let kind = take_canonical_string(bytes, cursor, &format!("{location}.kind"))?;
    let (key, expected) = match kind {
        "null" if fields == 1 => return Ok(()),
        "bool" => ("value", ExpectedValue::Bool),
        "number" => ("value", ExpectedValue::F64),
        "string" => ("value", ExpectedValue::String),
        "image" => ("value", ExpectedValue::Image),
        "resource" => ("value", ExpectedValue::Resource),
        "tuple" | "list" => ("items", ExpectedValue::RuntimeArray),
        "record" => ("fields", ExpectedValue::Bindings(BindingValueKind::Runtime)),
        "projected" => ("value", ExpectedValue::Projected),
        "null" => {
            return Err(non_canonical(
                location,
                "null value must contain only its kind",
            ));
        }
        _ => {
            return Err(invalid_at(
                location,
                &format!("unknown runtime value kind `{kind}`"),
            ));
        }
    };
    if fields != 2 {
        return Err(non_canonical(
            location,
            "runtime value has a non-canonical field count",
        ));
    }
    expect_key(bytes, cursor, key, location)?;
    push(
        pending,
        expected,
        format!("{location}.{key}"),
        depth + 1,
        value_depth,
    );
    Ok(())
}

fn validate_json_value(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    depth: usize,
    value_depth: usize,
    pending: &mut Vec<ValidationFrame>,
) -> Result<(), SnapshotDecodeError> {
    ensure_depth(depth)?;
    let fields = take_map_length(bytes, cursor, location, "projection JSON value")?;
    expect_key(bytes, cursor, "kind", location)?;
    let kind = take_canonical_string(bytes, cursor, &format!("{location}.kind"))?;
    let (key, expected) = match kind {
        "null" if fields == 1 => return Ok(()),
        "bool" => ("value", ExpectedValue::Bool),
        "number" => ("value", ExpectedValue::JsonNumber),
        "string" => ("value", ExpectedValue::String),
        "array" => ("items", ExpectedValue::JsonArray),
        "object" => ("fields", ExpectedValue::Bindings(BindingValueKind::Json)),
        "null" => {
            return Err(non_canonical(
                location,
                "null JSON value must contain only its kind",
            ));
        }
        _ => {
            return Err(invalid_at(
                location,
                &format!("unknown projection JSON kind `{kind}`"),
            ));
        }
    };
    if fields != 2 {
        return Err(non_canonical(
            location,
            "projection JSON value has a non-canonical field count",
        ));
    }
    expect_key(bytes, cursor, key, location)?;
    push(
        pending,
        expected,
        format!("{location}.{key}"),
        depth + 1,
        value_depth,
    );
    Ok(())
}

fn validate_image(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    depth: usize,
    value_depth: usize,
    pending: &mut Vec<ValidationFrame>,
) -> Result<(), SnapshotDecodeError> {
    ensure_depth(depth)?;
    expect_struct_map(bytes, cursor, 7, location, "image descriptor")?;
    expect_key(bytes, cursor, "type", location)?;
    let kind = take_canonical_string(bytes, cursor, &format!("{location}.type"))?;
    if kind != "image" {
        return Err(invalid_at(
            location,
            "image descriptor has a non-image type",
        ));
    }
    for (key, expected) in [
        (
            "height",
            ExpectedValue::OptionalUnsigned {
                maximum: u64::from(u32::MAX),
            },
        ),
        (
            "width",
            ExpectedValue::OptionalUnsigned {
                maximum: u64::from(u32::MAX),
            },
        ),
        ("size", ExpectedValue::Unsigned { maximum: u64::MAX }),
        ("label", ExpectedValue::String),
        ("mime", ExpectedValue::String),
        ("id", ExpectedValue::String),
    ] {
        push(
            pending,
            expected,
            format!("{location}.{key}"),
            depth + 1,
            value_depth,
        );
        push_key(pending, key, location, depth + 1, value_depth);
    }
    Ok(())
}

fn push(
    pending: &mut Vec<ValidationFrame>,
    expected: ExpectedValue,
    location: impl Into<String>,
    depth: usize,
    value_depth: usize,
) {
    pending.push(ValidationFrame {
        expected,
        location: location.into(),
        depth,
        value_depth,
    });
}

fn push_key(
    pending: &mut Vec<ValidationFrame>,
    key: &'static str,
    parent: &str,
    depth: usize,
    value_depth: usize,
) {
    push(pending, ExpectedValue::Key(key), parent, depth, value_depth);
}

fn ensure_value_depth(depth: usize) -> Result<(), SnapshotDecodeError> {
    if depth > MAX_SNAPSHOT_VALUE_DEPTH {
        return Err(SnapshotDecodeError::ValueDepthLimitExceeded {
            limit: MAX_SNAPSHOT_VALUE_DEPTH,
        });
    }
    Ok(())
}

fn ensure_depth(depth: usize) -> Result<(), SnapshotDecodeError> {
    if depth > MAX_SNAPSHOT_MESSAGEPACK_DEPTH {
        return Err(SnapshotDecodeError::DepthLimitExceeded {
            limit: MAX_SNAPSHOT_MESSAGEPACK_DEPTH,
        });
    }
    Ok(())
}

fn expect_struct_map(
    bytes: &[u8],
    cursor: &mut usize,
    expected: usize,
    location: &str,
    description: &str,
) -> Result<(), SnapshotDecodeError> {
    let length = take_map_length(bytes, cursor, location, description)?;
    if length != expected {
        return Err(non_canonical(
            location,
            &format!("{description} must contain exactly {expected} fields"),
        ));
    }
    Ok(())
}

fn take_map_length(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    description: &str,
) -> Result<usize, SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    match marker {
        0x80..=0x8f => Ok(usize::from(marker & 0x0f)),
        0xde => {
            let length = usize::from(take_u16(bytes, cursor)?);
            if length <= 15 {
                Err(non_canonical(
                    location,
                    "map length is not minimally encoded",
                ))
            } else {
                Ok(length)
            }
        }
        0xdf => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            if length <= usize::from(u16::MAX) {
                Err(non_canonical(
                    location,
                    "map length is not minimally encoded",
                ))
            } else {
                Ok(length)
            }
        }
        0x90..=0x9f | 0xdc | 0xdd => Err(non_canonical(
            location,
            &format!("{description} must use map form, not sequence form"),
        )),
        _ => Err(unexpected_marker(location, "a map", marker)),
    }
}

fn take_array_length(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
) -> Result<usize, SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    match marker {
        0x90..=0x9f => Ok(usize::from(marker & 0x0f)),
        0xdc => {
            let length = usize::from(take_u16(bytes, cursor)?);
            if length <= 15 {
                Err(non_canonical(
                    location,
                    "array length is not minimally encoded",
                ))
            } else {
                Ok(length)
            }
        }
        0xdd => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            if length <= usize::from(u16::MAX) {
                Err(non_canonical(
                    location,
                    "array length is not minimally encoded",
                ))
            } else {
                Ok(length)
            }
        }
        _ => Err(unexpected_marker(location, "an array", marker)),
    }
}

fn expect_key(
    bytes: &[u8],
    cursor: &mut usize,
    expected: &str,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let found = take_canonical_string(bytes, cursor, location)?;
    if found != expected {
        return Err(non_canonical(
            location,
            &format!(
                "struct fields must use canonical order; expected `{expected}`, found `{found}`"
            ),
        ));
    }
    Ok(())
}

fn take_canonical_string<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    location: &str,
) -> Result<&'a str, SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    let length = match marker {
        0xa0..=0xbf => usize::from(marker & 0x1f),
        0xd9 => {
            let length = usize::from(take_byte(bytes, cursor)?);
            if length <= 31 {
                return Err(non_canonical(
                    location,
                    "string length is not minimally encoded",
                ));
            }
            length
        }
        0xda => {
            let length = usize::from(take_u16(bytes, cursor)?);
            if length <= usize::from(u8::MAX) {
                return Err(non_canonical(
                    location,
                    "string length is not minimally encoded",
                ));
            }
            length
        }
        0xdb => {
            let length = usize_from_u32(take_u32(bytes, cursor)?)?;
            if length <= usize::from(u16::MAX) {
                return Err(non_canonical(
                    location,
                    "string length is not minimally encoded",
                ));
            }
            length
        }
        _ => return Err(unexpected_marker(location, "a string", marker)),
    };
    let value = take_slice(bytes, cursor, length)?;
    std::str::from_utf8(value).map_err(|_| invalid_at(location, "string is not valid UTF-8"))
}

fn validate_f64(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    if marker == 0xcb {
        let bits = u64::from_be_bytes(take_array::<8>(bytes, cursor)?);
        let value = f64::from_bits(bits);
        if value.is_nan() && bits != CANONICAL_NAN_BITS {
            return Err(non_canonical(
                location,
                "NaN must use the canonical bit pattern",
            ));
        }
        return Ok(());
    }
    if marker == 0xca || is_integer_marker(marker) {
        return Err(non_canonical(
            location,
            "runtime number must use f64 encoding",
        ));
    }
    Err(unexpected_marker(location, "an f64", marker))
}

fn validate_json_number(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    if marker == 0xcb {
        let value = f64::from_bits(u64::from_be_bytes(take_array::<8>(bytes, cursor)?));
        if !value.is_finite() {
            return Err(invalid_at(
                location,
                "projection JSON number must be finite",
            ));
        }
        return Ok(());
    }
    if marker == 0xca {
        return Err(non_canonical(
            location,
            "floating-point number must use f64 encoding",
        ));
    }
    take_canonical_integer(bytes, cursor, location, marker).map(|_| ())
}

fn validate_unsigned(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    maximum: u64,
) -> Result<(), SnapshotDecodeError> {
    let marker = take_byte(bytes, cursor)?;
    let value = take_canonical_integer(bytes, cursor, location, marker)?;
    if value < 0 || value > i128::from(maximum) {
        return Err(invalid_at(location, "unsigned integer is out of range"));
    }
    Ok(())
}

include!("state/canonical_wire.rs");

#[cfg(test)]
#[path = "state/fixes3_tests.rs"]
mod fixes3_tests;

#[cfg(test)]
mod tests {
    include!("state/tests.rs");
}
