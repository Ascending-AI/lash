use crate::sync::MutexExt;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::tool_contract::ToolContract;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LashSchema {
    pub schema: Value,
}

impl LashSchema {
    pub fn new(schema: Value) -> Self {
        Self { schema }
    }

    pub fn any() -> Self {
        Self::new(serde_json::json!({}))
    }

    pub fn object(properties: serde_json::Map<String, Value>, required: Vec<String>) -> Self {
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_string(), Value::String("object".to_string()));
        schema.insert("properties".to_string(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert(
                "required".to_string(),
                Value::Array(required.into_iter().map(Value::String).collect()),
            );
        }
        schema.insert("additionalProperties".to_string(), Value::Bool(true));
        Self::new(Value::Object(schema))
    }

    pub fn validate(&self, value: &Value) -> Result<(), String> {
        validate_schema(&self.schema, value)
    }
}

const COMPILED_SCHEMA_CACHE_CAPACITY: usize = 1_024;
const COMPILED_SCHEMA_CACHE_SCHEMA_BYTES: usize = 16 * 1024 * 1024;

struct CachedSchema {
    schema: Value,
    compiled: Result<Arc<jsonschema::JSONSchema>, String>,
}

#[derive(Default)]
struct CompiledSchemaCache {
    entries: HashMap<[u8; 32], Vec<CachedSchema>>,
    entry_count: usize,
    schema_bytes: usize,
}

impl CompiledSchemaCache {
    fn find_compiled(
        &self,
        hash: &[u8; 32],
        schema: &Value,
    ) -> Option<Result<Arc<jsonschema::JSONSchema>, String>> {
        self.entries
            .get(hash)
            .and_then(|entries| entries.iter().find(|entry| entry.schema == *schema))
            .map(|entry| entry.compiled.clone())
    }

    fn insert(
        &mut self,
        hash: [u8; 32],
        schema: &Value,
        serialized_bytes: usize,
        compiled: Result<Arc<jsonschema::JSONSchema>, String>,
    ) {
        // The byte cap accounts for serialized schema input rather than the
        // validator's opaque heap use; the entry cap is a second backstop.
        if self.entry_count >= COMPILED_SCHEMA_CACHE_CAPACITY
            || serialized_bytes
                > COMPILED_SCHEMA_CACHE_SCHEMA_BYTES.saturating_sub(self.schema_bytes)
        {
            return;
        }
        self.entries.entry(hash).or_default().push(CachedSchema {
            schema: schema.clone(),
            compiled,
        });
        self.entry_count += 1;
        self.schema_bytes += serialized_bytes;
    }
}

fn compiled_schema_cache() -> &'static Mutex<CompiledSchemaCache> {
    static CACHE: OnceLock<Mutex<CompiledSchemaCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CompiledSchemaCache::default()))
}

fn schema_content_fingerprint(schema: &Value) -> Result<([u8; 32], usize), String> {
    struct DigestWriter<'a> {
        digest: &'a mut Sha256,
        bytes_written: usize,
    }

    impl Write for DigestWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.digest.update(bytes);
            self.bytes_written = self.bytes_written.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut digest = Sha256::new();
    let serialized_bytes = {
        let mut writer = DigestWriter {
            digest: &mut digest,
            bytes_written: 0,
        };
        serde_json::to_writer(&mut writer, schema).map_err(|error| error.to_string())?;
        writer.bytes_written
    };
    Ok((digest.finalize().into(), serialized_bytes))
}

fn compiled_schema(schema: &Value) -> Result<Arc<jsonschema::JSONSchema>, String> {
    let (hash, serialized_bytes) = schema_content_fingerprint(schema)?;
    if let Some(cached) = compiled_schema_cache()
        .lock_recover()
        .find_compiled(&hash, schema)
    {
        return cached;
    }

    let compiled = reject_non_local_references(schema).and_then(|()| {
        jsonschema::JSONSchema::compile(schema)
            .map(Arc::new)
            .map_err(|error| error.to_string())
    });

    let mut cache = compiled_schema_cache().lock_recover();
    if let Some(existing) = cache.find_compiled(&hash, schema) {
        return existing;
    }
    cache.insert(hash, schema, serialized_bytes, compiled.clone());
    compiled
}

fn validate_schema(schema: &Value, value: &Value) -> Result<(), String> {
    let compiled = compiled_schema(schema)?;
    if compiled.is_valid(value) {
        return Ok(());
    }
    compiled.validate(value).map_err(|mut errors| {
        format_validation_error(errors.next().expect("validation failure contains an error"))
    })
}

pub fn validate_tool_input(contract: &ToolContract, args: &Value) -> Result<(), String> {
    validate_schema(contract.input_schema.canonical(), args)
}

