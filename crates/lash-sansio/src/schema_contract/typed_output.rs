//! Parsing for the typed-output schema witness vocabulary.

use serde_json::{Value, json};
use thiserror::Error;

/// Marker key that wraps a Type literal at its outermost level so a host-side
/// consumer can tell a Type value apart from a plain record. The inner value
/// is the JSON-Schema representation of the type.
pub const LASH_TYPE_KEY: &str = "$lash_type";

/// JSON Schema type names shared by witness parsing and value validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaScalarKind {
    String,
    Number,
    Integer,
    Boolean,
    Array,
    Object,
    Null,
}

impl SchemaScalarKind {
    pub fn from_schema_name(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Self::String,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            "object" => Self::Object,
            "null" => Self::Null,
            _ => return None,
        })
    }

    pub const fn as_schema_name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
            Self::Null => "null",
        }
    }
}

// Same value as the language JSON Schema importer cap, but opposite policy by
// design (FIG-1878): the importer widens to Any at the cap, this parser errors.
const MAX_SCHEMA_DEPTH: usize = 32;

/// Failure to parse an output-schema witness.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputSchemaError {
    /// The output-schema witness is not a record.
    #[error("invalid `output`: expected a record describing the typed shape")]
    ExpectedRecord,
    /// The output-schema witness does not declare any fields.
    #[error("at least one output field is required")]
    Empty,
    /// A record field has a descriptor that is not text.
    #[error("field `{field}`: type descriptor must be a string")]
    InvalidDescriptor { field: String },
    /// A `$lash_type` witness does not contain a JSON object.
    #[error("Type schema must be a JSON object")]
    TypeSchemaExpectedObject,
    /// A `$lash_type` object does not declare its JSON Schema type.
    #[error("Type schema missing `type` field")]
    TypeSchemaMissingType,
    /// A `$lash_type` union does not contain an array of branch schemas.
    #[error("Type schema `anyOf` must be an array")]
    TypeSchemaAnyOfExpectedArray,
    /// A `$lash_type` union does not contain any branch schemas.
    #[error("Type schema `anyOf` must contain at least one schema")]
    TypeSchemaAnyOfEmpty,
    /// A `$lash_type` union contains fields outside the producer vocabulary.
    #[error("Type schema `anyOf` cannot have sibling fields")]
    TypeSchemaAnyOfSiblings,
    /// A `$lash_type` schema is nested beyond the validation depth bound.
    #[error("Type schema exceeds maximum nesting depth of {MAX_SCHEMA_DEPTH}")]
    TypeSchemaDepthExceeded,
    /// A `$lash_type` object declares an unsupported JSON Schema type.
    #[error("unsupported Type schema kind `{kind}`")]
    UnsupportedTypeSchema { kind: String },
    #[error("unknown scalar type `{kind}`")]
    UnknownScalar { kind: String },
}

/// Two shapes are accepted:
/// - a record of field-name to type-descriptor strings (`"str"`, `"int"`,
///   `"float"`, `"bool"`, `"record"`, or `"list[...]"` of those), compiled
///   into a strict object schema; or
/// - a Lashlang `Type { ... }` literal, a single-field
///   `{"$lash_type": <schema>}` wrapper as produced by the Lashlang
///   compiler, whose inner schema is passed through after validation.
///
/// Returns `Ok(None)` when `output` is absent or `null` (the tool falls back
/// to its untyped default).
pub fn parse_output_schema(value: Option<&Value>) -> Result<Option<Value>, OutputSchemaError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let output = value.as_object().ok_or(OutputSchemaError::ExpectedRecord)?;
    if output.is_empty() {
        return Err(OutputSchemaError::Empty);
    }

    if output.len() == 1
        && let Some(schema) = output.get(LASH_TYPE_KEY)
    {
        validate_lash_type_schema(schema)?;
        return Ok(Some(schema.clone()));
    }

    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, descriptor) in output {
        let type_str = descriptor
            .as_str()
            .ok_or_else(|| OutputSchemaError::InvalidDescriptor {
                field: name.clone(),
            })?;
        properties.insert(name.clone(), type_descriptor_to_json_schema(type_str)?);
        required.push(Value::String(name.clone()));
    }
    Ok(Some(json!({
        "type": SchemaScalarKind::Object.as_schema_name(),
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })))
}

