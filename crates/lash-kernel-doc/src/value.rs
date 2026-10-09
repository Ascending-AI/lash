//! The kernel's values as data (`K-VAL-001`).
//!
//! [`Value`] is what a native function is handed and what a parked run
//! stores: an immutable value in place, or the identity of a heap object.
//! [`Object`] is what such an identity names. [`Datum`] is a value copied
//! out of, or into, a run across the effect boundary: a tree with no
//! identity. The heap that holds objects while a run executes belongs to the
//! machine.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::Site;
use crate::name::{Name, parse_hex, string_schema, write_hex};
use crate::number::{Float, Integer, NumberToken};

/// The identity of a heap object within one run.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ObjectId(pub u64);

/// A task's handle within one run. `main` is task 0 and every `spawn` takes
/// the next number (`K-TASK-002`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct TaskId(pub u64);

impl TaskId {
    pub const MAIN: Self = Self(0);
}

/// An instant: nanoseconds since 1970-01-01T00:00:00Z, with no zone
/// (`K-VAL-009`). Stored as its decimal spelling.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Timestamp {
    pub nanoseconds: Integer,
}

/// Immutable bytes. Stored as lower-case hexadecimal.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Bytes(Arc<[u8]>);

string_schema!(Bytes, "Bytes", "^([0-9a-f]{2})*$");

/// A text that is not lower-case hexadecimal bytes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not bytes: write pairs of lower-case hexadecimal digits")]
pub struct InvalidBytes {
    pub text: String,
}

impl Bytes {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn parse_hex(text: &str) -> Result<Self, InvalidBytes> {
        parse_hex(text)
            .map(|bytes| Self(bytes.into()))
            .ok_or_else(|| InvalidBytes {
                text: text.to_string(),
            })
    }

    pub fn to_hex(&self) -> String {
        let mut text = String::with_capacity(self.0.len() * 2);
        write_hex(&self.0, &mut text);
        text
    }
}

impl TryFrom<String> for Bytes {
    type Error = InvalidBytes;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse_hex(&text)
    }
}

impl From<Bytes> for String {
    fn from(bytes: Bytes) -> Self {
        bytes.to_hex()
    }
}

/// A typed reference to a host resource or a host projection. The kernel
/// reads neither field; a host gives them meaning.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Handle {
    /// The handle's type, as the type grammar's `Handle("…")` names it.
    pub kind: String,
    /// The host's own identifier for the resource.
    pub id: String,
}

/// The thing an identity key names (`K-KEY-004`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Object(ObjectId),
    Task(TaskId),
}

/// A kernel value: immutable data in place, or the identity of a heap
/// object.
///
/// Cloning is cheap: text, bytes, tuples and errors are shared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    Null,
    Absent,
    Bool(bool),
    Int(Integer),
    Float(Float),
    Text(Arc<str>),
    Bytes(Bytes),
    Timestamp(Timestamp),
    Tuple(Arc<[Value]>),
    List(ObjectId),
    Map(ObjectId),
    Set(ObjectId),
    Record(ObjectId),
    Closure(ObjectId),
    Error(Arc<ErrorValue>),
    Task(TaskId),
    /// A closed declared function of the document, by name.
    Function(Name),
    Handle(Arc<Handle>),
    /// The identity of a heap object or a task, as `ref(x)` takes it: an
    /// immutable value that is a legal map key (`K-KEY-004`).
    Ref(Identity),
}

/// The kind of a value: one per row of the value table, plus `ref`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    Null,
    Absent,
    Bool,
    Int,
    Float,
    Text,
    Bytes,
    Timestamp,
    Tuple,
    List,
    Map,
    Set,
    Record,
    Closure,
    Error,
    Task,
    Function,
    Handle,
    Ref,
}

impl Value {
    pub fn text(text: impl Into<Arc<str>>) -> Self {
        Self::Text(text.into())
    }

    pub fn kind(&self) -> ValueKind {
        match self {
            Self::Null => ValueKind::Null,
            Self::Absent => ValueKind::Absent,
            Self::Bool(_) => ValueKind::Bool,
            Self::Int(_) => ValueKind::Int,
            Self::Float(_) => ValueKind::Float,
            Self::Text(_) => ValueKind::Text,
            Self::Bytes(_) => ValueKind::Bytes,
            Self::Timestamp(_) => ValueKind::Timestamp,
            Self::Tuple(_) => ValueKind::Tuple,
            Self::List(_) => ValueKind::List,
            Self::Map(_) => ValueKind::Map,
            Self::Set(_) => ValueKind::Set,
            Self::Record(_) => ValueKind::Record,
            Self::Closure(_) => ValueKind::Closure,
            Self::Error(_) => ValueKind::Error,
            Self::Task(_) => ValueKind::Task,
            Self::Function(_) => ValueKind::Function,
            Self::Handle(_) => ValueKind::Handle,
            Self::Ref(_) => ValueKind::Ref,
        }
    }

    /// The heap object the value names, when it is one.
    pub fn object(&self) -> Option<ObjectId> {
        match self {
            Self::List(id)
            | Self::Map(id)
            | Self::Set(id)
            | Self::Record(id)
            | Self::Closure(id) => Some(*id),
            _ => None,
        }
    }
}

/// An error value: what `throw` raises and `catch` binds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorValue {
    /// The error's kind: a kernel kind (`K-ERR-002`), one a library function
    /// declares, or one a program chose.
    pub kind: String,
    pub message: String,
    pub data: Value,
}

impl ErrorValue {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            data: Value::Null,
        }
    }
}

/// What a heap object holds.
///
/// A map, a set and a record list their contents in insertion order. A map
/// key and a set member are legal keys (`K-KEY-001`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Object {
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    Record(Vec<(String, Value)>),
    Closure(ClosureObject),
    /// A variable a closure shares with its defining scope (`K-CLO-001`).
    Variable(Value),
}

/// A closure as data: the closure expression it was made from and the
/// variables it shares.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClosureObject {
    /// The site of the closure expression.
    pub site: Site,
    /// Each captured variable, as the [`Object::Variable`] it shares.
    pub captures: Vec<(Name, ObjectId)>,
}

/// A value copied across the effect boundary: an effect argument, an effect
/// result, a host read's answer, a run's result (`K-EFF-002`).
///
/// A datum is a tree. It has no identity and no cycle; an object reached
/// twice in the run is written twice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Datum {
    Null,
    Absent,
    Bool(bool),
    Int(Integer),
    Float(Float),
    /// A number not yet decoded: an effect result's JSON number
    /// (`K-EFF-005`). A run never writes one out.
    Number(NumberToken),
    Text(String),
    Bytes(Bytes),
    Timestamp(Timestamp),
    Tuple(Vec<Datum>),
    List(Vec<Datum>),
    Map(Vec<(Datum, Datum)>),
    Set(Vec<Datum>),
    Record(Vec<(String, Datum)>),
    Error(Box<ErrorDatum>),
    Function(Name),
    Handle(Handle),
}

/// An error value copied across the effect boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorDatum {
    pub kind: String,
    pub message: String,
    pub data: Datum,
}
