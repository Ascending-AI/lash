//! Cross-provider response-normalization conformance adapter. The shared suite
//! lives in `lash_llm_transport::conformance`; this module wraps the crate's
//! private parsers and supplies OpenAI wire fixtures for every scenario.
//!
//! Chat scenarios use the Chat Completions parser. Reasoning replay is an
//! explicit Responses-API scenario and is dispatched to that parser by the
//! scenario identity, never by sniffing fixture contents.

use super::request;
use crate::chat::ChatStreamState;
use crate::responses_shared as shared;
use crate::{OpenAiCompatibleProvider, OpenAiProvider};
use lash_core::llm::types::{
    LlmMessage, LlmOutputPart, LlmStreamEvent, LlmTerminalReason, LlmUsage,
};
use lash_core::provider::Provider;
use lash_llm_transport::conformance::{
    CanonicalUsage as U, ProviderNormalizer, ProviderWire, Scenario, StreamAssembly,
    provider_conformance, strong_replay_payload,
};
use serde_json::{Value, json};

struct OpenAiNormalizer;

impl ProviderNormalizer for OpenAiNormalizer {
    fn name(&self) -> &str {
        "openai-chat"
    }

    fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
        let wire = match scenario {
            Scenario::PlainTextStop => ProviderWire::body(json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "hello" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": U::BASE_INPUT, "completion_tokens": U::BASE_OUTPUT }
            })),
            Scenario::OutputCapped => ProviderWire::body(json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "trunc" },
                    "finish_reason": "length"
                }]
            })),
            Scenario::ContentFilter => ProviderWire::body(json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "" },
                    "finish_reason": "content_filter"
                }]
            })),
            Scenario::NonStreamingToolUse => ProviderWire::body(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "lookup", "arguments": "{\"q\":\"x\"}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })),
            Scenario::StreamingTextAssembly => ProviderWire::body(json!({})).with_text_stream(
                vec![
                    r#"{"choices":[{"delta":{"content":"hello "}}]}"#.to_string(),
                    r#"{"choices":[{"delta":{"content":"world"}}]}"#.to_string(),
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string(),
                    "[DONE]".to_string(),
                ],
                "hello world",
            ),
            Scenario::StreamingToolArgumentMerge => ProviderWire::body(json!({}))
                .with_tool_call_stream(
                    vec![
                        // arguments deliberately split across two SSE events
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]}}]}"#.to_string(),
                        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#.to_string(),
                        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_string(),
                        "[DONE]".to_string(),
                    ],
                    "lookup",
                    json!({ "q": "x" }),
                ),
            Scenario::StreamingToolCallAbortEquivalence => {
                ProviderWire::body(json!({})).with_aborted_tool_call_stream(
                    vec![r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abort","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]}}]}"#.to_string()],
                    "lookup",
                    json!({ "q": "x" }),
                )
            }
            Scenario::UsageCacheHit => ProviderWire::body(json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": U::BASE_INPUT,
                    "completion_tokens": U::BASE_OUTPUT,
                    "prompt_tokens_details": { "cached_tokens": U::CACHED_INPUT }
                }
            })),
            Scenario::UsageReasoning => ProviderWire::body(json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": U::BASE_INPUT,
                    "completion_tokens": U::OUTPUT_WITH_REASONING,
                    "completion_tokens_details": { "reasoning_tokens": U::REASONING }
                }
            })),
            Scenario::ReasoningExtraction => ProviderWire::body(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "reasoning_content": "thinking about it",
                        "content": "answer"
                    },
                    "finish_reason": "stop"
                }]
            }))
            .with_reasoning_text("thinking about it"),
            Scenario::ReasoningReplayRoundTrip => {
                let payload = strong_replay_payload("openai-responses");
                ProviderWire::body(json!({})).with_reasoning_replay_round_trip(
                    vec![
                        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_conformance"}}).to_string(),
                        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"thinking about it"}).to_string(),
                        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_conformance","summary":[{"type":"summary_text","text":"thinking about it"}],"encrypted_content":payload}}).to_string(),
                    ],
                    payload,
                    "/input/0/encrypted_content",
                )
            }
            Scenario::StreamingUsageMerge => ProviderWire::body(json!({}))
                .with_usage_merge_stream(vec![
                    // input arrives first, with no output yet
                    format!(
                        r#"{{"choices":[{{"delta":{{"content":"hi"}}}}],"usage":{{"prompt_tokens":{}}}}}"#,
                        U::BASE_INPUT
                    ),
                    // output arrives in a later event; merge must keep input
                    format!(
                        r#"{{"choices":[{{"delta":{{}}}}],"usage":{{"completion_tokens":{}}}}}"#,
                        U::BASE_OUTPUT
                    ),
                    "[DONE]".to_string(),
                ]),
        };
        Some(wire)
    }

    fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
        OpenAiCompatibleProvider::chat_response_parts_from_value(body)
    }

    fn usage_from_wire(&self, body: &Value) -> LlmUsage {
        lash_llm_transport::openai_usage_from_response_value(body)
    }

    fn terminal_from_wire(&self, body: &Value, parts: &[LlmOutputPart]) -> LlmTerminalReason {
        lash_llm_transport::openai_terminal_reason_from_chat_value(body, parts)
    }

    fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly {
        if matches!(scenario, Scenario::ReasoningReplayRoundTrip) {
            let mut state = shared::ResponsesStreamState::default();
            for raw in sse_events {
                shared::process_sse_event("OpenAI", raw, &mut state, None)
                    .expect("responses SSE event parses");
            }
            let provider = OpenAiProvider::new("key");
            let route = provider.route_identity("openai/gpt-5.4");
            let mut parts = state.response_parts();
            for part in &mut parts {
                part.stamp_replay_origin(&route)
                    .expect("conformance output accepts its minting route");
            }
            return StreamAssembly {
                parts,
                usage: state.usage.clone(),
                stream_events: Vec::new(),
            };
        }
        let mut state = ChatStreamState::default();
        let mut stream_events = Vec::new();
        for raw in sse_events {
            OpenAiCompatibleProvider::process_chat_sse_event(raw, &mut state)
                .expect("chat sse event parses");
            stream_events.extend(
                state
                    .take_completed_tool_call_parts()
                    .into_iter()
                    .map(LlmStreamEvent::Part),
            );
        }
        StreamAssembly {
            parts: state.parts(),
            usage: state.usage.clone(),
            stream_events,
        }
    }

    fn build_next_request(&self, messages: Vec<LlmMessage>) -> Value {
        OpenAiProvider::new("key")
            .build_responses_request_body(&request(messages), false)
            .expect("OpenAI next request serializes")
    }
}

#[test]
fn openai_satisfies_provider_conformance() {
    provider_conformance(&OpenAiNormalizer);
}