fn validate_lash_type_schema(schema: &Value) -> Result<(), OutputSchemaError> {
    validate_lash_type_schema_at_depth(schema, 0)
}

fn validate_lash_type_schema_at_depth(
    schema: &Value,
    depth: usize,
) -> Result<(), OutputSchemaError> {
    if depth >= MAX_SCHEMA_DEPTH {
        return Err(OutputSchemaError::TypeSchemaDepthExceeded);
    }
    let object = schema
        .as_object()
        .ok_or(OutputSchemaError::TypeSchemaExpectedObject)?;

    if let Some(any_of) = object.get("anyOf") {
        if object.len() != 1 {
            return Err(OutputSchemaError::TypeSchemaAnyOfSiblings);
        }
        let variants = any_of
            .as_array()
            .ok_or(OutputSchemaError::TypeSchemaAnyOfExpectedArray)?;
        if variants.is_empty() {
            return Err(OutputSchemaError::TypeSchemaAnyOfEmpty);
        }
        for variant in variants {
            validate_lash_type_schema_at_depth(variant, depth + 1)?;
        }
        return Ok(());
    }

    let Some(kind) = object.get("type").and_then(Value::as_str) else {
        // The empty schema is emitted for `any`, process, and trigger-handle
        // types. JSON Schema deliberately has no scalar name for that shape.
        if object.is_empty() {
            return Ok(());
        }
        return Err(OutputSchemaError::TypeSchemaMissingType);
    };
    if SchemaScalarKind::from_schema_name(kind).is_some() {
        Ok(())
    } else {
        Err(OutputSchemaError::UnsupportedTypeSchema {
            kind: kind.to_string(),
        })
    }
}

