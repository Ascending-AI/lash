use serde_json::Value;

use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_core::llm::types::LlmTerminalReason;

fn error_object(value: &Value) -> Option<&Value> {
    value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"))
}

pub(crate) fn classify_openai_error(
    value: &Value,
    mut failure: LlmTransportError,
) -> LlmTransportError {
    let Some(error) = error_object(value) else {
        return failure;
    };
    let Some(code) = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
    else {
        return failure;
    };

    failure.code = Some(code.to_string());
    match code {
        "context_length_exceeded" if matches!(failure.status, None | Some(400)) => {
            failure.kind = ProviderFailureKind::Validation;
            failure = failure.with_retry_verdict(TransportRetryVerdict::Forbidden);
            failure.terminal_reason = LlmTerminalReason::ContextOverflow;
        }
        "insufficient_quota" | "usage_limit_reached" | "usage_not_included" => {
            failure.kind = ProviderFailureKind::Quota;
            failure = failure.with_retry_verdict(TransportRetryVerdict::NotRetryable);
        }
        "content_filter" | "prohibited_content" => {
            failure = failure.with_retry_verdict(TransportRetryVerdict::Forbidden);
            failure.terminal_reason = LlmTerminalReason::ContentFilter;
        }
        "model_not_found" | "unsupported_model" => {
            failure.kind = ProviderFailureKind::Unsupported;
            failure = failure.with_retry_verdict(TransportRetryVerdict::NotRetryable);
        }
        _ => {}
    }
    failure
}

/// Classify an error object embedded in a Responses SSE event (or a non-2xx
/// Responses body) at the adapter boundary.
pub fn responses_error_retry_verdict(value: &Value) -> TransportRetryVerdict {
    let numeric_code = value
        .get("code")
        .or_else(|| value.get("status"))
        .and_then(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        });
    if matches!(numeric_code, Some(429)) {
        return TransportRetryVerdict::RetryableThrottle { retry_after: None };
    }
    if matches!(numeric_code, Some(400 | 401 | 403 | 422)) {
        return TransportRetryVerdict::Forbidden;
    }
    let semantic_code = value
        .get("code")
        .or_else(|| value.get("type"))
        .or_else(|| value.get("status"))
        .and_then(|v| v.as_str());
    match semantic_code {
        Some("rate_limit_exceeded" | "rate_limit_error" | "overloaded" | "capacity") => {
            TransportRetryVerdict::RetryableThrottle { retry_after: None }
        }
        Some(
            "authentication_error"
            | "permission_error"
            | "invalid_request_error"
            | "content_filter"
            | "prohibited_content",
        ) => TransportRetryVerdict::Forbidden,
        Some(
            "server_error"
            | "internal_server_error"
            | "service_unavailable"
            | "temporarily_unavailable",
        ) => TransportRetryVerdict::RetryableTransient,
        _ if matches!(numeric_code, Some(status) if status >= 500) => {
            TransportRetryVerdict::RetryableTransient
        }
        Some(_) | None => TransportRetryVerdict::NotRetryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_error_event_type_is_not_a_typed_provider_code() {
        let failure = classify_openai_error(
            &serde_json::json!({"type": "error", "message": "stream failed"}),
            LlmTransportError::new("stream failed"),
        );

        assert_eq!(failure.code, None);
    }

    #[test]
    fn response_failed_event_type_is_not_a_typed_provider_code() {
        let failure = classify_openai_error(
            &serde_json::json!({
                "type": "response.failed",
                "response": {"status": "failed"}
            }),
            LlmTransportError::new("response failed"),
        );

        assert_eq!(failure.code, None);
    }

    #[test]
    fn embedded_error_verdicts_distinguish_capacity_transient_and_forbidden() {
        assert_eq!(
            responses_error_retry_verdict(&serde_json::json!({"type": "overloaded"})),
            TransportRetryVerdict::RetryableThrottle { retry_after: None }
        );
        assert_eq!(
            responses_error_retry_verdict(&serde_json::json!({"status": 503})),
            TransportRetryVerdict::RetryableTransient
        );
        assert_eq!(
            responses_error_retry_verdict(&serde_json::json!({"status": 403})),
            TransportRetryVerdict::Forbidden
        );
    }
}
