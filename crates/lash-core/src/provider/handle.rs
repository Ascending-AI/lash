use super::support::*;

/// Component bundle returned by provider factories.
#[derive(Debug)]
pub struct ProviderComponents {
    pub provider: Box<dyn Provider>,
    pub failure_classifier: Arc<dyn ProviderFailureClassifier>,
    pub rate_limiter: Arc<ProviderRateLimiter>,
}

impl ProviderComponents {
    pub fn new(provider: Box<dyn Provider>) -> Self {
        let options = provider.options();
        Self {
            provider,
            failure_classifier: Arc::new(DefaultProviderFailureClassifier),
            rate_limiter: Arc::new(ProviderRateLimiter::new(options.reliability.rate_limits)),
        }
    }

    /// Install a transport-level decorator that wraps the provider.
    pub fn map_provider(
        mut self,
        map: impl FnOnce(Box<dyn Provider>) -> Box<dyn Provider>,
    ) -> Self {
        self.provider = map(self.provider);
        self
    }

    pub fn with_failure_classifier(
        mut self,
        classifier: Arc<dyn ProviderFailureClassifier>,
    ) -> Self {
        self.failure_classifier = classifier;
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        let options = self.provider.options();
        self.rate_limiter = Arc::new(ProviderRateLimiter::with_clock(
            options.reliability.rate_limits,
            clock,
        ));
        self
    }
}

impl Clone for ProviderComponents {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone_boxed(),
            failure_classifier: Arc::clone(&self.failure_classifier),
            rate_limiter: Arc::clone(&self.rate_limiter),
        }
    }
}

/// Owning handle to provider components. This is an executable transport
/// handle supplied by the host, not a persistence format.
pub struct ProviderHandle {
    components: ProviderComponents,
}

/// Successful provider-handle outcome with the sealed attempt history that
/// produced it. The inner provider response remains available through
/// `Deref` for source-compatible field access.
#[derive(Debug)]
pub struct ProviderCompletion {
    pub response: LlmResponse,
    pub call_record: LlmCallRecord,
}

impl std::ops::Deref for ProviderCompletion {
    type Target = LlmResponse;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

impl ProviderCompletion {
    pub fn into_response(self) -> LlmResponse {
        self.response
    }
}

/// Failed provider-handle outcome. The transport error is preserved intact,
/// and `call_record` makes all sealed attempts observable at this seam.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct ProviderCompletionError {
    #[source]
    pub error: LlmTransportError,
    pub call_record: LlmCallRecord,
}

impl std::ops::Deref for ProviderCompletionError {
    type Target = LlmTransportError;

    fn deref(&self) -> &Self::Target {
        &self.error
    }
}

impl ProviderHandle {
    pub fn new(components: ProviderComponents) -> Self {
        Self { components }
    }

    pub fn unconfigured() -> Self {
        Self::new(UnconfiguredProvider::default().into_components())
    }

    pub fn components(&self) -> &ProviderComponents {
        &self.components
    }

