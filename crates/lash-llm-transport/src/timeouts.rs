use std::time::Duration;

pub use lash_http_transport::{build_http_client, header_pairs, run_with_timeout};

pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_CHUNK_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlmTimeouts {
    pub request_timeout: Option<Duration>,
    /// Maximum wait for a streaming response to start. The whole-request
    /// timeout still wins when it is shorter.
    pub response_start_timeout: Duration,
    pub chunk_timeout: Duration,
}

impl Default for LlmTimeouts {
    fn default() -> Self {
        Self {
            request_timeout: Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS)),
            response_start_timeout: Duration::from_millis(DEFAULT_CHUNK_TIMEOUT_MS),
            chunk_timeout: Duration::from_millis(DEFAULT_CHUNK_TIMEOUT_MS),
        }
    }
}

/// Resolves the timeout applied while waiting for an HTTP response to start.
///
/// Streaming calls use `response_start_timeout`, capped by the remaining
/// whole-request timeout. Non-streaming calls retain the whole-request bound.
pub fn response_start_timeout(
    request_timeout: Option<Duration>,
    response_start_timeout: Duration,
    streaming: bool,
) -> Option<Duration> {
    if !streaming {
        return request_timeout;
    }
    Some(match request_timeout {
        Some(timeout) => timeout.min(response_start_timeout),
        None => response_start_timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::facade_support::LlmTransportError;

    #[test]
    fn streaming_response_start_timeout_honors_explicit_bound() {
        let timeout = response_start_timeout(
            Some(Duration::from_secs(300)),
            Duration::from_secs(20),
            true,
        );
        assert_eq!(timeout, Some(Duration::from_secs(20)));
    }

    #[test]
    fn non_stream_response_start_timeout_uses_request_deadline() {
        let timeout = response_start_timeout(
            Some(Duration::from_secs(300)),
            Duration::from_secs(120),
            false,
        );
        assert_eq!(timeout, Some(Duration::from_secs(300)));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_stream_start_uses_timeout_error_classification() {
        let result = run_with_timeout(
            async {
                tokio::time::sleep(Duration::from_secs(21)).await;
                Ok::<_, LlmTransportError>(())
            },
            response_start_timeout(
                Some(Duration::from_secs(300)),
                Duration::from_secs(20),
                true,
            ),
            "response start timed out",
        )
        .await;

        let error = result.expect_err("slow response start must time out");
        assert_eq!(error.kind, lash_core::ProviderFailureKind::Timeout);
        assert_eq!(error.code.as_deref(), Some("timeout"));
        assert_eq!(error.message, "response start timed out");
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn run_with_timeout_allows_successful_completion() {
        let result = run_with_timeout(
            async { Ok::<_, LlmTransportError>(42) },
            Some(Duration::from_secs(1)),
            "request timed out",
        )
        .await;

        assert_eq!(result.expect("success"), 42);
    }
}
