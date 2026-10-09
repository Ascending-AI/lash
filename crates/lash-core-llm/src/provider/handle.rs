use super::support::*;
use futures_util::FutureExt as _;
use lash_sansio::llm::attachment_delivery::Delivery;
use lash_sansio::{ErrorEnvelope, RetryProgress};
use lash_trace::EmissionPermit;
use lash_trace::telemetry::metrics::TelemetryMetrics;

use super::slot_delivery::{SlotDeliveries, fill_slots, invalidate_rejected};

fn replay_origin_conflict_error(conflict: ProviderReplayOriginConflict) -> LlmTransportError {
    LlmTransportError::new(conflict.to_string())
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(TurnFailureCode::ProviderReplayOriginConflict)
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
    provider_error.code = Some(FailureCode::lash(
        TurnFailureCode::ProviderReplayOriginConflict,
    ));
    provider_error.retry_verdict = TransportRetryVerdict::Forbidden;
    provider_error
}

mod completion;
pub use completion::ProviderCompletionSideband;

/// Component bundle returned by provider factories.
///
/// Admission usage is shared across clones of this bundle, including resolved session bindings
/// and per-turn overrides.
/// A separately constructed bundle starts a separate scope.
/// Each admission reads its provider's current options; changing a clock preserves the shared
/// limiter and accumulated window usage.
/// Reconfiguring concurrency affects future acquisitions; issued permits live until their
/// owners release them.
#[derive(Debug)]
pub struct ProviderComponents {
    pub provider: Box<dyn Provider>,
    pub failure_classifier: Arc<dyn ProviderFailureClassifier>,
    pub rate_limiter: Arc<ProviderRateLimiter>,
}

impl ProviderComponents {
    pub fn new(provider: Box<dyn Provider>) -> Self {
        Self {
            provider,
            failure_classifier: Arc::new(DefaultProviderFailureClassifier),
            rate_limiter: Arc::new(ProviderRateLimiter::new()),
        }
    }

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

