//! Cross-provider response-normalization conformance adapter. The shared suite
//! lives in `lash_llm_transport::conformance`; this module wraps the crate's
//! private parsers and supplies OpenAI wire fixtures for every scenario.
//!
//! Chat scenarios use the Chat Completions parser and request builder. Reasoning
//! replay is an explicit Responses-API scenario; tool-call replay is an explicit
//! Chat Completions scenario, because that dialect carries its own tool-call
//! replay obligation (a `reasoning.encrypted` detail keyed by `call_id`). Both
//! are dispatched by scenario identity, never by sniffing fixture contents. The
//! Responses tool-call chain (`fc_…` item ids) is gated by the Codex adapter,
//! which serves that dialect.
//!
//! The fixture builders below are `pub(crate)` (as is this module) so the Codex
//! conformance adapter can share the Responses-API replay wire instead of
//! keeping a second copy of it.

use super::request;
use crate::chat::ChatStreamState;
use crate::responses_shared as shared;
use crate::{OpenAiCompatibleProvider, OpenAiProvider};
use lash_core::llm::types::{
    LlmMessage, LlmOutputPart, LlmStreamEvent, LlmTerminalReason, LlmUsage,
};
use lash_core::provider::Provider;
use lash_llm_transport::conformance::{
    CanonicalUsage as U, ProviderNormalizer, ProviderWire, ReplayItemExpectation, Scenario,
    StreamAssembly, provider_conformance, strong_replay_payload,
};
use serde_json::{Value, json};

/// Two reasoning items, each with its own encrypted payload, so the scenario
/// gates every item rather than only the first.
pub(crate) fn reasoning_replay_wire(provider_tag: &str) -> ProviderWire {
    let first = strong_replay_payload(&format!("{provider_tag}/reasoning-0"));
    let second = strong_replay_payload(&format!("{provider_tag}/reasoning-1"));
    let mut sse = Vec::new();
    for (output_index, (item_id, payload)) in
        [("rs_conformance_0", &first), ("rs_conformance_1", &second)]
            .into_iter()
            .enumerate()
    {
        let text = format!("thinking about it {output_index}");
        sse.push(json!({"type":"response.output_item.added","output_index":output_index,"item":{"type":"reasoning","id":item_id}}).to_string());
        sse.push(json!({"type":"response.reasoning_summary_text.delta","output_index":output_index,"delta":text}).to_string());
        sse.push(json!({"type":"response.output_item.done","output_index":output_index,"item":{"type":"reasoning","id":item_id,"summary":[{"type":"summary_text","text":text}],"encrypted_content":payload}}).to_string());
    }
    ProviderWire::body(json!({})).with_reasoning_replay_round_trip(
        sse,
        vec![
            ReplayItemExpectation::new(first, "/input/0/encrypted_content"),
            ReplayItemExpectation::new(second, "/input/1/encrypted_content"),
        ],
    )
}

/// Two streamed function calls whose `fc_…` item ids chain each call to its
/// sibling reasoning item; the ids must reappear on the replayed items.
pub(crate) fn tool_call_replay_wire() -> ProviderWire {
    let mut sse = Vec::new();
    for (output_index, (item_id, call_id)) in [
        ("fc_conformance_0", "call_0"),
        ("fc_conformance_1", "call_1"),
    ]
    .into_iter()
    .enumerate()
    {
        sse.push(json!({"type":"response.output_item.added","output_index":output_index,"item":{"type":"function_call","id":item_id,"call_id":call_id,"name":"lookup","arguments":""}}).to_string());
        sse.push(json!({"type":"response.function_call_arguments.delta","output_index":output_index,"item_id":item_id,"delta":"{\"q\":\"x\"}"}).to_string());
        sse.push(json!({"type":"response.output_item.done","output_index":output_index,"item":{"type":"function_call","id":item_id,"call_id":call_id,"name":"lookup","arguments":"{\"q\":\"x\"}","status":"completed"}}).to_string());
    }
    ProviderWire::body(json!({})).with_tool_call_replay_round_trip(
        sse,
        vec![
            ReplayItemExpectation::new("fc_conformance_0", "/input/0/id")
                .associated_with("/input/0/call_id", "call_0"),
            ReplayItemExpectation::new("fc_conformance_1", "/input/1/id")
                .associated_with("/input/1/call_id", "call_1"),
        ],
    )
}

