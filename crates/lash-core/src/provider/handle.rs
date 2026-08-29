use super::support::*;
use futures_util::FutureExt as _;

fn replay_origin_conflict_error(conflict: ProviderReplayOriginConflict) -> LlmTransportError {
    LlmTransportError::new(conflict.to_string())
        .with_kind(ProviderFailureKind::Validation)
        .with_code("provider_replay_origin_conflict")
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}

fn replay_origin_conflict_with_provider_error(
    conflict: ProviderReplayOriginConflict,
    mut provider_error: LlmTransportError,
) -> LlmTransportError {
    provider_error.message = format!(
        "{conflict}; original LLM Provider failure: {}",
        provider_error.message
    );
    provider_error.kind = ProviderFailureKind::Validation;
    provider_error.code = Some("provider_replay_origin_conflict".to_string());
    provider_error.retry_verdict = TransportRetryVerdict::Forbidden;
    provider_error
}

#[derive(Debug)]
struct ProviderCompletionSidebandState {
    serving_route: ProviderRouteIdentity,
    replay_drops: Vec<crate::ProviderReplayDrop>,
    origin_conflict: Option<ProviderReplayOriginConflict>,
}

/// Replay safety state shared with the runtime independently of the spawned
/// LLM Provider task's terminal return.
#[derive(Clone, Debug)]
pub(crate) struct ProviderCompletionSideband {
    state: Arc<Mutex<ProviderCompletionSidebandState>>,
}

impl ProviderCompletionSideband {
    fn new(
        serving_route: ProviderRouteIdentity,
        replay_drops: Vec<crate::ProviderReplayDrop>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProviderCompletionSidebandState {
                serving_route,
                replay_drops,
                origin_conflict: None,
            })),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ProviderCompletionSidebandState) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state)
    }

    fn record_origin_conflict(&self, conflict: ProviderReplayOriginConflict) {
        self.with_state(|state| {
            if state.origin_conflict.is_none() {
                state.origin_conflict = Some(conflict);
            }
        });
    }

    pub(crate) fn replay_drops(&self) -> Vec<crate::ProviderReplayDrop> {
        self.with_state(|state| state.replay_drops.clone())
    }

    fn serving_route(&self) -> ProviderRouteIdentity {
        self.with_state(|state| state.serving_route.clone())
    }

    pub(crate) fn origin_conflict(&self) -> Option<ProviderReplayOriginConflict> {
        self.with_state(|state| state.origin_conflict.clone())
    }

    pub(crate) fn fence_response(
        &self,
        response: &mut LlmResponse,
    ) -> Result<(), LlmTransportError> {
        let serving_route = self.serving_route();
        if let Err(conflict) = response.stamp_replay_origin(&serving_route) {
            self.record_origin_conflict(conflict);
        }
        match self.origin_conflict() {
            Some(conflict) => Err(replay_origin_conflict_error(conflict)),
            None => Ok(()),
        }
    }

    fn fence_error(&self, mut error: LlmTransportError) -> LlmTransportError {
        let serving_route = self.serving_route();
        if let Some(partial) = error.partial_response.as_deref_mut()
            && let Err(conflict) = partial.stamp_replay_origin(&serving_route)
        {
            self.record_origin_conflict(conflict);
        }
        match self.origin_conflict() {
            Some(conflict) => replay_origin_conflict_with_provider_error(conflict, error),
            None => error,
        }
    }
}

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

