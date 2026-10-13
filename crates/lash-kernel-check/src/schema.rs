//! Kernel types of JSON schemas: a tool's input and result as the kernel
//! signature a document performs it under, and whether a tool fits an
//! effect (R7, D4).
//!
//! A tool's result is projected **open**: an object schema with closed
//! properties is a record whose rest is `Any`, so a tool that adds an output
//! field still serves a document checked against its earlier result. Input
//! stays closed: it is contravariant, and a tool accepts only what its
//! schema states. A manually authored closed kernel result stays closed, and
//! an open provided result does not serve it.

use std::collections::BTreeSet;

use lash_kernel_doc::{Name, Param, RecordType, RecordTypeField, Signature, Type};

/// The kernel type a JSON schema describes. A schema the grammar has no
/// exact type for is [`Type::Any`]: the tool still validates its own input.
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
/// it takes its input as one record and gives its result open.
#[must_use]
pub fn tool_signature(input: &serde_json::Value, output: &serde_json::Value) -> Signature {
    Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty: type_of_schema(input),
            optional: false,
        }],
        result: result_type_of_schema(output),
    }
}

/// Whether a function of signature `provided` stands wherever one of
/// `expected` is performed (K-ADM-002): it takes every argument list
/// `expected` allows, and returns only what `expected` promises.
#[must_use]
pub fn signature_serves(provided: &Signature, expected: &Signature) -> bool {
    crate::types::serves(provided, expected)
}

#[derive(Clone, Copy)]
enum Openness {
    Closed,
    Open,
}

fn project(schema: &serde_json::Value, openness: Openness) -> Type {
    let Some(object) = schema.as_object() else {
        return Type::Any;
    };
    if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
        let texts: Option<Vec<String>> = values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect();
        return texts
            .filter(|texts| !texts.is_empty())
            .map_or(Type::Any, Type::Enum);
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
                .map_or(Type::Any, |items| project(items, openness)),
        )),
        Some("object") => record_of(object, openness),
        _ => Type::Any,
    }
}

/// A closed object schema as a record, open when `openness` says so; any
/// other object schema is `Any`.
fn record_of(object: &serde_json::Map<String, serde_json::Value>, openness: Openness) -> Type {
    let Some(properties) = object
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Type::Any;
    };
    if object.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
        return Type::Any;
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
            Openness::Open => Some(Box::new(Type::Any)),
        },
    })
}
