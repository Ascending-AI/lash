//! Own one dispatched future through its return or bounded abandonment.
use super::super::traits::{AttemptTerminal, ProviderAttempt, ProviderAttemptControl};
use super::*;

pub(super) struct AttemptHorizon<'a> {
    pub deadline: std::time::Instant,
    pub valid_through_ms: u64,
    pub clock: &'a dyn crate::Clock,
    pub cancellation: &'a ProviderAttemptControl,
    pub terminal_policy: ProviderTerminalPolicy,
    pub metrics: &'a TelemetryMetrics,
}

pub(super) struct AttemptExecution {
    pub result: Option<Result<AttemptSend, Box<dyn std::any::Any + Send>>>,
    pub control: ProviderAttemptControl,
    pub delivery_timed_out: bool,
}

pub(super) async fn dispatch_attempt(
    provider: &mut dyn Provider,
    mut context: ResponseContext,
    ordinal: u32,
    template: &Arc<RecordedRequestTemplate>,
    deliveries: &dyn SlotDeliveries,
    horizon: AttemptHorizon<'_>,
) -> AttemptExecution {
    let control = ProviderAttemptControl::default();
    context.attempt_control = control.clone();
    context.scope.attempt = Some(ordinal);
    if let Some(downstream) = context.stream_events.take() {
        let retention = control.clone();
        context.stream_events = Some(crate::llm::types::LlmEventSender::new(move |event| {
            retention.observe(&event);
            downstream.send(event);
        }));
    }
    let identity = ProviderAttempt {
        call_id: call_id_for_scope(&context.scope),
        ordinal,
        scope: context.scope.clone(),
    };
    let mut dispatched = false;
    let (result, abandoned_reason) = {
        // Construct and poll the send inside containment. A panic, unlike a
        // returned Err, still owes the decorator a terminal.
        let attempt = std::panic::AssertUnwindSafe(async {
            match fill_slots(provider, template, deliveries, horizon.valid_through_ms).await {
                Ok((live, delivered)) => {
                    if control.reason().is_some() || horizon.cancellation.reason().is_some() {
                        return AttemptSend::Unsent(
                            LlmTransportError::new("cancelled before provider dispatch")
                                .with_terminal_reason(LlmTerminalReason::Cancelled)
                                .with_retry_verdict(TransportRetryVerdict::NotRetryable),
                        );
                    }
                    dispatched = true;
                    AttemptSend::Sent {
                        result: Box::new(provider.send(&live, context).await),
                        delivered,
                    }
                }
                Err(error) => AttemptSend::Unsent(error),
            }
        })
        .catch_unwind();
        let stop = async {
            tokio::select! {
                _ = horizon.clock.sleep_until(horizon.deadline) => AttemptEndReason::ModelDeadline,
                reason = horizon.cancellation.cancelled() => reason,
            }
        };
        futures_util::pin_mut!(attempt, stop);
        let (result, stopped) = match futures_util::future::select(attempt.as_mut(), stop).await {
            futures_util::future::Either::Left((attempt, _)) => (Some(attempt), None),
            futures_util::future::Either::Right((reason, _)) => {
                control.cancel(reason);
                (
                    tokio::time::timeout(horizon.terminal_policy.drain_timeout, attempt)
                        .await
                        .ok(),
                    Some(reason),
                )
            }
        };
        let abandoned_reason = match &result {
            Some(Err(_)) => Some(AttemptEndReason::ProviderPanicked),
            None => stopped,
            Some(Ok(_)) => None,
        };
        (result, abandoned_reason)
    };
    // All borrows held by the send are gone before re-entering this exact
    // instance. A completed send, including a returned Err, gets no callback.
    let mut delivery_timed_out = false;
    if dispatched && let Some(reason) = abandoned_reason {
        let terminal = AttemptTerminal::Abandoned {
            reason,
            drain_timed_out: result.is_none(),
            partial_response: control.partial_response(),
        };
        let delivery =
            std::panic::AssertUnwindSafe(provider.attempt_ended(identity, terminal)).catch_unwind();
        match tokio::time::timeout(horizon.terminal_policy.delivery_timeout, delivery).await {
            Err(_) => {
                delivery_timed_out = true;
                horizon
                    .metrics
                    .runtime_tuning
                    .record_provider_terminal_delivery_timeout(provider.kind());
                tracing::warn!(timeout = ?horizon.terminal_policy.delivery_timeout, "provider terminal delivery exceeded its bound")
            }
            Ok(Err(_)) => tracing::warn!("provider terminal delivery panicked"),
            Ok(Ok(())) => {}
        }
    }
    AttemptExecution {
        result,
        control,
        delivery_timed_out,
    }
}
