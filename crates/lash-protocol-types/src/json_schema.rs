//! Admitted JSON schemas and typed value mismatches.

use crate::allocation::{Allocated, AllocationBudget, AllocationError, SourceCapacity};
use std::sync::{Arc, Mutex, OnceLock};

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
    #[error(transparent)]
    Allocation(#[from] AllocationError),
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
    /// ([`lash_kernel_check::MAX_PROJECTED_TYPE_NODES`]), at the JSON
    /// pointer `schema_path`: a value of the schema could not be typed.
    #[error("JSON schema at `{schema_path}` has no kernel type: {message}")]
    Unprojectable {
        schema_path: String,
        message: String,
    },
}

impl From<lash_kernel_check::SchemaProjectionRefusal> for SchemaAdmissionError {
    fn from(refusal: lash_kernel_check::SchemaProjectionRefusal) -> Self {
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
    value: Arc<Allocated<Value>>,
    validator: Arc<PreparedValidator>,
    types: Arc<Allocated<KernelTypes>>,
}

struct PreparedValidator {
    // The engine's pools retain at most their owner cache and one returned
    // stack cache. No validator use bypasses this synchronization owner.
    engine: Arc<Mutex<jsonschema::Validator>>,
    _allocation: crate::allocation::AllocationHold,
    capacity: u64,
}

// Match the pinned regex builder configuration explicitly. These are the
// existing compiler limits, not a separate schema admission policy.
const REGEX_NFA_BYTES: usize = 10 * 1024 * 1024;
const REGEX_CACHE_BYTES: usize = 2 * 1024 * 1024;

/// A whole-source bound for the pinned compiler and its two kernel types.
/// Source nodes and complete paths cover registry/node/keyword storage;
/// simple reference paths cover location/dynamic-scope cache variants. The
/// kernel resolves at most MAX_RESOLVED_REFERENCES and each composition may
/// add MAX_PROJECTED_TYPE_NODES beyond its sides. A fancy expression has no
/// more delegated programs than source bytes plus one. Each admits four NFA
/// budgets for construction/search scratch and two explicit DFA cache budgets.
fn preparation_capacity(value: &Value) -> u64 {
    fn scan(
        value: &Value,
        nodes: &mut u64,
        refs: &mut u64,
        compositions: &mut u64,
        regex: &mut u64,
    ) {
        *nodes = nodes.saturating_add(1);
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                        *refs = refs.saturating_add(1);
                    }
                    if matches!(key.as_str(), "allOf" | "anyOf" | "oneOf") {
                        *compositions = compositions.saturating_add(
                            value.as_array().map_or(0, |members| members.len()) as u64,
                        );
                    }
                    let mut pattern = |text: &str| {
                        let programs = text.len() as u64 + 1;
                        let per_program = (4 * REGEX_NFA_BYTES + 2 * REGEX_CACHE_BYTES) as u64;
                        *regex = regex.saturating_add(programs.saturating_mul(
                            per_program.saturating_add((text.len() as u64).saturating_mul(4096)),
                        ));
                    };
                    if key == "pattern"
                        && let Some(text) = value.as_str()
                    {
                        pattern(text);
                    }
                    if key == "patternProperties"
                        && let Some(properties) = value.as_object()
                    {
                        for name in properties.keys() {
                            pattern(name);
                        }
                    }
                    scan(value, nodes, refs, compositions, regex);
                }
            }
            Value::Array(values) => {
                for value in values {
                    scan(value, nodes, refs, compositions, regex);
                }
            }
            _ => {}
        }
    }
    let (mut nodes, mut refs, mut compositions, mut regex) = (0u64, 0u64, 0u64, 0u64);
    scan(value, &mut nodes, &mut refs, &mut compositions, &mut regex);
    let source = value
        .owned_capacity()
        .saturating_add(std::mem::size_of::<Value>() as u64);
    let mut variants = 1u64;
    for edge in 1..=refs.saturating_add(1).min(64) {
        variants = variants.saturating_mul(edge);
    }
    if refs >= 64 {
        variants = u64::MAX;
    }
    let compiler = source
        .saturating_add(nodes.saturating_mul(4096))
        .saturating_mul(nodes.saturating_add(1))
        .saturating_mul(variants)
        .saturating_mul(4);
    let expanded = variants.min(lash_kernel_check::MAX_RESOLVED_REFERENCES as u64 + 1);
    let types = nodes
        .saturating_add(
            compositions.saturating_mul(lash_kernel_check::MAX_PROJECTED_TYPE_NODES as u64),
        )
        .saturating_mul(1024)
        .saturating_add(source.saturating_mul(8))
        .saturating_mul(expanded)
        .saturating_mul(2);
    compiler
        .saturating_add(types)
        .saturating_add(regex)
        .saturating_add(4096)
}

/// The kernel types an admitted schema projects to, as an input and as an
/// open result.
struct KernelTypes {
    input: lash_kernel_doc::Type,
    result: lash_kernel_doc::Type,
}

impl JsonSchema {
    pub fn admit(value: Value) -> Result<Self, SchemaAdmissionError> {
        Self::admit_in(value, &AllocationBudget::unowned())
    }

