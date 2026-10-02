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

#[test]
fn sse_error_event_top_level_code_is_classified() {
    let mut state = ResponsesStreamState::default();
    assert!(!state.output_started());

    let err = OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"error","code":"server_error","message":"failed","param":null,"sequence_number":1}"#,
        &mut state,
        None,
    )
    .expect_err("an in-band error event must fail the call");

    assert_eq!(err.retry_verdict, TransportRetryVerdict::RetryableTransient);
    assert_eq!(
        err.code.as_ref().map(|code| code.to_string()),
        Some("provider:server_error".to_string())
    );
}

#[tokio::test]
async fn typed_context_length_error_is_authoritative_at_provider_handle() {
    let failure = typed_http_failure(
        r#"{"error":{"code":"context_length_exceeded","message":"input exceeds this model's token limit","type":"invalid_request_error"}}"#,
    )
    .await;

    assert_eq!(
        failure.code.as_ref().map(|code| code.to_string()),
        Some("provider:context_length_exceeded".to_string())
    );
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

    assert_eq!(
        failure.code.as_ref().map(|code| code.to_string()),
        Some("provider:invalid_request_error".to_string())
    );
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

    assert_eq!(
        failure.code.as_ref().map(|code| code.to_string()),
        Some("provider:insufficient_quota".to_string())
    );
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

    assert_eq!(
        failure.code.as_ref().map(|code| code.to_string()),
        Some("provider:content_filter".to_string())
    );
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

    assert_eq!(
        failure.code.as_ref().map(|code| code.to_string()),
        Some("provider:model_not_found".to_string())
    );
    assert_eq!(failure.kind, ProviderFailureKind::Unsupported);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
    assert!(!failure.is_retryable());
}