    pub fn with_clock(self, clock: Arc<dyn crate::Clock>) -> Self {
        self.rate_limiter.set_clock(clock);
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

    /// Decompose the handle back into the component bundle it was built from.
    /// This is the exact inverse of [`Self::new`]: a host adapter that resolves
    /// its route per request can recover the inner provider without sealing a
    /// second `LlmCallRecord` around the call.
    pub fn into_components(self) -> ProviderComponents {
        self.components
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
        self.components.provider.set_options(options)
    }

    pub fn requires_streaming(&self) -> bool {
        self.components.provider.requires_streaming()
    }

    /// Completes `request` under `budgets`, the execution budgets its caller
    /// recorded, and the default charge-safety policy.
    #[allow(
        clippy::result_large_err,
        reason = "ProviderCompletionError carries the sealed call record for observability; boxing it would push the cost onto every caller"
    )]
    pub async fn complete(
        &mut self,
        request: LlmRequest,
        budgets: lash_sansio::ExecutionBudgets,
        deliveries: &dyn SlotDeliveries,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        self.complete_with_charge_safety(
            request,
            crate::ChargeSafetyPolicy::default(),
            budgets,
            deliveries,
        )
        .await
    }

    /// Completes a request under an explicit live charge-safety policy and
    /// `budgets`, the execution budgets its caller recorded.
    ///
    /// Prefer [`Self::complete`] unless the host has deliberately accepted a
    /// bounded duplicate-billing risk for this call.
    #[allow(
        clippy::result_large_err,
        reason = "ProviderCompletionError carries the sealed call record for observability; boxing it would push the cost onto every caller"
    )]
    pub async fn complete_with_charge_safety(
        &mut self,
        mut request: LlmRequest,
        charge_safety: crate::ChargeSafetyPolicy,
        budgets: lash_sansio::ExecutionBudgets,
        deliveries: &dyn SlotDeliveries,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        let sideband = self.prepare_completion(&mut request);
        let template = match self.lower(&request).await {
            Ok(template) => Arc::new(template),
            Err(error) => return Err(unsent(&request.scope, &sideband, error)),
        };
        self.complete_prepared(
            ResponseContext::of_request(&request),
            &template,
            deliveries,
            sideband,
            charge_safety,
            &TelemetryMetrics::default(),
            None,
            ModelCallBounds::unnested(budgets),
        )
        .await
    }

    /// Lower `request`, as [`Self::prepare_completion`] left it, to the
    /// request template its route sends (ADR 0133 §6, ADR 0135 §6): its
    /// exact literal JSON with one slot per attachment. A durable call
    /// lowers once, before its admission, and every attempt and resend
    /// fills and sends the template. A provider that panics while lowering
    /// is contained as a typed, non-retryable failure; a template that is
    /// not valid, or one lowered for another route, is refused.
    ///
    /// # Errors
    ///
    /// The provider's classified refusal, an invalid endpoint, a contained
    /// panic, an invalid template, or a template for another route.
    pub async fn lower(
        &mut self,
        request: &LlmRequest,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        let route = self.route_identity(request.model.wire_model());
        route.validate_endpoint().map_err(|error| {
            LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
                .with_retry_verdict(TransportRetryVerdict::Forbidden)
        })?;
        let lowered =
            std::panic::AssertUnwindSafe(async { self.components.provider.lower(request).await })
                .catch_unwind()
                .await;
        let template = match lowered {
            Ok(lowered) => {
                lowered.map_err(|failure| self.components.failure_classifier.classify(failure))?
            }
            Err(payload) => {
                return Err(
                    LlmTransportError::new(crate::panic_containment::payload_message(
                        payload.as_ref(),
                    ))
                    .with_kind(ProviderFailureKind::Unknown)
                    .with_lash_code(TurnFailureCode::ProviderPanicked)
                    .with_retry_verdict(TransportRetryVerdict::NotRetryable),
                );
            }
        };
        check_body_route(&template, &route)?;
        Ok(template)
    }

    pub(crate) fn prepare_completion(
        &self,
        request: &mut LlmRequest,
    ) -> ProviderCompletionSideband {
        let serving_route = self.route_identity(request.model.wire_model());
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

    #[expect(
        clippy::expect_used,
        reason = "the Backoff verdict selects the delay in the same match arm that schedules the retry, \
                  so it is always Some exactly when this code runs"
    )]
    #[allow(
        clippy::result_large_err,
        reason = "ProviderCompletionError carries the sealed call record for observability; boxing it would push the cost onto every caller"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "the admitted send: its template, deliveries and sideband with the call's policy and bounds"
    )]
    pub(crate) async fn complete_prepared(
        &mut self,
        mut context: ResponseContext,
        template: &Arc<RecordedRequestTemplate>,
        deliveries: &dyn SlotDeliveries,
        sideband: ProviderCompletionSideband,
        charge_safety: crate::ChargeSafetyPolicy,
        metrics: &TelemetryMetrics,
        permit: Option<&EmissionPermit>,
        bounds: ModelCallBounds,
    ) -> Result<ProviderCompletion, ProviderCompletionError> {
        let serving_route = sideband.serving_route();
        if let Err(error) = serving_route.validate_endpoint() {
            let error = LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
                .with_retry_verdict(TransportRetryVerdict::Forbidden);
            return Err(unsent(&context.scope, &sideband, error));
        }
        // The template is sent as it was lowered, and only by its own route.
        if let Err(error) = check_body_route(template, &serving_route) {
            return Err(unsent(&context.scope, &sideband, error));
        }
        // The template decides whether the response streams: a streamed
        // body is read through a sender even when the caller asked for none.
        match (template.stream(), context.stream_events.is_some()) {
            (true, false) => {
                context.stream_events = Some(crate::llm::types::LlmEventSender::new(|_| {}));
            }
            (false, true) => context.stream_events = None,
            _ => {}
        }
        let call_id = call_id_for_scope(&context.scope);
        // Every bound the route leaves unset is the runtime's; one it sets
        // above the runtime's is refused, never clipped.
        let provider_limits = bounds.budgets.provider();
        let route = self.options().reliability;
        let reliability = match route.within(&provider_limits) {
            Ok(reliability) => reliability,
            Err(refused) => {
                let error = LlmTransportError::new(refused.to_string())
                    .with_kind(ProviderFailureKind::Validation)
                    .with_lash_code(TurnFailureCode::ProviderRouteAboveBudget)
                    .with_retry_verdict(TransportRetryVerdict::Forbidden);
                return Err(unsent(&context.scope, &sideband, error));
            }
        };
        let attempts = reliability
            .retry
            .max_attempts
            .unwrap_or_else(|| provider_limits.max_attempts());
        // The model total is a hard cap over throttle, backoff and every
        // attempt (spec v3 L-C2). The limit is minted on lash's clock; the
        // granted remainder is then enforced on the monotonic clock.
        let clock = self.components.rate_limiter.clock();
        let limit = bounds
            .budgets
            .model_call_limit(clock.timestamp_ms(), bounds.enclosing.as_ref());
        let deadline = clock.now() + limit.remaining(clock.timestamp_ms());
        // A delivered URL or file id must outlive every attempt the total
        // allows, with its recorded slack for the provider to fetch it (ADR 0135 §4).
        let valid_through_ms = limit
            .expires_at
            .saturating_add(template.fetch_horizon.millis);
        let remaining = |clock: &dyn crate::Clock| deadline.saturating_duration_since(clock.now());
        let mut budget = RetryBudget::default();
        loop {
            let attempt_ordinal = sideband.next_attempt_ordinal();
            if remaining(clock.as_ref()).is_zero() {
                return Err(model_total_exceeded(
                    call_id,
                    &sideband,
                    attempt_ordinal,
                    limit,
                    None,
                ));
            }
            // A throttle window that would outlast the total settles the
            // call now instead of being waited out.
            let Some(_permit) = self
                .components
                .rate_limiter
                .admit_within(self.components.provider.as_ref(), template, deadline)
                .await
            else {
                return Err(model_total_exceeded(
                    call_id,
                    &sideband,
                    attempt_ordinal,
                    limit,
                    None,
                ));
            };
            sideband.begin_attempt(self.kind(), context.model().wire_model());
            // Every bound of this attempt is clipped to what remains of the
            // total; the route's own bounds come back after.
            let window = reliability.for_window(remaining(clock.as_ref()));
            let mut attempt_options = self.options();
            attempt_options.reliability.request_timeout = window.request_timeout;
            attempt_options.reliability.response_start_timeout = window.response_start_timeout;
            attempt_options.reliability.chunk_timeout = window.chunk_timeout;
            self.components.provider.set_options(attempt_options);
            let attempt = {
                // The call is built inside the caught future: a provider
                // that panics while constructing its future is contained.
                // Each attempt's context names its ordinal in its scope. Its
                // slots are delivered and encoded afresh inside the same
                // envelope, under the call's deadline and cancellation.
                let attempt = std::panic::AssertUnwindSafe(async {
                    let mut attempt_context = context.clone();
                    attempt_context.scope.attempt = Some(attempt_ordinal);
                    let provider = &mut self.components.provider;
                    match fill_slots(provider.as_ref(), template, deliveries, valid_through_ms)
                        .await
                    {
                        Ok((live, delivered)) => AttemptSend::Sent {
                            result: Box::new(provider.send(&live, attempt_context).await),
                            delivered,
                        },
                        Err(error) => AttemptSend::Unsent(error),
                    }
                })
                .catch_unwind();
                let expiry = clock.sleep_until(deadline);
                futures_util::pin_mut!(attempt, expiry);
                match futures_util::future::select(attempt, expiry).await {
                    futures_util::future::Either::Left((attempt, _)) => Some(attempt),
                    futures_util::future::Either::Right(((), _)) => None,
                }
            };
            let mut restored = self.options();
            restored.reliability.request_timeout = route.request_timeout;
            restored.reliability.response_start_timeout = route.response_start_timeout;
            restored.reliability.chunk_timeout = route.chunk_timeout;
            self.components.provider.set_options(restored);
            let Some(attempt) = attempt else {
                return Err(model_total_exceeded(
                    call_id,
                    &sideband,
                    attempt_ordinal,
                    limit,
                    Some(ProtocolPosition::NoResponse),
                ));
            };
            let (mut result, delivered, unsent_attempt, panic_payload) = match attempt {
                Ok(AttemptSend::Sent { result, delivered }) => (*result, delivered, false, None),
                Ok(AttemptSend::Unsent(error)) => (Err(error), Vec::new(), true, None),
                Err(payload) => {
                    let message = crate::panic_containment::payload_message(payload.as_ref());
                    (
                        Err(LlmTransportError::new(message)
                            .with_kind(ProviderFailureKind::Unknown)
                            .with_lash_code(TurnFailureCode::ProviderPanicked)
                            .with_retry_verdict(TransportRetryVerdict::NotRetryable)),
                        Vec::new(),
                        false,
                        Some(payload),
                    )
                }
            };
            // Classify provider-owned failures before applying Lash's replay
            // contract. A classifier must never reinterpret the synthetic,
            // non-retryable origin-conflict result from the fence below, nor
            // a delivery failure, which is Lash's own and reached no provider.
            if panic_payload.is_none() && !unsent_attempt {
                result =
                    result.map_err(|failure| self.components.failure_classifier.classify(failure));
            }
            // A provider that definitely rejected delivered slots (a missing
            // or expired file id) refused before generating: the
            // rejected deliveries are forgotten and the attempt may be made
            // again with fresh ones. Authentication is never a rejection.
            if let Err(failure) = &mut result
                && !failure.rejected_slots().is_empty()
            {
                invalidate_rejected(template, deliveries, &delivered, failure.rejected_slots())
                    .await;
                if failure.kind != ProviderFailureKind::Auth {
                    failure.retry_verdict = TransportRetryVerdict::RetryableTransient;
                }
            }
            drop(delivered);
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
                    let usage = response
                        .provider_usage
                        .as_ref()
                        .map(|_| response.usage.clone());
                    sideband.seal_attempt(AttemptRecord {
                        ordinal: attempt_ordinal,
                        outcome,
                        protocol_position: success_protocol_position(&response, outcome),
                        retry_budget_consumed: true,
                        retry_decision: None,
                        error: None,
                        evidence: response.execution_evidence.clone(),
                        generation_disposition: response.generation_disposition,
                        usage: usage.clone(),
                    });
                    return Ok(ProviderCompletion {
                        response,
                        call_record: sideband.call_record(call_id),
                    });
                }
                Err(failure) => {
                    // The outer error is Lash's typed conflict classification;
                    // the sealed attempt remains the provider's original
                    // failure evidence (kind, code, and status).
                    let recorded_failure = original_failure.as_ref().unwrap_or(&failure);
                    let protocol_position = failure_protocol_position(&failure);
                    let retry_guarantee = self
                        .components
                        .provider
                        .generation_retry_guarantee(&context, template);
                    let (verdict, charge_safety_decision) = retry_verdict(
                        &failure,
                        protocol_position,
                        retry_guarantee,
                        &reliability.retry,
                        attempts,
                        &charge_safety,
                        &budget,
                    );
                    let retry_class =
                        automatic_retry_class(&failure, protocol_position, retry_guarantee);
                    let throttle_wait =
                        budget.throttle_wait(&reliability.retry, failure.retry_verdict);
                    let counted_retry_available = budget.attempt + 1 < attempts;
                    if let Some(ChargeSafetyDecision::Denied { reason, .. }) =
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
                            http_status = ?failure.http_status,
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
                            reason = %charge_safety_retry_reason(reason, protocol_position),
                            "provider retry denied because another generation is not proven charge-safe"
                        );
                    }

                    let (decision, consumed) = match verdict {
                        RetryVerdict::Declined(cause) => (RetryDecision::Declined(cause), true),
                        RetryVerdict::Throttle { wait, class } => (
                            RetryDecision::Scheduled {
                                delay: wait,
                                wait: RetryWait::Throttle,
                                class,
                            },
                            false,
                        ),
                        RetryVerdict::Backoff { class } => (
                            RetryDecision::Scheduled {
                                delay: reliability
                                    .retry
                                    .delay_for_attempt(budget.attempt, failure.retry_after())
                                    .expect(
                                        "Retry-After was checked against the cap before scheduling",
                                    ),
                                wait: RetryWait::Backoff,
                                class,
                            },
                            true,
                        ),
                    };
                    // A wait that would outlast the total skips every
                    // further attempt: the call settles as timed out now.
                    let (decision, verdict) = match decision.delay() {
                        Some(delay) if delay >= remaining(clock.as_ref()) => {
                            let cause = RetryDeclineCause::TimedOut {
                                limit: lash_sansio::LimitCause::ExecutionTotal,
                            };
                            (
                                RetryDecision::Declined(cause),
                                RetryVerdict::Declined(cause),
                            )
                        }
                        _ => (decision, verdict),
                    };
                    let delay = decision.delay();
                    let unsafe_retry = charge_safety_decision.is_some();
                    sideband.seal_attempt(failure_attempt_record(
                        attempt_ordinal,
                        recorded_failure,
                        consumed,
                        protocol_position,
                        Some(decision),
                    ));
                    match verdict {
                        RetryVerdict::Declined(cause) => {
                            let error = match cause {
                                RetryDeclineCause::ChargeSafety { reason, .. } => {
                                    charge_safety_refusal(failure, protocol_position, reason)
                                }
                                RetryDeclineCause::TimedOut { .. } => {
                                    model_total_error(limit, Some(&failure))
                                }
                                RetryDeclineCause::NotRetryable
                                | RetryDeclineCause::RetryBudgetExhausted
                                | RetryDeclineCause::RetryAfterExceedsCap => failure,
                            };
                            let completion_error = ProviderCompletionError {
                                error,
                                call_record: Box::new(sideband.call_record(call_id)),
                            };
                            if matches!(
                                cause,
                                RetryDeclineCause::NotRetryable
                                    | RetryDeclineCause::RetryBudgetExhausted
                            ) && let Some(payload) = panic_payload
                            {
                                crate::panic_containment::enforce_loudness(payload);
                            }
                            return Err(completion_error);
                        }
                        RetryVerdict::Throttle { wait, class } => {
                            budget.charge_throttle(wait, unsafe_retry);
                            crate::operational_metrics::record_provider_retry(
                                metrics,
                                permit,
                                self.kind(),
                                "throttle",
                            );
                            crate::operational_metrics::record_provider_throttle_wait(
                                metrics,
                                permit,
                                self.kind(),
                                wait,
                            );
                            tracing::debug!(
                                target: "lash_core::provider::reliability",
                                provider = self.kind(), attempt = budget.attempt + 1,
                                max_attempts = attempts, wait_ms = wait.as_millis() as u64,
                                throttle_waited_ms = budget.throttle_waited.as_millis() as u64,
                                err = %failure.message,
                                "provider throttled with retry-after; waiting without consuming a retry attempt"
                            );
                            announce_retry(
                                &context,
                                class,
                                wait,
                                budget.attempt,
                                attempts,
                                &failure,
                            );
                            self.components.rate_limiter.clock().sleep(wait).await;
                        }
                        RetryVerdict::Backoff { class } => {
                            let delay = delay.expect("backoff delay was selected before sealing");
                            crate::operational_metrics::record_provider_retry(
                                metrics,
                                permit,
                                self.kind(),
                                "backoff",
                            );
                            tracing::debug!(
                                target: "lash_core::provider::reliability",
                                provider = self.kind(), attempt = budget.attempt + 1,
                                max_attempts = attempts, delay_ms = delay.as_millis() as u64,
                                err = %failure.message,
                                "provider call failed with retryable failure; sleeping before retry"
                            );
                            announce_retry(
                                &context,
                                class,
                                delay,
                                budget.attempt,
                                attempts,
                                &failure,
                            );
                            self.components.rate_limiter.clock().sleep(delay).await;
                            budget.consume(unsafe_retry);
                        }
                    }
                }
            }
        }
    }

    /// Release the underlying provider's host-visible transport resources.
    ///
    /// Hosts that want a graceful transport shutdown (for example, sending WebSocket Close
    /// frames on cached Codex sessions) retain a clone of the handle they hand to the core and
    /// call this before process exit.
    /// Providers with no reusable transport state close as a no-op.
    pub async fn close(&self) -> Result<(), LlmTransportError> {
        std::panic::AssertUnwindSafe(async { self.components.provider.close().await })
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
        .with_lash_code(TurnFailureCode::ProviderPanicked)
        .with_retry_verdict(TransportRetryVerdict::NotRetryable));
    crate::panic_containment::enforce_loudness(payload);
    failure
}

