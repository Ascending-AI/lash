//! Admitted JSON schemas and typed value mismatches.

use std::sync::{Arc, OnceLock};

use serde_json::Value;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InvalidSchemaKind {
    Null,
    Number,
    String,
    Array,
}

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchemaAdmissionError {
    #[error("a JSON schema must be an object or boolean, got {actual:?}")]
    InvalidKind { actual: InvalidSchemaKind },
    #[error("unusable JSON schema at {schema_path}: {message}")]
    Compilation {
        schema_path: String,
        message: String,
    },
    #[error("non-local schema reference at {schema_path}: {reference}")]
    NonLocalReference {
        schema_path: String,
        reference: String,
    },
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ValueMismatch {
    pub instance_path: String,
    pub message: String,
}

/// A schema admitted before it can enter a tool, payload, or wire contract.
/// Clones share the immutable JSON body and compiled validator. Serialization
/// contains only the JSON body; extracting an owned value leaves other clones intact.
#[derive(Clone)]
pub struct JsonSchema {
    value: Arc<Value>,
    validator: Arc<jsonschema::Validator>,
}

impl JsonSchema {
    pub fn admit(value: Value) -> Result<Self, SchemaAdmissionError> {
        let kind = match &value {
            Value::Null => Some(InvalidSchemaKind::Null),
            Value::Number(_) => Some(InvalidSchemaKind::Number),
            Value::String(_) => Some(InvalidSchemaKind::String),
            Value::Array(_) => Some(InvalidSchemaKind::Array),
            Value::Bool(_) | Value::Object(_) => None,
        };
        if let Some(kind) = kind {
            return Err(SchemaAdmissionError::InvalidKind { actual: kind });
        }
        // Empty object is the canonical spelling of an unconstrained schema.
        let value = if value == Value::Bool(true) {
            Value::Object(Default::default())
        } else {
            value
        };
        if value.as_object().is_some_and(serde_json::Map::is_empty) {
            return Ok(Self::any());
        }
        Self::compile(value)
    }

    fn compile(value: Value) -> Result<Self, SchemaAdmissionError> {
        let validator = jsonschema::options()
            .offline()
            .with_draft(jsonschema::Draft::Draft7.detect(&value))
            .should_validate_formats(true)
            .build(&value)
            .map_err(|error| {
                if let jsonschema::error::ValidationErrorKind::Referencing(
                    jsonschema::ReferencingError::Unretrievable { uri, .. },
                ) = error.kind()
                {
                    SchemaAdmissionError::NonLocalReference {
                        schema_path: error.schema_path().to_string(),
                        reference: uri.clone(),
                    }
                } else {
                    SchemaAdmissionError::Compilation {
                        schema_path: error.instance_path().to_string(),
                        message: error.to_string(),
                    }
                }
            })?;
        Ok(Self {
            value: Arc::new(value),
            validator: Arc::new(validator),
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "the empty object is a compilable schema"
    )]
    pub fn any() -> Self {
        static ANY: OnceLock<JsonSchema> = OnceLock::new();
        ANY.get_or_init(|| {
            Self::compile(Value::Object(Default::default()))
                .expect("empty object is a valid JSON schema")
        })
        .clone()
    }

    pub fn as_value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        Arc::unwrap_or_clone(self.value)
    }

    pub fn validate(&self, value: &Value) -> Result<(), ValueMismatch> {
        self.validator
            .validate(value)
            .map_err(|error| ValueMismatch {
                instance_path: error.instance_path().to_string(),
                message: self
                    .validator
                    .iter_errors(value)
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; "),
            })
    }

    /// Whether the empty object is an instance of this schema: a tool whose
    /// input schema accepts it may be called with its input omitted, and the
    /// omitted input is exactly `{}`.
    ///
    /// Composition keywords do not by themselves refuse `{}`. In particular,
    /// `oneOf` accepts it when exactly one branch accepts it; `allOf` accepts
    /// it when every branch does. References and negation likewise follow
    /// their validation semantics, rather than a keyword-based arity rule.
    pub fn accepts_empty_object(&self) -> bool {
        self.validator
            .is_valid(&Value::Object(serde_json::Map::new()))
    }

    pub fn object(
        properties: serde_json::Map<String, Value>,
        required: Vec<String>,
    ) -> Result<Self, SchemaAdmissionError> {
        let mut schema = serde_json::json!({"type": "object", "properties": properties, "additionalProperties": true});
        if !required.is_empty() {
            schema["required"] = serde_json::json!(required);
        }
        Self::admit(schema)
    }
}

/// The kernel signature a tool of admitted `input` and `output` schemas is
/// performed under ([`lash_kernel_check::tool_signature`]): its input may be
/// omitted exactly when the input schema accepts the empty object
/// ([`JsonSchema::accepts_empty_object`]).
#[must_use]
pub fn tool_signature(input: &JsonSchema, output: &JsonSchema) -> lash_kernel_doc::Signature {
    lash_kernel_check::tool_signature(
        input.as_value(),
        input.accepts_empty_object(),
        output.as_value(),
    )
}

impl Default for JsonSchema {
    fn default() -> Self {
        Self::any()
    }
}

impl PartialEq for JsonSchema {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}
impl Eq for JsonSchema {}

impl std::fmt::Debug for JsonSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.as_ref().fmt(f)
    }
}

impl serde::Serialize for JsonSchema {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}
impl<'de> serde::Deserialize<'de> for JsonSchema {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::admit(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl schemars::JsonSchema for JsonSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JsonSchema".into()
    }
    #[expect(clippy::expect_used, reason = "the schema is a fixed object literal")]
    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::json!({"anyOf": [{"type": "object"}, {"type": "boolean"}]})
            .try_into()
            .expect("valid JSON schema shape")
    }
}

impl TryFrom<Value> for JsonSchema {
    type Error = SchemaAdmissionError;
    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::admit(value)
    }
}

impl std::fmt::Display for ValueMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.instance_path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.instance_path, self.message)
        }
    }
}
impl std::error::Error for ValueMismatch {}

#[cfg(test)]
mod preparation_tests {
    use super::*;

    /// The canonical unconstrained schema has one immutable preparation,
    /// including defaults and decoding its equivalent spellings.
    #[test]
    fn unconstrained_schema_preparation_is_shared() {
        let first = JsonSchema::any();
        for repeated in [
            JsonSchema::default(),
            JsonSchema::admit(serde_json::json!({})).unwrap(),
            JsonSchema::admit(Value::Bool(true)).unwrap(),
            serde_json::from_str::<JsonSchema>("{}").unwrap(),
        ] {
            assert!(Arc::ptr_eq(&first.value, &repeated.value));
            assert!(Arc::ptr_eq(&first.validator, &repeated.validator));
            assert_eq!(serde_json::to_string(&repeated).unwrap(), "{}");
            for value in [
                Value::Null,
                serde_json::json!(123),
                serde_json::json!({"a": [true, "x"]}),
            ] {
                repeated.validate(&value).unwrap();
            }
        }
        assert_eq!(
            JsonSchema::admit(Value::Null).unwrap_err(),
            SchemaAdmissionError::InvalidKind {
                actual: InvalidSchemaKind::Null
            }
        );
        assert!(
            JsonSchema::admit(Value::Bool(false))
                .unwrap()
                .validate(&Value::Null)
                .is_err()
        );
    }
}
