use std::sync::Arc;

use super::{ContinuationError, ImageValue, ProjectedValue, Record, ResourceHandle, Value};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod canonical_messagepack;
pub use canonical_messagepack::{CanonicalMapOrder, validate_canonical_messagepack_structure};

const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;
pub(crate) const MAX_SNAPSHOT_VALUE_DEPTH: usize = 64;
// The raw-wire guard is secondary to the explicit value-depth guard below. A
// nested value advances through at most two MessagePack containers (the value
// map and its items/fields container); fixed root and leaf wrappers account for
// the remaining five frames. Deriving this bound keeps it coupled to the
// value-domain limit if the encoding gains or loses a wrapper layer, while
// leaving the explicit value-depth check as the primary boundary rejection.
const MAX_FIXED_SNAPSHOT_WRAPPER_DEPTH: usize = 5;
const MESSAGEPACK_CONTAINERS_PER_VALUE_LEVEL: usize = 2;
#[doc(hidden)]
pub const CANONICAL_MESSAGEPACK_DEPTH_LIMIT: usize = MAX_FIXED_SNAPSHOT_WRAPPER_DEPTH
    + MESSAGEPACK_CONTAINERS_PER_VALUE_LEVEL * MAX_SNAPSHOT_VALUE_DEPTH;
const MAX_SNAPSHOT_MESSAGEPACK_DEPTH: usize = CANONICAL_MESSAGEPACK_DEPTH_LIMIT;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub(super) globals: Record,
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
        }
    }

    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        Self {
            globals: snapshot.globals,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub globals: Record,
}

impl Snapshot {
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
        validate_canonical_messagepack(bytes)?;
        let wire: CanonicalSnapshot = rmp_serde::from_slice(bytes)
            .map_err(|error| SnapshotDecodeError::InvalidEncoding(error.to_string()))?;
        wire.try_into()
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SnapshotDecodeError {
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
    globals: Vec<CanonicalBinding>,
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
        Ok(Self {
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
        })
    }
}

impl TryFrom<CanonicalSnapshot> for Snapshot {
    type Error = SnapshotDecodeError;

