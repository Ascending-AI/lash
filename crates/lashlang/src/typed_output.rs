//! Parsing for the typed-output schema witness vocabulary.

use serde_json::{Value, json};
use thiserror::Error;

use crate::LASH_TYPE_KEY;

/// Parse a tool's output-schema witness into the JSON Schema it describes.
///
/// Accepts either record shorthand (field name to scalar/list descriptor) or
/// the `$lash_type` wrapper produced by a Lashlang `Type { ... }` literal.
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
    /// A `$lash_type` object declares an unsupported JSON Schema type.
    #[error("unsupported Type schema kind `{kind}`")]
    UnsupportedTypeSchema { kind: String },
    /// A shorthand field descriptor names an unknown scalar type.
    #[error("unknown scalar type `{kind}`")]
    UnknownScalar { kind: String },
}

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
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })))
}

fn validate_lash_type_schema(schema: &Value) -> Result<(), OutputSchemaError> {
    let object = schema
        .as_object()
        .ok_or(OutputSchemaError::TypeSchemaExpectedObject)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(OutputSchemaError::TypeSchemaMissingType)?;
    match kind {
        "object" | "array" | "string" | "integer" | "number" | "boolean" => Ok(()),
        other => Err(OutputSchemaError::UnsupportedTypeSchema {
            kind: other.to_string(),
        }),
    }
}

fn type_descriptor_to_json_schema(descriptor: &str) -> Result<Value, OutputSchemaError> {
    let scalar = |ty: &str| -> Result<Value, OutputSchemaError> {
        match ty {
            "str" | "string" => Ok(json!({"type": "string"})),
            "int" | "integer" => Ok(json!({"type": "integer"})),
            "float" | "number" => Ok(json!({"type": "number"})),
            "bool" | "boolean" => Ok(json!({"type": "boolean"})),
            "record" | "dict" | "object" => {
                Ok(json!({"type": "object", "additionalProperties": true}))
            }
            other => Err(OutputSchemaError::UnknownScalar {
                kind: other.to_string(),
            }),
        }
    };
    let trimmed = descriptor.trim();
    if let Some(inner) = trimmed
        .strip_prefix("list[")
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return Ok(json!({
            "type": "array",
            "items": scalar(inner.trim())?,
        }));
    }
    scalar(trimmed)
}
