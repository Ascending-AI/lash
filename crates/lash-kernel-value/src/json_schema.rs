//! Admitted JSON schemas and typed value mismatches.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde_json::Value;

/// Resolve the validator's schema draft, defaulting undeclared schemas to draft 7.
/// Resource wrappers use this same resolution before admission; resolution alone
/// does not admit a schema or change compilation's refusal semantics.
pub fn resolve_schema_draft(value: &Value) -> jsonschema::Draft {
    jsonschema::Draft::Draft7.detect(value)
}

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
    /// A composition whose members no kernel type states, or whose meet
    /// grows past the projection's bound
    /// ([`crate::schema::MAX_PROJECTED_TYPE_NODES`]), at the JSON
    /// pointer `schema_path`: a value of the schema could not be typed.
    #[error("JSON schema at `{schema_path}` has no kernel type: {message}")]
    Unprojectable {
        schema_path: String,
        message: String,
    },
}

impl From<crate::schema::SchemaProjectionRefusal> for SchemaAdmissionError {
    fn from(refusal: crate::schema::SchemaProjectionRefusal) -> Self {
        Self::Unprojectable {
            schema_path: refusal.path,
            message: refusal.conflict.to_string(),
        }
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ValueMismatch {
    pub instance_path: String,
    pub message: String,
}

/// A schema admitted before it can enter a tool, payload, or wire contract:
/// one that compiles and has a kernel type. Clones share the immutable JSON
/// body, compiled validator and kernel types. Serialization contains only
/// the JSON body; extracting an owned value leaves other clones intact.
#[derive(Clone)]
pub struct JsonSchema {
    admitted: Arc<Admitted>,
}

struct Admitted {
    value: Arc<Value>,
    validator: Arc<jsonschema::Validator>,
    types: Arc<KernelTypes>,
}

/// The kernel types an admitted schema projects to, as an input and as an
/// open result.
struct KernelTypes {
    input: crate::Type,
    result: crate::Type,
    input_widenings: Vec<crate::schema::Widening>,
    result_widenings: Vec<crate::schema::Widening>,
}

pub(crate) mod admissions {
    use super::*;
    use std::hash::{Hash, Hasher};

    struct Entry {
        hash: u64,
        admitted: Weak<Admitted>,
    }

    fn fingerprint(value: &Value) -> u64 {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hash);
        hash.finish()
    }

    /// Share only pure admission results while a schema still owns them. No
    /// cache entry keeps a body or validator alive, and keys compare whole JSON
    /// bodies so annotations, defaults and refusal diagnostics remain exact.
    #[derive(Default)]
    pub struct Admissions(Mutex<std::collections::VecDeque<Entry>>);
    impl Admissions {
        /// Admit a schema, sharing its immutable preparation with live owners
        /// in this cache. Dropping the cache discards all lookup state.
        pub fn admit(&self, value: Value) -> Result<JsonSchema, SchemaAdmissionError> {
            let hash = fingerprint(&value);
            if let Some(admitted) = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|entry| entry.hash == hash)
                .filter_map(|entry| entry.admitted.upgrade())
                .find(|found| *found.value == value)
            {
                return Ok(JsonSchema { admitted });
            }
            let compiled = JsonSchema::admit(value)?;
            Ok(self.keep(compiled))
        }

        // Compilation is outside the lock. Classify again under its owner so
        // concurrent admissions transfer the same immutable result to callers.
        pub(super) fn keep(&self, compiled: JsonSchema) -> JsonSchema {
            let hash = fingerprint(&compiled.admitted.value);
            let mut entries = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.retain(|entry| entry.admitted.strong_count() != 0);
            if let Some(admitted) = entries
                .iter()
                .filter(|entry| entry.hash == hash)
                .filter_map(|entry| entry.admitted.upgrade())
                .find(|found| found.value == compiled.admitted.value)
            {
                return JsonSchema { admitted };
            }
            entries.push_back(Entry {
                hash,
                admitted: Arc::downgrade(&compiled.admitted),
            });
            while entries.len() > 128 {
                entries.pop_front();
            }
            compiled
        }
    }
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
            .with_draft(resolve_schema_draft(&value))
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
        let (input, input_widenings) = crate::schema::type_of_schema(&value)?;
        let (result, result_widenings) = crate::schema::result_type_of_schema(&value)?;
        let types = KernelTypes {
            input,
            result,
            input_widenings,
            result_widenings,
        };
        Ok(Self {
            admitted: Arc::new(Admitted {
                value: Arc::new(value),
                validator: Arc::new(validator),
                types: Arc::new(types),
            }),
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
        &self.admitted.value
    }

    /// The kernel type this schema describes as a tool's input
    /// ([`crate::schema::type_of_schema`]).
    pub fn input_type(&self) -> &crate::Type {
        &self.admitted.types.input
    }

    /// The kernel type this schema describes as a tool's result, each
    /// record of it open ([`crate::schema::result_type_of_schema`]).
    pub fn result_type(&self) -> &crate::Type {
        &self.admitted.types.result
    }

    /// Losses retained with the cached input projection. Widened event types
    /// can prove a fit into an entry parameter; a schema-defined consumer's
    /// widened type cannot prove that it accepts typed values.
    pub fn input_widenings(&self) -> &[crate::schema::Widening] {
        &self.admitted.types.input_widenings
    }

    /// Losses retained with the ordinary open-result projection.
    pub fn result_widenings(&self) -> &[crate::schema::Widening] {
        &self.admitted.types.result_widenings
    }

    pub fn into_value(self) -> Value {
        Arc::try_unwrap(self.admitted).map_or_else(
            |admitted| admitted.value.as_ref().clone(),
            |admitted| Arc::unwrap_or_clone(admitted.value),
        )
    }

    pub fn validate(&self, value: &Value) -> Result<(), ValueMismatch> {
        self.admitted
            .validator
            .validate(value)
            .map_err(|error| ValueMismatch {
                instance_path: error.instance_path().to_string(),
                message: self
                    .admitted
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
        self.admitted
            .validator
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
/// performed under ([`crate::schema::tool_signature`]): its input may be
/// omitted exactly when the input schema accepts the empty object
/// ([`JsonSchema::accepts_empty_object`]).
#[must_use]
pub fn tool_signature(input: &JsonSchema, output: &JsonSchema) -> crate::Signature {
    crate::schema::tool_signature(
        input.input_type().clone(),
        input.accepts_empty_object(),
        output.result_type().clone(),
    )
}

impl Default for JsonSchema {
    fn default() -> Self {
        Self::any()
    }
}

impl PartialEq for JsonSchema {
    fn eq(&self, other: &Self) -> bool {
        self.admitted.value == other.admitted.value
    }
}
impl Eq for JsonSchema {}

impl std::fmt::Debug for JsonSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.admitted.value.as_ref().fmt(f)
    }
}

impl serde::Serialize for JsonSchema {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.admitted.value.serialize(serializer)
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

    /// FIG-7354: simultaneous compilations classify under one cache owner;
    /// exact bodies retain annotations and only live owners retain admissions.
    #[test]
    fn raced_schema_admissions_share_exact_bodies_without_retaining_them() {
        let cache = Arc::new(admissions::Admissions::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let value = serde_json::json!({"type":"string", "title":"original"});
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                let value = value.clone();
                std::thread::spawn(move || {
                    let compiled = JsonSchema::compile(value).unwrap();
                    barrier.wait();
                    cache.keep(compiled)
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(Arc::ptr_eq(&results[0].admitted, &results[1].admitted));
        let repeated = cache.admit(value.clone()).unwrap();
        assert!(Arc::ptr_eq(&results[0].admitted, &repeated.admitted));
        repeated.validate(&serde_json::json!("ok")).unwrap();
        assert!(repeated.validate(&Value::Null).is_err());
        let other = cache
            .admit(serde_json::json!({"type":"string", "title":"other"}))
            .unwrap();
        assert_ne!(repeated, other);
        assert_eq!(repeated.as_value(), &value);
        let public = JsonSchema::admit(value.clone()).unwrap();
        let decoded: JsonSchema = serde_json::from_value(value.clone()).unwrap();
        assert!(
            !Arc::ptr_eq(&public.admitted, &decoded.admitted),
            "schema admission outside a caller-owned cache must not share lookup state"
        );
        assert_eq!(public, decoded);
        let public_weak = Arc::downgrade(&public.admitted);
        drop(public);
        drop(decoded);
        assert!(public_weak.upgrade().is_none());
        let weak = Arc::downgrade(&repeated.admitted);
        drop(results);
        drop(repeated);
        assert!(weak.upgrade().is_none());
    }

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
            assert!(Arc::ptr_eq(&first.admitted.value, &repeated.admitted.value));
            assert!(Arc::ptr_eq(
                &first.admitted.validator,
                &repeated.admitted.validator
            ));
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

    /// SCHEMA-BOUND at registration (FIG-6966): a tool schema whose meet
    /// grows past the projection's bound is refused where every tool,
    /// payload and MCP schema is admitted, naming the conjunction.
    #[test]
    fn a_schema_past_the_projection_bound_is_refused_at_admission_naming_where() {
        let members = |prefix: &str| {
            (0..80)
                .map(|index| {
                    serde_json::json!({
                        "type": "object",
                        "properties": {format!("{prefix}{index}"): {"type": "string"}}
                    })
                })
                .collect::<Vec<_>>()
        };
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"allOf": [{"anyOf": members("a")}, {"anyOf": members("b")}]}
            }
        });
        assert_eq!(
            JsonSchema::admit(schema).unwrap_err(),
            SchemaAdmissionError::Unprojectable {
                schema_path: "/properties/query/allOf".into(),
                message: format!(
                    "the meet of its members would hold more than {} type nodes",
                    crate::schema::MAX_PROJECTED_TYPE_NODES
                ),
            }
        );
    }
}