/// The Chat Completions dialect carries tool-call replay as a whole
/// `reasoning.encrypted` detail keyed by `call_id`: the stream parser matches
/// each detail to its tool call and stores the detail in `replay.opaque`, and
/// the chat request builder re-emits it in the assistant message's
/// `reasoning_details`. That is a second, independent replay obligation from the
/// Responses `fc_…` chain, so it gets its own fixture through the chat parser
/// and the chat request builder.
pub(crate) fn chat_tool_call_replay_wire() -> ProviderWire {
    let detail = |call_id: &str| {
        json!({
            "type": "reasoning.encrypted",
            "id": call_id,
            "data": strong_replay_payload(&format!("openai-chat/{call_id}")),
        })
    };
    let first = detail("call_0");
    let second = detail("call_1");
    // Each detail rides the event that completes the call it signs, so the
    // live-emitted tool-call part carries the replay too — not only the
    // end-of-stream assembly.
    let sse = vec![
        json!({"choices":[{"delta":{
            "tool_calls":[{"index":0,"id":"call_0","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}],
            "reasoning_details":[first.clone()]
        }}]}).to_string(),
        json!({"choices":[{"delta":{
            "tool_calls":[{"index":1,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"y\"}"}}],
            "reasoning_details":[second.clone()]
        }}]}).to_string(),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        "[DONE]".to_string(),
    ];
    ProviderWire::body(json!({})).with_tool_call_replay_round_trip(
        sse,
        vec![
            ReplayItemExpectation::new(first.to_string(), "/messages/0/reasoning_details/0")
                .associated_with("/messages/0/tool_calls/0/id", "call_0"),
            ReplayItemExpectation::new(second.to_string(), "/messages/0/reasoning_details/1")
                .associated_with("/messages/0/tool_calls/1/id", "call_1"),
        ],
    )
}

/// The Chat Completions half of this provider. Stamping the replay origin and
/// building the next request must use the same handle, because the chat builder
/// scrubs replay state whose origin is not the route it is serving.
fn chat_provider() -> OpenAiCompatibleProvider {
    OpenAiProvider::new("key").inner
}

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
                reasoning_replay_wire("openai-responses")
            }
            Scenario::ToolCallReplayRoundTrip => chat_tool_call_replay_wire(),
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
        let mut parts = state.parts();
        if matches!(scenario, Scenario::ToolCallReplayRoundTrip) {
            let route = chat_provider().route_identity("openai/gpt-5.4");
            for part in &mut parts {
                part.stamp_replay_origin(&route)
                    .expect("conformance output accepts its minting route");
            }
        }
        StreamAssembly {
            parts,
            usage: state.usage.clone(),
            stream_events,
        }
    }

    fn build_next_request(&self, scenario: Scenario, messages: Vec<LlmMessage>) -> Value {
        let provider = OpenAiProvider::new("key");
        let req = request(messages);
        // Tool-call replay is a Chat Completions obligation here (the
        // `reasoning_details` detail keyed by call_id); reasoning replay is a
        // Responses obligation. Build the dialect the fixture belongs to.
        if matches!(scenario, Scenario::ToolCallReplayRoundTrip) {
            return chat_provider()
                .build_chat_request_body(&req, false)
                .expect("OpenAI chat next request serializes");
        }
        provider
            .build_responses_request_body(&req, false)
            .expect("OpenAI next request serializes")
    }
}

#[test]
fn openai_satisfies_provider_conformance() {
    provider_conformance(&OpenAiNormalizer);
}