/// The bounds one model call runs under: the runtime's execution budgets
/// and, for a call nested in another executable stretch, that stretch's
/// limit, which clips the call's total (spec v3 L-C2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallBounds {
    pub budgets: lash_sansio::ExecutionBudgets,
    pub enclosing: Option<lash_sansio::ExecutionLimit>,
}

impl ModelCallBounds {
    /// The bounds of a call no other stretch encloses: `budgets` alone.
    #[must_use]
    pub fn unnested(budgets: lash_sansio::ExecutionBudgets) -> Self {
        Self {
            budgets,
            enclosing: None,
        }
    }
}

/// The typed model timeout a call settles with once its total expires.
fn model_total_error(
    limit: lash_sansio::ExecutionLimit,
    last_failure: Option<&LlmTransportError>,
) -> LlmTransportError {
    let mut message = format!(
        "the model call reached its total limit (expired at {} ms)",
        limit.expires_at
    );
    if let Some(failure) = last_failure {
        message = format!("{message}; last provider failure: {}", failure.message);
    }
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Timeout)
        .with_lash_code(TurnFailureCode::ModelTotalExceeded)
        .with_retry_verdict(TransportRetryVerdict::NotRetryable)
}

/// Settle a call whose total expired before or during attempt `ordinal`:
/// `TimedOut { ExecutionTotal }`. An attempt cut while it ran is recorded at
/// `cut_at`; a call that expired between attempts records none.
fn model_total_exceeded(
    call_id: LlmCallId,
    sideband: &ProviderCompletionSideband,
    ordinal: u32,
    limit: lash_sansio::ExecutionLimit,
    cut_at: Option<ProtocolPosition>,
) -> ProviderCompletionError {
    let error = model_total_error(limit, None);
    if let Some(position) = cut_at {
        sideband.seal_attempt(failure_attempt_record(
            ordinal,
            &error,
            true,
            position,
            Some(RetryDecision::Declined(RetryDeclineCause::TimedOut {
                limit: lash_sansio::LimitCause::ExecutionTotal,
            })),
        ));
    }
    ProviderCompletionError {
        error,
        call_record: Box::new(sideband.call_record(call_id)),
    }
}

