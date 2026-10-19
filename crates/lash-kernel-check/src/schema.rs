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

use std::collections::BTreeSet;

use lash_kernel_doc::{Name, Param, RecordType, RecordTypeField, Signature, Type};
use lash_tool_shape::{X_LASH_KEYWORD, XLashType};

/// The kernel type a JSON schema describes. A JSON schema describes data:
/// a form the grammar has no exact type for is [`Type::Json`], which
/// admits no function, and only a slot an `x-lash` declaration marks
/// admits one. The tool still validates its own input.
#[must_use]
pub fn type_of_schema(schema: &serde_json::Value) -> Type {
    project(schema, Openness::Closed)
}

/// The kernel type a tool's result schema describes: [`type_of_schema`],
/// each record of it open (D4).
#[must_use]
pub fn result_type_of_schema(schema: &serde_json::Value) -> Type {
    project(schema, Openness::Open)
}

/// The signature a tool of `input` and `output` schemas is performed under:
/// it takes its input as one record and gives its result open. The input
/// may be omitted when `input_omittable`, a fact the schema layer decides
/// (the input schema accepts the empty object), and an omitted input is
/// performed as `{}`.
#[must_use]
pub fn tool_signature(
    input: &serde_json::Value,
    input_omittable: bool,
    output: &serde_json::Value,
) -> Signature {
    Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty: type_of_schema(input),
            optional: input_omittable,
        }],
        result: result_type_of_schema(output),
    }
}

/// Whether a function of signature `provided` stands wherever one of
/// `expected` is performed (K-ADM-002): it takes every argument list
/// `expected` allows, and returns only what `expected` promises.
#[must_use]
pub fn signature_serves(provided: &Signature, expected: &Signature) -> bool {
    signature_mismatch(provided, expected).is_none()
}

#[derive(Clone, Copy)]
enum Openness {
    Closed,
    Open,
}

fn project(schema: &serde_json::Value, openness: Openness) -> Type {
    let Some(object) = schema.as_object() else {
        return Type::Json;
    };
    if let Some(declaration) = object.get(X_LASH_KEYWORD) {
        return lash_type(declaration);
    }
    if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
        let texts: Option<Vec<String>> = values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect();
        return texts
            .filter(|texts| !texts.is_empty())
            .map_or(Type::Json, Type::Enum);
    }
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("null") => Type::Null,
        Some("boolean") => Type::Bool,
        Some("integer") => Type::Int,
        Some("number") => Type::Number,
        Some("string") => Type::Text,
        Some("array") => Type::List(Box::new(
            object
                .get("items")
                .map_or(Type::Json, |items| project(items, openness)),
        )),
        Some("object") => record_of(object, openness),
        _ => Type::Json,
    }
}

/// The kernel type of an `x-lash` declaration, one for each kind of the
/// one decode the presenters read too. A process value is a handle record,
/// which a cell calls through its dialect, so a process slot stays open.
/// A declaration this vocabulary has no kind for is data.
fn lash_type(declaration: &serde_json::Value) -> Type {
    let Some(declared) = XLashType::decode(declaration) else {
        return Type::Json;
    };
    match declared {
        XLashType::Function {
            signature: Some(signature),
        } => Type::Function(Box::new(Signature {
            params: signature
                .params
                .iter()
                .map(|param| Param {
                    name: Name::new(param.name.as_str()),
                    ty: type_of_schema(&param.schema),
                    optional: false,
                })
                .collect(),
            result: result_type_of_schema(&signature.output),
        })),
        XLashType::Function { signature: None }
        | XLashType::Process { .. }
        | XLashType::ProcessUnknown => Type::Any,
    }
}

/// A closed object schema as a record, open when `openness` says so, the
/// rest of an open one data; any other object schema is data.
fn record_of(object: &serde_json::Map<String, serde_json::Value>, openness: Openness) -> Type {
    let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Type::Json;
    };
    if object.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
        return Type::Json;
    }
    let required: BTreeSet<&str> = object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    Type::Record(RecordType {
        fields: properties
            .iter()
            .map(|(field, schema)| RecordTypeField {
                name: field.clone(),
                ty: project(schema, openness),
                optional: !required.contains(field.as_str()),
            })
            .collect(),
        rest: match openness {
            Openness::Closed => None,
            Openness::Open => Some(Box::new(Type::Json)),
        },
    })
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
