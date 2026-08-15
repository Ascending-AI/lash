use std::sync::Arc;

use lash_core::llm::types::{
    LlmContentBlock, LlmEventSender, LlmMessage, LlmRequest, LlmRole, LlmToolChoice, LlmToolSpec,
    ProviderRouteIdentity,
};
use lash_core::provider::{ModelCapability, Provider};
use lash_sansio::sync::MutexExt;

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
