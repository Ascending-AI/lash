//! The type grammar of effect signatures, function signatures and document
//! entries.
//!
//! A type describes kernel values and nothing else. A document's own
//! variables are untyped: there is no typed-binding form, and a strict
//! operation is what refuses a wrong value (`K-EVAL-006`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::names::Name;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Type {
    /// Every value.
    Any,
    /// Every value `Any` admits that holds no closure, function reference,
    /// dialect token holding one, task or handle anywhere in it: data, as a
    /// JSON-schema slot describes it (`K-EFF-002`). It serves where `Any` is
    /// expected; `Any` does not serve where it is.
    Json,
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

use std::fmt;
/// The side of an effect signature that cannot serve its declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignatureSide {
    Input,
    Output,
}

impl fmt::Display for SignatureSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Input => "input",
            Self::Output => "output",
        })
    }
}

/// The first incompatible part found by the signature's serving relation.
/// `path` uses JSON pointer escaping in the projected type structure:
/// `/input`, `/output`, and their properties, items or additional properties.
/// The types are the kernel's projections, not the original JSON schemas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignatureMismatch {
    pub side: SignatureSide,
    pub path: String,
    pub expected: Type,
    pub provided: Type,
}

impl fmt::Display for SignatureMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}: expected {}, provided {}",
            self.side,
            self.path,
            describe_type(&self.expected),
            describe_type(&self.provided)
        )
    }
}
fn describe_type(ty: &Type) -> String {
    match ty {
        Type::Any => "open/unconstrained (Any)".into(),
        Type::Json => "JSON data, holding no function (Json)".into(),
        Type::Record(record) if record.rest.is_none() => format!("closed object {ty:?}"),
        Type::Record(_) => format!("object with declared fields and open rest {ty:?}"),
        other => format!("{other:?}"),
    }
}
pub use crate::type_relations::{data_only, serves as signature_serves};

/// The first incompatible value at a public boundary. `path` is an RFC 6901
/// pointer into the value, never a path through the type grammar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeMismatch {
    pub path: String,
    pub expected: Type,
    /// A bounded kind witness; no user value is included.
    pub actual: Type,
}
impl TypeMismatch {
    pub fn new(expected: Type, actual: Type) -> Self {
        Self {
            path: String::new(),
            expected,
            actual,
        }
    }
    pub fn within(mut self, segment: &str) -> Self {
        self.path = format!(
            "/{}{}",
            segment.replace('~', "~0").replace('/', "~1"),
            self.path
        );
        self
    }
}
impl fmt::Display for TypeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "type mismatch at {}: expected {:?}, actual {:?}",
            self.path, self.expected, self.actual
        )
    }
}
impl std::error::Error for TypeMismatch {}

/// A public entry's input or settled result violated its declared contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryTypeMismatch {
    pub callable: crate::names::CallableId,
    pub mismatch: TypeMismatch,
}
impl fmt::Display for EntryTypeMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.callable, self.mismatch)
    }
}
impl std::error::Error for EntryTypeMismatch {}

/// Source identity before a callable has been linked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FunctionLocation {
    pub name: Option<String>,
    pub site: Option<crate::sites::Site>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SignaturePosition {
    Parameter { index: u32, name: Name },
    Result,
}

/// Only entry promotion requires a written parameter and settled-result type.
/// Producers resolve explicit any/unknown to Json before calling this seam.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema, thiserror::Error)]
#[serde(tag = "code", rename = "SIGNATURE_REQUIRED", deny_unknown_fields)]
#[error("SIGNATURE_REQUIRED: {function:?} {position:?}")]
pub struct SignatureRequired {
    pub function: FunctionLocation,
    pub position: SignaturePosition,
    pub span: Option<crate::sites::Span>,
}

// A tagged struct's derived deserializer treats its serialized tag as an
// unknown field. An internally tagged enum consumes and validates that tag.
impl<'de> Deserialize<'de> for SignatureRequired {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "code", deny_unknown_fields)]
        enum Wire {
            #[serde(rename = "SIGNATURE_REQUIRED")]
            Required {
                function: FunctionLocation,
                position: SignaturePosition,
                span: Option<crate::sites::Span>,
            },
        }
        let Wire::Required {
            function,
            position,
            span,
        } = Wire::deserialize(deserializer)?;
        Ok(Self {
            function,
            position,
            span,
        })
    }
}

/// Check a resolved entry signature. Any denotes missing annotation evidence;
/// explicit unconstrained annotations must already have resolved to Json.
/// Dialects can attach the parameter/result insertion span to the returned fact.
pub fn require_entry_signature(
    signature: &Signature,
    function: FunctionLocation,
) -> Result<(), Box<SignatureRequired>> {
    fn incomplete(ty: &Type) -> bool {
        match ty {
            Type::Any => true,
            Type::Tuple(items) | Type::Union(items) => items.iter().any(incomplete),
            Type::List(item) | Type::Set(item) | Type::Task(item) => incomplete(item),
            Type::Map(map) => incomplete(&map.key) || incomplete(&map.value),
            Type::Record(record) => {
                record.fields.iter().any(|f| incomplete(&f.ty))
                    || record.rest.as_deref().is_some_and(incomplete)
            }
            Type::Function(signature) => {
                signature.params.iter().any(|p| incomplete(&p.ty)) || incomplete(&signature.result)
            }
            _ => false,
        }
    }
    for (index, param) in signature.params.iter().enumerate() {
        if incomplete(&param.ty) {
            return Err(Box::new(SignatureRequired {
                function,
                position: SignaturePosition::Parameter {
                    index: index as u32,
                    name: param.name.clone(),
                },
                span: None,
            }));
        }
    }
    if incomplete(&signature.result) {
        return Err(Box::new(SignatureRequired {
            function,
            position: SignaturePosition::Result,
            span: None,
        }));
    }
    Ok(())
}
