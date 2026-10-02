use super::*;

#[test]
fn chat_tools_use_projected_openai_schema_and_preserve_override() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model.metadata_mut().wire_model = "anthropic/claude-sonnet-4.6".to_string();
    req.tools = Arc::new(vec![
        LlmToolSpec {
            name: "empty".to_string(),
            description: "Empty".to_string(),
            input_schema: lash_sansio::SchemaContract::admit(json!({"type": "object"}))
                .expect("valid declared schema"),
            output_schema: lash_sansio::SchemaContract::admit(json!({}))
                .expect("valid declared schema"),
        },
        LlmToolSpec {
            name: "override".to_string(),
            description: "Override".to_string(),
            input_schema: lash_core::SchemaContract::admit(json!({
                "type": "object",
                "properties": {"raw": {"const": "x"}}
            }))
            .expect("valid declared schema")
            .with_override(
                lash_core::SchemaDialect::OpenaiToolParameters,
                lash_sansio::JsonSchema::admit(json!({
                    "type": "object",
                    "properties": { "raw": { "type": "string", "enum": ["x"] } }
                }))
                .expect("valid declared projection schema"),
            ),
            output_schema: lash_sansio::SchemaContract::admit(json!({}))
                .expect("valid declared schema"),
        },
        LlmToolSpec {
            name: "schemars".to_string(),
            description: "Schemars".to_string(),
            input_schema: lash_sansio::SchemaContract::admit(json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "description": "Maximum number of results.",
                        "allOf": [
                            {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 100
                            }
                        ]
                    }
                }
            }))
            .expect("valid declared schema"),
            output_schema: lash_sansio::SchemaContract::admit(json!({}))
                .expect("valid declared schema"),
        },
    ]);

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"],
        json!({})
    );
    assert_eq!(
        body["tools"][1]["function"]["parameters"]["properties"]["raw"],
        json!({ "type": "string", "enum": ["x"] })
    );
    assert_eq!(
        body["tools"][2]["function"]["parameters"]["properties"]["limit"],
        json!({
            "description": "Maximum number of results.",
            "type": "integer",
            "minimum": 1,
            "maximum": 100
        })
    );
}
