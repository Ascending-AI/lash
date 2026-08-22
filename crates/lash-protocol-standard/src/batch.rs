use lash_core::ToolDefinition;
use lash_tool_support::{ToolBinding, ToolDefinitionBindingExt};

pub fn batch_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:batch",
        "batch",
        "Execute up to 25 independent tool calls concurrently. Calls start in parallel; ordering is not guaranteed. Calls past index 25 are rejected.",
        object_schema(
            serde_json::json!({
                "tool_calls": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 25,
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string" },
                            "parameters": { "type": "object", "additionalProperties": true }
                        },
                        "required": ["tool", "parameters"],
                        "additionalProperties": false
                    },
                    "description": "Array of 1-25 objects like { tool: \"fetch_url\", parameters: { url: \"https://example.com\" } }. Use only for independent calls. Do not include another batch call. More than 25 calls is rejected as a tool error."
                }
            }),
            &["tool_calls"],
        ),
        batch_output_schema(),
    )
    .with_examples(vec![
            r#"await tools.batch({ tool_calls: [{ tool: "fetch_url", parameters: { url: "https://example.com" } }, { tool: "search_web", parameters: { query: "Lash ToolProvider" } }] })?"#.to_string(),
        ])
    .with_tool_binding(ToolBinding::new(["tools"], "batch"))
}

fn batch_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "index": { "type": "integer", "minimum": 0 },
                        "tool": { "type": "string" },
                        "success": { "type": "boolean" },
                        "duration_ms": { "type": "integer", "minimum": 0 },
                        "result": {},
                        "error": {}
                    },
                    "required": ["index", "tool", "success", "duration_ms"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["results"],
        "additionalProperties": false
    })
}

fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_contract_documents_results_array() {
        let definition = batch_tool_definition();

        assert_eq!(
            definition.contract.output_schema.canonical["required"],
            serde_json::json!(["results"])
        );
        let rendered = definition.compact_contract().render_signature();
        assert!(rendered.contains("results"), "{rendered}");
    }

    #[test]
    fn batch_contract_uses_only_surviving_tool_examples() {
        let definition = batch_tool_definition();
        let description =
            definition.contract.input_schema.canonical["properties"]["tool_calls"]["description"]
                .as_str()
                .expect("batch tool_calls description");
        let model_facing_text =
            format!("{} {}", description, definition.contract.examples.join(" "));

        for removed_tool in [
            "read_file",
            r#"tool: "edit""#,
            r#"tool: "write""#,
            r#"tool: "glob""#,
        ] {
            assert!(
                !model_facing_text.contains(removed_tool),
                "batch contract should not mention removed tool `{removed_tool}`"
            );
        }
        assert!(model_facing_text.contains("fetch_url"));
        assert!(model_facing_text.contains("search_web"));
    }
}
