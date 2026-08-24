use lash_core::{ToolDefinition, ToolExecutionGrant};

#[test]
fn tool_execution_grant_json_layout_is_stable() {
    let grant = ToolExecutionGrant::from_definition(ToolDefinition::raw(
        "tool:layout_probe",
        "layout_probe",
        "Pinned grant layout",
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    ))
    .with_source_id("registry:layout")
    .with_execution_binding(serde_json::json!({ "route": "pinned" }));

    let serialized = serde_json::to_string(&grant).expect("grant must serialize");
    assert_eq!(grant.manifest().id.as_str(), "tool:layout_probe");
    assert_eq!(
        grant.contract().input_schema.canonical["required"],
        serde_json::json!(["query"])
    );
    assert_eq!(
        serialized,
        r#"{"manifest":{"id":"tool:layout_probe","name":"layout_probe","description":"Pinned grant layout","compact_contract":{"name":"layout_probe","signature":"layout_probe({ query: str })","returns":"str","parameters":[{"name":"query","required":true,"signature":"query: str","type":"str"}],"description":"Pinned grant layout"}},"contract":{"input_schema":{"canonical":{"additionalProperties":false,"properties":{"query":{"type":"string"}},"required":["query"],"type":"object"}},"output_schema":{"canonical":{"type":"string"}}},"source_id":"registry:layout","execution_binding":{"route":"pinned"}}"#
    );
    let round_tripped: ToolExecutionGrant =
        serde_json::from_str(&serialized).expect("grant must deserialize");
    assert_eq!(
        serde_json::to_string(&round_tripped).expect("round-tripped grant must serialize"),
        serialized
    );
}