    pub fn components_mut(&mut self) -> &mut ProviderComponents {
        &mut self.components
    }

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.components = self.components.with_clock(clock);
        self
    }

    pub fn kind(&self) -> &'static str {
        self.components.provider.kind()
    }

    pub fn options(&self) -> ProviderOptions {
        self.components.provider.options()
    }

    pub fn set_options(&mut self, options: ProviderOptions) {
        self.components
            .rate_limiter
            .configure(options.reliability.rate_limits.clone());
        self.components.provider.set_options(options)
    }

    pub fn requires_streaming(&self) -> bool {
        self.components.provider.requires_streaming()
    }

    pub async fn complete(
        &mut self,
        request: LlmRequest,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        let reliability = self.options().reliability;
        let attempts = reliability.retry.attempts();
        let mut attempt = 0;
        let call_id = LlmCallId(uuid::Uuid::new_v4().to_string());
        let mut records = Vec::new();
        // Cumulative time already spent deferring to provider throttles
        // without consuming attempts, bounded by the policy's budget.
        let throttle_budget = Duration::from_millis(reliability.retry.throttle_wait_budget_ms);
        let mut throttle_waited = Duration::ZERO;
        loop {
            let _permit = self.components.rate_limiter.admit(&request).await;
            let clock = self.components.rate_limiter.clock();
            let started_at = clock.timestamp_ms();
            let started = clock.now();
            let result = self.components.provider.complete(request.clone()).await;
            match result {
                Ok(response) => {
                    let outcome = success_outcome(response.terminal_reason);
                    records.push(AttemptRecord {
                        ordinal: records.len() as u32 + 1,
                        started_at,
                        duration: clock.now().saturating_duration_since(started),
                        outcome,
                        protocol_position: success_protocol_position(&response, outcome),
                        retry_budget_consumed: true,
                        retry_decision: None,
                        error: None,
                        evidence: response.execution_evidence.clone(),
                        generation_disposition: response.generation_disposition,
                        usage: response
                            .provider_usage
                            .as_ref()
                            .map(|_| response.usage.clone()),
                    });
                    return Ok(ProviderCompletion {
                        response,
                        call_record: LlmCallRecord {
                            call_id,
                            label: None,
                            attempts: records,
                        },
                    });
                }
                Err(failure) => {
                    let failure = self.components.failure_classifier.classify(failure);
                    // Throttle deference: when the provider signals a throttle
                    // (retryable `Quota`) AND states how long to back off
                    // (`Retry-After`), honor the wait without consuming a
                    // retry attempt — the provider is asking us to come back,
                    // not failing. The courtesy is bounded: each deferred wait
                    // charges at least `MIN_THROTTLE_BUDGET_CHARGE` against
                    // the cumulative `throttle_wait_budget_ms`, and once the
                    // budget is spent a throttle counts as an ordinary
                    // retryable failure. A throttle WITHOUT `Retry-After`
                    // never defers: there is no server-stated wait to honor,
                    // so the normal backoff-and-count ladder applies.
                    if failure.retryable
                        && failure.kind == ProviderFailureKind::Quota
                        && let Some(retry_after) = failure.retry_after
                    {
                        let wait = reliability.retry.cap_retry_after(retry_after);
                        let charge = wait.max(MIN_THROTTLE_BUDGET_CHARGE);
                        // Saturating: an absurd uncapped `Retry-After` must
                        // overflow the budget check, not panic the ladder.
                        if throttle_waited.saturating_add(charge) <= throttle_budget {
                            throttle_waited += charge;
                            records.push(failure_attempt_record(
                                records.len() as u32 + 1,
                                started_at,
                                clock.now().saturating_duration_since(started),
                                &failure,
                                false,
                                Some(RetryDecision {
                                    scheduled: true,
                                    delay: Some(wait),
                                    reason: Some("provider_retry_after".to_string()),
                                }),
                            ));
                            tracing::debug!(
                                target: "lash_core::provider::reliability",
                                provider = self.kind(),
                                attempt = attempt + 1,
                                max_attempts = attempts,
                                wait_ms = wait.as_millis() as u64,
                                throttle_waited_ms = throttle_waited.as_millis() as u64,
                                err = %failure.message,
                                "provider throttled with retry-after; waiting without consuming a retry attempt"
                            );
                            if let Some(events) = request.stream_events.as_ref() {
                                events.send(crate::llm::types::LlmStreamEvent::AttemptReset);
                                events.send(crate::llm::types::LlmStreamEvent::RetryStatus {
                                    wait_seconds: wait.as_secs(),
                                    attempt: (attempt + 1) as usize,
                                    max_attempts: attempts as usize,
                                    reason: failure.message.clone(),
                                });
                            }
                            self.components.rate_limiter.clock().sleep(wait).await;
                            continue;
                        }
                    }
                    if attempt + 1 >= attempts || !failure.retryable {
                        let reason = if !failure.retryable {
                            "not_retryable"
                        } else {
                            "retry_budget_exhausted"
                        };
                        records.push(failure_attempt_record(
                            records.len() as u32 + 1,
                            started_at,
                            clock.now().saturating_duration_since(started),
                            &failure,
                            true,
                            Some(RetryDecision {
                                scheduled: false,
                                delay: None,
                                reason: Some(reason.to_string()),
                            }),
                        ));
                        return Err(ProviderCompletionError {
                            error: failure,
                            call_record: LlmCallRecord {
                                call_id,
                                label: None,
                                attempts: records,
                            },
                        });
                    }
                    let delay = reliability
                        .retry
                        .delay_for_attempt(attempt, failure.retry_after);
                    records.push(failure_attempt_record(
                        records.len() as u32 + 1,
                        started_at,
                        clock.now().saturating_duration_since(started),
                        &failure,
                        true,
                        Some(RetryDecision {
                            scheduled: true,
                            delay: Some(delay),
                            reason: Some("retryable_failure".to_string()),
                        }),
                    ));
                    tracing::debug!(
                        target: "lash_core::provider::reliability",
                        provider = self.kind(),
                        attempt = attempt + 1,
                        max_attempts = attempts,
                        delay_ms = delay.as_millis() as u64,
                        err = %failure.message,
                        "provider call failed with retryable failure; sleeping before retry"
                    );
                    if let Some(events) = request.stream_events.as_ref() {
                        events.send(crate::llm::types::LlmStreamEvent::AttemptReset);
                        events.send(crate::llm::types::LlmStreamEvent::RetryStatus {
                            wait_seconds: delay.as_secs(),
                            attempt: (attempt + 1) as usize,
                            max_attempts: attempts as usize,
                            reason: failure.message.clone(),
                        });
                    }
                    self.components.rate_limiter.clock().sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Release the underlying provider's host-visible transport resources.
    ///
    /// This forwards to [`Provider::close`]. Hosts that want a graceful
    /// transport shutdown (for example, sending WebSocket Close frames on
    /// cached Codex sessions) retain a clone of the handle they hand to the
    /// core and call this before process exit. Providers with no reusable
    /// transport state close as a no-op.
    pub async fn close(&self) -> Result<(), LlmTransportError> {
        self.components.provider.close().await
    }

    pub fn to_spec(&self) -> ProviderSpec {
        ProviderSpec {
            kind: self.kind().to_string(),
            config: self.components.provider.serialize_config(),
        }
    }

    /// Validate model syntax only.
    pub fn validate_model_name(&self, model: &str) -> Result<(), String> {
        let m = model.trim();
        if m.is_empty() {
            return Err("model cannot be empty".to_string());
        }
        if m.contains(char::is_whitespace) {
            return Err("model cannot contain whitespace".to_string());
        }
        Ok(())
    }
}

fn success_outcome(reason: LlmTerminalReason) -> AttemptOutcome {
    match reason {
        LlmTerminalReason::Cancelled => AttemptOutcome::Aborted,
        LlmTerminalReason::Unknown => AttemptOutcome::Interrupted,
        _ => AttemptOutcome::Completed,
    }
}

fn success_protocol_position(response: &LlmResponse, outcome: AttemptOutcome) -> ProtocolPosition {
    if outcome == AttemptOutcome::Completed {
        ProtocolPosition::TerminalObserved
    } else if !response.full_text.is_empty() || !response.parts.is_empty() {
        ProtocolPosition::OutputStarted
    } else {
        ProtocolPosition::ResponseObserved
    }
}

fn failure_attempt_record(
    ordinal: u32,
    started_at: u64,
    duration: Duration,
    failure: &LlmTransportError,
    retry_budget_consumed: bool,
    retry_decision: Option<RetryDecision>,
) -> AttemptRecord {
    let partial = failure.partial_response.as_deref();
    // Providers that do not send this header simply report nothing, which is
    // the honest result. Header-name variance across vendors (Anthropic uses
    // `request-id`) is a separate concern.
    let provider_request_id = header_value(&failure.headers, "x-request-id");
    let mut evidence = partial.and_then(|response| response.execution_evidence.clone());
    if let Some(provider_request_id) = provider_request_id.clone() {
        evidence
            .get_or_insert_with(ExecutionEvidence::default)
            .provider_request_id = Some(provider_request_id);
    }
    AttemptRecord {
        ordinal,
        started_at,
        duration,
        outcome: match (failure.terminal_reason, failure.kind) {
            (LlmTerminalReason::Cancelled, _) => AttemptOutcome::Aborted,
            (_, ProviderFailureKind::Timeout | ProviderFailureKind::Stream) => {
                AttemptOutcome::Interrupted
            }
            _ => AttemptOutcome::Failed,
        },
        protocol_position: partial
            .map(|response| {
                if !response.full_text.is_empty() || !response.parts.is_empty() {
                    ProtocolPosition::OutputStarted
                } else {
                    ProtocolPosition::ResponseObserved
                }
            })
            .unwrap_or_else(|| {
                if failure.status.is_some() {
                    ProtocolPosition::ResponseObserved
                } else {
                    ProtocolPosition::NoResponse
                }
            }),
        retry_budget_consumed,
        retry_decision,
        error: Some(NormalizedError {
            class: failure.kind.code().to_string(),
            provider_code: failure.code.clone(),
            http_status: failure.status,
            provider_request_id,
            retry_after: failure.retry_after,
            diagnostic: bounded_redacted_diagnostic(&failure.message),
        }),
        evidence,
        generation_disposition: partial.and_then(|response| response.generation_disposition),
        usage: partial.and_then(|response| {
            (response.provider_usage.is_some()
                || response.usage != crate::llm::types::LlmUsage::default())
            .then(|| response.usage.clone())
        }),
    }
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

pub(super) const MAX_ATTEMPT_DIAGNOSTIC_CHARS: usize = 1_024;

pub(super) fn bounded_redacted_diagnostic(message: &str) -> Option<String> {
    let mut redacted = Vec::new();
    let mut redact_next = false;
    for word in message.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if redact_next
            || lower.starts_with("sk-")
            || lower.contains("api_key=")
            || lower.contains("api-key=")
            || lower.contains("authorization:")
        {
            redacted.push("[REDACTED]");
            redact_next = false;
        } else {
            redacted.push(word);
            redact_next =
                lower == "bearer" || lower.ends_with("api_key=") || lower.ends_with("api-key=");
        }
    }
    let diagnostic: String = redacted
        .join(" ")
        .chars()
        .take(MAX_ATTEMPT_DIAGNOSTIC_CHARS)
        .collect();
    (!diagnostic.is_empty()).then_some(diagnostic)
}

impl std::fmt::Debug for ProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.components.fmt(f)
    }
}

impl Clone for ProviderHandle {
    fn clone(&self) -> Self {
        Self {
            components: self.components.clone(),
        }
    }
}

impl PartialEq for ProviderHandle {
    fn eq(&self, other: &Self) -> bool {
        self.kind() == other.kind() && self.to_spec().config == other.to_spec().config
    }
}

impl Eq for ProviderHandle {}

/// Placeholder provider used by runtime policy defaults before a host resolver
/// installs the executable provider. Every transport-level method errors;
/// calling code MUST replace this before executing a turn.
#[derive(Clone, Debug, Default)]
pub struct UnconfiguredProvider {
    options: ProviderOptions,
}

impl UnconfiguredProvider {
    fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

#[async_trait]
impl Provider for UnconfiguredProvider {
    fn kind(&self) -> &'static str {
        "unconfigured"
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Object(Default::default())
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        Err(LlmTransportError::new(
            "no provider configured: host must set SessionPolicy.provider before running a turn",
        ))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
