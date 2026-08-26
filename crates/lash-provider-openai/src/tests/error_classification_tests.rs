use super::*;

async fn typed_http_failure(body: &'static str) -> lash_core::provider::ProviderCompletionError {
    let transport = Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from([(400, Vec::new(), body)])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let provider = OpenAiProvider::new("key").with_transport(transport);
    let mut handle = ProviderHandle::new(provider.into_components());

    handle
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect_err("typed HTTP failure must not succeed")
}

#[tokio::test]
async fn typed_context_length_error_is_authoritative_at_provider_handle() {
    let failure = typed_http_failure(
        r#"{"error":{"code":"context_length_exceeded","message":"input exceeds this model's token limit","type":"invalid_request_error"}}"#,
    )
    .await;

    assert_eq!(failure.code.as_deref(), Some("context_length_exceeded"));
    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ContextOverflow);
    assert!(!failure.is_retryable());
}

#[tokio::test]
async fn typed_validation_error_is_not_overridden_by_user_text_echo() {
    let failure = typed_http_failure(
        r#"{"error":{"code":"invalid_request_error","message":"user input said: context length is a useful phrase","type":"invalid_request_error"}}"#,
    )
    .await;

    assert_eq!(failure.code.as_deref(), Some("invalid_request_error"));
    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
    assert!(!failure.is_retryable());
}

#[tokio::test]
async fn typed_hard_quota_code_is_authoritative_at_provider_handle() {
    let failure = typed_http_failure(
        r#"{"error":{"code":"insufficient_quota","message":"billing quota exhausted","type":"insufficient_quota"}}"#,
    )
    .await;

    assert_eq!(failure.code.as_deref(), Some("insufficient_quota"));
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
    assert!(!failure.is_retryable());
}

#[tokio::test]
async fn typed_content_filter_code_is_authoritative_at_provider_handle() {
    let failure = typed_http_failure(
        r#"{"error":{"code":"content_filter","message":"request blocked","type":"content_filter"}}"#,
    )
    .await;

    assert_eq!(failure.code.as_deref(), Some("content_filter"));
    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ContentFilter);
    assert!(!failure.is_retryable());
}

#[tokio::test]
async fn typed_unsupported_model_code_is_authoritative_at_provider_handle() {
    let failure = typed_http_failure(
        r#"{"error":{"code":"model_not_found","message":"unknown model","type":"model_not_found"}}"#,
    )
    .await;

    assert_eq!(failure.code.as_deref(), Some("model_not_found"));
    assert_eq!(failure.kind, ProviderFailureKind::Unsupported);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
    assert!(!failure.is_retryable());
}