fn reject_non_local_references(schema: &Value) -> Result<(), String> {
    match schema {
        Value::Array(values) => {
            for value in values {
                reject_non_local_references(value)?;
            }
        }
        Value::Object(object) => {
            for (keyword, value) in object {
                if matches!(keyword.as_str(), "$ref" | "$dynamicRef")
                    && !value
                        .as_str()
                        .is_some_and(|reference| reference.starts_with('#'))
                {
                    return Err(format!(
                        "non-local schema reference rejected: `{keyword}` must start with `#`, got {value}"
                    ));
                }
                reject_non_local_references(value)?;
            }
        }
        _ => {}
    }

    Ok(())
}

fn format_validation_error(error: jsonschema::ValidationError<'_>) -> String {
    let instance_path = error.instance_path.to_string();
    if instance_path.is_empty() {
        error.to_string()
    } else {
        format!("{instance_path}: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDefinition;
    use std::time::{Duration, Instant};

    #[test]
    fn repeated_schema_compilation_reuses_the_cached_validator() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "cache_probe_20260723": { "type": "string" } }
        });

        let first = compiled_schema(&schema).expect("compile schema once");
        let second = compiled_schema(&schema).expect("reuse compiled schema");

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn cache_hits_require_structural_equality_after_hash_match() {
        let hash = [7; 32];
        let first_schema = serde_json::json!({ "type": "string" });
        let second_schema = serde_json::json!({ "type": "integer" });
        let first = Arc::new(
            jsonschema::JSONSchema::compile(&first_schema).expect("compile first validator"),
        );
        let second = Arc::new(
            jsonschema::JSONSchema::compile(&second_schema).expect("compile second validator"),
        );
        let mut cache = CompiledSchemaCache::default();
        cache.insert(hash, &first_schema, 17, Ok(first.clone()));
        cache.insert(hash, &second_schema, 18, Ok(second.clone()));

        let first_hit = cache
            .find_compiled(&hash, &first_schema)
            .expect("find first colliding schema")
            .expect("first validator");
        let second_hit = cache
            .find_compiled(&hash, &second_schema)
            .expect("find second colliding schema")
            .expect("second validator");

        assert!(Arc::ptr_eq(&first, &first_hit));
        assert!(Arc::ptr_eq(&second, &second_hit));
        assert!(!Arc::ptr_eq(&first_hit, &second_hit));
    }

    #[test]
    fn validation_rejects_values_that_violate_local_refs() {
        for definitions_key in ["$defs", "definitions"] {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "item": { "$ref": format!("#/{definitions_key}/Item") }
                },
                "required": ["item"],
                "additionalProperties": false,
                (definitions_key): {
                    "Item": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    }
                }
            });

            let error = LashSchema::new(schema)
                .validate(&serde_json::json!({ "item": { "name": 42 } }))
                .unwrap_err();

            assert_eq!(error, "/item/name: 42 is not of type \"string\"");
        }
    }

    #[test]
    fn validation_rejects_bad_value_through_all_of_wrapped_ref() {
        let schema = LashSchema::new(serde_json::json!({
            "definitions": {
                "Inner": { "type": "string" }
            },
            "allOf": [
                { "$ref": "#/definitions/Inner" }
            ],
            "description": "A documented field"
        }));

        let error = schema.validate(&serde_json::json!(42)).unwrap_err();

        assert_eq!(error, "42 is not of type \"string\"");
    }

    #[test]
    fn validation_of_chained_doubling_refs_is_bounded() {
        // Keep the adversarial fanout large enough to catch eager expansion,
        // but small enough that a regression cannot exhaust the test machine.
        const DEPTH: usize = 16;
        const TIME_LIMIT: Duration = Duration::from_secs(1);

        let mut definitions = serde_json::Map::new();
        for depth in 0..DEPTH {
            definitions.insert(
                format!("D{depth}"),
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "left": { "$ref": format!("#/definitions/D{}", depth + 1) },
                        "right": { "$ref": format!("#/definitions/D{}", depth + 1) }
                    }
                }),
            );
        }
        definitions.insert(format!("D{DEPTH}"), serde_json::json!({ "type": "string" }));
        let schema = LashSchema::new(serde_json::json!({
            "$ref": "#/definitions/D0",
            "definitions": definitions
        }));
        let mut value = serde_json::json!(42);
        for _ in 0..DEPTH {
            value = serde_json::json!({ "left": value });
        }

        let started = Instant::now();
        let error = schema.validate(&value).unwrap_err();
        let elapsed = started.elapsed();

        assert!(error.ends_with(": 42 is not of type \"string\""), "{error}");
        assert!(error.contains("/left"), "{error}");
        assert!(
            elapsed < TIME_LIMIT,
            "chained reference validation took {elapsed:?}, limit is {TIME_LIMIT:?}"
        );
    }

    #[test]
    fn validation_rejects_unresolvable_local_ref() {
        let schema = LashSchema::new(serde_json::json!({
            "$ref": "#/definitions/Missing"
        }));

        let error = schema.validate(&serde_json::json!(42)).unwrap_err();

        assert!(error.starts_with("Invalid reference:"), "{error}");
    }

    #[test]
    fn validation_rejects_external_ref_before_compilation() {
        let schema = LashSchema::new(serde_json::json!({
            "$ref": "https://example.com/schema.json"
        }));

        let error = schema.validate(&serde_json::json!(42)).unwrap_err();

        assert_eq!(
            error,
            "non-local schema reference rejected: `$ref` must start with `#`, got \"https://example.com/schema.json\""
        );
    }

    #[test]
    fn validation_rejects_external_dynamic_ref_before_compilation() {
        let schema = LashSchema::new(serde_json::json!({
            "$dynamicRef": "https://example.com/schema.json#node"
        }));

        let error = schema.validate(&serde_json::json!(42)).unwrap_err();

        assert!(
            error.contains("`$dynamicRef` must start with `#`"),
            "{error}"
        );
    }

    #[test]
    fn jsonschema_dependency_cannot_resolve_http_references() {
        let schema = serde_json::json!({
            "$ref": "https://example.com/schema.json"
        });

        let compiled = jsonschema::JSONSchema::compile(&schema).unwrap();
        let error = compiled
            .validate(&serde_json::json!(42))
            .unwrap_err()
            .next()
            .expect("external reference resolution produces an error")
            .to_string();

        assert!(
            error.contains("`resolve-http` feature or a custom resolver is required"),
            "{error}"
        );
    }

    #[test]
    fn validation_rejects_deep_recursive_violation() {
        let schema = LashSchema::new(serde_json::json!({
            "$ref": "#/definitions/Node",
            "definitions": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "child": { "$ref": "#/definitions/Node" }
                    },
                    "required": ["name"]
                }
            }
        }));

        let error = schema
            .validate(&serde_json::json!({
                "name": "root",
                "child": {
                    "name": "level 1",
                    "child": {
                        "name": "level 2",
                        "child": { "name": 123 }
                    }
                }
            }))
            .unwrap_err();

        assert_eq!(
            error,
            "/child/child/child/name: 123 is not of type \"string\""
        );
    }

    #[test]
    fn validation_combines_ref_target_and_sibling_constraints() {
        let schema = LashSchema::new(serde_json::json!({
            "$ref": "#/$defs/AtLeastFive",
            "minimum": 1,
            "$defs": {
                "AtLeastFive": {
                    "type": "number",
                    "minimum": 5
                }
            }
        }));

        let error = schema.validate(&serde_json::json!(2)).unwrap_err();

        assert_eq!(error, "2 is less than the minimum of 5");
    }

    #[test]
    fn validation_reports_missing_required_property_by_path() {
        let tool = ToolDefinition::raw(
            "tool:spotify",
            "spotify",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "access_token": { "type": "string" }
                },
                "required": ["access_token"],
                "additionalProperties": false
            }),
            serde_json::json!({}),
        );

        let error = validate_tool_input(&tool.contract(), &serde_json::json!({})).unwrap_err();
        assert_eq!(error, "\"access_token\" is a required property");
    }

    #[test]
    fn validation_reports_numeric_limits_by_path() {
        let tool = ToolDefinition::raw(
            "tool:spotify",
            "spotify",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "page_limit": { "type": "integer", "maximum": 20 }
                },
                "required": ["page_limit"],
                "additionalProperties": false
            }),
            serde_json::json!({}),
        );

        let error =
            validate_tool_input(&tool.contract(), &serde_json::json!({ "page_limit": 100 }))
                .unwrap_err();
        assert_eq!(error, "/page_limit: 100 is greater than the maximum of 20");
    }

    #[test]
    fn validation_allows_unknown_property_when_additional_properties_is_omitted() {
        let tool = ToolDefinition::raw(
            "tool:mcp__appworld__venmo_show_transactions",
            "mcp__appworld__venmo_show_transactions",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "min_created_at": { "type": "string" },
                    "max_created_at": { "type": "string" },
                    "limit": { "type": "integer", "maximum": 100 }
                },
                "required": ["limit"]
            }),
            serde_json::json!({}),
        );

        validate_tool_input(
            &tool.contract(),
            &serde_json::json!({
                "min_datetime": "2024-01-01T00:00:00Z",
                "limit": 20
            }),
        )
        .unwrap();
    }

    #[test]
    fn validation_allows_unknown_property_when_additional_properties_is_true() {
        let tool = ToolDefinition::raw(
            "tool:open",
            "open",
            "",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "additionalProperties": true
            }),
            serde_json::json!({}),
        );

        validate_tool_input(
            &tool.contract(),
            &serde_json::json!({
                "path": "README.md",
                "unknown": "preserved"
            }),
        )
        .unwrap();
    }
}
