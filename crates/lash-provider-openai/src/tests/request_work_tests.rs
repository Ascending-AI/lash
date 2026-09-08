use super::*;
use crate::request_work::needs_blocking;

#[test]
fn request_work_budget_covers_text_inline_and_resolved_payloads() {
    let small = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    assert!(!needs_blocking(&small));
    let text = request(vec![LlmMessage::text(LlmRole::User, "x".repeat(64 * 1024))]);
    assert!(needs_blocking(&text));
    let inline = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment {
            source: Box::new(AttachmentSource::inline(
                lash_core::MediaType::parse("image/png").unwrap(),
                vec![0; 64 * 1024],
            )),
        }],
    )]);
    assert!(needs_blocking(&inline));
    let mut stored = small;
    stored.resolved_stored.insert(
        lash_core::AttachmentId::parse("stored").unwrap(),
        vec![0; 64 * 1024],
    );
    assert!(needs_blocking(&stored));
}

#[derive(Debug)]
struct LargeErrorTransport;

#[async_trait]
impl LlmHttpTransport for LargeErrorTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        assert!(request.body.len() > 64 * 1024);
        let diagnostic = request.body_for_error.unwrap();
        assert!(diagnostic.len() < 4200);
        assert!(diagnostic.ends_with(&format!("[body bytes: {}]", request.body.len())));
        // Put the authoritative code beyond the excerpt: classification must
        // inspect the original response, not its diagnostic prefix.
        let text = serde_json::to_string(&json!({
            "echo": "x".repeat(80 * 1024),
            "error": {"code": "context_length_exceeded", "message": "too large"},
        }))
        .unwrap();
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 400,
            headers: Vec::new(),
            body: LlmHttpBody::buffered(text),
        })
    }
}

#[tokio::test]
async fn both_endpoints_bound_http_error_bodies_and_preserve_late_error_codes() {
    for endpoint in [
        CompletionEndpoint::Responses,
        CompletionEndpoint::ChatCompletions,
    ] {
        let mut provider = OpenAiCompatibleProvider::new("key", "https://example.test/v1")
            .with_transport(Arc::new(LargeErrorTransport));
        let req = request(vec![LlmMessage::text(LlmRole::User, "x".repeat(80 * 1024))]);
        let error = crate::driver::complete(&mut provider, req, endpoint)
            .await
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("context_length_exceeded"));
        assert_eq!(error.terminal_reason, LlmTerminalReason::ContextOverflow);
        for diagnostic in [error.raw.as_deref(), error.request_body.as_deref()] {
            let diagnostic = diagnostic.unwrap();
            assert!(diagnostic.len() < 4200);
            assert!(diagnostic.contains("[body bytes: "));
        }
    }
}

#[test]
fn bounded_error_projection_preserves_retry_delay_and_message_after_large_echo() {
    let value = json!({
        "echo": "x".repeat(80 * 1024),
        "error": {"message": "try again", "details": [{"retryDelay": "1.5s"}]},
    });
    let metadata = crate::request_work::error_metadata(&value).unwrap();
    assert!(metadata.len() < 100);
    let failure = http_error_envelope("request failed", 429, Vec::new(), metadata, None);
    assert_eq!(failure.message, "request failed: try again");
    assert_eq!(
        failure.retry_verdict,
        TransportRetryVerdict::RetryableThrottle {
            retry_after: Some(std::time::Duration::from_millis(1500)),
        }
    );
}
