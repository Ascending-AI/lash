use super::*;

#[test]
fn default_failure_classifier_classifies_429_as_retryable_throttle() {
    let classifier = DefaultProviderFailureClassifier;
    let failure = classifier.classify(
        LlmTransportError::new("Rate limit reached for requests")
            .with_http_status(429)
            .with_retry_verdict(TransportRetryVerdict::RetryableThrottle {
                retry_after: Some(Duration::from_secs(7)),
            }),
    );
    assert_eq!(failure.kind, ProviderFailureKind::Quota);
    assert!(failure.is_retryable());
    assert_eq!(failure.retry_after(), Some(Duration::from_secs(7)));
}

#[test]
fn default_failure_classifier_keeps_quota_exhaustion_non_retryable() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "insufficient_quota",
        "usage_limit_reached",
        "usage_not_included in your plan",
    ] {
        let failure = classifier.classify(LlmTransportError::new(message).with_http_status(429));
        assert_eq!(failure.kind, ProviderFailureKind::Quota);
        assert!(!failure.is_retryable());
    }
}

// Per-minute throttling reads as "quota" at several providers but is exactly
// the case the retry ladder exists for. Treating it as exhaustion both fails
// the turn and skips throttle deference, which needs `Quota` + retryable.
#[test]
fn default_failure_classifier_keeps_rate_throttling_retryable() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "Quota exceeded for quota metric 'Generate requests per model per minute' \
         and limit 'GenerateRequestsPerDayPerProjectPerModel' of service \
         'generativelanguage.googleapis.com'",
        "Resource has been exhausted (e.g. check quota).",
        "429 RESOURCE_EXHAUSTED: Quota exceeded for aiplatform.googleapis.com",
    ] {
        let failure = classifier.classify(LlmTransportError::new(message).with_http_status(429));
        assert_eq!(
            failure.kind,
            ProviderFailureKind::Quota,
            "throttling stays Quota: {message}"
        );
        assert!(
            failure.is_retryable(),
            "per-minute throttling must stay retryable: {message}"
        );
    }
}

#[test]
fn default_failure_classifier_uses_context_overflow_text_for_unclassified_failures() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "Anthropic request failed: prompt is too long",
        "context_length_exceeded",
        "This model's maximum context length is 128000 tokens",
        "Google says input is too long",
        "OpenRouter error: too many tokens",
        "Together: request too large",
        "Copilot: exceeds the maximum number of tokens",
        "local model: context window exceeded",
        "Anthropic: request_too_large",
        "OpenAI: Your input exceeds the context window of this model",
        "Google: The input token count (1196265) exceeds the maximum number of tokens allowed",
        "xAI: This model's maximum prompt length is 131072 but the request contains more",
        "Groq: Please reduce the length of the messages",
        "OpenRouter: This endpoint's maximum context length is 128000 tokens",
        "Together: The input (150000 tokens) is longer than the model's context length (128000 tokens).",
        "llama.cpp: the request exceeds the available context size",
        "LM Studio: tokens to keep from the initial prompt is greater than the context length",
        "MiniMax: invalid params, context window exceeds limit",
        "Kimi: Your request exceeded model token limit: 131072 (requested: 150000)",
        "Mistral: Prompt contains too many tokens; too large for model with 128000 maximum context length",
        "z.ai: model_context_window_exceeded",
    ] {
        let failure = classifier.classify(
            LlmTransportError::new(message)
                .with_kind(ProviderFailureKind::Http)
                .with_http_status(400),
        );
        assert_eq!(failure.kind, ProviderFailureKind::Validation);
        assert_eq!(
            failure.terminal_reason,
            crate::LlmTerminalReason::ContextOverflow
        );
        assert!(!failure.is_retryable());
    }
}

#[test]
fn generic_anthropic_and_google_http_overflow_envelopes_use_the_text_fallback() {
    let classifier = DefaultProviderFailureClassifier;
    for raw in [
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        r#"{"error":{"code":400,"message":"The input token count (1200000) exceeds the maximum number of tokens allowed"}}"#,
    ] {
        let failure = classifier.classify(
            LlmTransportError::new("provider request failed with 400")
                .with_kind(ProviderFailureKind::Http)
                .with_http_status(400)
                .with_raw(raw),
        );

        assert_eq!(failure.kind, ProviderFailureKind::Validation, "{raw}");
        assert!(!failure.is_retryable(), "{raw}");
        assert_eq!(
            failure.terminal_reason,
            LlmTerminalReason::ContextOverflow,
            "{raw}"
        );
    }
}

#[test]
fn default_failure_classifier_fails_open_when_unclassified_text_is_uncertain() {
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("upstream request failed")
            .with_raw(r#"{"error":{"message":"ambiguous provider failure"}}"#),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Unknown);
    assert!(failure.is_retryable());
    assert_eq!(
        failure.terminal_reason,
        crate::LlmTerminalReason::ProviderError
    );
}

#[test]
fn default_failure_classifier_preserves_explicit_non_retryability() {
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("Anthropic stream error: invalid request")
            .with_retry_verdict(TransportRetryVerdict::NotRetryable),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Unknown);
    assert!(!failure.is_retryable());
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
}

