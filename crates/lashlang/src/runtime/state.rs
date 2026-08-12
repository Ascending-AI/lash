use std::sync::Arc;

use super::{
    ContinuationError, HEAP_SIZE_SCHEDULE_VERSION, Heap, HeapEntry, HeapId, HeapObject, ImageValue,
    ProjectedValue, Record, ResourceHandle, RuntimeError, Value, record_with_capacity,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod canonical_messagepack;
pub use canonical_messagepack::{CanonicalMapOrder, validate_canonical_messagepack_structure};

const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;
pub const LASHLANG_SNAPSHOT_VERSION: u32 = 2;
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

#[derive(Clone, Debug, Default)]
pub struct State {
    pub(super) globals: Record,
    pub(super) runtime_globals: Record,
    pub(super) heap: Heap,
}

impl PartialEq for State {
    fn eq(&self, other: &Self) -> bool {
        self.globals == other.globals
    }
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn globals(&self) -> &Record {
        &self.globals
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            globals: self.globals.clone(),
            runtime_globals: self.runtime_globals.clone(),
            heap: self.heap.clone(),
        }
    }

    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        Self {
            globals: snapshot.globals,
            runtime_globals: snapshot.runtime_globals,
            heap: snapshot.heap,
        }
    }

    pub(super) fn take_runtime(&mut self) -> (Record, Heap) {
        let globals = if self.runtime_globals.is_empty() && !self.globals.is_empty() {
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
        let mut globals = record_with_capacity(runtime_globals.len());
        for entry in runtime_globals.entries.iter() {
            globals.insert_symbolized(
                entry.symbol,
                entry.name.clone(),
                heap.export_for_instruction(&entry.value)?,
            );
        }
        self.globals = globals;
        self.runtime_globals = runtime_globals;
        self.heap = heap;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub globals: Record,
    runtime_globals: Record,
    heap: Heap,
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.globals == other.globals
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
        let canonical = rmp_serde::to_vec_named(&wire)
            .map_err(|error| SnapshotDecodeError::InvalidEncoding(error.to_string()))?;
        if canonical != bytes {
            return Err(SnapshotDecodeError::NonCanonicalEncoding {
                location: "snapshot".to_string(),
                reason: "wire is not a byte-for-byte canonical fixed point".to_string(),
            });
        }
        wire.try_into()
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
    globals: Vec<CanonicalBinding>,
    heap: CanonicalHeap,
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

impl Default for CanonicalHeap {
    fn default() -> Self {
        Self {
            next_id: 1,
            allocation_counter: 0,
            live_logical_bytes: 0,
            size_schedule_version: HEAP_SIZE_SCHEDULE_VERSION,
            roots: Vec::new(),
            objects: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CanonicalHeapEntry {
    id: HeapId,
    object: CanonicalHeapObject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanonicalHeapObject {
    Tuple { items: Vec<CanonicalValue> },
    List { items: Vec<CanonicalValue> },
    Record { fields: Vec<CanonicalBinding> },
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
        let (mut heap, runtime_globals) = if snapshot.runtime_globals.is_empty() {
            let mut heap = Heap::default();
            let mut runtime_globals = Record::new();
            for (name, value) in &globals {
                let value = heap.import((*value).clone()).map_err(|_| {
                    ContinuationError::UnserializableValue {
                        location: child_location("globals", name),
                        variant: "value rejected by heap import",
                    }
                })?;
                runtime_globals.insert((*name).to_string(), value);
            }
            (heap, runtime_globals)
        } else {
            (snapshot.heap.clone(), snapshot.runtime_globals.clone())
        };
        let root_values = runtime_globals.values().cloned().collect::<Vec<_>>();
        heap.collect(root_values.iter());
        let mut roots = runtime_globals.iter().collect::<Vec<_>>();
        roots.sort_unstable_by_key(|(name, _)| *name);
        Ok(Self {
            version: LASHLANG_SNAPSHOT_VERSION,
            globals: globals
                .into_iter()
                .map(|(name, value)| {
                    let location = child_location("globals", name);
                    Ok(CanonicalBinding {
                        name: name.to_string(),
                        value: CanonicalValue::from_runtime(value, &location, 0)?,
                    })
                })
                .collect::<Result<_, ContinuationError>>()?,
            heap: CanonicalHeap {
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
            },
        })
    }
}

impl TryFrom<CanonicalSnapshot> for Snapshot {
    type Error = SnapshotDecodeError;

    fn try_from(snapshot: CanonicalSnapshot) -> Result<Self, Self::Error> {
        if snapshot.heap.size_schedule_version != HEAP_SIZE_SCHEDULE_VERSION {
            return Err(SnapshotDecodeError::InvalidEncoding(format!(
                "unsupported heap size schedule version {}",
                snapshot.heap.size_schedule_version
            )));
        }
        let globals = snapshot
            .globals
            .into_iter()
            .map(|binding| {
                binding
                    .value
                    .into_runtime()
                    .map(|value| (binding.name, value))
            })
            .collect::<Result<_, _>>()?;
        let mut heap = Heap::default();
        heap.next_id = snapshot.heap.next_id;
        heap.allocations = snapshot.heap.allocation_counter;
        heap.schedule_version = snapshot.heap.size_schedule_version;
        heap.restore_collection_schedule();
        for entry in snapshot.heap.objects {
            let id_index = usize::try_from(entry.id.get()).map_err(|_| {
                SnapshotDecodeError::InvalidEncoding(
                    "heap object ID exceeds the platform storage index".to_string(),
                )
            })?;
            if entry.id.get() >= heap.next_id
                || heap.id_to_slot.get(id_index).is_some_and(Option::is_some)
            {
                return Err(SnapshotDecodeError::InvalidEncoding(
                    "heap object IDs must be unique, ordered, and below next_id".to_string(),
                ));
            }
            let object = entry.object.into_runtime()?;
            let logical_bytes = object.logical_bytes();
            let slot = heap.slots.len();
            heap.slots.push(Some(HeapEntry {
                id: entry.id,
                object,
                logical_bytes,
            }));
            if heap.id_to_slot.len() <= id_index {
                heap.id_to_slot.resize(id_index + 1, None);
            }
            heap.id_to_slot[id_index] = Some(slot);
            heap.live_logical_bytes = heap.live_logical_bytes.saturating_add(logical_bytes);
        }
        if heap.live_logical_bytes != snapshot.heap.live_logical_bytes {
            return Err(SnapshotDecodeError::InvalidEncoding(
                "heap logical-byte counter does not match live objects".to_string(),
            ));
        }
        let runtime_globals = snapshot
            .heap
            .roots
            .into_iter()
            .map(|binding| {
                binding
                    .value
                    .into_runtime()
                    .map(|value| (binding.name, value))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            globals,
            runtime_globals,
            heap,
        })
    }
}

impl CanonicalHeapObject {
    fn from_runtime(object: &HeapObject, id: HeapId) -> Result<Self, ContinuationError> {
        let location = format!("heap.objects[{}]", id.get());
        Ok(match object {
            HeapObject::Tuple(values) => Self::Tuple {
                items: canonical_items(values, &location, 0)?,
            },
            HeapObject::List(values) => Self::List {
                items: canonical_items(values, &location, 0)?,
            },
            HeapObject::Record(record) => {
                let mut fields = record.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Record {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            Ok(CanonicalBinding {
                                name: name.to_string(),
                                value: CanonicalValue::from_runtime(
                                    value,
                                    &child_location(&location, name),
                                    0,
                                )?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
        })
    }

    fn into_runtime(self) -> Result<HeapObject, SnapshotDecodeError> {
        Ok(match self {
            Self::Tuple { items } => HeapObject::Tuple(
                items
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
            ),
            Self::List { items } => HeapObject::List(
                items
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Record { fields } => HeapObject::Record(Box::new(
                fields
                    .into_iter()
                    .map(|field| field.value.into_runtime().map(|value| (field.name, value)))
                    .collect::<Result<_, _>>()?,
            )),
        })
    }
}

impl CanonicalValue {
    fn from_runtime(
        value: &Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        if depth > MAX_SNAPSHOT_VALUE_DEPTH {
            return Err(ContinuationError::UnserializableValue {
                location: location.to_string(),
                variant: "value beyond the snapshot depth limit",
            });
        }
        Ok(match value {
            Value::Null => Self::Null {},
            Value::Bool(value) => Self::Bool { value: *value },
            Value::Number(value) => Self::Number {
                value: normalize_number(*value),
            },
            Value::String(value) => Self::String {
                value: value.to_string(),
            },
            Value::Image(value) => Self::Image {
                value: (**value).clone(),
            },
            Value::Resource(value) => Self::Resource {
                value: value.clone(),
            },
            Value::Ref(value) => Self::Ref { value: *value },
            Value::Tuple(values) => Self::Tuple {
                items: canonical_items(values, location, depth)?,
            },
            Value::List(values) => Self::List {
                items: canonical_items(values, location, depth)?,
            },
            Value::Record(record) => {
                let mut fields = record.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Record {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            let location = child_location(location, name);
                            Ok(CanonicalBinding {
                                name: name.to_string(),
                                value: Self::from_runtime(value, &location, depth + 1)?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
            Value::Projected(projected) => Self::Projected {
                value: CanonicalProjectedValue {
                    name: projected.name().to_string(),
                    type_name: projected.value_type_name().to_string(),
                    projection_ref: projected
                        .projection_ref()
                        .map(|value| {
                            CanonicalJsonValue::from_json(
                                value,
                                &format!("{location}.projection_ref"),
                                depth + 1,
                            )
                        })
                        .transpose()?,
                },
            },
        })
    }

    fn into_runtime(self) -> Result<Value, SnapshotDecodeError> {
        Ok(match self {
            Self::Null {} => Value::Null,
            Self::Bool { value } => Value::Bool(value),
            Self::Number { value } => Value::Number(normalize_number(value)),
            Self::String { value } => Value::String(value.into()),
            Self::Image { value } => Value::Image(Box::new(value)),
            Self::Resource { value } => Value::Resource(value),
            Self::Ref { value } => Value::Ref(value),
            Self::Tuple { items } => Value::Tuple(
                items
                    .into_iter()
                    .map(Self::into_runtime)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            Self::List { items } => Value::List(
                items
                    .into_iter()
                    .map(Self::into_runtime)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            Self::Record { fields } => Value::Record(Arc::new(
                fields
                    .into_iter()
                    .map(|field| field.value.into_runtime().map(|value| (field.name, value)))
                    .collect::<Result<_, _>>()?,
            )),
            Self::Projected { value } => Value::Projected(
                ProjectedValue::unavailable_after_restore_with_projection_ref(
                    value.name,
                    value.type_name,
                    value
                        .projection_ref
                        .map(CanonicalJsonValue::into_json)
                        .transpose()?,
                ),
            ),
        })
    }
}

fn canonical_items(
    values: &[Value],
    location: &str,
    depth: usize,
) -> Result<Vec<CanonicalValue>, ContinuationError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            CanonicalValue::from_runtime(value, &format!("{location}[{index}]"), depth + 1)
        })
        .collect()
}

impl CanonicalJsonValue {
    fn from_json(
        value: &serde_json::Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        if depth > MAX_SNAPSHOT_VALUE_DEPTH {
            return Err(ContinuationError::UnserializableValue {
                location: location.to_string(),
                variant: "value beyond the snapshot depth limit",
            });
        }
        Ok(match value {
            serde_json::Value::Null => Self::Null {},
            serde_json::Value::Bool(value) => Self::Bool { value: *value },
            serde_json::Value::Number(value) => Self::Number {
                value: value.clone(),
            },
            serde_json::Value::String(value) => Self::String {
                value: value.clone(),
            },
            serde_json::Value::Array(items) => Self::Array {
                items: items
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        Self::from_json(value, &format!("{location}[{index}]"), depth + 1)
                    })
                    .collect::<Result<_, _>>()?,
            },
            serde_json::Value::Object(fields) => {
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Object {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            let location = child_location(location, name);
                            Ok(CanonicalJsonField {
                                name: name.clone(),
                                value: Self::from_json(value, &location, depth + 1)?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
        })
    }

    fn into_json(self) -> Result<serde_json::Value, SnapshotDecodeError> {
        Ok(match self {
            Self::Null {} => serde_json::Value::Null,
            Self::Bool { value } => serde_json::Value::Bool(value),
            Self::Number { value } => serde_json::Value::Number(value),
            Self::String { value } => serde_json::Value::String(value),
            Self::Array { items } => serde_json::Value::Array(
                items
                    .into_iter()
                    .map(Self::into_json)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Object { fields } => serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|field| field.value.into_json().map(|value| (field.name, value)))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
}

fn normalize_number(value: f64) -> f64 {
    if value.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else {
        value
    }
}

fn child_location(parent: &str, name: &str) -> String {
    if is_path_identifier(name) {
        format!("{parent}.{name}")
    } else {
        let quoted = serde_json::to_string(name).expect("string serialization cannot fail");
        format!("{parent}[{quoted}]")
    }
}

fn is_path_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|character| matches!(character, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
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
const TAGGED_VALUE_FIELDS: &[&str] = &["kind", "value", "items", "fields"];

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
    if fields != 3 {
        return Err(non_canonical(
            "snapshot",
            "snapshot must contain exactly three fields",
        ));
    }
    expect_key(bytes, &mut cursor, "version", "snapshot")?;
    skip_messagepack_value(bytes, &mut cursor)?;
    expect_key(bytes, &mut cursor, "globals", "snapshot")?;
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