    /// Prepare a schema only after its complete construction and retained
    /// cache bound fits the serving owner's host-supplied ledger.
    pub fn admit_in(value: Value, budget: &AllocationBudget) -> Result<Self, SchemaAdmissionError> {
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
        if budget.identity() == 0 && value.as_object().is_some_and(serde_json::Map::is_empty) {
            return Ok(Self::any());
        }
        Self::compile(value, budget)
    }

    /// A cold or foreign immutable preparation is adopted before this owner
    /// retains it. Its shared engine keeps the same lock across ledgers.
    pub fn adopt_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.value.allocation_budget().identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(self.validator.capacity)?;
        Ok(Self {
            value: Arc::new(Allocated::new(
                self.as_value().clone(),
                allocation.clone(),
                budget.clone(),
            )),
            validator: Arc::new(PreparedValidator {
                engine: self.validator.engine.clone(),
                _allocation: allocation.clone(),
                capacity: self.validator.capacity,
            }),
            types: Arc::new(Allocated::new(
                KernelTypes {
                    input: self.types.input.clone(),
                    result: self.types.result.clone(),
                },
                allocation,
                budget.clone(),
            )),
        })
    }

    pub fn allocation_budget(&self) -> &AllocationBudget {
        self.value.allocation_budget()
    }

    fn compile(value: Value, budget: &AllocationBudget) -> Result<Self, SchemaAdmissionError> {
        let capacity = preparation_capacity(&value);
        let allocation = budget.reserve(capacity)?;
        let validator = jsonschema::options()
            .offline()
            .with_pattern_options(
                jsonschema::PatternOptions::fancy_regex()
                    .size_limit(REGEX_NFA_BYTES)
                    .dfa_size_limit(REGEX_CACHE_BYTES),
            )
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
        let types = KernelTypes {
            input: lash_kernel_check::type_of_schema(&value)?,
            result: lash_kernel_check::result_type_of_schema(&value)?,
        };
        Ok(Self {
            value: Arc::new(Allocated::new(value, allocation.clone(), budget.clone())),
            validator: Arc::new(PreparedValidator {
                engine: Arc::new(Mutex::new(validator)),
                _allocation: allocation.clone(),
                capacity,
            }),
            types: Arc::new(Allocated::new(types, allocation, budget.clone())),
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "the empty object is a compilable schema"
    )]
    pub fn any() -> Self {
        static ANY: OnceLock<JsonSchema> = OnceLock::new();
        ANY.get_or_init(|| {
            Self::compile(
                Value::Object(Default::default()),
                &AllocationBudget::unowned(),
            )
            .expect("empty object is a valid JSON schema")
        })
        .clone()
    }

    pub fn as_value(&self) -> &Value {
        &self.value
    }

    /// The kernel type this schema describes as a tool's input
    /// ([`lash_kernel_check::type_of_schema`]).
    pub fn input_type(&self) -> &lash_kernel_doc::Type {
        &self.types.input
    }

    /// The kernel type this schema describes as a tool's result, each
    /// record of it open ([`lash_kernel_check::result_type_of_schema`]).
    pub fn result_type(&self) -> &lash_kernel_doc::Type {
        &self.types.result
    }

    pub fn into_value(self) -> Value {
        self.as_value().clone()
    }

    pub fn validate(&self, value: &Value) -> Result<(), ValueMismatch> {
        let validator = self
            .validator
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        validator.validate(value).map_err(|error| ValueMismatch {
            instance_path: error.instance_path().to_string(),
            message: validator
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
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        self.as_value() == other.as_value()
    }
}
impl Eq for JsonSchema {}

impl std::fmt::Debug for JsonSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_value().fmt(f)
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
                    lash_kernel_check::MAX_PROJECTED_TYPE_NODES
                ),
            }
        );
    }
}

#[cfg(test)]
mod allocation_laws {
    use super::*;
    use crate::llm::provider_body::canonical::tests::allocation_counter::Measurement;

    #[test]
    fn concurrent_schema_validation_keeps_retained_regex_caches_independent_of_thread_count() {
        fn retained(threads: usize) -> i64 {
            let ledger = crate::allocation::testing::Ledger::default();
            ledger.limit(u64::MAX);
            let construction = Measurement::start();
            let schema = JsonSchema::admit_in(
                serde_json::json!({"type":"string", "pattern":"^(?:[a-z]+[0-9]?)+b$"}),
                &ledger.budget(),
            )
            .unwrap();
            let prepared = construction.finish().retained;

            let input = Value::String(format!("{}b", "a".repeat(1_000_000)));
            schema.validate(&input).unwrap();
            let barrier = std::sync::Barrier::new(threads);
            let caches: i64 = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            let measured = Measurement::start();
                            schema.validate(&input).unwrap();
                            measured.finish().retained
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|thread| thread.join().unwrap())
                    .sum()
            });
            assert!(
                (prepared + caches).max(0) as u64 <= ledger.used(),
                "prepared source and all retained caches fit their admitted bound"
            );
            drop(schema);
            assert_eq!(ledger.used(), 0);
            caches
        }
        let two = retained(2);
        let many = retained(32);
        eprintln!("retained regex cache bytes at 2/32 validation threads: {two}/{many}");
        assert!(
            many <= two + 4096,
            "one shared schema must not retain one regex cache per concurrent caller: {two}/{many}"
        );
    }
}