fn type_descriptor_to_json_schema(descriptor: &str) -> Result<Value, OutputSchemaError> {
    let scalar = |ty: &str| -> Result<Value, OutputSchemaError> {
        let kind = match ty {
            "str" => Some(SchemaScalarKind::String),
            "int" => Some(SchemaScalarKind::Integer),
            "float" => Some(SchemaScalarKind::Number),
            "bool" => Some(SchemaScalarKind::Boolean),
            "record" | "dict" => Some(SchemaScalarKind::Object),
            other => SchemaScalarKind::from_schema_name(other).filter(|kind| {
                matches!(
                    kind,
                    SchemaScalarKind::String
                        | SchemaScalarKind::Integer
                        | SchemaScalarKind::Number
                        | SchemaScalarKind::Boolean
                        | SchemaScalarKind::Object
                )
            }),
        };
        match kind {
            Some(SchemaScalarKind::Object) if matches!(ty, "record" | "dict" | "object") => {
                Ok(json!({
                    "type": SchemaScalarKind::Object.as_schema_name(),
                    "additionalProperties": true
                }))
            }
            Some(kind) => Ok(json!({"type": kind.as_schema_name()})),
            None => Err(OutputSchemaError::UnknownScalar {
                kind: ty.to_string(),
            }),
        }
    };
    let trimmed = descriptor.trim();
    if let Some(inner) = trimmed
        .strip_prefix("list[")
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return Ok(json!({
            "type": SchemaScalarKind::Array.as_schema_name(),
            "items": scalar(inner.trim())?,
        }));
    }
    scalar(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_null_output_use_the_untyped_default() {
        assert_eq!(parse_output_schema(None), Ok(None));
        assert_eq!(parse_output_schema(Some(&Value::Null)), Ok(None));
    }

    #[test]
    fn lash_type_union_preserves_empty_and_nested_branches() {
        let schema = json!({"anyOf": [
            {},
            {"type": "null"},
            {"anyOf": [{"type": "integer"}, {"type": "array", "items": {"type": "string"}}]}
        ]});
        let wrapped = json!({(LASH_TYPE_KEY): schema.clone()});
        assert_eq!(parse_output_schema(Some(&wrapped)), Ok(Some(schema)));
    }

    #[test]
    fn lash_type_marker_with_siblings_is_a_record_descriptor() {
        let output = json!({(LASH_TYPE_KEY): "str", "value": "int"});
        let schema = parse_output_schema(Some(&output))
            .expect("schema")
            .expect("present");
        assert_eq!(
            schema["properties"][LASH_TYPE_KEY],
            json!({"type": "string"})
        );
        assert_eq!(schema["required"], json!([LASH_TYPE_KEY, "value"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn object_descriptor_alias_preserves_open_object_schema() {
        let output = serde_json::json!({"value": "object"});

        assert_eq!(
            parse_output_schema(Some(&output)),
            Ok(Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "value": {
                        "type": "object",
                        "additionalProperties": true
                    }
                },
                "required": ["value"],
                "additionalProperties": false
            })))
        );
    }

    #[test]
    fn rejects_any_of_with_non_array_value() {
        let output = serde_json::json!({
            (LASH_TYPE_KEY): {"anyOf": 5}
        });

        assert_eq!(
            parse_output_schema(Some(&output)),
            Err(OutputSchemaError::TypeSchemaAnyOfExpectedArray)
        );
    }

    #[test]
    fn rejects_any_of_with_unsupported_branch_schema() {
        let output = serde_json::json!({
            (LASH_TYPE_KEY): {"anyOf": [{"type": "frobnicate"}]}
        });

        assert_eq!(
            parse_output_schema(Some(&output)),
            Err(OutputSchemaError::UnsupportedTypeSchema {
                kind: "frobnicate".into()
            })
        );
    }

    #[test]
    fn rejects_empty_any_of() {
        let output = serde_json::json!({
            (LASH_TYPE_KEY): {"anyOf": []}
        });

        assert_eq!(
            parse_output_schema(Some(&output)),
            Err(OutputSchemaError::TypeSchemaAnyOfEmpty)
        );
    }

    #[test]
    fn rejects_any_of_with_junk_siblings() {
        let output = serde_json::json!({
            (LASH_TYPE_KEY): {
                "anyOf": [],
                "type": null,
                "x": 1
            }
        });

        assert_eq!(
            parse_output_schema(Some(&output)),
            Err(OutputSchemaError::TypeSchemaAnyOfSiblings)
        );
    }

    #[test]
    fn rejects_any_of_beyond_max_schema_depth() {
        let mut schema = serde_json::json!({"type": "string"});
        for _ in 0..MAX_SCHEMA_DEPTH {
            schema = serde_json::json!({"anyOf": [schema]});
        }

        assert_eq!(
            parse_output_schema(Some(&serde_json::json!({
                (LASH_TYPE_KEY): schema
            }))),
            Err(OutputSchemaError::TypeSchemaDepthExceeded)
        );
    }
    #[test]
    fn output_schema_supports_scalars_and_lists() {
        let schema = parse_output_schema(Some(&json!({
            "answer": "str",
            "count": "int",
            "items": "list[str]"
        })))
        .expect("schema")
        .expect("present");
        assert_eq!(schema["properties"]["answer"]["type"], json!("string"));
        assert_eq!(schema["properties"]["count"]["type"], json!("integer"));
        assert_eq!(schema["properties"]["items"]["type"], json!("array"));
    }

    #[test]
    fn output_schema_passes_through_lash_type_wrapper() {
        let inner_schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "status": { "type": "string", "enum": ["ok", "err"] }
            },
            "required": ["name", "tags", "status"],
            "additionalProperties": false
        });
        let wrapped = json!({ LASH_TYPE_KEY: inner_schema.clone() });
        let schema = parse_output_schema(Some(&wrapped))
            .expect("schema")
            .expect("present");
        assert_eq!(schema, inner_schema);
    }

    #[test]
    fn output_schema_rejects_lash_type_without_type_field() {
        let wrapped = json!({ LASH_TYPE_KEY: {"properties": {}} });
        let err = parse_output_schema(Some(&wrapped)).expect_err("missing type");
        assert!(err.to_string().contains("type"), "error: {err}");
    }

    #[test]
    fn output_schema_accepts_array_top_level_type() {
        let wrapped = json!({
            LASH_TYPE_KEY: {
                "type": "array",
                "items": {"type": "string"}
            }
        });
        let schema = parse_output_schema(Some(&wrapped))
            .expect("schema")
            .expect("present");
        assert_eq!(schema["type"], json!("array"));
    }
}
