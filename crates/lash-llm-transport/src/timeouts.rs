use std::time::Duration;

pub use lash_http_transport::{build_http_client, header_pairs, run_with_timeout};

pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_CHUNK_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlmTimeouts {
    pub request_timeout: Option<Duration>,
    pub chunk_timeout: Duration,
}

impl Default for LlmTimeouts {
    fn default() -> Self {
        Self {
            request_timeout: Some(Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS)),
            chunk_timeout: Duration::from_millis(DEFAULT_CHUNK_TIMEOUT_MS),
        }
    }
}

pub fn response_start_timeout(
    request_timeout: Option<Duration>,
    chunk_timeout: Duration,
    streaming: bool,
) -> Option<Duration> {
    if !streaming {
        return request_timeout;
    }
    Some(match request_timeout {
        Some(timeout) => timeout.min(chunk_timeout),
        None => chunk_timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::facade_support::LlmTransportError;

    #[test]
    fn streaming_response_start_timeout_prefers_chunk_deadline() {
        let timeout = response_start_timeout(
            Some(Duration::from_secs(300)),
            Duration::from_secs(120),
            true,
        );
        assert_eq!(timeout, Some(Duration::from_secs(120)));
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
    async fn run_with_timeout_returns_timeout_error() {
        let result = run_with_timeout(
            async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok::<_, LlmTransportError>(())
            },
            Some(Duration::from_millis(5)),
            "request timed out",
        )
        .await;

        let err = result.expect_err("expected timeout");
        assert_eq!(err.message, "request timed out");
        assert_eq!(err.code.as_deref(), Some("timeout"));
        assert!(err.retryable);
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
