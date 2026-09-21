use lash_core::ToolDefinition;
use lash_tool_support::{ToolBinding, ToolDefinitionBindingExt, object_schema};

pub type BatchResultRow = lash_sansio::BatchResultRow;

pub fn batch_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:batch",
        "batch",
        "Run 1-25 independent tool calls concurrently. Execution order is not guaranteed; results return in input order, each with a success flag and result or error. Do not nest batch calls.",
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
                    "description": "1-25 objects { tool, parameters }; each tool must be exposed and parameters must match its schema."
                }
            }),
            &["tool_calls"],
        ),
        batch_output_schema(),
    )
    .with_examples(vec![
            r#"await tools.batch({ tool_calls: [{ tool: "<first_tool>", parameters: { arg: "value" } }, { tool: "<second_tool>", parameters: { arg: "value" } }] })?"#.to_string(),
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
    fn batch_result_row_keys_match_the_declared_output_schema() {
        let item_schema = &batch_output_schema()["properties"]["results"]["items"];
        let declared_keys: std::collections::BTreeSet<&str> = item_schema["properties"]
            .as_object()
            .expect("row properties")
            .keys()
            .map(String::as_str)
            .collect();
        let required_keys: std::collections::BTreeSet<&str> = item_schema["required"]
            .as_array()
            .expect("row required")
            .iter()
            .map(|key| key.as_str().expect("required key"))
            .collect();

        for row in [
            BatchResultRow::success(0, "tool:alpha", 0, serde_json::json!("ok")),
            BatchResultRow::failure(1, "tool:beta", 0, serde_json::json!("boom")),
        ] {
            let serialized = serde_json::to_value(&row).expect("row serializes");
            let keys: std::collections::BTreeSet<&str> = serialized
                .as_object()
                .expect("row object")
                .keys()
                .map(String::as_str)
                .collect();
            assert!(
                keys.is_subset(&declared_keys),
                "row keys {keys:?} must be a subset of the declared properties {declared_keys:?}"
            );
            assert!(
                required_keys.is_subset(&keys),
                "row keys {keys:?} must cover the required keys {required_keys:?}"
            );
        }
    }

    #[test]
    fn batch_result_row_decode_names_missing_required_field() {
        let error = serde_json::from_value::<BatchResultRow>(serde_json::json!({
            "index": 0,
            "success": true,
            "duration_ms": 0,
            "result": "ok"
        }))
        .expect_err("row without tool must fail");

        assert!(
            error.to_string().contains("missing field `tool`"),
            "{error}"
        );
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
            "fetch_url",
            "search_web",
        ] {
            assert!(
                !model_facing_text.contains(removed_tool),
                "batch contract should not mention removed tool `{removed_tool}`"
            );
        }
        assert!(model_facing_text.contains("<first_tool>"));
        assert!(model_facing_text.contains("<second_tool>"));
    }
}