#[test]
fn default_failure_classifier_makes_structured_validation_forbidden_without_scraping_echo() {
    // Deliberately, the more-specific provider-kind semantics take precedence
    // over an explicitly classified but conflicting transport verdict.
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code(FailureCode::provider("invalid_request_error"))
            .with_raw(
                r#"{"error":{"message":"The user wrote: context length is a useful phrase"}}"#,
            )
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert_eq!(failure.retry_verdict, TransportRetryVerdict::Forbidden);
    assert!(!failure.is_retryable());
    assert_eq!(code_of(&failure), "provider:invalid_request_error");
    assert_eq!(
        failure.terminal_reason,
        crate::LlmTerminalReason::ProviderError
    );
}

#[test]
fn default_failure_classifier_does_not_override_structured_hard_quota_echo() {
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code(FailureCode::provider("invalid_request_error"))
            .with_raw(r#"{"echo":"insufficient_quota"}"#),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert!(!failure.is_retryable());
    assert_eq!(code_of(&failure), "provider:invalid_request_error");
}

#[test]
fn default_failure_classifier_does_not_override_structured_content_filter_echo() {
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code(FailureCode::provider("invalid_request_error"))
            .with_raw(r#"{"echo":"the user asked about safety"}"#),
    );

    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
}

#[test]
fn default_failure_classifier_does_not_override_structured_unsupported_model_echo() {
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("request rejected")
            .with_kind(ProviderFailureKind::Validation)
            .with_code(FailureCode::provider("invalid_request_error"))
            .with_raw(r#"{"echo":"the example model does not exist"}"#),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert!(!failure.is_retryable());
    assert_eq!(code_of(&failure), "provider:invalid_request_error");
}

// FIG-3536: error-prose substring matching must never downgrade a retryable
// 5xx or a transport failure into a terminal refusal. A CDN outage page that
// says "safety" is not content-filter evidence.
#[test]
fn default_failure_classifier_keeps_5xx_error_pages_retryable() {
    let classifier = DefaultProviderFailureClassifier;
    for (status, raw) in [
        (
            503u16,
            "<html>for your safety, this request was blocked</html>",
        ),
        (502, "upstream host does not exist"),
        (500, "sensitive internal error"),
    ] {
        let failure = classifier.classify(
            LlmTransportError::new(format!("provider request failed with {status}"))
                .with_kind(ProviderFailureKind::Http)
                .with_http_status(status)
                .with_raw(raw),
        );
        assert_eq!(failure.kind, ProviderFailureKind::Http, "{raw}");
        assert_eq!(
            failure.terminal_reason,
            LlmTerminalReason::ProviderError,
            "{raw}"
        );
        assert!(failure.is_retryable(), "{raw}");
    }
}

#[test]
fn default_failure_classifier_keeps_transport_failures_retryable() {
    let classifier = DefaultProviderFailureClassifier;
    for kind in [ProviderFailureKind::Transport, ProviderFailureKind::Timeout] {
        let failure = classifier.classify(
            LlmTransportError::new("connect failed: upstream does not exist")
                .with_kind(kind)
                .with_raw("safety timeout, sensitive path"),
        );
        assert_eq!(failure.kind, kind);
        assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
        assert!(failure.is_retryable(), "{kind:?} must stay retryable");
    }
}

#[test]
fn default_failure_classifier_keeps_unrelated_400_as_validation_not_content_filter() {
    let failure = DefaultProviderFailureClassifier.classify(
        LlmTransportError::new("request failed with 400: header names are case-sensitive")
            .with_http_status(400),
    );

    assert_eq!(failure.kind, ProviderFailureKind::Validation);
    assert_eq!(failure.terminal_reason, LlmTerminalReason::ProviderError);
    assert!(!failure.is_retryable());
}

#[test]
fn default_failure_classifier_marks_unsupported_model_from_typed_code_only() {
    let classifier = DefaultProviderFailureClassifier;

    let typed = classifier.classify(
        LlmTransportError::new("unknown model").with_code(FailureCode::provider("model_not_found")),
    );
    assert_eq!(typed.kind, ProviderFailureKind::Unsupported);
    assert!(!typed.is_retryable());

    // The same spelling as free text is not evidence of a missing model.
    let prose = classifier.classify(
        LlmTransportError::new("model_not_found: the requested endpoint does not exist")
            .with_http_status(400),
    );
    assert_eq!(prose.kind, ProviderFailureKind::Validation);
    assert_eq!(prose.terminal_reason, LlmTerminalReason::ProviderError);
}

#[test]
fn default_failure_classifier_does_not_treat_rate_limits_as_context_overflow() {
    let classifier = DefaultProviderFailureClassifier;
    for message in [
        "rate limit: too many tokens per minute",
        "Too many requests",
        "throttling because token rate exceeded",
        "insufficient_quota",
    ] {
        let failure = classifier.classify(LlmTransportError::new(message).with_http_status(429));
        assert_ne!(
            failure.terminal_reason,
            crate::LlmTerminalReason::ContextOverflow
        );
    }
}
