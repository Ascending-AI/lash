use lash_sansio::llm::types::{LlmResponse, LlmTerminalReason, ProviderFailureKind};

/// Adapter-owned retry classification for a transport failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportRetryVerdict {
    /// The provider rejected admission because of throttling or capacity.
    /// `retry_after` is the provider's requested delay when one was supplied.
    RetryableThrottle {
        retry_after: Option<std::time::Duration>,
    },
    /// A connect, timeout, or server-shaped transient failure.
    RetryableTransient,
    /// The failure is not retryable, but future policy is not structurally barred.
    #[default]
    NotRetryable,
    /// Authentication, request validation, or content policy rejected the request.
    /// No retry policy may override this verdict.
    Forbidden,
}

impl TransportRetryVerdict {
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RetryableThrottle { .. } | Self::RetryableTransient
        )
    }

    pub const fn retry_after(self) -> Option<std::time::Duration> {
        match self {
            Self::RetryableThrottle { retry_after } => retry_after,
            Self::RetryableTransient | Self::NotRetryable | Self::Forbidden => None,
        }
    }
}

/// Failure crossing the host-configurable HTTP transport boundary.
///
/// The provider-oriented aliases retain the richer diagnostic fields because
/// the HTTP seam is also the wire boundary for LLM providers.
#[derive(Debug, thiserror::Error, Clone)]
#[error("{message}")]
pub struct HttpTransportError {
    pub kind: ProviderFailureKind,
    pub message: String,
    pub retry_verdict: TransportRetryVerdict,
    retry_verdict_classified: bool,
    pub status: Option<u16>,
    /// Cold raw provider evidence stays off the inline `Result` error path.
    pub raw: Option<Box<String>>,
    pub code: Option<String>,
    pub terminal_reason: LlmTerminalReason,
    /// Cold diagnostic metadata stays off the inline `Result` error path.
    pub headers: Box<Vec<(String, String)>>,
    /// Cold request evidence stays off the inline `Result` error path.
    pub request_body: Option<Box<String>>,
    /// The adapter observed provider-generated output before this failure,
    /// including output that cannot yet be projected into an [`LlmResponse`]
    /// part (for example unfinished tool arguments or opaque reasoning).
    pub output_started: bool,
    /// Provider output observed before this failure. It is diagnostic and
    /// accounting evidence, never a successful response.
    pub partial_response: Option<Box<LlmResponse>>,
}

impl HttpTransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderFailureKind::Unknown,
            message: message.into(),
            retry_verdict: TransportRetryVerdict::NotRetryable,
            retry_verdict_classified: false,
            status: None,
            raw: None,
            code: None,
            terminal_reason: LlmTerminalReason::ProviderError,
            headers: Box::default(),
            request_body: None,
            output_started: false,
            partial_response: None,
        }
    }

    pub fn with_kind(mut self, kind: ProviderFailureKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_retry_verdict(mut self, retry_verdict: TransportRetryVerdict) -> Self {
        self.retry_verdict = retry_verdict;
        self.retry_verdict_classified = true;
        self
    }

    /// Whether the driver explicitly supplied the typed transport verdict.
    pub fn retry_verdict_is_classified(&self) -> bool {
        self.retry_verdict_classified
    }

    pub const fn is_retryable(&self) -> bool {
        self.retry_verdict.is_retryable()
    }

    pub const fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_verdict.retry_after()
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        if self.code.is_none() {
            self.code = Some(status.to_string());
        }
        if !self.retry_verdict_classified {
            self.retry_verdict =
                retry_verdict_for_status(status, retry_after_from_headers(self.headers.as_slice()));
        }
        self
    }

    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.raw = Some(Box::new(raw.into()));
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_terminal_reason(mut self, reason: LlmTerminalReason) -> Self {
        self.terminal_reason = reason;
        self
    }

    pub fn with_headers<I, K, V>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.headers = Box::new(
            headers
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        );
        if let TransportRetryVerdict::RetryableThrottle { retry_after } = &mut self.retry_verdict {
            *retry_after = retry_after_from_headers(&self.headers);
        }
        self
    }

    pub fn with_request_body(mut self, request_body: impl Into<String>) -> Self {
        self.request_body = Some(Box::new(request_body.into()));
        self
    }

    pub fn with_output_started(mut self, output_started: bool) -> Self {
        self.output_started |= output_started;
        self
    }

    pub fn with_partial_response(mut self, response: LlmResponse) -> Self {
        self.partial_response = Some(Box::new(response));
        self
    }
}

fn retry_verdict_for_status(
    status: u16,
    retry_after: Option<std::time::Duration>,
) -> TransportRetryVerdict {
    match status {
        429 => TransportRetryVerdict::RetryableThrottle { retry_after },
        408 | 500..=599 => TransportRetryVerdict::RetryableTransient,
        400 | 401 | 403 | 422 => TransportRetryVerdict::Forbidden,
        _ => TransportRetryVerdict::NotRetryable,
    }
}

pub fn retry_after_from_headers(headers: &[(String, String)]) -> Option<std::time::Duration> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))?
        .1
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    Some(
        retry_at
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::retry_after_from_headers;
    use std::time::Duration;

    #[test]
    fn retry_after_accepts_http_dates_and_clamps_past_dates_to_zero() {
        let delta = retry_after_from_headers(&[("Retry-After".to_string(), "17".to_string())])
            .expect("delta-seconds remains valid Retry-After");
        assert_eq!(delta, Duration::from_secs(17));

        let future = retry_after_from_headers(&[(
            "Retry-After".to_string(),
            "Sat, 06 Nov 2094 08:49:37 GMT".to_string(),
        )])
        .expect("future HTTP-date is valid Retry-After");
        assert!(future > Duration::ZERO);

        let past = retry_after_from_headers(&[(
            "retry-after".to_string(),
            "Sun, 06 Nov 1994 08:49:37 GMT".to_string(),
        )])
        .expect("past HTTP-date is valid Retry-After");
        assert_eq!(past, Duration::ZERO);

        assert_eq!(
            retry_after_from_headers(&[(
                "retry-after".to_string(),
                "not an HTTP date".to_string(),
            )]),
            None
        );
    }
}
