use super::support::*;
use crate::LlmTerminalReason;

/// A configured LLM backend: its identity, host-config serialization, its
/// generation options, and the request transport.
///
/// Lash contains a panic from [`Provider::complete`] as a typed call failure,
/// but `complete` receives `&mut self`: after an unwind, the provider object's
/// own internal state is undefined from the host's perspective. The host owns
/// replacement, reset, or any other recovery needed before reusing it.
#[async_trait]
pub trait Provider: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> &'static str;

    fn options(&self) -> ProviderOptions;
    fn set_options(&mut self, options: ProviderOptions);

    /// Emit the provider-specific JSON body used by [`ProviderSpec`]. The
    /// object must NOT contain a `type` field — the [`ProviderSpec`]
    /// `Serialize` impl layers that on top.
    fn serialize_config(&self) -> serde_json::Value;

    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError>;

    fn requires_streaming(&self) -> bool {
        false
    }

    /// Release any host-visible transport resources this provider holds —
    /// cached connections, pooled sockets, background tasks — sending whatever
    /// graceful close a clean shutdown requires.
    ///
    /// Hosts call this before process exit so protocol niceties (e.g. WebSocket
    /// Close frames) are sent rather than skipped by an abrupt drop. It takes
    /// `&self` because a provider's reusable transport state lives behind its
    /// own synchronization; a shared clone can therefore be closed from the
    /// shutdown path. The default is a no-op: providers that hold no reusable
    /// transport state have nothing to release.
    async fn close(&self) -> Result<(), LlmTransportError> {
        Ok(())
    }

    fn clone_boxed(&self) -> Box<dyn Provider>;
}

pub trait ProviderFailureClassifier: Send + Sync + std::fmt::Debug {
    fn classify(&self, failure: ProviderFailure) -> ProviderFailure;
}

#[derive(Clone, Debug, Default)]
pub struct DefaultProviderFailureClassifier;

impl ProviderFailureClassifier for DefaultProviderFailureClassifier {
    fn classify(&self, mut failure: ProviderFailure) -> ProviderFailure {
        if let Some(status) = failure.status.or_else(|| {
            failure
                .code
                .as_deref()
                .and_then(|code| code.parse::<u16>().ok())
        }) {
            failure.status = Some(status);
            if failure.kind == ProviderFailureKind::Unknown {
                failure.kind = ProviderFailureKind::Http;
            }
            failure.retryable = matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504);
            if status == 429 {
                // Provider-side throttling. `Quota` + `retryable: true` is the
                // combination `ProviderHandle`'s retry ladder defers to as a
                // throttle; hard quota exhaustion (the text markers below)
                // downgrades to `retryable: false`.
                failure.kind = ProviderFailureKind::Quota;
            } else if matches!(status, 401 | 403) {
                failure.kind = ProviderFailureKind::Auth;
            } else if matches!(status, 400 | 413 | 422) {
                failure.kind = ProviderFailureKind::Validation;
            }
        } else if matches!(
            failure.kind,
            ProviderFailureKind::Transport | ProviderFailureKind::Timeout
        ) {
            failure.retryable = true;
        }

        let haystack = format!(
            "{}\n{}\n{}",
            failure.code.as_deref().unwrap_or_default(),
            failure.message,
            failure
                .raw
                .as_deref()
                .map(String::as_str)
                .unwrap_or_default()
        )
        .to_ascii_lowercase();
        if is_context_overflow_text(&haystack) {
            failure.kind = ProviderFailureKind::Validation;
            failure.retryable = false;
            failure.terminal_reason = LlmTerminalReason::ContextOverflow;
        }
        // Hard exhaustion only. A bare "quota" match is too broad: Google
        // phrases ordinary per-minute throttling as "Quota exceeded for quota
        // metric ... per minute", which arrives as a retryable 429 and must
        // stay retryable — downgrading it also loses the throttle-deference
        // path, which requires `Quota` + `retryable: true`.
        // A transient Google 429 with none of the RetryInfo, per-metric, or
        // textual retry markers is indistinguishable here and will therefore
        // be classified as terminal by this heuristic.
        let google_hard_quota = haystack.contains("you exceeded your current quota")
            && !haystack.contains("quota exceeded for metric")
            && !haystack.contains("retrydelay")
            && !haystack.contains("please retry");
        if haystack.contains("insufficient_quota")
            || haystack.contains("usage_limit_reached")
            || haystack.contains("usage_not_included")
            || haystack.contains("credit balance is too low")
            || google_hard_quota
        {
            failure.kind = ProviderFailureKind::Quota;
            failure.retryable = false;
        }
        if haystack.contains("content_filter")
            || haystack.contains("prohibited_content")
            || haystack.contains("safety")
            || haystack.contains("sensitive")
        {
            failure.terminal_reason = LlmTerminalReason::ContentFilter;
        }
        if haystack.contains("model_not_found")
            || haystack.contains("unsupported model")
            || haystack.contains("does not exist")
        {
            failure.kind = ProviderFailureKind::Unsupported;
            failure.retryable = false;
        }
        failure
    }
}

pub fn is_context_overflow_text(haystack: &str) -> bool {
    let lower = haystack.to_ascii_lowercase();
    if lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("ratelimit")
        || lower.contains("throttle")
        || lower.contains("throttling")
        || lower.contains("too many requests")
        || lower.contains("tokens per minute")
        || lower.contains("tpm")
        || lower.contains("quota")
    {
        return false;
    }

    lower.contains("context_length_exceeded")
        || lower.contains("context_length")
        || lower.contains("context length")
        || lower.contains("maximum context")
        || lower.contains("max context")
        || lower.contains("context window")
        || lower.contains("context window exceeds limit")
        || lower.contains("exceeds the context window")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("request_too_large")
        || lower.contains("input token count") && lower.contains("exceeds the maximum")
        || lower.contains("maximum prompt length is")
        || lower.contains("reduce the length of the messages")
        || lower.contains("maximum context length is")
        || lower.contains("model's context length")
        || lower.contains("models context length")
        || lower.contains("exceeds the available context size")
        || lower.contains("greater than the context length")
        || lower.contains("exceeded model token limit")
        || lower.contains("too large for model with")
        || lower.contains("model_context_window_exceeded")
        || lower.contains("too many tokens")
        || lower.contains("exceeds the maximum number of tokens")
        || lower.contains("exceeds maximum number of tokens")
        || lower.contains("request too large")
        || lower.contains("input is too long")
        || lower.contains("token limit exceeded")
        || lower.contains("reduce the length of the messages")
        || lower.contains("reduce the length of your prompt")
}
