use std::sync::Arc;

use lash_core::llm::types::{
    LlmContentBlock, LlmEventSender, LlmMessage, LlmOutputPart, LlmRequest, LlmRole, LlmToolChoice,
    LlmToolSpec, LlmUsage, ProviderRouteIdentity,
};
use lash_core::provider::{ModelCapability, Provider};
use lash_sansio::sync::MutexExt;
use serde_json::json;

use super::GoogleOAuthProvider;

#[derive(Debug)]
struct CapturingTransport {
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for CapturingTransport {
    async fn send(
        &self,
        request: lash_llm_transport::LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
    {
        self.bodies
            .lock_recover()
            .push(String::from_utf8(request.body.to_vec()).expect("JSON request body"));
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: Vec::new(),
            body: lash_llm_transport::LlmHttpBody::buffered(
                r#"{"response":{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"done"}]}}]}}"#,
            ),
        })
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "gemini-2.5-pro".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "hello")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: ModelCapability::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None::<LlmEventSender>,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

#[test]
fn foreign_anthropic_reasoning_signature_is_not_forwarded_to_google() {
    let mut req = request();
    req.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Reasoning {
            text: "neutral summary".to_string(),
            replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                signature: Some("anthropic-signature".to_string()),
                origin: Some(ProviderRouteIdentity::for_endpoint(
                    "anthropic",
                    "https://api.anthropic.com",
                    "claude-sonnet-4-6",
                )),
                ..Default::default()
            }),
        }],
    )];

    let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);

    assert_eq!(contents[0]["parts"][0]["text"], "neutral summary");
    assert!(contents[0]["parts"][0].get("thought").is_none());
    assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
}

#[test]
fn raw_google_builder_drops_unstamped_reasoning_replay() {
    let mut req = request();
    req.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Reasoning {
            text: "portable summary".to_string(),
            replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                signature: Some("unstamped-signature".to_string()),
                ..Default::default()
            }),
        }],
    )];

    let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);
    assert_eq!(contents[0]["parts"][0]["text"], "portable summary");
    assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
    assert!(
        !serde_json::to_string(&contents)
            .expect("contents serialize")
            .contains("unstamped-signature")
    );
}

#[tokio::test]
async fn raw_provider_complete_drops_foreign_and_unstamped_replay_from_google_wire() {
    let mut req = request();
    req.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![
            LlmContentBlock::Reasoning {
                text: "foreign summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("foreign-google-wire-secret".to_string()),
                    origin: Some(ProviderRouteIdentity::for_endpoint(
                        "anthropic",
                        "https://api.anthropic.com",
                        "claude-sonnet-4-6",
                    )),
                    ..Default::default()
                }),
            },
            LlmContentBlock::Reasoning {
                text: "unstamped summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("unstamped-google-wire-secret".to_string()),
                    ..Default::default()
                }),
            },
        ],
    )];
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut provider = GoogleOAuthProvider::new("access", "refresh", u64::MAX).with_transport(
        Arc::new(CapturingTransport {
            bodies: Arc::clone(&bodies),
        }),
    );

    Provider::complete(&mut provider, req)
        .await
        .expect("raw completion");
    let wire = bodies.lock_recover().join("\n");
    assert!(wire.contains("foreign summary"));
    assert!(wire.contains("unstamped summary"));
    assert!(!wire.contains("foreign-google-wire-secret"));
    assert!(!wire.contains("unstamped-google-wire-secret"));
}

#[test]
fn foreign_openai_chat_opaque_tool_replay_is_not_forwarded_to_google() {
    let mut req = request();
    req.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call-1".to_string(),
            tool_name: "lookup".to_string(),
            input_json: "{}".to_string(),
            replay: Some(lash_core::llm::types::ProviderReplayMeta {
                item_id: None,
                opaque: Some("openai-chat-opaque".to_string()),
                origin: Some(ProviderRouteIdentity::for_endpoint(
                    "openai-compatible",
                    "https://openrouter.ai/api/v1",
                    "gpt-5.4",
                )),
            }),
        }],
    )];

    let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);

    assert!(contents[0]["parts"][0].get("thoughtSignature").is_none());
}

#[test]
fn same_route_reasoning_and_tool_replay_are_forwarded_to_google() {
    let provider = GoogleOAuthProvider::new("access", "refresh", 0);
    let native_route = provider.route_identity("gemini-2.5-pro");
    let mut req = request();
    req.messages = vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![
            LlmContentBlock::Reasoning {
                text: "native summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    signature: Some("native-reasoning-signature".to_string()),
                    origin: Some(native_route.clone()),
                    ..Default::default()
                }),
            },
            LlmContentBlock::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: Some(lash_core::llm::types::ProviderReplayMeta {
                    item_id: None,
                    opaque: Some("native-tool-signature".to_string()),
                    origin: Some(native_route.clone()),
                }),
            },
        ],
    )];

    let contents = GoogleOAuthProvider::build_contents_with_attachment_parts(&req, &[]);

    assert_eq!(
        contents[0]["parts"][0]["thoughtSignature"],
        "native-reasoning-signature"
    );
    assert_eq!(
        contents[0]["parts"][1]["thoughtSignature"],
        "native-tool-signature"
    );
}

