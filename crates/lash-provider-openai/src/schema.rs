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
    let code = error_object(value)
        .and_then(|error| {
            error
                .get("code")
                .or_else(|| error.get("type"))
                .and_then(Value::as_str)
        })
        // An in-band `{"type":"error","code":"…"}` event carries its code at
        // the top level. A top-level `type` is the event name, never a
        // provider code, so the fallback consults `code` alone.
        .or_else(|| value.get("code").and_then(Value::as_str));
    let Some(code) = code else {
        return failure;
    };

    failure.code = Some(lash_sansio::FailureCode::provider(code));
    match code {
        "context_length_exceeded" if matches!(failure.http_status, None | Some(400)) => {
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

/// Retry verdict for an in-band `{"type":"error",…}` SSE event: a nested
/// `error` object that carries a code wins, and otherwise the event's own
/// top-level `code` decides through the same mapping. A top-level `type` is
/// the event name, never a provider code, so the fallback consults `code`
/// alone.
pub(crate) fn sse_error_event_retry_verdict(event: &Value) -> TransportRetryVerdict {
    let coded_error = event.get("error").filter(|error| {
        ["code", "type", "status"]
            .iter()
            .any(|field| error.get(field).is_some_and(|value| !value.is_null()))
    });
    if let Some(error) = coded_error {
        return responses_error_retry_verdict(error);
    }
    event
        .get("code")
        .map(|code| responses_error_retry_verdict(&serde_json::json!({ "code": code })))
        .unwrap_or_default()
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
    fn top_level_error_event_code_is_a_typed_provider_code() {
        let failure = classify_openai_error(
            &serde_json::json!({"type": "error", "code": "server_error", "message": "stream failed"}),
            LlmTransportError::new("stream failed"),
        );

        assert_eq!(
            failure.code.as_ref().map(|code| code.namespaced()),
            Some("provider:server_error".to_string())
        );
    }

    #[test]
    fn sse_error_event_verdict_reads_top_level_code_never_type() {
        assert_eq!(
            sse_error_event_retry_verdict(&serde_json::json!({
                "type": "error",
                "code": "server_error",
                "message": "failed"
            })),
            TransportRetryVerdict::RetryableTransient
        );
        assert_eq!(
            sse_error_event_retry_verdict(&serde_json::json!({
                "type": "error",
                "message": "failed"
            })),
            TransportRetryVerdict::NotRetryable
        );
        assert_eq!(
            sse_error_event_retry_verdict(&serde_json::json!({
                "type": "error",
                "code": "server_error",
                "error": {"code": "rate_limit_exceeded"}
            })),
            TransportRetryVerdict::RetryableThrottle { retry_after: None }
        );
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
