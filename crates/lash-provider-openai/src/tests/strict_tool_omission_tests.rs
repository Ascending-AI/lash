use super::*;

use lash_core::llm::types::{LlmContentBlock, LlmRole};

const TOOL_NAME: &str = "strict_omission_probe";

#[derive(Clone, Copy, Debug)]
enum Endpoint {
    Chat,
    Responses,
}

impl Endpoint {}

fn tool_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "required_name": { "type": "string" },
            "limit": { "type": "integer", "minimum": 1 },
            "nullable_note": { "type": ["string", "null"] },
            "nullable_referenced": { "$ref": "#/$defs/Nullable" },
            "nested": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "optional_count": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "rows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "optional_count": { "type": "integer" }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            },
            "referenced": { "$ref": "#/$defs/Referenced" }
        },
        "required": ["required_name", "nested", "rows", "referenced"],
        "additionalProperties": false,
        "$defs": {
            "Nullable": { "type": ["string", "null"] },
            "Referenced": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "optional_count": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        }
    })
}

fn canonical_arguments() -> Value {
    json!({
        "required_name": "ok",
        "nullable_note": null,
        "nullable_referenced": null,
        "nested": { "id": "nested" },
        "rows": [{ "id": "first" }, { "id": "second", "optional_count": 9 }],
        "referenced": { "id": "ref" }
    })
}

fn recorded_arguments(endpoint: Endpoint, request: &Value) -> Value {
    let item = match endpoint {
        Endpoint::Chat => request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find_map(|message| message["tool_calls"].as_array()?.first())
            .expect("recorded Chat tool call"),
        Endpoint::Responses => request["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("recorded Responses tool call"),
    };
    let encoded = match endpoint {
        Endpoint::Chat => item["function"]["arguments"].as_str(),
        Endpoint::Responses => item["arguments"].as_str(),
    }
    .expect("encoded arguments");
    serde_json::from_str(encoded).expect("recorded arguments JSON")
}

fn replay_request_with_canonical_call() -> LlmRequest {
    let mut req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call-1".to_string(),
            tool_name: TOOL_NAME.to_string(),
            input_json: serde_json::to_string(&canonical_arguments()).unwrap(),
            replay: None,
        }],
    )]);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: TOOL_NAME.to_string(),
        description: "Capture strict omission behavior.".to_string(),
        input_schema: lash_sansio::SchemaContract::admit(tool_input_schema())
            .expect("valid declared schema"),
        output_schema: lash_sansio::SchemaContract::admit(json!({ "type": "object" }))
            .expect("valid declared schema"),
    }]);
    req
}

fn assert_journaled_call_replay_ignores_strict_toggle(endpoint: Endpoint) {
    let req = replay_request_with_canonical_call();
    let chat_body = |strict_tools| {
        OpenAiCompatibleProvider::new("key", "https://openai.test/v1")
            .with_compat(OpenAiCompat {
                schema_capabilities: Some(ProviderSchemaCapabilities::openai(strict_tools)),
                ..OpenAiCompat::default()
            })
            .build_chat_request_body(&req, false)
            .unwrap()
    };
    let responses_body = |strict_tools| {
        let mut provider = OpenAiProvider::new("key");
        provider.inner.compat.schema_capabilities =
            Some(ProviderSchemaCapabilities::openai(strict_tools));
        provider.build_responses_request_body(&req, false).unwrap()
    };

    let (strict, non_strict) = match endpoint {
        Endpoint::Chat => (chat_body(true), chat_body(false)),
        Endpoint::Responses => (responses_body(true), responses_body(false)),
    };
    assert_eq!(
        recorded_arguments(endpoint, &strict),
        recorded_arguments(endpoint, &non_strict)
    );
}

#[test]
fn chat_journaled_canonical_call_replays_identically_when_strict_tools_toggle() {
    assert_journaled_call_replay_ignores_strict_toggle(Endpoint::Chat);
}

#[test]
fn responses_journaled_canonical_call_replays_identically_when_strict_tools_toggle() {
    assert_journaled_call_replay_ignores_strict_toggle(Endpoint::Responses);
}

