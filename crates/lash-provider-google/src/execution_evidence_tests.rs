use std::sync::Arc;

use lash_core::llm::types::LlmEventSender;
use lash_core::provider::StreamTermination;
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport};
use serde_json::json;

use crate::GoogleOAuthProvider;

#[derive(Debug)]
struct StaticResponseTransport(String);

#[async_trait::async_trait]
impl LlmHttpTransport for StaticResponseTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, lash_core::facade_support::LlmTransportError> {
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: LlmHttpBody::buffered(self.0.clone()),
        })
    }
}

#[derive(Debug)]
struct StaticSseTransport(String);

#[async_trait::async_trait]
impl LlmHttpTransport for StaticSseTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<LlmHttpResponse, lash_core::facade_support::LlmTransportError> {
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/event-stream".to_string())],
            body: LlmHttpBody::buffered(self.0.clone()),
        })
    }
}

#[tokio::test]
async fn google_non_streaming_response_carries_provider_execution_evidence() {
    let body = json!({
        "responseId": "google-batch-1",
        "modelVersion": "gemini-batch-served",
        "candidates": [{
            "finishReason": "STOP",
            "content": {"parts": [{"text": "batch done"}]}
        }],
        "usageMetadata": {
            "promptTokenCount": 4,
            "candidatesTokenCount": 2,
            "thoughtsTokenCount": 0
        }
    })
    .to_string();
    let provider = GoogleOAuthProvider::new(
        "access",
        "refresh",
        0,
        crate::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_transport(Arc::new(StaticResponseTransport(body)));
    let response = provider
        .execute_request(
            "access",
            json!({ "model": "gemini-requested" }),
            None,
            None,
            StreamTermination::RequireTerminalEvidence,
            None,
        )
        .await
        .expect("batch response completes");
    let evidence = response
        .execution_evidence
        .expect("Google batch response has typed provider evidence");
    assert_eq!(
        evidence.provider_response_id.as_deref(),
        Some("google-batch-1")
    );
    assert_eq!(
        evidence.served_model.as_deref(),
        Some("gemini-batch-served")
    );
    assert_eq!(evidence.provider_finish_reason.as_deref(), Some("STOP"));
    assert_eq!(evidence.reasoning_output_tokens, Some(0));
}

#[tokio::test]
async fn google_stream_evidence_is_monotonic_and_rejects_identity_drift() {
    let monotonic = GoogleOAuthProvider::new("access", "refresh", 0, crate::GoogleOAuthClient { id: "oauth-client-id".into(), secret: "oauth-client-secret".into() }).with_transport(Arc::new(
        StaticSseTransport(
            "data: {\"response\":{\"responseId\":\"google-stable\",\"modelVersion\":\"gemini-stable\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done\"}]}}],\"usageMetadata\":{\"thoughtsTokenCount\":7}}}\n\ndata: {\"response\":{\"responseId\":\"google-stable\",\"modelVersion\":\"gemini-stable\",\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"thoughtsTokenCount\":0}}}\n\n"
                .to_string(),
        ),
    ));
    let response = monotonic
        .execute_request(
            "access",
            json!({ "model": "gemini-test" }),
            Some(LlmEventSender::new(|_| {})),
            None,
            StreamTermination::RequireTerminalEvidence,
            None,
        )
        .await
        .expect("a cumulative trailing zero must not erase a positive count");
    assert_eq!(
        response
            .execution_evidence
            .expect("stream evidence")
            .reasoning_output_tokens,
        Some(7)
    );

    let drifting = GoogleOAuthProvider::new("access", "refresh", 0, crate::GoogleOAuthClient { id: "oauth-client-id".into(), secret: "oauth-client-secret".into() }).with_transport(Arc::new(
        StaticSseTransport(
            "data: {\"response\":{\"responseId\":\"google-first\",\"modelVersion\":\"gemini-first\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}}\n\ndata: {\"response\":{\"responseId\":\"google-second\",\"modelVersion\":\"gemini-second\",\"candidates\":[{\"finishReason\":\"STOP\"}]}}\n\n"
                .to_string(),
        ),
    ));
    let error = drifting
        .execute_request(
            "access",
            json!({ "model": "gemini-test" }),
            Some(LlmEventSender::new(|_| {})),
            None,
            StreamTermination::RequireTerminalEvidence,
            None,
        )
        .await
        .expect_err("one stream cannot change provider response identity");
    assert!(
        error.message.contains("served_model") || error.message.contains("provider_response_id"),
        "identity-drift error must name the conflicting field: {error:?}"
    );
}