fn success_outcome(reason: LlmTerminalReason) -> AttemptOutcome {
    match reason {
        LlmTerminalReason::Cancelled => AttemptOutcome::Aborted,
        LlmTerminalReason::Unknown => AttemptOutcome::Interrupted,
        LlmTerminalReason::Stop
        | LlmTerminalReason::ToolUse
        | LlmTerminalReason::OutputLimit
        | LlmTerminalReason::ContextOverflow
        | LlmTerminalReason::ContentFilter
        | LlmTerminalReason::ProviderError => AttemptOutcome::Completed,
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

fn resets_stream(class: RetryClass) -> bool {
    match class {
        RetryClass::NoResponse
        | RetryClass::RejectedHttpResponse
        | RetryClass::EmptyStreamPartial
        | RetryClass::ProviderIdempotency => true,
        RetryClass::ProviderResume | RetryClass::ChargeAuthorized { .. } => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryVerdict {
    Declined(RetryDeclineCause),
    Throttle { wait: Duration, class: RetryClass },
    Backoff { class: RetryClass },
}

/// Policy only: no clocks, random jitter, provider calls, or attempt writes.
fn retry_verdict(
    failure: &LlmTransportError,
    position: ProtocolPosition,
    guarantee: GenerationRetryGuarantee,
    policy: &ProviderRetryPolicy,
    attempts: u32,
    charge_safety: &crate::ChargeSafetyPolicy,
    budget: &RetryBudget,
) -> (RetryVerdict, Option<ChargeSafetyDecision>) {
    let automatic = automatic_retry_class(failure, position, guarantee);
    let exceeds_cap = failure
        .retry_after()
        .is_some_and(|wait| policy.retry_after_within_cap(wait).is_none());
    let charge = if failure.is_retryable() && automatic.is_none() {
        match charge_safety_decision(
            failure.retry_verdict,
            guarantee,
            charge_safety,
            failure
                .partial_response
                .as_deref()
                .map(|response| &response.usage),
            budget.unsafe_retries.saturating_add(1),
        ) {
            ChargeSafetyEvaluation::Evaluated(decision) => Some(decision),
            ChargeSafetyEvaluation::NotEvaluated(_) => None,
        }
    } else {
        None
    };
    if let Some(ChargeSafetyDecision::Denied {
        tokens_at_stake,
        attempt_number,
        reason,
    }) = charge.as_ref()
    {
        return (
            RetryVerdict::Declined(RetryDeclineCause::ChargeSafety {
                tokens_at_stake: *tokens_at_stake,
                attempt_number: *attempt_number,
                reason: *reason,
            }),
            charge,
        );
    }
    if failure.is_retryable() && exceeds_cap {
        return (
            RetryVerdict::Declined(RetryDeclineCause::RetryAfterExceedsCap),
            charge,
        );
    }
    if !failure.is_retryable() {
        return (
            RetryVerdict::Declined(RetryDeclineCause::NotRetryable),
            charge,
        );
    }
    let class = match automatic {
        Some(class) => class,
        None => match charge.as_ref() {
            Some(ChargeSafetyDecision::Authorized {
                tokens_at_stake,
                attempt_number,
            }) => RetryClass::ChargeAuthorized {
                tokens_at_stake: *tokens_at_stake,
                attempt_number: *attempt_number,
            },
            Some(ChargeSafetyDecision::Denied { .. }) | None => {
                unreachable!("retryable failure requires retry permission")
            }
        },
    };
    if let Some(wait) = budget.throttle_wait(policy, failure.retry_verdict) {
        return (RetryVerdict::Throttle { wait, class }, charge);
    }
    if budget.attempt + 1 >= attempts {
        return (
            RetryVerdict::Declined(RetryDeclineCause::RetryBudgetExhausted),
            charge,
        );
    }
    (RetryVerdict::Backoff { class }, charge)
}

fn announce_retry(
    context: &ResponseContext,
    class: RetryClass,
    delay: Duration,
    attempt: u32,
    attempts: u32,
    failure: &LlmTransportError,
) {
    if let Some(events) = context.stream_events.as_ref() {
        if resets_stream(class) {
            events.send(crate::llm::types::LlmStreamEvent::AttemptReset);
        }
        events.send(crate::llm::types::LlmStreamEvent::RetryStatus(
            RetryProgress {
                wait_seconds: delay.as_secs(),
                attempt: (attempt + 1) as usize,
                max_attempts: attempts as usize,
                reason: failure.message.clone(),
                envelope: Some(ErrorEnvelope {
                    kind: crate::TurnFailureKind::LlmProvider,
                    code: failure.code.clone(),
                    terminal_reason: Some(failure.terminal_reason),
                    user_message: failure.message.clone(),
                    raw: failure.raw.as_deref().cloned(),
                    retryable: Some(failure.is_retryable()),
                    provider_failure_kind: (failure.kind != ProviderFailureKind::Unknown)
                        .then_some(failure.kind),
                }),
            },
        ));
    }
}

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
            if attempt_number > *max_unsafe_retries {
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
            if failure.http_status.is_some() {
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
) -> Option<RetryClass> {
    if matches!(
        failure.retry_verdict,
        TransportRetryVerdict::NotRetryable | TransportRetryVerdict::Forbidden
    ) {
        return None;
    }
    match guarantee {
        GenerationRetryGuarantee::Idempotent => return Some(RetryClass::ProviderIdempotency),
        GenerationRetryGuarantee::Resumable => return Some(RetryClass::ProviderResume),
        GenerationRetryGuarantee::None => {}
    }

    match position {
        ProtocolPosition::NoResponse => Some(RetryClass::NoResponse),
        ProtocolPosition::ResponseObserved
            if retryable_http_rejection(failure) || rejected_delivery(failure) =>
        {
            Some(RetryClass::RejectedHttpResponse)
        }
        ProtocolPosition::ResponseObserved if empty_stream_partial(failure) => {
            Some(RetryClass::EmptyStreamPartial)
        }
        ProtocolPosition::ResponseObserved
        | ProtocolPosition::OutputStarted
        | ProtocolPosition::TerminalObserved => None,
    }
}

/// A provider's refusal of delivered slots, before any output: retrying
/// with fresh deliveries cannot buy a second generation.
fn rejected_delivery(failure: &LlmTransportError) -> bool {
    failure.partial_response.is_none() && !failure.rejected_slots().is_empty()
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
    let message = match position {
        ProtocolPosition::OutputStarted => format!(
            "provider output was already paid for and cannot be safely regenerated without an idempotency or resume guarantee: {original_message}"
        ),
        ProtocolPosition::ResponseObserved => format!(
            "the provider response is not in a charge-safe retry class and cannot be safely regenerated: {original_message}"
        ),
        ProtocolPosition::NoResponse => format!(
            "the provider failure is not in a charge-safe retry class and cannot be safely regenerated: {original_message}"
        ),
        ProtocolPosition::TerminalObserved => format!(
            "the provider attempt already reached a terminal response and cannot be safely regenerated: {original_message}"
        ),
    };
    failure.message = message;
    failure.code = Some(FailureCode::lash(
        ChargeSafetyDenialReason::GuaranteeRequired.failure_code(position),
    ));
    failure.retry_verdict = TransportRetryVerdict::Forbidden;
    failure
}

fn charge_safety_retry_reason(
    reason: ChargeSafetyDenialReason,
    position: ProtocolPosition,
) -> String {
    if reason == ChargeSafetyDenialReason::GuaranteeRequired {
        retry_refusal_reason(position).to_owned()
    } else {
        reason.failure_code(position).as_str().to_owned()
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
    let code = reason.failure_code(position);
    failure.message = format!(
        "host charge-safety policy denied the retry ({}): {original_message}",
        code.as_str()
    );
    failure.code = Some(FailureCode::lash(code));
    failure.retry_verdict = TransportRetryVerdict::Forbidden;
    failure
}

/// A call record's id is the request scope's caller-owned request id: the
/// scope already carries one identity per logical provider call, so sealing
/// the same request again yields the same `call_id` instead of a fresh uuid.
pub fn call_id_for_scope(scope: &LlmRequestScope) -> LlmCallId {
    LlmCallId(scope.request_id.clone())
}

#[allow(
    clippy::too_many_arguments,
    reason = "each argument is one field of the sealed record; bundling them would only rename the same list"
)]
pub fn synthetic_terminal_call_record(
    call_id: LlmCallId,
    outcome: AttemptOutcome,
    failure: &LlmTransportError,
    retry_budget_consumed: bool,
    protocol_position: ProtocolPosition,
    replay_drops: Vec<crate::ProviderReplayDrop>,
) -> LlmCallRecord {
    let mut attempt =
        failure_attempt_record(1, failure, retry_budget_consumed, protocol_position, None);
    attempt.outcome = outcome;
    LlmCallRecord {
        call_id,
        label: None,
        replay_drops,
        attempts: vec![attempt],
    }
}

fn failure_attempt_record(
    ordinal: u32,
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
    let outcome = match failure.terminal_reason {
        LlmTerminalReason::Cancelled => AttemptOutcome::Aborted,
        LlmTerminalReason::Stop
        | LlmTerminalReason::ToolUse
        | LlmTerminalReason::OutputLimit
        | LlmTerminalReason::ContextOverflow
        | LlmTerminalReason::ContentFilter
        | LlmTerminalReason::ProviderError
        | LlmTerminalReason::Unknown => match failure.kind {
            ProviderFailureKind::Timeout | ProviderFailureKind::Stream => {
                AttemptOutcome::Interrupted
            }
            ProviderFailureKind::Transport
            | ProviderFailureKind::Http
            | ProviderFailureKind::Auth
            | ProviderFailureKind::Validation
            | ProviderFailureKind::Quota
            | ProviderFailureKind::Unsupported
            | ProviderFailureKind::Unknown => AttemptOutcome::Failed,
        },
    };
    let usage = partial.and_then(|response| {
        (response.provider_usage.is_some()
            || response.usage != crate::llm::types::LlmUsage::default())
        .then(|| response.usage.clone())
    });
    AttemptRecord {
        ordinal,
        outcome,
        protocol_position,
        retry_budget_consumed,
        retry_decision,
        error: Some(NormalizedError {
            class: failure.kind,
            code: failure.code.clone(),
            http_status: failure.http_status,
            provider_request_id,
            retry_after: failure.retry_after(),
        }),
        evidence,
        generation_disposition: partial.and_then(|response| response.generation_disposition),
        usage,
    }
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
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

    async fn send(
        &mut self,
        _body: &LiveRequestBody,
        _context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        Err(LlmTransportError::new(
            "no provider configured: host must set SessionPolicy.provider before running a turn",
        ))
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
/// Detaches the replay-safety and sealed-attempt sideband from `request` before `handle` serves
/// it: the runtime's turn driver prepares the request, spawns the completion,
/// and reads the sideband however the task ends. The runtime's seam;
/// `core_internal` re-exports it and the `lash` facade does not.
pub fn prepare_completion(
    handle: &ProviderHandle,
    request: &mut LlmRequest,
) -> ProviderCompletionSideband {
    handle.prepare_completion(request)
}

/// Sends `template`, an admitted call's request template, under the
/// sideband [`prepare_completion`] made for it: every attempt sends its
/// literals with its slots filled afresh through `deliveries`, and reads
/// its response under `context`, the call's recorded response context with
/// this send's live senders. No request reaches the provider.
#[allow(
    clippy::result_large_err,
    reason = "ProviderCompletionError carries the sealed call record for observability; boxing it would push the cost onto every caller"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "the runtime's one seam into the handle's admitted send"
)]
pub async fn complete_prepared(
    handle: &mut ProviderHandle,
    context: ResponseContext,
    template: &Arc<RecordedRequestTemplate>,
    deliveries: &dyn SlotDeliveries,
    sideband: ProviderCompletionSideband,
    charge_safety: crate::ChargeSafetyPolicy,
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    bounds: ModelCallBounds,
) -> Result<ProviderCompletion, ProviderCompletionError> {
    handle
        .complete_prepared(
            context,
            template,
            deliveries,
            sideband,
            charge_safety,
            metrics,
            permit,
            bounds,
        )
        .await
}

/// Refuse `template` unless `route` lowered it: an admitted template is
/// sent only by the route that lowered it, never rebuilt for another.
fn check_body_route(
    template: &RecordedRequestTemplate,
    route: &ProviderRouteIdentity,
) -> Result<(), LlmTransportError> {
    if template.route == *route {
        return Ok(());
    }
    Err(LlmTransportError::new(format!(
        "the request template was lowered for route {}:{}:{}, not the serving route {}:{}:{}",
        template.route.provider,
        template.route.endpoint,
        template.route.model,
        route.provider,
        route.endpoint,
        route.model
    ))
    .with_kind(ProviderFailureKind::Validation)
    .with_lash_code(TurnFailureCode::AdmittedRequestUnavailable)
    .with_retry_verdict(TransportRetryVerdict::Forbidden))
}

/// What one attempt did: sent its filled template, keeping the deliveries
/// until the provider answered, or failed to fill it and sent nothing.
enum AttemptSend {
    Sent {
        result: Box<Result<LlmResponse, LlmTransportError>>,
        delivered: Vec<Arc<Delivery>>,
    },
    Unsent(LlmTransportError),
}

/// The failure of a call refused before its first attempt: nothing was sent.
fn unsent(
    scope: &LlmRequestScope,
    sideband: &ProviderCompletionSideband,
    error: LlmTransportError,
) -> ProviderCompletionError {
    ProviderCompletionError {
        call_record: Box::new(synthetic_terminal_call_record(
            call_id_for_scope(scope),
            AttemptOutcome::Failed,
            &error,
            false,
            ProtocolPosition::NoResponse,
            sideband.replay_drops(),
        )),
        error,
    }
}

#[cfg(test)]
mod retry_verdict_tests {
    use super::*;

    #[test]
    fn retry_verdict_table_needs_no_provider() {
        let policy = ProviderRetryPolicy::default();
        let failure = LlmTransportError::new("failure")
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient);
        for position in [
            ProtocolPosition::NoResponse,
            ProtocolPosition::ResponseObserved,
            ProtocolPosition::OutputStarted,
            ProtocolPosition::TerminalObserved,
        ] {
            for guarantee in [
                GenerationRetryGuarantee::None,
                GenerationRetryGuarantee::Idempotent,
                GenerationRetryGuarantee::Resumable,
            ] {
                for attempt in [0, 1] {
                    let mut budget = RetryBudget::default();
                    for _ in 0..attempt {
                        budget.consume(false);
                    }
                    let (verdict, _) = retry_verdict(
                        &failure,
                        position,
                        guarantee,
                        &policy,
                        2,
                        &crate::ChargeSafetyPolicy::RequireGuarantee,
                        &budget,
                    );
                    if position != ProtocolPosition::NoResponse
                        && guarantee == GenerationRetryGuarantee::None
                    {
                        assert!(
                            matches!(
                                verdict,
                                RetryVerdict::Declined(RetryDeclineCause::ChargeSafety { .. })
                            ),
                            "{position:?}/{guarantee:?}/{attempt}: {verdict:?}"
                        );
                    } else if attempt == 1 {
                        assert_eq!(
                            verdict,
                            RetryVerdict::Declined(RetryDeclineCause::RetryBudgetExhausted)
                        );
                    } else {
                        assert!(matches!(
                            verdict,
                            RetryVerdict::Backoff {
                                class: RetryClass::NoResponse
                                    | RetryClass::RejectedHttpResponse
                                    | RetryClass::EmptyStreamPartial
                                    | RetryClass::ProviderIdempotency
                                    | RetryClass::ProviderResume
                            }
                        ));
                    }
                }
            }
        }
    }
}