/// Failed provider-handle outcome. The transport error is preserved intact,
/// and `call_record` makes all sealed attempts observable at this seam.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct ProviderCompletionError {
    #[source]
    pub error: LlmTransportError,
    pub call_record: Box<LlmCallRecord>,
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

    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.components = self.components.with_clock(clock);
        self
    }

    pub fn kind(&self) -> &'static str {
        self.components.provider.kind()
    }

    pub fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        self.components.provider.route_identity(model)
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
        mut request: LlmRequest,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        let sideband = self.prepare_completion(&mut request);
        self.complete_prepared(request, sideband, crate::ChargeSafetyPolicy::default())
            .await
    }

    /// Completes a request under an explicit live charge-safety policy.
    ///
    /// Prefer [`Self::complete`] unless the host has deliberately accepted a
    /// bounded duplicate-billing risk for this call.
    pub async fn complete_with_charge_safety(
        &mut self,
        mut request: LlmRequest,
        charge_safety: crate::ChargeSafetyPolicy,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        let sideband = self.prepare_completion(&mut request);
        self.complete_prepared(request, sideband, charge_safety)
            .await
    }

    pub(crate) fn prepare_completion(
        &self,
        request: &mut LlmRequest,
    ) -> ProviderCompletionSideband {
        let serving_route = self.route_identity(&request.model);
        // Do not manufacture trace evidence containing an invalid endpoint:
        // URL userinfo may itself be credential material. `complete_prepared`
        // rejects the route before the LLM Provider is invoked.
        let replay_drops = if serving_route.validate_endpoint().is_ok() {
            request.drop_foreign_replay(&serving_route)
        } else {
            Vec::new()
        };
        let sideband = ProviderCompletionSideband::new(serving_route.clone(), replay_drops);
        if let Some(stream_events) = request.stream_events.take() {
            let stream_route = serving_route.clone();
            let stream_sideband = sideband.clone();
            request.stream_events =
                Some(crate::llm::types::LlmEventSender::new(move |mut event| {
                    if let crate::llm::types::LlmStreamEvent::Part(part) = &mut event {
                        // Conflicting origins are deliberately preserved. The
                        // stream remains foreign instead of being laundered into
                        // the serving route; terminal response stamping surfaces
                        // the typed contract error where a Result is available.
                        if let Err(conflict) = part.stamp_replay_origin(&stream_route) {
                            stream_sideband.record_origin_conflict(conflict);
                        }
                    }
                    stream_events.send(event);
                }));
        }
        sideband
    }

    pub(crate) async fn complete_prepared(
        &mut self,
        request: LlmRequest,
        sideband: ProviderCompletionSideband,
        charge_safety: crate::ChargeSafetyPolicy,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        let serving_route = sideband.serving_route();
        if let Err(error) = serving_route.validate_endpoint() {
            let error = LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_code("invalid_provider_endpoint")
                .with_retry_verdict(TransportRetryVerdict::Forbidden);
            return Err(ProviderCompletionError {
                call_record: Box::new(synthetic_terminal_call_record(
                    self.components.rate_limiter.clock().timestamp_ms(),
                    Duration::ZERO,
                    AttemptOutcome::Failed,
                    &error,
                    false,
                    ProtocolPosition::NoResponse,
                    sideband.replay_drops(),
                )),
                error,
            });
        }
        let reliability = self.options().reliability;
        let attempts = reliability.retry.attempts();
        let mut attempt = 0;
        let call_id = LlmCallId(uuid::Uuid::new_v4().to_string());
        let mut records = Vec::new();
        // Cumulative time and calls already spent deferring to provider
        // throttles without consuming attempts. Both dimensions are bounded.
        let throttle_budget = Duration::from_millis(reliability.retry.throttle_wait_budget_ms);
        let mut throttle_waited = Duration::ZERO;
        let mut courtesy_throttle_calls = 0;
        let mut unsafe_retries = 0u8;
        loop {
            let _permit = self.components.rate_limiter.admit(&request).await;
            let clock = self.components.rate_limiter.clock();
            let started_at = clock.timestamp_ms();
            let started = clock.now();
            let (mut result, panic_payload) = match std::panic::AssertUnwindSafe(
                self.components.provider.complete(request.clone()),
            )
            .catch_unwind()
            .await
            {
                Ok(result) => (result, None),
                Err(payload) => {
                    let message = crate::panic_containment::payload_message(payload.as_ref());
                    (
                        Err(LlmTransportError::new(message)
                            .with_kind(ProviderFailureKind::Unknown)
                            .with_code("provider_panicked")
                            .with_retry_verdict(TransportRetryVerdict::NotRetryable)),
                        Some(payload),
                    )
                }
            };
            // Classify provider-owned failures before applying Lash's replay
            // contract. A classifier must never reinterpret the synthetic,
            // non-retryable origin-conflict result from the fence below.
            if panic_payload.is_none() {
                result =
                    result.map_err(|failure| self.components.failure_classifier.classify(failure));
            }
            let (result, original_failure) = match result {
                Ok(mut response) => match sideband.fence_response(&mut response) {
                    Ok(()) => (Ok(response), None),
                    Err(error) => (Err(error), None),
                },
                Err(error) => {
                    let original_failure = error.clone();
                    (Err(sideband.fence_error(error)), Some(original_failure))
                }
            };
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
                            replay_drops: sideband.replay_drops(),
                            attempts: records,
                        },
                    });
                }
                Err(failure) => {
                    // The outer error is Lash's typed conflict classification;
                    // the sealed attempt remains the provider's original
                    // failure evidence (kind, code, status, and diagnostic).
                    let recorded_failure = original_failure.as_ref().unwrap_or(&failure);
                    let protocol_position = failure_protocol_position(&failure);
                    let retry_guarantee = self
                        .components
                        .provider
                        .generation_retry_guarantee(&request);
                    let retry_class =
                        automatic_retry_class(&failure, protocol_position, retry_guarantee);
                    let retry_after_exceeds_cap =
                        failure.retry_after().is_some_and(|retry_after| {
                            reliability
                                .retry
                                .retry_after_within_cap(retry_after)
                                .is_none()
                        });
                    let throttle_wait = if let TransportRetryVerdict::RetryableThrottle {
                        retry_after: Some(retry_after),
                    } = failure.retry_verdict
                    {
                        reliability
                            .retry
                            .retry_after_within_cap(retry_after)
                            .and_then(|wait| {
                                let charge = wait;
                                (wait >= MIN_FREE_THROTTLE_WAIT
                                    && courtesy_throttle_calls < MAX_COURTESY_THROTTLE_CALLS
                                    && throttle_waited.saturating_add(charge) <= throttle_budget)
                                    .then_some((wait, charge))
                            })
                    } else {
                        None
                    };
                    let counted_retry_available = attempt + 1 < attempts;
                    let mut charge_safety_decision =
                        if failure.is_retryable() && retry_class.is_none() {
                            match charge_safety_decision(
                                failure.retry_verdict,
                                retry_guarantee,
                                &charge_safety,
                                failure
                                    .partial_response
                                    .as_deref()
                                    .map(|response| &response.usage),
                                unsafe_retries.saturating_add(1),
                            ) {
                                ChargeSafetyEvaluation::Evaluated(decision) => Some(decision),
                                ChargeSafetyEvaluation::NotEvaluated(_) => None,
                            }
                        } else {
                            None
                        };
                    if retry_after_exceeds_cap
                        && let Some(ChargeSafetyDecision::Authorized {
                            tokens_at_stake,
                            attempt_number,
                        }) = charge_safety_decision.clone()
                    {
                        charge_safety_decision = Some(ChargeSafetyDecision::Denied {
                            tokens_at_stake,
                            attempt_number,
                            reason: ChargeSafetyDenialReason::RetryAfterExceedsCap,
                        });
                    }

                    // A retryable transport classification is necessary but
                    // not sufficient to buy another generation. These are the
                    // only automatic retry classes:
                    //
                    // * `NoResponse`: no provider response was observed, so
                    //   Lash has no output or response evidence to discard.
                    // * `RejectedHttpResponse`: only HTTP 429 classified as a
                    //   throttle and carrying `Retry-After`. That combination
                    //   is the provider's explicit evidence that admission was
                    //   rejected and says when resubmission is allowed. HTTP
                    //   408/409/425/500/502/503/504 are intentionally excluded:
                    //   none proves an upstream generation did not start.
                    // * `EmptyStreamPartial`: a streaming adapter explicitly
                    //   returned an empty partial response with no output or
                    //   usage. This is intentionally narrower than blanket
                    //   `ResponseObserved`; arbitrary response-observed
                    //   failures may hide provider-side generation.
                    // * `ProviderGuarantee`: the provider explicitly promises
                    //   idempotent replay or partial-generation resume.
                    //
                    // In particular, `OutputStarted` never qualifies through
                    // position or emptiness. It requires the provider guarantee
                    // above; none of Lash's bundled providers declares one.
                    if let Some(decision @ ChargeSafetyDecision::Denied { reason, .. }) =
                        charge_safety_decision.clone()
                    {
                        let retry_after_header_present = failure
                            .headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("retry-after"));
                        let partial = failure.partial_response.as_deref();
                        let partial_response_present = partial.is_some();
                        let partial_response_empty =
                            partial.map(|response| !response_has_output_evidence(response));
                        tracing::warn!(
                            target: "lash_core::provider::reliability",
                            provider = self.kind(),
                            failure_kind = failure.kind.code(),
                            http_status = ?failure.status,
                            retry_after_header_present,
                            retry_after_parsed_ms = ?failure
                                .retry_after()
                                .map(|duration| duration.as_millis() as u64),
                            partial_response_present,
                            partial_response_empty = ?partial_response_empty,
                            usage = ?partial.map(|response| &response.usage),
                            provider_usage = ?partial
                                .and_then(|response| response.provider_usage.as_ref()),
                            protocol_position = ?protocol_position,
                            provider_retry_guarantee = ?retry_guarantee,
                            retry_class = ?retry_class,
                            transport_retryable = failure.is_retryable(),
                            throttle_retry_available = throttle_wait.is_some(),
                            counted_retry_available,
                            decision = "deny",
                            reason = charge_safety_retry_reason(reason, protocol_position),
                            "provider retry denied because another generation is not proven charge-safe"
                        );
                        records.push(failure_attempt_record(
                            records.len() as u32 + 1,
                            started_at,
                            clock.now().saturating_duration_since(started),
                            recorded_failure,
                            true,
                            protocol_position,
                            Some(RetryDecision {
                                scheduled: false,
                                delay: None,
                                reason: Some(
                                    charge_safety_retry_reason(reason, protocol_position)
                                        .to_string(),
                                ),
                                charge_safety: Some(decision),
                            }),
                        ));
                        return Err(ProviderCompletionError {
                            error: charge_safety_refusal(failure, protocol_position, reason),
                            call_record: Box::new(LlmCallRecord {
                                call_id,
                                label: None,
                                replay_drops: sideband.replay_drops(),
                                attempts: records,
                            }),
                        });
                    }
                    if failure.is_retryable() && retry_after_exceeds_cap {
                        records.push(failure_attempt_record(
                            records.len() as u32 + 1,
                            started_at,
                            clock.now().saturating_duration_since(started),
                            recorded_failure,
                            true,
                            protocol_position,
                            Some(RetryDecision {
                                scheduled: false,
                                delay: None,
                                reason: Some("retry_after_exceeds_cap".to_string()),
                                charge_safety: charge_safety_decision,
                            }),
                        ));
                        return Err(ProviderCompletionError {
                            error: failure,
                            call_record: Box::new(LlmCallRecord {
                                call_id,
                                label: None,
                                replay_drops: sideband.replay_drops(),
                                attempts: records,
                            }),
                        });
                    }
                    // Throttle deference: when the adapter's typed throttle
                    // verdict states how long to back off, honor the wait
                    // without consuming a retry attempt — the provider is asking us to come back,
                    // not failing. The courtesy is bounded: each deferred wait
                    // requires at least `MIN_FREE_THROTTLE_WAIT`, charges the
                    // actual delay against `throttle_wait_budget_ms`, and is
                    // capped at `MAX_COURTESY_THROTTLE_CALLS`. Once either
                    // bound is spent, a throttle counts as an ordinary
                    // retryable failure. A missing or shorter `Retry-After`
                    // never defers: there is no meaningful server-stated wait
                    // to honor, so the normal backoff-and-count ladder applies.
                    if let Some((wait, charge)) = throttle_wait {
                        if charge_safety_decision.is_some() {
                            unsafe_retries = unsafe_retries.saturating_add(1);
                        }
                        throttle_waited += charge;
                        courtesy_throttle_calls += 1;
                        crate::operational_metrics::record_provider_retry(self.kind(), "throttle");
                        crate::operational_metrics::record_provider_throttle_wait(
                            self.kind(),
                            wait,
                        );
                        records.push(failure_attempt_record(
                            records.len() as u32 + 1,
                            started_at,
                            clock.now().saturating_duration_since(started),
                            recorded_failure,
                            false,
                            protocol_position,
                            Some(RetryDecision {
                                scheduled: true,
                                delay: Some(wait),
                                reason: Some("provider_retry_after".to_string()),
                                charge_safety: charge_safety_decision,
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
                            if retry_class.is_some_and(AutomaticRetryClass::resets_stream) {
                                events.send(crate::llm::types::LlmStreamEvent::AttemptReset);
                            }
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
                    if attempt + 1 >= attempts || !failure.is_retryable() {
                        let reason = if !failure.is_retryable() {
                            "not_retryable"
                        } else {
                            "retry_budget_exhausted"
                        };
                        records.push(failure_attempt_record(
                            records.len() as u32 + 1,
                            started_at,
                            clock.now().saturating_duration_since(started),
                            recorded_failure,
                            true,
                            protocol_position,
                            Some(RetryDecision {
                                scheduled: false,
                                delay: None,
                                reason: Some(reason.to_string()),
                                charge_safety: charge_safety_decision,
                            }),
                        ));
                        let completion_error = ProviderCompletionError {
                            error: failure,
                            call_record: Box::new(LlmCallRecord {
                                call_id,
                                label: None,
                                replay_drops: sideband.replay_drops(),
                                attempts: records,
                            }),
                        };
                        if let Some(payload) = panic_payload {
                            crate::panic_containment::enforce_loudness(payload);
                        }
                        return Err(completion_error);
                    }
                    let delay = reliability
                        .retry
                        .delay_for_attempt(attempt, failure.retry_after())
                        .expect("Retry-After was checked against the cap before scheduling");
                    crate::operational_metrics::record_provider_retry(self.kind(), "backoff");
                    if charge_safety_decision.is_some() {
                        unsafe_retries = unsafe_retries.saturating_add(1);
                    }
                    records.push(failure_attempt_record(
                        records.len() as u32 + 1,
                        started_at,
                        clock.now().saturating_duration_since(started),
                        recorded_failure,
                        true,
                        protocol_position,
                        Some(RetryDecision {
                            scheduled: true,
                            delay: Some(delay),
                            reason: retry_class.map(|class| class.reason().to_string()),
                            charge_safety: charge_safety_decision,
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
                        if retry_class.is_some_and(AutomaticRetryClass::resets_stream) {
                            events.send(crate::llm::types::LlmStreamEvent::AttemptReset);
                        }
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
        std::panic::AssertUnwindSafe(self.components.provider.close())
            .catch_unwind()
            .await
            .unwrap_or_else(provider_close_panicked)
    }
}

fn provider_close_panicked(
    payload: Box<dyn std::any::Any + Send>,
) -> Result<(), LlmTransportError> {
    let message = crate::panic_containment::payload_message(payload.as_ref());
    let failure = Err(LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Unknown)
        .with_code("provider_panicked")
        .with_retry_verdict(TransportRetryVerdict::NotRetryable));
    crate::panic_containment::enforce_loudness(payload);
    failure
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
    } else if response_has_output_evidence(response) {
        ProtocolPosition::OutputStarted
    } else {
        ProtocolPosition::ResponseObserved
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutomaticRetryClass {
    NoResponse,
    RejectedHttpResponse,
    EmptyStreamPartial,
    ProviderGuarantee(GenerationRetryGuarantee),
}

pub(super) const MAX_UNSAFE_RETRIES: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChargeSafetyPrecedence {
    Forbidden,
    ServerPushback,
    ProviderGuarantee,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ChargeSafetyEvaluation {
    NotEvaluated(ChargeSafetyPrecedence),
    Evaluated(ChargeSafetyDecision),
}

pub(super) fn charge_safety_decision(
    retry_verdict: TransportRetryVerdict,
    guarantee: GenerationRetryGuarantee,
    policy: &crate::ChargeSafetyPolicy,
    usage: Option<&crate::llm::types::LlmUsage>,
    attempt_number: u8,
) -> ChargeSafetyEvaluation {
    match retry_verdict {
        TransportRetryVerdict::Forbidden => {
            return ChargeSafetyEvaluation::NotEvaluated(ChargeSafetyPrecedence::Forbidden);
        }
        TransportRetryVerdict::NotRetryable => {
            return ChargeSafetyEvaluation::NotEvaluated(ChargeSafetyPrecedence::ServerPushback);
        }
        TransportRetryVerdict::RetryableThrottle { .. }
        | TransportRetryVerdict::RetryableTransient => {}
    }
    if guarantee != GenerationRetryGuarantee::None {
        return ChargeSafetyEvaluation::NotEvaluated(ChargeSafetyPrecedence::ProviderGuarantee);
    }

    let tokens_at_stake = usage.map(duplicate_cost_tokens).unwrap_or_default();
    let denied = |reason| {
        ChargeSafetyEvaluation::Evaluated(ChargeSafetyDecision::Denied {
            tokens_at_stake,
            attempt_number,
            reason,
        })
    };
    match policy {
        crate::ChargeSafetyPolicy::RequireGuarantee => {
            denied(ChargeSafetyDenialReason::GuaranteeRequired)
        }
        crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries,
            max_duplicate_cost_tokens,
        } => {
            if attempt_number > (*max_unsafe_retries).min(MAX_UNSAFE_RETRIES) {
                return denied(ChargeSafetyDenialReason::UnsafeRetryLimitExceeded);
            }
            if max_duplicate_cost_tokens.is_some_and(|maximum| tokens_at_stake > maximum) {
                return denied(ChargeSafetyDenialReason::DuplicateCostLimitExceeded);
            }
            ChargeSafetyEvaluation::Evaluated(ChargeSafetyDecision::Authorized {
                tokens_at_stake,
                attempt_number,
            })
        }
    }
}

fn duplicate_cost_tokens(usage: &crate::llm::types::LlmUsage) -> u64 {
    let total = i128::from(usage.input_tokens)
        + i128::from(usage.output_tokens)
        + i128::from(usage.cache_read_input_tokens)
        + i128::from(usage.cache_write_input_tokens);
    total.clamp(0, i128::from(u64::MAX)) as u64
}

impl AutomaticRetryClass {
    fn reason(self) -> &'static str {
        match self {
            Self::NoResponse => "failure_before_response",
            Self::RejectedHttpResponse => "retryable_http_rejection",
            Self::EmptyStreamPartial => "empty_stream_partial_before_output",
            Self::ProviderGuarantee(GenerationRetryGuarantee::Idempotent) => {
                "provider_idempotency_guarantee"
            }
            Self::ProviderGuarantee(GenerationRetryGuarantee::Resumable) => {
                "provider_resume_guarantee"
            }
            Self::ProviderGuarantee(GenerationRetryGuarantee::None) => {
                unreachable!("None is never classified as a provider guarantee")
            }
        }
    }

    fn resets_stream(self) -> bool {
        !matches!(
            self,
            Self::ProviderGuarantee(GenerationRetryGuarantee::Resumable)
        )
    }
}

pub(super) fn failure_protocol_position(failure: &LlmTransportError) -> ProtocolPosition {
    if failure.output_started {
        return ProtocolPosition::OutputStarted;
    }
    failure
        .partial_response
        .as_deref()
        .map(|response| {
            if response_has_output_evidence(response) {
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
        })
}

pub(super) fn automatic_retry_class(
    failure: &LlmTransportError,
    position: ProtocolPosition,
    guarantee: GenerationRetryGuarantee,
) -> Option<AutomaticRetryClass> {
    if matches!(
        failure.retry_verdict,
        TransportRetryVerdict::NotRetryable | TransportRetryVerdict::Forbidden
    ) {
        return None;
    }
    if guarantee != GenerationRetryGuarantee::None {
        return Some(AutomaticRetryClass::ProviderGuarantee(guarantee));
    }

    match position {
        ProtocolPosition::NoResponse => Some(AutomaticRetryClass::NoResponse),
        ProtocolPosition::ResponseObserved if retryable_http_rejection(failure) => {
            Some(AutomaticRetryClass::RejectedHttpResponse)
        }
        ProtocolPosition::ResponseObserved if empty_stream_partial(failure) => {
            Some(AutomaticRetryClass::EmptyStreamPartial)
        }
        ProtocolPosition::ResponseObserved
        | ProtocolPosition::OutputStarted
        | ProtocolPosition::TerminalObserved => None,
    }
}

fn retryable_http_rejection(failure: &LlmTransportError) -> bool {
    failure.partial_response.is_none()
        && matches!(
            failure.retry_verdict,
            TransportRetryVerdict::RetryableThrottle { .. }
        )
}

pub(super) fn response_has_output_evidence(response: &LlmResponse) -> bool {
    !response.full_text().is_empty()
        || response
            .provider_usage
            .as_ref()
            .is_some_and(crate::llm::types::provider_usage_has_quantities)
        || response.usage != crate::llm::types::LlmUsage::default()
        || response.parts.iter().any(|part| match part {
            crate::llm::types::LlmOutputPart::Text { text, .. } => !text.is_empty(),
            crate::llm::types::LlmOutputPart::Reasoning { text, replay } => {
                !text.is_empty()
                    || replay.as_ref().is_some_and(|replay| {
                        replay.encrypted_content.is_some()
                            || replay.summary.iter().any(|text| !text.is_empty())
                    })
            }
            crate::llm::types::LlmOutputPart::ToolCall { input_json, .. } => !input_json.is_empty(),
        })
}

fn empty_stream_partial(failure: &LlmTransportError) -> bool {
    failure.kind == ProviderFailureKind::Stream
        && failure
            .partial_response
            .as_deref()
            .is_some_and(|partial| !response_has_output_evidence(partial))
}

fn retry_refusal_reason(position: ProtocolPosition) -> &'static str {
    match position {
        ProtocolPosition::OutputStarted => "output_started_without_retry_guarantee",
        ProtocolPosition::ResponseObserved => "response_observed_without_safe_retry_class",
        ProtocolPosition::NoResponse => "no_response_without_safe_retry_class",
        ProtocolPosition::TerminalObserved => "terminal_observed_without_retry_guarantee",
    }
}

fn unsafe_retry_refusal(
    mut failure: LlmTransportError,
    position: ProtocolPosition,
) -> LlmTransportError {
    let original_message = std::mem::take(&mut failure.message);
    let (code, message) = match position {
        ProtocolPosition::OutputStarted => (
            "unsafe_retry_after_output_started",
            format!(
                "provider output was already paid for and cannot be safely regenerated without an idempotency or resume guarantee: {original_message}"
            ),
        ),
        ProtocolPosition::ResponseObserved => (
            "unsafe_retry_after_response_observed",
            format!(
                "the provider response is not in a charge-safe retry class and cannot be safely regenerated: {original_message}"
            ),
        ),
        ProtocolPosition::NoResponse => (
            "unsafe_retry_without_transport_classification",
            format!(
                "the provider failure is not in a charge-safe retry class and cannot be safely regenerated: {original_message}"
            ),
        ),
        ProtocolPosition::TerminalObserved => (
            "unsafe_retry_after_terminal_observed",
            format!(
                "the provider attempt already reached a terminal response and cannot be safely regenerated: {original_message}"
            ),
        ),
    };
    failure.message = message;
    failure.code = Some(code.to_string());
    failure.retry_verdict = TransportRetryVerdict::Forbidden;
    failure
}

fn charge_safety_denial_reason(reason: ChargeSafetyDenialReason) -> &'static str {
    match reason {
        ChargeSafetyDenialReason::GuaranteeRequired => "charge_safety_guarantee_required",
        ChargeSafetyDenialReason::UnsafeRetryLimitExceeded => {
            "charge_safety_unsafe_retry_limit_exceeded"
        }
        ChargeSafetyDenialReason::DuplicateCostLimitExceeded => {
            "charge_safety_duplicate_cost_limit_exceeded"
        }
        ChargeSafetyDenialReason::RetryAfterExceedsCap => "retry_after_exceeds_cap",
    }
}

fn charge_safety_retry_reason(
    reason: ChargeSafetyDenialReason,
    position: ProtocolPosition,
) -> &'static str {
    if reason == ChargeSafetyDenialReason::GuaranteeRequired {
        retry_refusal_reason(position)
    } else {
        charge_safety_denial_reason(reason)
    }
}

fn charge_safety_refusal(
    failure: LlmTransportError,
    position: ProtocolPosition,
    reason: ChargeSafetyDenialReason,
) -> LlmTransportError {
    if reason == ChargeSafetyDenialReason::GuaranteeRequired {
        return unsafe_retry_refusal(failure, position);
    }
    let mut failure = failure;
    let original_message = std::mem::take(&mut failure.message);
    let code = charge_safety_denial_reason(reason);
    failure.message =
        format!("host charge-safety policy denied the retry ({code}): {original_message}");
    failure.code = Some(code.to_string());
    failure.retry_verdict = TransportRetryVerdict::Forbidden;
    failure
}

pub(crate) fn synthetic_terminal_call_record(
    started_at: u64,
    duration: Duration,
    outcome: AttemptOutcome,
    failure: &LlmTransportError,
    retry_budget_consumed: bool,
    protocol_position: ProtocolPosition,
    replay_drops: Vec<crate::ProviderReplayDrop>,
) -> LlmCallRecord {
    let mut attempt = failure_attempt_record(
        1,
        started_at,
        duration,
        failure,
        retry_budget_consumed,
        protocol_position,
        None,
    );
    attempt.outcome = outcome;
    LlmCallRecord {
        call_id: LlmCallId(uuid::Uuid::new_v4().to_string()),
        label: None,
        replay_drops,
        attempts: vec![attempt],
    }
}

fn failure_attempt_record(
    ordinal: u32,
    started_at: u64,
    duration: Duration,
    failure: &LlmTransportError,
    retry_budget_consumed: bool,
    protocol_position: ProtocolPosition,
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
        protocol_position,
        retry_budget_consumed,
        retry_decision,
        error: Some(NormalizedError {
            class: failure.kind.code().to_string(),
            provider_code: failure.code.clone(),
            http_status: failure.status,
            provider_request_id,
            retry_after: failure.retry_after(),
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

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
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
