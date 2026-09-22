//! Cross-provider replay of a historical tool call whose argument text never
//! parsed as JSON. The standard protocol refuses such a call pre-dispatch but
//! keeps the model's raw text in history; every provider request builder must
//! then replay it through the shared `{"_raw": ...}` rule instead of failing
//! the request or inventing a representation.

use std::sync::Arc;

use lash_core::llm::types::{LlmContentBlock, LlmMessage, LlmRequest, LlmRole};
use lash_core::provider::CacheRetention;
use serde_json::{Value, json};

const RAW_ARGS: &str = r#"{"path": "a.txt", "content": "he said "hi"}"#;

fn malformed_history_request(model: &str) -> LlmRequest {
    LlmRequest {
        instructions: Some(Arc::from("stable system")),
        model: model.to_string(),
        messages: vec![
            LlmMessage::text(LlmRole::User, "write the greeting"),
            LlmMessage::new(
                LlmRole::Assistant,
                vec![LlmContentBlock::ToolCall {
                    call_id: "call-1".to_string(),
                    tool_name: "write_file".to_string(),
                    input_json: RAW_ARGS.to_string(),
                    replay: None,
                }],
            ),
            LlmMessage::new(
                LlmRole::User,
                vec![LlmContentBlock::ToolResult {
                    call_id: "call-1".to_string(),
                    tool_name: Some("write_file".to_string()),
                    content: "the call was not executed".to_string(),
                }],
            ),
            LlmMessage::text(LlmRole::User, "try again with valid JSON"),
        ],
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        model_variant: Default::default(),
        model_capability: Default::default(),
        scope: lash_core::LlmRequestScope::new("replay-session", "replay-frame", "replay-request"),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}

/// True when `value` is, or contains, the shared `_raw` wrapper — either as a
/// JSON value (Anthropic `input`, Google `args`) or as a string field carrying
/// the wrapper's JSON encoding (OpenAI `arguments`).
fn contains_raw_wrapper(value: &Value, wrapped: &Value) -> bool {
    if value == wrapped {
        return true;
    }
    match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok().as_ref() == Some(wrapped),
        Value::Array(items) => items.iter().any(|item| contains_raw_wrapper(item, wrapped)),
        Value::Object(map) => map
            .values()
            .any(|child| contains_raw_wrapper(child, wrapped)),
        _ => false,
    }
}

#[test]
fn malformed_tool_arguments_replay_on_every_provider() {
    let wrapped = json!({ lash_core::llm::types::RAW_TOOL_CALL_ARGUMENTS_KEY: RAW_ARGS });
    let request = malformed_history_request("test-model");

    let anthropic =
        lash_provider_anthropic::testing::serialize_request(&request, CacheRetention::Short)
            .expect("Anthropic must replay malformed history instead of failing the request");
    assert!(
        contains_raw_wrapper(&anthropic, &wrapped),
        "Anthropic tool_use input must wrap the raw text: {anthropic}"
    );

    let google = lash_provider_google::testing::serialize_request(&request, CacheRetention::Short)
        .expect("Google must replay malformed history");
    assert!(
        contains_raw_wrapper(&google, &wrapped),
        "Google functionCall args must wrap the raw text: {google}"
    );

    let (chat, _) =
        lash_provider_openai::testing::serialize_chat_request(&request, CacheRetention::None)
            .expect("OpenAI Chat must replay malformed history");
    assert!(
        contains_raw_wrapper(&chat, &wrapped),
        "OpenAI Chat tool call arguments must wrap the raw text: {chat}"
    );

    let responses =
        lash_provider_openai::testing::serialize_responses_request(&request, CacheRetention::None)
            .expect("OpenAI Responses must replay malformed history");
    assert!(
        contains_raw_wrapper(&responses, &wrapped),
        "OpenAI Responses function_call arguments must wrap the raw text: {responses}"
    );

    let codex =
        lash_provider_openai::testing::serialize_codex_request(&request, CacheRetention::None)
            .expect("Codex must replay malformed history");
    assert!(
        contains_raw_wrapper(&codex, &wrapped),
        "Codex function_call arguments must wrap the raw text: {codex}"
    );
}
