//! Kernel types of JSON schemas: a tool's input and result as the kernel
//! signature a document performs it under, and whether a tool fits an
//! effect (R7, D4).
//!
//! A tool's result is projected **open**: an object schema with closed
//! properties is a record whose rest is `Json`, so a tool that adds an output
//! field still serves a document checked against its earlier result. Input
//! stays closed: it is contravariant, and a tool accepts only what its
//! schema states. A manually authored closed kernel result stays closed, and
//! an open provided result does not serve it.
//!
//! Compositions are types too. `anyOf`, `oneOf` and a `type` list are the
//! union of their members; `allOf`, and every keyword beside another in one
//! schema, is the meet of its members. `oneOf` is typed as `anyOf`: that
//! exactly one member matches is a fact about values no kernel type states,
//! and every value of the `oneOf` is a value of the union, so the union is
//! the tightest type that admits them all; the tool validates exclusivity.
//! A meet the grammar cannot state refuses the schema, naming where.

use std::collections::BTreeSet;
use std::fmt;

use lash_kernel_doc::{Name, Param, RecordType, RecordTypeField, Signature, Type, print_type};
use lash_tool_shape::{X_LASH_KEYWORD, XLashType};

use crate::types::{data_only, union};

/// The kernel type a JSON schema describes. A JSON schema describes data:
/// a form the grammar has no exact type for is [`Type::Json`], which
/// admits no function, and only a slot an `x-lash` declaration marks
/// admits one. The tool still validates its own input.
///
/// # Errors
///
/// [`SchemaProjectionRefusal`]: a composition with no member, or members
/// whose meet no kernel type states.
pub fn type_of_schema(schema: &serde_json::Value) -> Result<Type, SchemaProjectionRefusal> {
    project(schema, Openness::Closed, &Pointer::Root)
}

/// The kernel type a tool's result schema describes: [`type_of_schema`],
/// each record of it open (D4).
///
/// # Errors
///
/// As [`type_of_schema`].
pub fn result_type_of_schema(schema: &serde_json::Value) -> Result<Type, SchemaProjectionRefusal> {
    project(schema, Openness::Open, &Pointer::Root)
}

/// The signature a tool is performed under, from its input's
/// [`type_of_schema`] and its result's [`result_type_of_schema`]: it takes
/// its input as one record and gives its result open. The input may be
/// omitted when `input_omittable`, a fact the schema layer decides (the
/// input schema accepts the empty object), and an omitted input is
/// performed as `{}`.
#[must_use]
pub fn tool_signature(input: Type, input_omittable: bool, result: Type) -> Signature {
    Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty: input,
            optional: input_omittable,
        }],
        result,
    }
}

/// Whether a function of signature `provided` stands wherever one of
/// `expected` is performed (K-ADM-002): it takes every argument list
/// `expected` allows, and returns only what `expected` promises.
#[must_use]
pub fn signature_serves(provided: &Signature, expected: &Signature) -> bool {
    signature_mismatch(provided, expected).is_none()
}

/// Why a JSON schema has no kernel type: the JSON pointer of the
/// composition that has none, and why.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the schema at `{path}` has no kernel type: {conflict}")]
pub struct SchemaProjectionRefusal {
    /// The JSON pointer, from the schema's root, of the composition keyword
    /// whose members have no kernel type; empty for the root itself.
    pub path: String,
    pub conflict: SchemaConflict,
}

/// What a composition's members cannot be.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaConflict {
    /// A composition or `type` list with no member, which no value is of.
    NoMembers,
    /// No kernel type is both `left` and `right`, at `fields` within them.
    Meet {
        fields: Vec<String>,
        left: Type,
        right: Type,
    },
    /// One member requires `field`, at `fields` within them, and another
    /// member's closed record has no room for it.
    ClosedOut { fields: Vec<String>, field: String },
}

