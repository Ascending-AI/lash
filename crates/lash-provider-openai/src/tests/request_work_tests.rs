use super::*;

#[test]
fn bounded_error_projection_preserves_retry_delay_and_message_after_large_echo() {
    let value = json!({
        "echo": "x".repeat(80 * 1024),
        "error": {"message": "try again", "details": [{"retryDelay": "1.5s"}]},
    });
    let metadata = crate::RequestDiagnosticLimits { excerpt_bytes: 1 }
        .error_metadata(&value)
        .unwrap();
    assert!(metadata.len() < 100);
    let failure = http_error_envelope("request failed", 429, Vec::new(), metadata, None);
    assert!(
        failure
            .message
            .starts_with("request failed: t\n[body bytes: 9]")
    );
    assert_eq!(
        failure.retry_verdict,
        TransportRetryVerdict::RetryableThrottle {
            retry_after: Some(std::time::Duration::from_millis(1500)),
        }
    );
}

/// A non-default facade policy reaches buffered and streaming provider failures.
#[tokio::test]
async fn facade_provider_error_excerpt_policy_reaches_the_transport() {
    use lash::provider::{NoSlotDeliveries, Provider as _};
    for (status, headers, body, stream) in [
        (
            429,
            vec![],
            r#"{"error":{"message":"oversized diagnostic"}}"#,
            false,
        ),
        (200, vec![], "invalid response body", false),
        (
            200,
            vec![("content-type".into(), "text/event-stream".into())],
            "data: invalid SSE body\n\n",
            true,
        ),
    ] {
        let transport = Arc::new(super::ScriptedHttpTransport {
            responses: std::sync::Mutex::new(VecDeque::from([(status, headers, body)])),
            calls: Default::default(),
        });
        let mut provider =
            lash::openai::OpenAiCompatibleProvider::new("key", "https://example.test/v1")
                .with_diagnostic_limits(lash::openai::RequestDiagnosticLimits { excerpt_bytes: 1 })
                .with_transport(transport);
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "test")]);
        if stream {
            req.stream_events = Some(lash_core::llm::types::LlmEventSender::new(|_| {}));
        }
        let failure = provider.complete(req, &NoSlotDeliveries).await.unwrap_err();
        let raw = failure.raw.as_deref().unwrap();
        assert!(raw.lines().next().unwrap().len() <= 1, "{raw}");
        assert!(raw.contains("[body bytes:"), "{raw}");
    }
}
