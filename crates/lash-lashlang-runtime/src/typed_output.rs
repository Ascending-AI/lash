//! The RLM typed-output vocabulary: parsing the `output` argument that
//! RLM tools (`llm.query`, `agents.spawn`) accept into a JSON Schema.

use serde_json::Value;

/// Parse a tool's `output` argument into the JSON Schema it requests.
///
/// Two shapes are accepted:
/// - a record of field-name → type-descriptor strings (`"str"`, `"int"`,
///   `"float"`, `"bool"`, `"record"`, or `"list[...]"` of those), compiled
///   into a strict object schema; or
/// - a Lashlang `Type { ... }` literal — a single-field
///   `{"$lash_type": <schema>}` wrapper as produced by the Lashlang
///   compiler — whose inner schema is passed through after validation.
///
/// Returns `Ok(None)` when `output` is absent or `null` (the tool falls back
/// to its untyped default).
pub fn parse_output_schema(
    value: Option<&Value>,
) -> Result<Option<Value>, crate::LashlangRuntimeError> {
    lashlang::parse_output_schema(value).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::LASH_TYPE_KEY;

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
