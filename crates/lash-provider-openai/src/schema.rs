use serde_json::Value;

use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind};
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
            failure = failure.retryable(false);
            failure.terminal_reason = LlmTerminalReason::ContextOverflow;
        }
        "insufficient_quota" | "usage_limit_reached" | "usage_not_included" => {
            failure.kind = ProviderFailureKind::Quota;
            failure = failure.retryable(false);
        }
        "content_filter" | "prohibited_content" => {
            failure = failure.retryable(false);
            failure.terminal_reason = LlmTerminalReason::ContentFilter;
        }
        "model_not_found" | "unsupported_model" => {
            failure.kind = ProviderFailureKind::Unsupported;
            failure = failure.retryable(false);
        }
        _ => {}
    }
    failure
}

/// Decide whether an error object embedded in a Responses SSE event (or a
/// non-2xx Responses body) is retryable.
pub fn responses_error_is_retryable(value: &Value) -> bool {
    let numeric_code = value
        .get("code")
        .or_else(|| value.get("status"))
        .and_then(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        });
    matches!(numeric_code, Some(429))
        || matches!(numeric_code, Some(status) if status >= 500)
        || value
            .get("code")
            .or_else(|| value.get("type"))
            .or_else(|| value.get("status"))
            .and_then(|v| v.as_str())
            .is_some_and(|code| {
                matches!(
                    code,
                    "server_error"
                        | "internal_server_error"
                        | "service_unavailable"
                        | "temporarily_unavailable"
                        | "overloaded"
                        | "rate_limit_exceeded"
                )
            })
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
}
