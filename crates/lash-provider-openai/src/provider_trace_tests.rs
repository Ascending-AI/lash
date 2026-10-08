use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use crate::codex::ws_testing::{ScriptedWsAction, spawn_scripted_websocket};
use crate::{CodexProvider, OpenAiCompatibleProvider};
use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{
    LlmMessage, LlmProviderTraceEvent, LlmProviderTraceSender, LlmRequest, LlmRequestScope,
    LlmRole, LlmToolChoice,
};
use lash_core::provider::Provider;
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};

const SECRET_SENTINEL: &str = "sk-super-secret-do-not-log";

#[derive(Debug)]
struct RecordingTransport {
    requests: Mutex<Vec<LlmHttpRequest>>,
    status: u16,
}

impl RecordingTransport {
    fn success() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            status: 200,
        }
    }

    fn error() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            status: 500,
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
            r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#
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

fn assert_auth_material_absent_from_error(error: &LlmTransportError) {
    let captured = format!("{error:?}");
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

fn traced_request(
    events: &Arc<Mutex<Vec<LlmProviderTraceEvent>>>,
) -> (LlmRequest, Arc<Mutex<Vec<LlmProviderTraceEvent>>>) {
    let mut req = request();
    let event_sink = Arc::clone(events);
    req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));
    (req, Arc::clone(events))
}

fn provider_request_event(
    events: &Arc<Mutex<Vec<LlmProviderTraceEvent>>>,
) -> LlmProviderTraceEvent {
    events
        .lock_recover()
        .iter()
        .find(|event| event.request_endpoint().is_some())
        .cloned()
        .expect("provider request trace")
}

fn request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("test-model".to_string())
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::text(
            LlmRole::User,
            format!("large prompt: {}", "x".repeat(3_000)),
        )],

        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: crate::attachment_test_acceptance(),
        generation: Default::default(),
        scope: LlmRequestScope::new("session", "frame", "request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

#[tokio::test]
async fn extended_provider_trace_captures_exact_serialized_chat_body() {
    let transport = Arc::new(RecordingTransport::success());
    let mut provider = OpenAiCompatibleProvider::new(SECRET_SENTINEL, "https://example.test/v1")
        .with_transport(transport.clone());
    let mut req = request();

    let events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let event_sink = Arc::clone(&events);
    req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));

    let response = provider
        .complete(
            req,
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("completion succeeds");

    let request_event = events
        .lock_recover()
        .iter()
        .find(|event| event.request_endpoint().is_some())
        .cloned()
        .expect("provider request trace");
    assert_eq!(request_event.provider, "openai_compatible");
    assert_eq!(request_event.request_endpoint(), Some("chat/completions"));
    assert!(request_event.raw.len() > 2_048);
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

    let untraced_transport = Arc::new(RecordingTransport::success());
    let mut untraced_provider =
        OpenAiCompatibleProvider::new(SECRET_SENTINEL, "https://example.test/v1")
            .with_transport(untraced_transport.clone());
    untraced_provider
        .complete(
            request(),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("untraced completion succeeds");
    let untraced_body = {
        let untraced_requests = untraced_transport.requests.lock_recover();
        untraced_requests[0].body.clone()
    };
    assert_eq!(untraced_body, traced_body);

    let error_transport = Arc::new(RecordingTransport::error());
    let mut error_provider =
        OpenAiCompatibleProvider::new(SECRET_SENTINEL, "https://example.test/v1")
            .with_transport(error_transport);
    let mut error_req = request();
    let error_events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let error_event_sink = Arc::clone(&error_events);
    error_req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
        error_event_sink.lock_recover().push(event);
    }));

    let error = error_provider
        .complete(
            error_req,
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
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
        error.request_body.as_ref().map(|body| body.as_str()),
        Some(error_event.raw.as_str())
    );
    assert_auth_material_absent_from_error(&error);
}

#[tokio::test]
async fn codex_sse_provider_trace_captures_exact_serialized_request_body() {
    let transport = Arc::new(RecordingTransport::error());
    let mut provider = CodexProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new(SECRET_SENTINEL),
    ))
    .force_sse_transport()
    .with_http_transport(transport.clone());
    let events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let (req, events) = traced_request(&events);

    let error = provider
        .complete(
            req,
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("provider error is returned");

    let request_event = provider_request_event(&events);
    assert_eq!(request_event.provider, "codex");
    assert_eq!(request_event.request_endpoint(), Some("responses"));
    assert_auth_material_absent(&request_event);
    assert_auth_material_absent_from_error(&error);

    let observed_body = {
        let requests = transport.requests.lock_recover();
        assert_eq!(requests.len(), 1);
        requests[0].body.clone()
    };
    assert_eq!(request_event.raw.as_bytes(), observed_body.as_ref());
    assert_eq!(
        error.request_body.as_ref().map(|body| body.as_str()),
        Some(request_event.raw.as_str())
    );
}

#[tokio::test]
async fn codex_websocket_provider_trace_captures_exact_serialized_request_body() {
    let server = spawn_scripted_websocket(vec![ScriptedWsAction::Error {
        message: "provider unavailable",
    }])
    .await;
    let mut provider = CodexProvider::new(std::sync::Arc::new(
        lash_core::provider::ProviderToken::new(SECRET_SENTINEL),
    ))
    .with_endpoint_urls("http://unused.test/codex/responses", server.url.clone())
    .force_websocket_transport();
    let events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let (req, events) = traced_request(&events);

    let error = provider
        .complete(
            req,
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("provider error is returned");

    let request_event = provider_request_event(&events);
    assert_eq!(request_event.provider, "codex");
    assert_eq!(request_event.request_endpoint(), Some("responses"));
    assert_auth_material_absent(&request_event);
    assert_auth_material_absent_from_error(&error);

    // The frame is the traced body inside its `response.create` envelope.
    let observed_bodies = server.captured_raw();
    assert_eq!(observed_bodies.len(), 1);
    let mut frame: serde_json::Value =
        serde_json::from_slice(&observed_bodies[0]).expect("the frame is JSON");
    assert_eq!(
        frame.as_object_mut().and_then(|frame| frame.remove("type")),
        Some(serde_json::json!("response.create"))
    );
    assert_eq!(
        frame,
        serde_json::from_str::<serde_json::Value>(&request_event.raw).expect("the trace is JSON")
    );
    assert_eq!(
        error.request_body.as_ref().map(|body| body.as_str()),
        Some(request_event.raw.as_str())
    );
}
