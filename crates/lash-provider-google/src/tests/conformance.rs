//! Cross-provider response-normalization conformance. Wraps this crate's
//! (private) Gemini parsers in a `ProviderNormalizer`. Gemini materializes
//! non-streaming function calls, but it does not expose the streaming
//! chunk-merge scenarios in the same shape as SSE-first providers.

use super::*;
use lash_llm_transport::conformance::{
    CanonicalUsage as U, ProviderConformanceSpec, ProviderNormalizer, ProviderWire,
    ReplayItemExpectation, Scenario, StreamAssembly, provider_conformance, strong_replay_payload,
};

struct GoogleNormalizer;

impl ProviderNormalizer for GoogleNormalizer {
    fn name(&self) -> &str {
        "google-gemini"
    }

    fn conformance_spec(&self) -> ProviderConformanceSpec {
        ProviderConformanceSpec::with_unsupported(&[
            (
                Scenario::StreamingToolArgumentMerge,
                "Gemini streams complete functionCall objects, not argument deltas",
            ),
            (
                Scenario::StreamingUsageMerge,
                "Gemini usage events replace aggregate usage instead of incremental SSE deltas",
            ),
        ])
    }

    fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
        let wire = match scenario {
            Scenario::PlainTextStop => ProviderWire::body(json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "hello" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": U::BASE_INPUT,
                    "candidatesTokenCount": U::BASE_OUTPUT
                }
            })),
            Scenario::OutputCapped => ProviderWire::body(json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "trunc" }] },
                    "finishReason": "MAX_TOKENS"
                }]
            })),
            Scenario::ContentFilter => ProviderWire::body(json!({
                "candidates": [{ "content": { "parts": [] }, "finishReason": "SAFETY" }]
            })),
            Scenario::NonStreamingToolUse => ProviderWire::body(json!({
                "candidates": [{
                    "content": { "parts": [{
                        "functionCall": {
                            "id": "call_1",
                            "name": "lookup",
                            "args": { "q": "x" }
                        }
                    }] }
                }]
            })),
            Scenario::StreamingTextAssembly => {
                ProviderWire::body(json!({})).with_text_stream(
                    vec![
                        r#"{"response":{"candidates":[{"content":{"parts":[{"text":"hello "}]}}]}}"#.to_string(),
                        r#"{"response":{"candidates":[{"content":{"parts":[{"text":"world"}]},"finishReason":"STOP"}]}}"#.to_string(),
                    ],
                    "hello world",
                )
            }
            Scenario::StreamingToolArgumentMerge => return None,
            Scenario::StreamingToolCallAbortEquivalence => {
                ProviderWire::body(json!({})).with_aborted_tool_call_stream(
                    vec![json!({
                        "response": { "candidates": [{
                            "content": { "parts": [{
                                "functionCall": {
                                    "id": "call_abort",
                                    "name": "lookup",
                                    "args": { "q": "x" }
                                }
                            }] }
                        }] }
                    })
                    .to_string()],
                    "lookup",
                    json!({ "q": "x" }),
                )
            }
            Scenario::UsageCacheHit => ProviderWire::body(json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "ok" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": U::BASE_INPUT,
                    "candidatesTokenCount": U::BASE_OUTPUT,
                    "cachedContentTokenCount": U::CACHED_INPUT
                }
            })),
            Scenario::UsageReasoning => ProviderWire::body(json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "ok" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": U::BASE_INPUT,
                    "candidatesTokenCount": U::BASE_OUTPUT,
                    "thoughtsTokenCount": U::REASONING
                }
            })),
            Scenario::ReasoningExtraction => ProviderWire::body(json!({
                "candidates": [{
                    "content": { "parts": [
                        { "text": "thinking about it", "thought": true },
                        { "text": "answer" }
                    ] },
                    "finishReason": "STOP"
                }]
            }))
            .with_reasoning_text("thinking about it"),
            Scenario::ReasoningReplayRoundTrip => {
                // Gemini signs per thought run, so a real turn can carry
                // several signatures; each must survive on its own part.
                let first = strong_replay_payload("google-gemini/thought-0");
                let second = strong_replay_payload("google-gemini/thought-1");
                ProviderWire::body(json!({})).with_reasoning_replay_round_trip(
                    vec![
                        json!({
                            "response": { "candidates": [{
                                "content": { "parts": [{
                                    "text": "thinking",
                                    "thought": true,
                                    "thoughtSignature": first
                                }] }
                            }] }
                        })
                        .to_string(),
                        json!({
                            "response": { "candidates": [{
                                "content": { "parts": [{
                                    "text": "carefully",
                                    "thought": true,
                                    "thoughtSignature": second
                                }] },
                                "finishReason": "STOP"
                            }] }
                        })
                        .to_string(),
                    ],
                    vec![
                        ReplayItemExpectation::new(
                            first.clone(),
                            "/request/contents/0/parts/0/thoughtSignature",
                            json!(first),
                        ),
                        ReplayItemExpectation::new(
                            second.clone(),
                            "/request/contents/0/parts/1/thoughtSignature",
                            json!(second),
                        ),
                    ],
                )
            }
            Scenario::ToolCallReplayRoundTrip => {
                // Gemini 3 rejects a replayed functionCall from a
                // thinking run whose thoughtSignature is missing.
                let first = strong_replay_payload("google-gemini/function-call-0");
                let second = strong_replay_payload("google-gemini/function-call-1");
                let event = |call_id: &str, signature: &str, finished: bool| {
                    let mut candidate = json!({
                        "content": { "parts": [{
                            "functionCall": {
                                "id": call_id,
                                "name": "lookup",
                                "args": { "q": "x" }
                            },
                            "thoughtSignature": signature
                        }] }
                    });
                    if finished {
                        candidate["finishReason"] = json!("STOP");
                    }
                    json!({ "response": { "candidates": [candidate] } }).to_string()
                };
                ProviderWire::body(json!({})).with_tool_call_replay_round_trip(
                    vec![
                        event("call_0", &first, false),
                        event("call_1", &second, true),
                    ],
                    vec![
                        ReplayItemExpectation::new(
                            first.clone(),
                            "/request/contents/0/parts/0/thoughtSignature",
                            json!(first),
                        )
                        .associated_with("/request/contents/0/parts/0/functionCall/id", "call_0"),
                        ReplayItemExpectation::new(
                            second.clone(),
                            "/request/contents/0/parts/1/thoughtSignature",
                            json!(second),
                        )
                        .associated_with("/request/contents/0/parts/1/functionCall/id", "call_1"),
                    ],
                )
            }
            Scenario::StreamingUsageMerge => return None,
        };
        Some(wire)
    }

    fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
        GoogleOAuthProvider::response_parts_from_value(body, None)
    }

    fn usage_from_wire(&self, body: &Value) -> LlmUsage {
        // `usage_from_event` reads `event.response.usageMetadata`; the
        // unwrapped body carries `usageMetadata` at the top level, so
        // re-wrap it exactly as the non-streaming path does.
        let meta = body.get("usageMetadata").cloned().unwrap_or(Value::Null);
        GoogleOAuthProvider::usage_from_event(&json!({
            "response": { "usageMetadata": meta }
        }))
    }

    fn terminal_from_wire(&self, body: &Value, parts: &[LlmOutputPart]) -> LlmTerminalReason {
        GoogleOAuthProvider::terminal_reason_from_value(body, parts)
    }

    fn assemble_stream(&self, scenario: Scenario, sse_events: &[String]) -> StreamAssembly {
        let mut full = String::new();
        let mut text_deltas = Vec::new();
        let mut reasoning_deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut provider_usage = None;
        let mut execution_evidence = None;
        let mut output_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut finish_event = None;
        let stream_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_sink = Arc::clone(&stream_events);
        let sender = LlmEventSender::new(move |event| {
            event_sink.lock_recover().push(event);
        });
        for raw in sse_events {
            let first_new_tool_call = tool_calls.len();
            GoogleOAuthProvider::process_sse_event_with_text_parts(
                raw,
                crate::support::SseTextPartSink {
                    full: &mut full,
                    text_deltas: &mut text_deltas,
                    reasoning_deltas: &mut reasoning_deltas,
                    usage: &mut usage,
                    provider_usage: &mut provider_usage,
                    execution_evidence: &mut execution_evidence,
                    tool_call_parts: Some(&mut tool_calls),
                    output_parts: Some(&mut output_parts),
                    finish_event: &mut finish_event,
                },
                None,
            )
            .expect("google sse event parses");
            for part in &tool_calls[first_new_tool_call..] {
                sender.send(LlmStreamEvent::Part(part.clone()));
            }
        }
        let mut parts = output_parts;
        parts.extend(tool_calls);
        if matches!(
            scenario,
            Scenario::ReasoningReplayRoundTrip | Scenario::ToolCallReplayRoundTrip
        ) {
            crate::conformance_route::stamp_google_replay_origin(&mut parts);
        }
        StreamAssembly {
            parts,
            usage,
            stream_events: stream_events.lock_recover().clone(),
        }
    }

    fn build_next_request(&self, _scenario: Scenario, messages: Vec<LlmMessage>) -> Value {
        let mut req = request(None);
        req.messages = messages;
        let provider = GoogleOAuthProvider::new("access", "refresh", 0);
        let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);
        GoogleOAuthProvider::build_request(&provider, &req, contents, None)
    }
}

#[test]
fn google_satisfies_provider_conformance() {
    provider_conformance(&GoogleNormalizer);
}