/// Gemini 3 can emit a thought part that carries only a
/// `thoughtSignature` (no text, or empty text). Both read paths must keep
/// it as an empty-text `Reasoning` part so the signature survives into the
/// next request — dropping it makes Gemini reject the replayed turn.
#[test]
fn google_signature_only_thought_part_round_trips_from_streaming_and_batch() {
    let signature = "dGhvdWdodC1zaWduYXR1cmUtb25seQ==";
    for thought_part in [
        json!({ "thought": true, "thoughtSignature": signature }),
        json!({ "text": "", "thought": true, "thoughtSignature": signature }),
    ] {
        let streaming_event = json!({"response":{"candidates":[{
            "content":{"parts":[thought_part.clone()]},
            "finishReason":"STOP"
        }]}});
        let mut full = String::new();
        let mut text_deltas = Vec::new();
        let mut reasoning_deltas = Vec::new();
        let mut usage = LlmUsage::default();
        let mut provider_usage = None;
        let mut execution_evidence = None;
        let mut output_parts = Vec::new();
        let mut tool_call_parts = Vec::new();
        let mut finish_event = None;
        GoogleOAuthProvider::process_sse_event_with_text_parts(
            &streaming_event.to_string(),
            crate::support::SseTextPartSink {
                full: &mut full,
                text_deltas: &mut text_deltas,
                reasoning_deltas: &mut reasoning_deltas,
                usage: &mut usage,
                provider_usage: &mut provider_usage,
                execution_evidence: &mut execution_evidence,
                tool_call_parts: Some(&mut tool_call_parts),
                output_parts: Some(&mut output_parts),
                finish_event: &mut finish_event,
            },
            Some("gemini-test"),
        )
        .expect("streaming signature-only thought parses");
        assert!(
            text_deltas.is_empty() && reasoning_deltas.is_empty(),
            "a text-less thought part must not emit visible deltas"
        );
        let batch_parts = GoogleOAuthProvider::response_parts_from_value(
            &json!({"candidates":[{
                "content":{"parts":[thought_part.clone()]},
                "finishReason":"STOP"
            }]}),
            Some("gemini-test"),
        );

        for (path, parts) in [("streaming", output_parts), ("batch", batch_parts)] {
            assert!(
                matches!(
                    parts.as_slice(),
                    [LlmOutputPart::Reasoning {
                        text,
                        replay: Some(replay),
                    }] if text.is_empty()
                        && replay.signature.as_deref() == Some(signature)
                        && replay.origin.as_ref()
                            == Some(&GoogleOAuthProvider::route_identity_for_model("gemini-test"))
                ),
                "{path} parser must retain the text-less thoughtSignature, got {parts:?}"
            );
            let request = crate::tests::next_request_from_response_parts(&parts);
            assert_eq!(
                request.pointer("/request/contents/0/parts/0/thoughtSignature"),
                Some(&json!(signature)),
                "{path} replay projection must restore the text-less thoughtSignature"
            );
            assert_eq!(
                request.pointer("/request/contents/0/parts/0/thought"),
                Some(&json!(true)),
                "{path} replay projection must keep the part flagged as a thought"
            );
            assert_eq!(
                request.pointer("/request/contents/0/parts/0/text"),
                Some(&json!(" ")),
                "{path} replay projection must emit the empty-text placeholder"
            );
        }
    }
}

/// A part can carry a `functionCall` and be flagged `thought: true` with a
/// signature and no text. The signature belongs to the ToolCall replay — the
/// text gate must not also mint an empty `Reasoning` part, or the same
/// signature would be replayed twice.
#[test]
fn google_signature_on_function_call_thought_part_yields_only_the_tool_call() {
    let signature = "ZnVuY3Rpb24tY2FsbC10aG91Z2h0LXNpZw==";
    let part = json!({
        "functionCall": { "id": "call-1", "name": "lookup", "args": { "q": "x" } },
        "thought": true,
        "thoughtSignature": signature
    });
    let streaming_event = json!({"response":{"candidates":[{
        "content":{"parts":[part.clone()]},
        "finishReason":"STOP"
    }]}});
    let mut full = String::new();
    let mut text_deltas = Vec::new();
    let mut reasoning_deltas = Vec::new();
    let mut usage = LlmUsage::default();
    let mut provider_usage = None;
    let mut execution_evidence = None;
    let mut output_parts = Vec::new();
    let mut tool_call_parts = Vec::new();
    let mut finish_event = None;
    GoogleOAuthProvider::process_sse_event_with_text_parts(
        &streaming_event.to_string(),
        crate::support::SseTextPartSink {
            full: &mut full,
            text_deltas: &mut text_deltas,
            reasoning_deltas: &mut reasoning_deltas,
            usage: &mut usage,
            provider_usage: &mut provider_usage,
            execution_evidence: &mut execution_evidence,
            tool_call_parts: Some(&mut tool_call_parts),
            output_parts: Some(&mut output_parts),
            finish_event: &mut finish_event,
        },
        Some("gemini-test"),
    )
    .expect("streaming functionCall thought part parses");
    // The streaming sink splits tool calls from the other output parts; both
    // sinks together are what the driver emits for the turn, so a duplicate
    // empty Reasoning part shows up here.
    let streaming_parts = [output_parts, tool_call_parts].concat();
    let batch_parts = GoogleOAuthProvider::response_parts_from_value(
        &json!({"candidates":[{
            "content":{"parts":[part]},
            "finishReason":"STOP"
        }]}),
        Some("gemini-test"),
    );

    for (path, parts) in [("streaming", streaming_parts), ("batch", batch_parts)] {
        assert!(
            matches!(
                parts.as_slice(),
                [LlmOutputPart::ToolCall {
                    replay: Some(replay),
                    ..
                }] if replay.opaque.as_deref() == Some(signature)
            ),
            "{path} parser must yield only the signed ToolCall, got {parts:?}"
        );
    }
}
