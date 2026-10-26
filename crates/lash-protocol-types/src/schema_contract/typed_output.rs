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
    #[error("Type schema exceeds the configured {limit}-level nesting limit")]
    TypeSchemaDepthExceeded { limit: usize },
    /// A `$lash_type` object declares an unsupported JSON Schema type.
    #[error("unsupported Type schema kind `{kind}`")]
    UnsupportedTypeSchema { kind: String },
    #[error("unknown scalar type `{kind}`")]
    UnknownScalar { kind: String },
}

/// Host narrowing of the recursive Type-schema validator's 32-level safety ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSchemaLimits {
    max_depth: usize,
}
impl OutputSchemaLimits {
    pub const fn standard() -> Self {
        Self {
            max_depth: MAX_SCHEMA_DEPTH,
        }
    }
    pub fn new(max_depth: usize) -> Option<Self> {
        (max_depth <= MAX_SCHEMA_DEPTH).then_some(Self { max_depth })
    }
}
impl Default for OutputSchemaLimits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Two shapes are accepted:
/// - a record of field-name to type-descriptor strings (`"str"`, `"int"`,
///   `"float"`, `"bool"`, `"record"`, or `"list[...]"` of those), compiled
///   into a strict object schema; or
/// - a Lash VM `Type { ... }` literal, a single-field
///   `{"$lash_type": <schema>}` wrapper as produced by the Lash VM
///   compiler, whose inner schema is passed through after validation.
///
/// Returns `Ok(None)` when `output` is absent or `null` (the tool falls back
/// to its untyped default).
pub fn parse_output_schema(value: Option<&Value>) -> Result<Option<Value>, OutputSchemaError> {
    parse_output_schema_with_limits(value, OutputSchemaLimits::standard())
}
/// Parse under the host tool's recursive schema admission policy.
pub fn parse_output_schema_with_limits(
    value: Option<&Value>,
    limits: OutputSchemaLimits,
) -> Result<Option<Value>, OutputSchemaError> {
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
        validate_lash_type_schema(schema, limits)?;
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

fn validate_lash_type_schema(
    schema: &Value,
    limits: OutputSchemaLimits,
) -> Result<(), OutputSchemaError> {
    validate_lash_type_schema_at_depth(schema, 0, limits.max_depth)
}

fn validate_lash_type_schema_at_depth(
    schema: &Value,
    depth: usize,
    limit: usize,
) -> Result<(), OutputSchemaError> {
    if depth >= limit {
        return Err(OutputSchemaError::TypeSchemaDepthExceeded { limit });
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
            validate_lash_type_schema_at_depth(variant, depth + 1, limit)?;
        }
        return Ok(());
    }

    let Some(kind) = object.get("type").and_then(Value::as_str) else {
        // The empty schema is emitted for `any` and process types. JSON Schema deliberately has no scalar name for that shape.
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