impl fmt::Display for SchemaConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let within = |f: &mut fmt::Formatter<'_>, fields: &[String]| {
            if fields.is_empty() {
                Ok(())
            } else {
                write!(f, "at field `{}`, ", fields.join("."))
            }
        };
        match self {
            Self::NoMembers => f.write_str("it has no member, so no value is of it"),
            Self::Meet {
                fields,
                left,
                right,
            } => {
                within(f, fields)?;
                write!(
                    f,
                    "no kernel type is both `{}` and `{}`",
                    print_type(left),
                    print_type(right)
                )
            }
            Self::ClosedOut { fields, field } => {
                within(f, fields)?;
                write!(
                    f,
                    "one member requires field `{field}` and another's closed record has no room for it"
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Openness {
    Closed,
    Open,
}

/// Where a schema stands within its root, written out only for a refusal.
enum Pointer<'a> {
    Root,
    Key(&'a Pointer<'a>, &'a str),
    Index(&'a Pointer<'a>, usize),
}

impl<'a> Pointer<'a> {
    fn key(&'a self, key: &'a str) -> Self {
        Self::Key(self, key)
    }

    fn index(&'a self, index: usize) -> Self {
        Self::Index(self, index)
    }

    fn refuse(&self, conflict: SchemaConflict) -> SchemaProjectionRefusal {
        SchemaProjectionRefusal {
            path: self.to_string(),
            conflict,
        }
    }
}

impl fmt::Display for Pointer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => Ok(()),
            Self::Key(parent, key) => {
                write!(f, "{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
            }
            Self::Index(parent, index) => write!(f, "{parent}/{index}"),
        }
    }
}

type Projected = Result<Type, SchemaProjectionRefusal>;

/// Every keyword of a schema at once: its `x-lash` declaration alone when
/// it has one, otherwise the meet of its own type and its compositions.
fn project(schema: &serde_json::Value, openness: Openness, at: &Pointer<'_>) -> Projected {
    let Some(object) = schema.as_object() else {
        return Ok(Type::Json);
    };
    if let Some(declaration) = object.get(X_LASH_KEYWORD) {
        return lash_type(declaration, &at.key(X_LASH_KEYWORD));
    }
    let mut ty = own_type(object, openness, at)?;
    for keyword in ["allOf", "anyOf", "oneOf"] {
        let Some(members) = object.get(keyword) else {
            continue;
        };
        let at = at.key(keyword);
        let Some(members) = members.as_array() else {
            continue;
        };
        if members.is_empty() {
            return Err(at.refuse(SchemaConflict::NoMembers));
        }
        let mut projected = Vec::with_capacity(members.len());
        for (index, member) in members.iter().enumerate() {
            projected.push(project(member, openness, &at.index(index))?);
        }
        let composed = if keyword == "allOf" {
            projected
                .into_iter()
                .try_fold(Type::Json, |left, right| meet(left, right, &mut Vec::new()))
        } else {
            Ok(alternatives(projected))
        };
        ty = composed
            .and_then(|composed| meet(ty, composed, &mut Vec::new()))
            .map_err(|conflict| at.refuse(conflict))?;
    }
    Ok(ty)
}

/// The type a schema's own `enum` or `type` states, before its
/// compositions: `Json` when it states none the grammar has.
fn own_type(
    object: &serde_json::Map<String, serde_json::Value>,
    openness: Openness,
    at: &Pointer<'_>,
) -> Projected {
    if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
        return Ok(enum_type(values));
    }
    match object.get("type") {
        Some(serde_json::Value::String(name)) => named_type(name, object, openness, at),
        Some(serde_json::Value::Array(names)) => {
            if names.is_empty() {
                return Err(at.key("type").refuse(SchemaConflict::NoMembers));
            }
            let mut members = Vec::with_capacity(names.len());
            for name in names {
                members.push(match name.as_str() {
                    Some(name) => named_type(name, object, openness, at)?,
                    None => Type::Json,
                });
            }
            Ok(alternatives(members))
        }
        _ => Ok(Type::Json),
    }
}

