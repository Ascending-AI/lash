//! Admitted JSON schemas and typed value mismatches.

use std::sync::Arc;

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
/// Clones retain the compiled validator; it never enters serialized state.
#[derive(Clone)]
pub struct JsonSchema {
    value: Value,
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
            value,
            validator: Arc::new(validator),
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "the empty object is a compilable schema"
    )]
    pub fn any() -> Self {
        Self::admit(Value::Object(Default::default())).expect("empty object is a valid JSON schema")
    }

    pub fn as_value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
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
        self.value.fmt(f)
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