    fn try_from(snapshot: CanonicalSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            globals: snapshot
                .globals
                .into_iter()
                .map(|binding| {
                    binding
                        .value
                        .into_runtime()
                        .map(|value| (binding.name, value))
                })
                .collect::<Result<_, _>>()?,
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

fn take_canonical_integer(
    bytes: &[u8],
    cursor: &mut usize,
    location: &str,
    marker: u8,
) -> Result<i128, SnapshotDecodeError> {
    let value = match marker {
        0x00..=0x7f => i128::from(marker),
        0xe0..=0xff => i128::from(i8::from_be_bytes([marker])),
        0xcc => {
            let value = take_byte(bytes, cursor)?;
            if value <= 127 {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xcd => {
            let value = take_u16(bytes, cursor)?;
            if value <= u16::from(u8::MAX) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xce => {
            let value = take_u32(bytes, cursor)?;
            if value <= u32::from(u16::MAX) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xcf => {
            let value = take_u64(bytes, cursor)?;
            if value <= u64::from(u32::MAX) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd0 => {
            let value = i8::from_be_bytes([take_byte(bytes, cursor)?]);
            if value >= -32 {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd1 => {
            let value = i16::from_be_bytes(take_array::<2>(bytes, cursor)?);
            if value >= i16::from(i8::MIN) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd2 => {
            let value = i32::from_be_bytes(take_array::<4>(bytes, cursor)?);
            if value >= i32::from(i16::MIN) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        0xd3 => {
            let value = i64::from_be_bytes(take_array::<8>(bytes, cursor)?);
            if value >= i64::from(i32::MIN) {
                return Err(non_canonical(location, "integer width is not minimal"));
            }
            i128::from(value)
        }
        _ => return Err(unexpected_marker(location, "an integer", marker)),
    };
    Ok(value)
}

fn is_integer_marker(marker: u8) -> bool {
    matches!(marker, 0x00..=0x7f | 0xcc..=0xcf | 0xd0..=0xd3 | 0xe0..=0xff)
}

fn take_byte(bytes: &[u8], cursor: &mut usize) -> Result<u8, SnapshotDecodeError> {
    let byte = bytes
        .get(*cursor)
        .copied()
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?;
    *cursor += 1;
    Ok(byte)
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, SnapshotDecodeError> {
    let value = take_array::<2>(bytes, cursor)?;
    Ok(u16::from_be_bytes(value))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, SnapshotDecodeError> {
    let value = take_array::<4>(bytes, cursor)?;
    Ok(u32::from_be_bytes(value))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, SnapshotDecodeError> {
    let value = take_array::<8>(bytes, cursor)?;
    Ok(u64::from_be_bytes(value))
}

fn take_array<const N: usize>(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<[u8; N], SnapshotDecodeError> {
    let end = cursor
        .checked_add(N)
        .ok_or_else(|| invalid_messagepack("length overflow"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| invalid_messagepack("unexpected end of input"))?
        .try_into()
        .expect("slice length was checked");
    *cursor = end;
    Ok(value)
}

fn skip_bytes(bytes: &[u8], cursor: &mut usize, length: usize) -> Result<(), SnapshotDecodeError> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| invalid_messagepack("length overflow"))?;
    if end > bytes.len() {
        return Err(invalid_messagepack("unexpected end of input"));
    }
    *cursor = end;
    Ok(())
}

fn take_slice<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], SnapshotDecodeError> {
    let start = *cursor;
    skip_bytes(bytes, cursor, length)?;
    Ok(&bytes[start..*cursor])
}

fn usize_from_u32(value: u32) -> Result<usize, SnapshotDecodeError> {
    usize::try_from(value).map_err(|_| invalid_messagepack("length does not fit usize"))
}

fn invalid_messagepack(message: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::InvalidEncoding(message.to_string())
}

fn invalid_at(location: &str, message: &str) -> SnapshotDecodeError {
    invalid_messagepack(&format!("at `{location}`: {message}"))
}

fn unexpected_marker(location: &str, expected: &str, marker: u8) -> SnapshotDecodeError {
    invalid_at(
        location,
        &format!("expected {expected}, found marker 0x{marker:02x}"),
    )
}

fn non_canonical(location: &str, reason: &str) -> SnapshotDecodeError {
    SnapshotDecodeError::NonCanonicalEncoding {
        location: location.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
#[path = "state/fixes3_tests.rs"]
mod fixes3_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_encoding_is_deterministic_for_map_order_and_nan_payload() {
        let left_nan = f64::from_bits(0x7ff0_0000_0000_0001);
        let right_nan = f64::from_bits(0xfff8_0000_0000_0042);

        let mut left_record = Record::new();
        left_record.insert("z".to_string(), Value::Number(left_nan));
        left_record.insert("a".to_string(), Value::String("same\0\u{fffd}".into()));
        let mut left_globals = Record::new();
        left_globals.insert("z-last".to_string(), Value::Bool(true));
        left_globals.insert("session".to_string(), Value::Record(Arc::new(left_record)));

        let mut right_record = Record::new();
        right_record.insert("a".to_string(), Value::String("same\0\u{fffd}".into()));
        right_record.insert("z".to_string(), Value::Number(right_nan));
        let mut right_globals = Record::new();
        right_globals.insert("session".to_string(), Value::Record(Arc::new(right_record)));
        right_globals.insert("z-last".to_string(), Value::Bool(true));

        let left = Snapshot {
            globals: left_globals,
        }
        .to_canonical_bytes()
        .expect("left encode");
        let right = Snapshot {
            globals: right_globals,
        }
        .to_canonical_bytes()
        .expect("right encode");

        assert_eq!(left, right);
    }

    #[test]
    fn canonical_decode_rejects_non_minimal_integer_width_with_location() {
        let snapshot = Snapshot {
            globals: [(
                "root".to_string(),
                Value::Projected(
                    ProjectedValue::unavailable_after_restore_with_projection_ref(
                        "root",
                        "number",
                        Some(serde_json::json!(1)),
                    ),
                ),
            )]
            .into_iter()
            .collect(),
        };
        let mut bytes = snapshot.to_canonical_bytes().expect("canonical bytes");
        let needle = [0xa5, b'v', b'a', b'l', b'u', b'e', 0x01];
        let offset = bytes
            .windows(needle.len())
            .rposition(|window| window == needle)
            .expect("projection JSON integer");
        bytes.splice(
            offset + needle.len() - 1..offset + needle.len(),
            [0xcc, 0x01],
        );

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("non-minimal integer width must be rejected");
        assert!(matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "globals.root.value.projection_ref.value"
                    && reason.contains("integer width is not minimal")
        ));
    }

    #[test]
    fn canonical_decode_rejects_integer_encoded_runtime_number() {
        let snapshot = Snapshot {
            globals: [("root".to_string(), Value::Number(1.0))]
                .into_iter()
                .collect(),
        };
        let mut bytes = snapshot.to_canonical_bytes().expect("canonical bytes");
        let mut needle = vec![0xa5, b'v', b'a', b'l', b'u', b'e', 0xcb];
        needle.extend_from_slice(&1.0_f64.to_bits().to_be_bytes());
        let offset = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("runtime f64");
        bytes.splice(offset + 6..offset + needle.len(), [0x01]);

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("integer-encoded runtime number must be rejected");
        assert!(matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "globals.root.value"
                    && reason.contains("must use f64 encoding")
        ));
    }

    #[test]
    fn canonical_decode_rejects_sequence_form_structs() {
        let wire = CanonicalSnapshot {
            globals: vec![CanonicalBinding {
                name: "root".to_string(),
                value: CanonicalValue::Null {},
            }],
        };
        let bytes = rmp_serde::to_vec(&wire).expect("sequence-form bytes");

        let error = Snapshot::from_canonical_bytes(&bytes)
            .expect_err("sequence-form structs must be rejected");
        assert!(matches!(
            &error,
            SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                if location == "snapshot" && reason.contains("map form, not sequence form")
        ));
    }

    #[test]
    fn canonical_decode_rejects_unsorted_and_duplicate_dynamic_keys() {
        for names in [["z", "a"], ["same", "same"]] {
            let wire = CanonicalSnapshot {
                globals: names
                    .into_iter()
                    .map(|name| CanonicalBinding {
                        name: name.to_string(),
                        value: CanonicalValue::Null {},
                    })
                    .collect(),
            };
            let bytes = rmp_serde::to_vec_named(&wire).expect("non-canonical bytes");

            let error = Snapshot::from_canonical_bytes(&bytes)
                .expect_err("dynamic keys must be sorted and unique");
            assert!(matches!(
                &error,
                SnapshotDecodeError::NonCanonicalEncoding { location, reason }
                    if location == "globals"
                        && reason.contains("strictly sorted and unique")
            ));
        }
    }

    #[test]
    fn canonical_encode_error_names_the_nested_value_path() {
        let mut too_deep = Value::Null;
        for _ in 0..=MAX_SNAPSHOT_VALUE_DEPTH {
            too_deep = Value::List(vec![too_deep].into());
        }
        let mut session = Record::new();
        session.insert(
            "items".to_string(),
            Value::List(vec![Value::Null, Value::Null, Value::Null, too_deep].into()),
        );
        let snapshot = Snapshot {
            globals: [("session".to_string(), Value::Record(Arc::new(session)))]
                .into_iter()
                .collect(),
        };

        let error = snapshot
            .to_canonical_bytes()
            .expect_err("over-depth value must fail at encode");
        let ContinuationError::UnserializableValue { location, variant } = error else {
            panic!("expected typed unserializable-value error");
        };
        assert!(
            location.starts_with("globals.session.items[3]"),
            "{location}"
        );
        assert_eq!(variant, "value beyond the snapshot depth limit");
    }

    #[test]
    fn canonical_decode_rejects_a_depth_bomb_before_deserializing() {
        let mut value = CanonicalValue::Null {};
        for _ in 0..120 {
            value = CanonicalValue::List { items: vec![value] };
        }
        let bomb = CanonicalSnapshot {
            globals: vec![CanonicalBinding {
                name: "bomb".to_string(),
                value,
            }],
        };
        let bytes = rmp_serde::to_vec_named(&bomb).expect("construct depth bomb");

        assert_eq!(
            Snapshot::from_canonical_bytes(&bytes),
            Err(SnapshotDecodeError::ValueDepthLimitExceeded {
                limit: MAX_SNAPSHOT_VALUE_DEPTH,
            })
        );
    }

    #[test]
    fn canonical_wire_golden_covers_every_value_kind_and_projection_ref() {
        let image = ImageValue::new(
            "sha256:00ff",
            crate::MediaType::parse("image/png").expect("media type"),
            "pixel",
            2,
            Some(1),
            Some(1),
        );
        let projection_ref = serde_json::json!({
            "array": [null, true, 7, "bytes\u{0000}\u{007f}"],
            "object": {"key": "value"}
        });
        let snapshot = Snapshot {
            globals: [
                ("bool".to_string(), Value::Bool(true)),
                ("image".to_string(), Value::Image(Box::new(image))),
                ("list".to_string(), Value::List(vec![Value::Null].into())),
                ("null".to_string(), Value::Null),
                ("number".to_string(), Value::Number(-12.5)),
                (
                    "projected".to_string(),
                    Value::Projected(
                        ProjectedValue::unavailable_after_restore_with_projection_ref(
                            "memory",
                            "object",
                            Some(projection_ref),
                        ),
                    ),
                ),
                (
                    "record".to_string(),
                    Value::Record(Arc::new(
                        [("field".to_string(), Value::String("body".into()))]
                            .into_iter()
                            .collect(),
                    )),
                ),
                (
                    "resource".to_string(),
                    Value::Resource(ResourceHandle::new("files", "workspace")),
                ),
                (
                    "string".to_string(),
                    Value::String("body\u{0000}\u{007f}".into()),
                ),
                (
                    "tuple".to_string(),
                    Value::Tuple(vec![Value::Number(1.0), Value::String("two".into())].into()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let bytes = snapshot.to_canonical_bytes().expect("golden snapshot");
        let hex = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "81a7676c6f62616c739a82a46e616d65a4626f6f6ca576616c756582a46b696e64a4626f6f6ca576616c7565c382a46e616d65a5696d616765a576616c756582a46b696e64a5696d616765a576616c756587a474797065a5696d616765a26964ab7368613235363a30306666a46d696d65a9696d6167652f706e67a56c6162656ca5706978656ca473697a6502a5776964746801a66865696768740182a46e616d65a46c697374a576616c756582a46b696e64a46c697374a56974656d739181a46b696e64a46e756c6c82a46e616d65a46e756c6ca576616c756581a46b696e64a46e756c6c82a46e616d65a66e756d626572a576616c756582a46b696e64a66e756d626572a576616c7565cbc02900000000000082a46e616d65a970726f6a6563746564a576616c756582a46b696e64a970726f6a6563746564a576616c756583a46e616d65a66d656d6f7279a9747970655f6e616d65a66f626a656374ae70726f6a656374696f6e5f72656682a46b696e64a66f626a656374a66669656c64739282a46e616d65a56172726179a576616c756582a46b696e64a56172726179a56974656d739481a46b696e64a46e756c6c82a46b696e64a4626f6f6ca576616c7565c382a46b696e64a66e756d626572a576616c75650782a46b696e64a6737472696e67a576616c7565a76279746573007f82a46e616d65a66f626a656374a576616c756582a46b696e64a66f626a656374a66669656c64739182a46e616d65a36b6579a576616c756582a46b696e64a6737472696e67a576616c7565a576616c756582a46e616d65a67265636f7264a576616c756582a46b696e64a67265636f7264a66669656c64739182a46e616d65a56669656c64a576616c756582a46b696e64a6737472696e67a576616c7565a4626f647982a46e616d65a87265736f75726365a576616c756582a46b696e64a87265736f75726365a576616c756582ad7265736f757263655f74797065a566696c6573a5616c696173a9776f726b737061636582a46e616d65a6737472696e67a576616c756582a46b696e64a6737472696e67a576616c7565a6626f6479007f82a46e616d65a57475706c65a576616c756582a46b696e64a57475706c65a56974656d739282a46b696e64a66e756d626572a576616c7565cb3ff000000000000082a46b696e64a6737472696e67a576616c7565a374776f"
        );
    }

    #[test]
    fn canonical_decode_accepts_every_max_depth_encode_shape() {
        fn round_trip(value: Value) {
            let snapshot = Snapshot {
                globals: [("root".to_string(), value)].into_iter().collect(),
            };
            let bytes = snapshot.to_canonical_bytes().expect("max-depth encode");
            let decoded = Snapshot::from_canonical_bytes(&bytes).expect("max-depth decode");
            assert_eq!(decoded, snapshot);
        }

        let mut record = Value::Null;
        for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
            record = Value::Record(Arc::new(
                [("child".to_string(), record)].into_iter().collect(),
            ));
        }
        round_trip(record);

        let mut list = Value::Null;
        for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH {
            list = Value::List(vec![list].into());
        }
        round_trip(list);

        let mut projection_ref = serde_json::Value::Null;
        // `Projected` enters its JSON payload at depth one, so 63 nested
        // objects place the terminal null at the shared depth limit of 64.
        for _ in 0..MAX_SNAPSHOT_VALUE_DEPTH - 1 {
            projection_ref = serde_json::json!({"child": projection_ref});
        }
        round_trip(Value::Projected(
            ProjectedValue::unavailable_after_restore_with_projection_ref(
                "root",
                "object",
                Some(projection_ref),
            ),
        ));
    }
}
