use super::*;
use async_trait::async_trait;
use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_core::llm::types::{LlmMessage, LlmRole, ProtocolPosition};
use lash_core::provider::ProviderHandle;
use lash_llm_transport::{
    LlmByteStream, LlmHttpBody, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport,
};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn codex_error_summary_uses_top_level_detail() {
    let summary =
        CodexProvider::codex_error_summary(400, r#"{"detail":"Unsupported parameter: foo"}"#);
    assert_eq!(
        summary.as_deref(),
        Some("Codex request failed with 400: Unsupported parameter: foo")
    );
}

#[derive(Debug)]
struct TimeoutBodyStream;

#[async_trait]
impl LlmByteStream for TimeoutBodyStream {
    async fn next_chunk(&mut self) -> Result<Option<bytes::Bytes>, LlmTransportError> {
        Err(LlmTransportError::response_read("injected body timeout")
            .with_kind(ProviderFailureKind::Timeout)
            .with_retry_verdict(TransportRetryVerdict::NotRetryable)
            .with_code("body_timeout"))
    }
}

#[derive(Debug)]
struct NonSseBodyReadFailureTransport;

#[async_trait]
impl LlmHttpTransport for NonSseBodyReadFailureTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<Duration>,
    ) -> Result<LlmHttpResponse, LlmTransportError> {
        Ok(LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: LlmHttpBody::streamed(TimeoutBodyStream),
        })
    }
}

#[tokio::test]
async fn codex_non_sse_body_read_failure_preserves_observed_response_evidence() {
    let provider = CodexProvider::new("access", "refresh", 0)
        .force_sse_transport()
        .with_http_transport(Arc::new(NonSseBodyReadFailureTransport));
    let mut handle = ProviderHandle::new(provider.into_components());

    let failure = handle
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect_err("the injected body timeout must fail the Codex attempt");

    assert_eq!(failure.call_record.attempts.len(), 1);
    let attempt = &failure.call_record.attempts[0];
    assert_eq!(
        attempt.protocol_position,
        ProtocolPosition::ResponseObserved
    );
    let recorded = attempt.error.as_ref().expect("failed attempt error");
    assert_eq!(recorded.http_status, Some(200));
    assert_eq!(recorded.class, ProviderFailureKind::Timeout.code());
    assert_eq!(recorded.provider_code.as_deref(), Some("body_timeout"));
    assert_eq!(failure.status, Some(200));
    assert_eq!(failure.kind, ProviderFailureKind::Timeout);
    assert_eq!(failure.retry_verdict, TransportRetryVerdict::NotRetryable);
    assert_eq!(failure.code.as_deref(), Some("body_timeout"));
}
