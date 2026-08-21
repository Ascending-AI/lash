use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use crate::GoogleOAuthProvider;
use async_trait::async_trait;
use lash_core::facade_support::LlmTransportError;
use lash_core::llm::types::{LlmProviderTraceEvent, LlmProviderTraceSender};
use lash_core::provider::StreamTermination;
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use serde_json::json;

const SECRET_SENTINEL: &str = "sk-super-secret-do-not-log";
const CLIENT_SECRET_SENTINEL: &str = "oauth-super-secret-do-not-log";

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
            r#"{"response":{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"done"}]}}]}}"#
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
        for secret in [SECRET_SENTINEL, CLIENT_SECRET_SENTINEL] {
            assert!(!captured.contains(secret), "secret leaked: {captured}");
        }
        let lowercase = captured.to_ascii_lowercase();
        assert!(
            !lowercase.contains("authorization") && !lowercase.contains("bearer"),
            "authorization material leaked: {captured}"
        );
    }
}

#[tokio::test]
async fn extended_provider_trace_captures_exact_serialized_google_body_without_auth() {
    let request = json!({
        "model": "gemini-test",
        "contents": [{"role": "user", "parts": [{"text": "x".repeat(3_000)}]}],
    });
    let transport = Arc::new(RecordingTransport::new(200));
    let provider = GoogleOAuthProvider::new(
        SECRET_SENTINEL,
        "refresh-secret",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: CLIENT_SECRET_SENTINEL.into(),
        },
    )
    .with_transport(transport.clone());
    let events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let event_sink = Arc::clone(&events);

    let response = provider
        .execute_request(
            SECRET_SENTINEL,
            request.clone(),
            None,
            Some(LlmProviderTraceSender::new(move |event| {
                event_sink.lock_recover().push(event);
            })),
            StreamTermination::EofTolerated,
            None,
        )
        .await
        .expect("completion succeeds");
    let request_event = events
        .lock_recover()
        .iter()
        .find(|event| event.request_endpoint().is_some())
        .cloned()
        .expect("provider request trace");
    assert_eq!(request_event.provider, "google");
    assert_eq!(request_event.request_endpoint(), Some("generateContent"));
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
    let error_provider = GoogleOAuthProvider::new(
        SECRET_SENTINEL,
        "refresh-secret",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: CLIENT_SECRET_SENTINEL.into(),
        },
    )
    .with_transport(error_transport);
    let error_events = Arc::new(Mutex::new(Vec::<LlmProviderTraceEvent>::new()));
    let error_event_sink = Arc::clone(&error_events);
    let error = error_provider
        .execute_request(
            SECRET_SENTINEL,
            request,
            None,
            Some(LlmProviderTraceSender::new(move |event| {
                error_event_sink.lock_recover().push(event);
            })),
            StreamTermination::EofTolerated,
            None,
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
    let error_capture = format!("{error:?}");
    assert!(!error_capture.contains(SECRET_SENTINEL));
    assert!(!error_capture.contains(CLIENT_SECRET_SENTINEL));
    let lowercase = error_capture.to_ascii_lowercase();
    assert!(!lowercase.contains("authorization"));
    assert!(!lowercase.contains("bearer"));
}
