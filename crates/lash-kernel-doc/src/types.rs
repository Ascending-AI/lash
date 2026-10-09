//! The type grammar of effect signatures, function signatures and document
//! entries.
//!
//! A type describes kernel values and nothing else. A document's own
//! variables are untyped: there is no typed-binding form, and a strict
//! operation is what refuses a wrong value (`K-EVAL-006`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::name::Name;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Type {
    /// Every value.
    Any,
    Null,
    Absent,
    Bool,
    Int,
    Float,
    /// An integer or a float. In an effect's result this is the bare number
    /// the document's number policy decodes (`K-EFF-006`).
    Number,
    Text,
    Bytes,
    Timestamp,
    /// A tuple of exactly these member types.
    Tuple(Vec<Type>),
    List(Box<Type>),
    Map(Box<MapType>),
    Set(Box<Type>),
    Record(RecordType),
    /// A text that is one of these.
    Enum(Vec<String>),
    /// A closure or a function reference that takes and returns these.
    Function(Box<Signature>),
    /// A task handle whose task ends with this.
    Task(Box<Type>),
    Error,
    /// A handle of this host kind.
    Handle(String),
    /// A value of any one of these types. Two or more, none itself a union.
    Union(Vec<Type>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapType {
    pub key: Type,
    pub value: Type,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordType {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<RecordTypeField>,
    /// The type of every field not named in `fields`. `None` closes the
    /// record: it has no other field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<Box<Type>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordTypeField {
    pub name: String,
    pub ty: Type,
    /// The field may be missing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

/// What a function, an effect or an entry takes and returns.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    pub result: Type,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub name: Name,
    pub ty: Type,
    /// The argument may be omitted, and is then absent (`K-FN-004`). No
    /// required parameter follows an optional one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

impl Signature {
    /// How many leading parameters a call must supply.
    pub fn required(&self) -> usize {
        self.params.iter().filter(|param| !param.optional).count()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}
