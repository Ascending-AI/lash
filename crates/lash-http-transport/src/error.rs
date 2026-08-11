use lash_sansio::llm::types::{LlmResponse, LlmTerminalReason, ProviderFailureKind};

/// Failure crossing the host-configurable HTTP transport boundary.
///
/// The provider-oriented aliases retain the richer diagnostic fields because
/// the HTTP seam is also the wire boundary for LLM providers.
#[derive(Debug, thiserror::Error, Clone)]
#[error("{message}")]
pub struct HttpTransportError {
    pub kind: ProviderFailureKind,
    pub message: String,
    pub retryable: bool,
    pub status: Option<u16>,
    /// Cold raw provider evidence stays off the inline `Result` error path.
    pub raw: Option<Box<String>>,
    pub code: Option<String>,
    pub terminal_reason: LlmTerminalReason,
    /// Cold diagnostic metadata stays off the inline `Result` error path.
    pub headers: Box<Vec<(String, String)>>,
    pub retry_after: Option<std::time::Duration>,
    pub request_body: Option<String>,
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
            retryable: false,
            status: None,
            raw: None,
            code: None,
            terminal_reason: LlmTerminalReason::ProviderError,
            headers: Box::default(),
            retry_after: None,
            request_body: None,
            output_started: false,
            partial_response: None,
        }
    }

    pub fn with_kind(mut self, kind: ProviderFailureKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        if self.code.is_none() {
            self.code = Some(status.to_string());
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
        self.retry_after = retry_after_from_headers(&self.headers);
        self
    }

    pub fn with_retry_after(mut self, retry_after: std::time::Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    pub fn with_request_body(mut self, request_body: impl Into<String>) -> Self {
        self.request_body = Some(request_body.into());
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

pub fn retry_after_from_headers(headers: &[(String, String)]) -> Option<std::time::Duration> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))?
        .1
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    None
}