/// A text enum, nullable when `null` is among its values; any other enum
/// is data.
fn enum_type(values: &[serde_json::Value]) -> Type {
    let nullable = values.iter().any(serde_json::Value::is_null);
    let texts: Option<Vec<String>> = values
        .iter()
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().map(str::to_owned))
        .collect();
    match texts {
        Some(texts) if !texts.is_empty() && nullable => {
            Type::Union(vec![Type::Enum(texts), Type::Null])
        }
        Some(texts) if !texts.is_empty() => Type::Enum(texts),
        Some(_) if nullable => Type::Null,
        _ => Type::Json,
    }
}

fn named_type(
    name: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    openness: Openness,
    at: &Pointer<'_>,
) -> Projected {
    Ok(match name {
        "null" => Type::Null,
        "boolean" => Type::Bool,
        "integer" => Type::Int,
        "number" => Type::Number,
        "string" => Type::Text,
        "array" => Type::List(Box::new(match object.get("items") {
            Some(items) => project(items, openness, &at.key("items"))?,
            None => Type::Json,
        })),
        "object" => record_of(object, openness, at)?,
        _ => Type::Json,
    })
}

/// The union of a composition's members, through the union normalizer.
/// `Json` already holds every member whose every value is data, so it
/// absorbs them: what is left beside it may hold a function.
fn alternatives(members: Vec<Type>) -> Type {
    if members.contains(&Type::Json) {
        union(
            members
                .into_iter()
                .filter(|member| *member == Type::Json || !data_only(member)),
        )
    } else {
        union(members)
    }
}

/// The type of every value of both `left` and `right`, the members of one
/// conjunction, at `fields` within it. `Json` is what a projection gives a
/// form it has no exact type for, so it adds nothing to a meet; neither
/// does `Any`, which only a marker gives.
fn meet(left: Type, right: Type, fields: &mut Vec<String>) -> Result<Type, SchemaConflict> {
    let conflict = |fields: &[String], left: Type, right: Type| SchemaConflict::Meet {
        fields: fields.to_vec(),
        left,
        right,
    };
    Ok(match (left, right) {
        (Type::Json, other) | (other, Type::Json) | (Type::Any, other) | (other, Type::Any) => {
            other
        }
        (left, right) if left == right => left,
        (Type::Union(members), other) | (other, Type::Union(members)) => {
            let met: Vec<Type> = members
                .iter()
                .filter_map(|member| meet(member.clone(), other.clone(), fields).ok())
                .collect();
            if met.is_empty() {
                return Err(conflict(fields, Type::Union(members), other));
            }
            alternatives(met)
        }
        (Type::Number, Type::Int) | (Type::Int, Type::Number) => Type::Int,
        (Type::Number, Type::Float) | (Type::Float, Type::Number) => Type::Float,
        (Type::Text, Type::Enum(texts)) | (Type::Enum(texts), Type::Text) => Type::Enum(texts),
        (Type::Enum(left), Type::Enum(right)) => {
            let shared: Vec<String> = left
                .iter()
                .filter(|text| right.contains(text))
                .cloned()
                .collect();
            if shared.is_empty() {
                return Err(conflict(fields, Type::Enum(left), Type::Enum(right)));
            }
            Type::Enum(shared)
        }
        (Type::List(left), Type::List(right)) => Type::List(Box::new(meet(*left, *right, fields)?)),
        (Type::Record(left), Type::Record(right)) => {
            Type::Record(meet_records(left, right, fields)?)
        }
        (left, right) => return Err(conflict(fields, left, right)),
    })
}

