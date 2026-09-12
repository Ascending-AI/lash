use super::*;

#[test]
fn fig1123_responses_body_composes_effort_with_native_current_turn_retention() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = reasoning_capability();
    *req.model_capability.reasoning_retention = ReasoningRetentionPolicy {
        capability: Some(ReasoningRetentionCapability::OpenAiContext {
            supported: vec![OpenAiReasoningContext::CurrentTurn],
        }),
        selection: ReasoningRetentionSelection::OpenAiContext {
            context: OpenAiReasoningContext::CurrentTurn,
        },
    };

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert_eq!(
        body["reasoning"],
        json!({ "effort": "high", "context": "current_turn" })
    );
}

#[tokio::test]
async fn fig1123_unsupported_retention_is_refused_before_network() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = OpenAiProvider::new("key").with_transport(transport.clone());
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    *req.model_capability.reasoning_retention = ReasoningRetentionPolicy {
        capability: Some(ReasoningRetentionCapability::AnthropicClearThinking),
        selection: ReasoningRetentionSelection::AnthropicClearThinking {
            keep: lash_core::llm::types::AnthropicThinkingRetention::All,
        },
    };

    let error = provider
        .complete(req)
        .await
        .expect_err("cross-provider retention must be refused");

    assert_eq!(error.kind, ProviderFailureKind::Unsupported);
    assert_eq!(
        error.code.as_deref(),
        Some("unsupported_reasoning_retention")
    );
    assert!(!error.is_retryable());
    assert!(transport.requests.lock_recover().is_empty());
}

#[test]
fn fig1123_chat_fallback_evicts_whole_genuine_user_segments() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::User, "old input").with_user_segment_start(),
        LlmMessage::text(LlmRole::Assistant, "old answer"),
        LlmMessage::text(LlmRole::User, "synthetic observation"),
        LlmMessage::text(LlmRole::User, "new input").with_user_segment_start(),
        LlmMessage::text(LlmRole::Assistant, "new answer"),
    ]);
    *req.model_capability.reasoning_retention = ReasoningRetentionPolicy {
        capability: Some(ReasoningRetentionCapability::ClientSideUserSegments),
        selection: ReasoningRetentionSelection::ClientSideUserSegments {
            max_segments: NonZeroUsize::new(1).unwrap(),
        },
    };

    let body = openrouter_provider()
        .build_chat_request_body(&req, false)
        .expect("fallback request");

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"][0]["text"], "new input");
    assert_eq!(messages[1]["content"][0]["text"], "new answer");
}