#[test]
fn strict_decoder_leaves_override_and_ref_backed_ambiguous_union_nulls_untouched() {
    let override_schema = json!({
        "type": "object",
        "properties": { "value": { "type": ["integer", "null"] } },
        "required": ["value"],
        "additionalProperties": false
    });
    let mut req = request(Vec::new());
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "override_probe".to_string(),
        description: "override".to_string(),
        input_schema: lash_sansio::SchemaContract::admit(json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } }
        }))
        .expect("valid declared schema")
        .with_override(
            lash_sansio::SchemaDialect::OpenaiStrictToolParameters,
            lash_sansio::JsonSchema::admit(override_schema)
                .expect("valid declared projection schema"),
        ),
        output_schema: lash_sansio::SchemaContract::admit(json!({}))
            .expect("valid declared schema"),
    }]);
    let capabilities = lash_sansio::ProviderSchemaCapabilities::openai(true);
    let override_decoder =
        crate::responses_shared::ToolArgumentDecoder::for_request("test", &req, &capabilities)
            .unwrap();
    assert_eq!(
        override_decoder.decode("override_probe", r#"{"value":null}"#.to_string()),
        r#"{"value":null}"#
    );

    req.tools = Arc::new(vec![LlmToolSpec {
        name: "union_probe".to_string(),
        description: "union".to_string(),
        input_schema: lash_sansio::SchemaContract::admit(json!({
            "type": "object",
            "properties": {
                "choice": {
                    "anyOf": [
                        { "$ref": "#/$defs/Left" },
                        { "$ref": "#/$defs/Right" }
                    ]
                }
            },
            "required": ["choice"],
            "$defs": {
                "Left": {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "left" },
                        "value": { "type": "integer" }
                    },
                    "required": ["kind"],
                    "additionalProperties": false
                },
                "Right": {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "right" },
                        "value": { "type": ["integer", "null"] }
                    },
                    "required": ["kind", "value"],
                    "additionalProperties": false
                }
            }
        }))
        .expect("valid declared schema"),
        output_schema: lash_sansio::SchemaContract::admit(json!({}))
            .expect("valid declared schema"),
    }]);
    let union_decoder =
        crate::responses_shared::ToolArgumentDecoder::for_request("test", &req, &capabilities)
            .unwrap();
    let arguments = r#"{"choice":{"kind":"left","value":null}}"#;
    assert_eq!(
        union_decoder.decode("union_probe", arguments.to_string()),
        arguments
    );
}

#[test]
fn strict_decoder_preserves_nested_ref_union_omission_null() {
    let mut req = request(Vec::new());
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "nested_union_probe".to_string(),
        description: "nested union".to_string(),
        input_schema: lash_sansio::SchemaContract::admit(json!({
            "type": "object",
            "properties": {
                "choice": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": { "v": { "$ref": "#/$defs/A" } },
                            "required": ["v"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": { "v": { "$ref": "#/$defs/B" } },
                            "required": ["v"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "required": ["choice"],
            "$defs": {
                "A": {
                    "type": "object",
                    "properties": { "n": { "type": "integer" } },
                    "additionalProperties": false
                },
                "B": {
                    "type": "object",
                    "properties": { "n": { "type": ["integer", "null"] } },
                    "required": ["n"],
                    "additionalProperties": false
                }
            }
        }))
        .expect("valid declared schema"),
        output_schema: lash_sansio::SchemaContract::admit(json!({}))
            .expect("valid declared schema"),
    }]);
    let decoder = crate::responses_shared::ToolArgumentDecoder::for_request(
        "test",
        &req,
        &lash_sansio::ProviderSchemaCapabilities::openai(true),
    )
    .unwrap();
    let arguments = r#"{"choice":{"v":{"n":null}}}"#;

    assert_eq!(
        decoder.decode("nested_union_probe", arguments.to_string()),
        arguments
    );
}

#[test]
fn strict_decoder_strips_single_branch_all_of_omission_null() {
    let mut req = request(Vec::new());
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "all_of_probe".to_string(),
        description: "single-branch allOf".to_string(),
        input_schema: lash_sansio::SchemaContract::admit(json!({
            "type": "object",
            "properties": {
                "limit": {
                    "allOf": [{ "type": "integer" }],
                    "default": 37
                }
            }
        }))
        .expect("valid declared schema"),
        output_schema: lash_sansio::SchemaContract::admit(json!({}))
            .expect("valid declared schema"),
    }]);
    let decoder = crate::responses_shared::ToolArgumentDecoder::for_request(
        "test",
        &req,
        &lash_sansio::ProviderSchemaCapabilities::openai(true),
    )
    .unwrap();

    assert_eq!(
        decoder.decode("all_of_probe", r#"{"limit":null}"#.to_string()),
        "{}"
    );
}

#[test]
fn resolved_tool_dialect_owns_wire_strictness_and_omission_decoder() {
    let req = replay_request_with_canonical_call();
    let capabilities = ProviderSchemaCapabilities::openai(true);
    let compat = OpenAiCompat {
        schema_capabilities: Some(capabilities.clone()),
        ..OpenAiCompat::default()
    };
    let chat = OpenAiCompatibleProvider::new("key", "https://openai.test/v1")
        .with_compat(compat.clone())
        .build_chat_request_body(&req, false)
        .unwrap();
    let mut provider = OpenAiProvider::new("key");
    provider.inner.compat = compat;
    let responses = provider.build_responses_request_body(&req, false).unwrap();
    assert_eq!(chat["tools"][0]["function"]["strict"], true);
    assert_eq!(responses["tools"][0]["strict"], true);
    let decoder =
        crate::responses_shared::ToolArgumentDecoder::for_request("openai", &req, &capabilities)
            .unwrap();
    let decoded: Value = serde_json::from_str(&decoder.decode(
        TOOL_NAME,
        json!({"limit": null, "nullable_note": null}).to_string(),
    ))
    .unwrap();
    assert_eq!(decoded, json!({"nullable_note": null}));
}
