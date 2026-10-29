//! Parsing for the typed-output schema witness vocabulary.

use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use thiserror::Error;
use {
    lash_kernel_value::EntrySchemaPolicy, lash_kernel_value::RecordType,
    lash_kernel_value::RecordTypeField, lash_kernel_value::Type,
    lash_kernel_value::TypeSchemaRefusal, lash_kernel_value::json_schema_of_type,
};

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
    #[error("{0}")]
    Projection(TypeSchemaRefusal),
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

    let mut fields = Vec::new();
    for (name, descriptor) in output {
        let type_str = descriptor
            .as_str()
            .ok_or_else(|| OutputSchemaError::InvalidDescriptor {
                field: name.clone(),
            })?;
        fields.push(RecordTypeField {
            name: name.clone(),
            ty: type_descriptor(type_str)?,
            optional: false,
        });
    }
    json_schema_of_type(
        &Type::Record(RecordType {
            fields,
            rest: lash_kernel_value::RecordRest::Closed,
        }),
        EntrySchemaPolicy::default(),
    )
    .map(|schema| {
        // The witness grammar historically returns an unlabelled subschema.
        let mut value = schema.into_value();
        if let Some(object) = value.as_object_mut() {
            object.remove("$schema");
        }
        Some(value)
    })
    .map_err(OutputSchemaError::Projection)
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

fn type_descriptor(descriptor: &str) -> Result<Type, OutputSchemaError> {
    let scalar = |ty: &str| -> Result<Type, OutputSchemaError> {
        Ok(match ty {
            "str" | "string" => Type::Text,
            "int" | "integer" => Type::Int,
            "float" | "number" => Type::Number,
            "bool" | "boolean" => Type::Bool,
            "record" | "dict" | "object" => Type::Record(RecordType {
                fields: vec![],
                rest: lash_kernel_value::RecordRest::Keep(Box::new(Type::Json)),
            }),
            _ => return Err(OutputSchemaError::UnknownScalar { kind: ty.into() }),
        })
    };
    let trimmed = descriptor.trim();
    if let Some(inner) = trimmed
        .strip_prefix("list[")
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return Ok(Type::List(Box::new(scalar(inner.trim())?)));
    }
    scalar(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                        "properties": {},
                        "required": [],
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
                "anyOf": [{"type": "string"}],
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
            Err(OutputSchemaError::TypeSchemaDepthExceeded {
                limit: MAX_SCHEMA_DEPTH
            })
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
    fn output_schema_rejects_lash_type_without_type_field() {
        let wrapped = json!({ LASH_TYPE_KEY: {"properties": {}} });
        let err = parse_output_schema(Some(&wrapped)).expect_err("missing type");
        assert!(err.to_string().contains("type"), "error: {err}");
    }
    /// OUTPUT-SCHEMA-ADMISSION: a host's narrower recursion policy is honored.
    #[test]
    fn a_host_output_schema_policy_refuses_its_configured_depth() {
        let value = serde_json::json!({"$lash_type": {"anyOf": [{"type": "string"}]}});
        parse_output_schema(Some(&value)).unwrap();
        assert_eq!(
            parse_output_schema_with_limits(Some(&value), OutputSchemaLimits::new(1).unwrap()),
            Err(OutputSchemaError::TypeSchemaDepthExceeded { limit: 1 })
        );
    }
}
