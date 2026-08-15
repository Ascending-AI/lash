use std::sync::Arc;

use lash_core::llm::types::{
    LlmContentBlock, LlmMessage, LlmRequest, LlmRequestScope, LlmRole, LlmToolChoice, LlmToolSpec,
    ProviderReasoningReplay, ProviderRouteIdentity,
};
use lash_core::provider::{ModelCapability, Provider};
use lash_sansio::sync::MutexExt;

use crate::AnthropicProvider;

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
        Err(lash_core::facade_support::LlmTransportError::new(
            "capture complete",
        ))
    }
}

fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        model: "claude-sonnet-4-6".to_string(),
        messages,
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: ModelCapability::default(),
        scope: LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None,
        generation: Default::default(),
        provider_trace: None,
    }
}

#[tokio::test]
async fn raw_provider_complete_drops_foreign_and_unstamped_replay_from_anthropic_wire() {
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![
            LlmContentBlock::Reasoning {
                text: "foreign summary".to_string(),
                replay: Some(ProviderReasoningReplay {
                    signature: Some("foreign-anthropic-wire-secret".to_string()),
                    origin: Some(ProviderRouteIdentity::for_endpoint(
                        "google_oauth",
                        "https://cloudcode-pa.googleapis.com/v1internal",
                        "gemini-2.5-pro",
                    )),
                    ..Default::default()
                }),
            },
            LlmContentBlock::Reasoning {
                text: "unstamped summary".to_string(),
                replay: Some(ProviderReasoningReplay {
                    signature: Some("unstamped-anthropic-wire-secret".to_string()),
                    ..Default::default()
                }),
            },
        ],
    )]);
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut provider = AnthropicProvider::new("key").with_transport(Arc::new(CapturingTransport {
        bodies: Arc::clone(&bodies),
    }));

    Provider::complete(&mut provider, req)
        .await
        .expect_err("capture transport stops after observing the wire body");
    let wire = bodies.lock_recover().join("\n");
    assert!(wire.contains("foreign summary"));
    assert!(wire.contains("unstamped summary"));
    assert!(!wire.contains("foreign-anthropic-wire-secret"));
    assert!(!wire.contains("unstamped-anthropic-wire-secret"));
}