/// Two records' meet: each field either names, typed by both, and required
/// when either requires it. A field one names and the other does not is
/// typed by the other's rest; a closed record has no room for it, so it is
/// dropped when optional and refused when required. The meet is closed
/// when either is.
fn meet_records(
    left: RecordType,
    right: RecordType,
    fields: &mut Vec<String>,
) -> Result<RecordType, SchemaConflict> {
    let mut met = Vec::new();
    let mut within = |field: RecordTypeField,
                      other: Option<RecordTypeField>,
                      other_rest: Option<&Type>,
                      fields: &mut Vec<String>|
     -> Result<(), SchemaConflict> {
        let (ty, optional) = match (other, other_rest) {
            (Some(other), _) => (other.ty, field.optional && other.optional),
            (None, Some(rest)) => (rest.clone(), field.optional),
            (None, None) if field.optional => return Ok(()),
            (None, None) => {
                return Err(SchemaConflict::ClosedOut {
                    fields: fields.clone(),
                    field: field.name,
                });
            }
        };
        fields.push(field.name.clone());
        let ty = meet(field.ty, ty, fields)?;
        fields.pop();
        met.push(RecordTypeField {
            name: field.name,
            ty,
            optional,
        });
        Ok(())
    };
    let mut others = right.fields;
    for field in left.fields {
        let other = others
            .iter()
            .position(|other| other.name == field.name)
            .map(|index| others.remove(index));
        within(field, other, right.rest.as_deref(), fields)?;
    }
    for field in others {
        within(field, None, left.rest.as_deref(), fields)?;
    }
    let rest = match (left.rest, right.rest) {
        (Some(left), Some(right)) => Some(Box::new(meet(*left, *right, fields)?)),
        _ => None,
    };
    Ok(RecordType { fields: met, rest })
}

/// The kernel type of an `x-lash` declaration, one for each kind of the
/// one decode the presenters read too. A process value is a handle record,
/// which a cell calls through its dialect, so a process slot stays open.
/// A declaration this vocabulary has no kind for is data.
fn lash_type(declaration: &serde_json::Value, at: &Pointer<'_>) -> Projected {
    let Some(declared) = XLashType::decode(declaration) else {
        return Ok(Type::Json);
    };
    Ok(match declared {
        XLashType::Function {
            signature: Some(signature),
        } => {
            let at = at.key("signature");
            let params_at = at.key("params");
            let mut params = Vec::with_capacity(signature.params.len());
            for (index, param) in signature.params.iter().enumerate() {
                params.push(Param {
                    name: Name::new(param.name.as_str()),
                    ty: project(
                        &param.schema,
                        Openness::Closed,
                        &params_at.index(index).key("schema"),
                    )?,
                    optional: false,
                });
            }
            Type::Function(Box::new(Signature {
                params,
                result: project(&signature.output, Openness::Open, &at.key("output"))?,
            }))
        }
        XLashType::Function { signature: None }
        | XLashType::Process { .. }
        | XLashType::ProcessUnknown => Type::Any,
    })
}

/// A closed object schema as a record, open when `openness` says so, the
/// rest of an open one data; any other object schema is data.
fn record_of(
    object: &serde_json::Map<String, serde_json::Value>,
    openness: Openness,
    at: &Pointer<'_>,
) -> Projected {
    let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(Type::Json);
    };
    if object.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
        return Ok(Type::Json);
    }
    let required: BTreeSet<&str> = object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let properties_at = at.key("properties");
    let mut fields = Vec::with_capacity(properties.len());
    for (field, schema) in properties {
        fields.push(RecordTypeField {
            name: field.clone(),
            ty: project(schema, openness, &properties_at.key(field))?,
            optional: !required.contains(field.as_str()),
        });
    }
    Ok(Type::Record(RecordType {
        fields,
        rest: match openness {
            Openness::Closed => None,
            Openness::Open => Some(Box::new(Type::Json)),
        },
    }))
}

/// The first part of `provided` that cannot serve `expected`, found by the
/// same relation as [`signature_serves`].
#[must_use]
pub fn signature_mismatch(
    provided: &Signature,
    expected: &Signature,
) -> Option<crate::SignatureMismatch> {
    crate::types::serves(provided, expected)
}
