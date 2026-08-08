use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use crate::AnthropicProvider;
use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{
    LlmMessage, LlmProviderTraceEvent, LlmProviderTraceSender, LlmRequest, LlmRequestScope,
    LlmRole, LlmToolChoice,
};
use lash_core::provider::{ModelCapability, Provider};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};

const SECRET_SENTINEL: &str = "sk-super-secret-do-not-log";

#[derive(Debug)]
struct RecordingTransport {
    requests: Mutex<Vec<LlmHttpRequest>>,
    status: u16,
}

impl RecordingTransport {
    fn new(status: u16) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            status,
        }
    }
}

#[async_trait]
impl LlmHttpTransport for RecordingTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        let body = if self.status == 200 {
            concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            )
        } else {
            r#"{"error":{"message":"provider unavailable"}}"#
        };
        Ok(LlmHttpResponse {
            status: self.status,
            headers: Vec::new(),
            body: LlmHttpBody::buffered(body),
        })
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "claude-test".to_string(),
        messages: vec![LlmMessage::text(
            LlmRole::User,
            format!("large prompt: {}", "x".repeat(3_000)),
        )],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: ModelCapability::default(),
        generation: Default::default(),
        scope: LlmRequestScope::new("session", "frame", "request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

fn assert_auth_material_absent(event: &LlmProviderTraceEvent) {
    let body_json: serde_json::Value =
        serde_json::from_str(&event.raw).expect("traced body is JSON");
    for captured in [
        event.raw.clone(),
        body_json.to_string(),
        format!("{event:?}"),
    ] {
        assert!(
            !captured.contains(SECRET_SENTINEL),
            "secret leaked: {captured}"
        );
        let lowercase = captured.to_ascii_lowercase();
        assert!(
            !lowercase.contains("authorization") && !lowercase.contains("bearer"),
            "authorization material leaked: {captured}"
        );
    }
}

#[tokio::test]
async fn extended_provider_trace_captures_exact_serialized_anthropic_body_without_auth() {
    let transport = Arc::new(RecordingTransport::new(200));
    let mut provider = AnthropicProvider::new(SECRET_SENTINEL).with_transport(transport.clone());
    let mut req = request();
    let events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let event_sink = Arc::clone(&events);
    req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));

    let response = provider.complete(req).await.expect("completion succeeds");
    let request_event = events
        .lock_recover()
        .iter()
        .find(|event| event.request_endpoint().is_some())
        .cloned()
        .expect("provider request trace");
    assert_eq!(request_event.provider, "anthropic");
    assert_eq!(request_event.request_endpoint(), Some("messages"));
    assert_auth_material_absent(&request_event);

    let traced_body = {
        let requests = transport.requests.lock_recover();
        assert_eq!(requests.len(), 1);
        requests[0].body.clone()
    };
    assert_eq!(traced_body.as_ref(), request_event.raw.as_bytes());
    assert_eq!(
        response.request_body.as_deref(),
        Some(request_event.raw.as_str())
    );

    let error_transport = Arc::new(RecordingTransport::new(500));
    let mut error_provider =
        AnthropicProvider::new(SECRET_SENTINEL).with_transport(error_transport);
    let mut error_req = request();
    let error_events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let error_event_sink = Arc::clone(&error_events);
    error_req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
        error_event_sink.lock_recover().push(event);
    }));

    let error = error_provider
        .complete(error_req)
        .await
        .expect_err("provider error is returned");
    let error_event = error_events
        .lock_recover()
        .iter()
        .find(|event| event.request_endpoint().is_some())
        .cloned()
        .expect("error-path provider request trace");
    assert_auth_material_absent(&error_event);
    assert_eq!(
        error.request_body.as_deref(),
        Some(error_event.raw.as_str())
    );
    let error_capture = format!("{error:?}");
    assert!(!error_capture.contains(SECRET_SENTINEL));
    let lowercase = error_capture.to_ascii_lowercase();
    assert!(!lowercase.contains("authorization"));
    assert!(!lowercase.contains("bearer"));
}
