use std::sync::Arc;

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
    let provider = GoogleOAuthProvider::new("access", "refresh", 0)
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
