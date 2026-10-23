//! Direct completions retain response usage and sealed attempt history.
use super::super::envelope::RuntimeDirectLlmOutcome;
use super::super::outcome::llm_call_error_from_transport;
use super::*;
use crate::LlmRequest as CoreLlmRequest;
use crate::provider::ProviderHandle;
use lash_protocol_types::sync::MutexExt as _;

impl LocalDirectEffectRunner {
    pub(super) async fn run_direct(
        &mut self,
        invocation: &crate::RuntimeEffectInvocation,
        provider: crate::ProviderHandle,
        mut request: crate::LlmRequest,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // The call's id names it on every attempt's request scope and on its
        // trace records alike, whether or not anything observes it.
        let llm_call_id = lash_core_llm::core_internal::call_id_for_scope(&self.response.scope).0;
        request.scope.llm_call_id = Some(llm_call_id.clone());
        // The request is this body's own work: its records are made here,
        // under the body's live step. Made from an extension body, the call
        // is the child of the position it was made at (§7.2).
        let traced = self
            .live
            .as_ref()
            .map(
                |live| match crate::trace::extension::ambient_standing(&self.tracing) {
                    Some(standing) => standing.in_body(live),
                    None => self.tracing.effect_body(live),
                },
            )
            .filter(crate::trace::TraceStanding::is_observed)
            .map(|standing| {
                let context = super::super::direct_trace_context(
                    &self.owner,
                    Some(&llm_call_id),
                    invocation.caused_by.as_ref(),
                );
                let call = super::super::emit_llm_trace_started(
                    &standing,
                    context.clone(),
                    &request,
                    provider.kind(),
                );
                (call, context, request.model.wire_model().to_string())
            });
        request.scope.trace_carrier = traced
            .as_ref()
            .and_then(|(call, _, _)| call.carrier().cloned().map(Arc::new));
        let (result, call_record) = self
            .run_direct_llm_request(provider, request, traced.as_ref(), effect_attempt)
            .await?;
        if let Some((call, context, request_model)) = traced {
            match &result {
                Ok(response) => super::super::emit_llm_trace_completed(
                    call,
                    context,
                    response,
                    &request_model,
                    0,
                    None,
                    call_record.as_ref(),
                ),
                Err(error) => super::super::emit_llm_trace_failed(
                    call,
                    context,
                    super::super::LlmTraceFailure::from(error),
                    None,
                    call_record.as_ref(),
                ),
            }
        }
        Ok(RuntimeEffectOutcome::Direct {
            result: Box::new(result),
            call_record,
        })
    }
}

impl<'run> RuntimeEffectLocalExecutor<'run> {
    pub async fn execute_recorded(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> crate::RecordedEffectExecution {
        if let Some(refusal) = self.served_only_refusal() {
            return crate::RecordedEffectExecution {
                outcome: Err(refusal),
                attempt_fault: None,
            };
        }
        let attempt = crate::EffectAttempt::default();
        let body = Box::pin(self.run_body(envelope, Some(attempt.clone())));
        let outcome = tokio::select! {
            biased;
            fault = attempt.attempt_faulted() => Err(fault),
            outcome = body => outcome,
        };
        crate::RecordedEffectExecution {
            outcome,
            attempt_fault: attempt.attempt_fault(),
        }
    }

    pub async fn execute_within_attempt(
        self,
        envelope: RuntimeEffectEnvelope,
        effect_attempt: Option<crate::EffectAttempt>,
        controller: &crate::ActorContext,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if let Some(refusal) = self.served_only_refusal() {
            return Err(refusal);
        }
        // Nested model work has no independent journal command. Its body
        // still runs under the enclosing attempt's admitted scope and
        // transport observation, with the same attempt fault latch.
        let executor = self.issued_under(
            controller.frontier().clone(),
            None,
            controller.trace_scope().cloned(),
        );
        let outcome = Box::pin(executor.run_body(envelope, effect_attempt.clone())).await;
        match (outcome, effect_attempt) {
            (Err(fault), Some(attempt)) if fault.is_attempt_fault() => {
                match attempt.end_at(fault).await {}
            }
            (outcome, _) => outcome,
        }
    }

    pub async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.execute_recorded(envelope).await.outcome
    }
}

impl LocalDirectEffectRunner {
    pub(super) async fn run_direct_llm_request(
        &mut self,
        mut provider: ProviderHandle,
        request: CoreLlmRequest,
        traced: Option<&(crate::trace::OpenSpan, lash_trace::TraceContext, String)>,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeDirectLlmOutcome, RuntimeEffectControllerError> {
        // The request is the effect's own input: it attributes the call and
        // carries this send's senders. What is sent is the admitted
        // template, and its response is read under the admitted context.
        let mut request = request;
        let first = self.response.scope.attempt.unwrap_or(1);
        let reservation_fault = Arc::new(std::sync::Mutex::new(None));
        let fault = Arc::clone(&reservation_fault);
        let enclosing = effect_attempt
            .as_ref()
            .and_then(|attempt| attempt.model_call_owner().cloned());
        let cx = self.reservation.0.clone();
        let key = self.reservation.1.clone();
        let call_id = lash_core_llm::core_internal::call_id_for_scope(&self.response.scope);
        let sideband = lash_core_llm::core_internal::prepare_completion(&provider, &mut request)
            .with_unobserved_attempts(first.saturating_sub(1))
            .with_attempt_reservation(
                first,
                Arc::new(move |attempt| {
                    let cx = cx.clone();
                    let key = key.clone();
                    let call_id = call_id.clone();
                    let fault = Arc::clone(&fault);
                    let enclosing = enclosing.clone();
                    Box::pin(async move {
                        let result = async {
                            #[cfg(any(test, feature = "testing"))]
                            if attempt > first {
                                cx.entered_model_retry_for_testing().await;
                            }
                            cx.commit_fresh(
                                lash_durable::CommitLabel::new("completion.attempt"),
                                |tx| {
                                    tx.write(lash_durable::DomainWrite::ModelCallPin(
                                        lash_durable::domain::ModelCallPinWrite::Reserve {
                                            key,
                                            call_id,
                                            attempt,
                                            enclosing,
                                        },
                                    ));
                                },
                            )
                            .await?;
                            Ok::<_, lash_durable::DurableError>(())
                        }
                        .await;
                        result.map_err(|error| {
                            let message = error.to_string();
                            *fault.lock_recover() = Some(error);
                            lash_transport_failure::LlmTransportError::new(message)
                                .with_retry_verdict(
                                    lash_transport_failure::TransportRetryVerdict::Forbidden,
                                )
                        })
                    })
                }),
            );
        let mut context = self
            .response
            .clone()
            .with_senders(request.stream_events.take(), request.provider_trace.take());
        context.scope.llm_call_id = request.scope.llm_call_id.clone();
        context.scope.trace_carrier = request.scope.trace_carrier.clone();
        let sideband = traced.map_or_else(
            || sideband.clone(),
            |(call, context, _)| call.provider_attempts(sideband.clone(), context.clone()),
        );
        // The provider polls under the call's span (§7.7).
        let ambient = traced.map(|(call, _, _)| call.standing().clone());
        let mut cancellation =
            sideband.cancel_on_drop(crate::provider::AttemptEndReason::ImmediateCancel);
        let template = Arc::clone(&self.template);
        let deliveries = Arc::clone(&self.deliveries);
        let charge_safety = self.charge_safety.clone();
        let metrics = self.tracing.metrics().clone();
        let bounds = self.bounds.clone();
        // A cancelled cell may drop this await. The task keeps the same
        // provider alive through cooperative drain and terminal delivery.
        let (send, receive) = tokio::sync::oneshot::channel();
        let attempt = effect_attempt.clone();
        let key = self.reservation.1.clone();
        let owner = self.reservation.0.clone();
        let task = crate::task::spawn(async move {
            let completion = lash_core_llm::core_internal::complete_prepared(
                &mut provider,
                context,
                &template,
                deliveries.as_ref(),
                sideband,
                charge_safety,
                &metrics,
                ambient.as_ref().and_then(|standing| standing.body_permit()),
                bounds,
            );
            let completed = match &ambient {
                Some(ambient) => ambient.ambient(completion).await,
                None => completion.await,
            };
            use lash_core_store::model_calls::{ModelCallEnd, ModelCallEndReason, ModelCallFact};
            let (record, end) = match &completed {
                Ok(completion) => (&completion.call_record, ModelCallEnd::Answered),
                Err(failure) => {
                    let reason = if failure.error.code
                        == Some(crate::FailureCode::lash(
                            crate::TurnFailureCode::ModelTotalExceeded,
                        )) {
                        Some(crate::provider::AttemptEndReason::ModelDeadline)
                    } else if failure.error.terminal_reason == crate::LlmTerminalReason::Cancelled {
                        Some(crate::provider::AttemptEndReason::ImmediateCancel)
                    } else {
                        None
                    };
                    (
                        failure.call_record.as_ref(),
                        reason.map_or(ModelCallEnd::Answered, |reason| ModelCallEnd::Unanswered {
                            reason: ModelCallEndReason::Attempt(reason),
                        }),
                    )
                }
            };
            let call = lash_durable::domain::model_calls::OwnedModelCall {
                key,
                fact: ModelCallFact {
                    call: 0,
                    call_id: record.call_id.clone(),
                    end,
                    record: record.clone(),
                },
            };
            if let Some(attempt) = attempt {
                attempt.record_model_call(call);
            } else {
                owner.record_owned_model_call(call);
            }
            let _ = send.send(completed);
        });
        if let Some(attempt) = effect_attempt {
            attempt.retain_model_task(task);
        } else {
            self.reservation.0.retain_owned_model_task(task);
        }
        let completed = match receive.await {
            Ok(completed) => completed,
            Err(error) => {
                return Ok((
                    Err(llm_call_error_from_transport(
                        lash_transport_failure::LlmTransportError::new(format!(
                            "direct provider task failed: {error}"
                        ))
                        .with_lash_code(crate::TurnFailureCode::TaskJoinFailed)
                        .with_retry_verdict(
                            lash_transport_failure::TransportRetryVerdict::NotRetryable,
                        ),
                    )),
                    None,
                ));
            }
        };
        if let Some(error) = reservation_fault.lock_recover().take() {
            return Err(reservation_refusal(error).retryable_uncommitted_derivation());
        }
        cancellation.disarm();
        Ok(match completed {
            Ok(completion) => (Ok(completion.response), Some(completion.call_record)),
            Err(failure) => (
                Err(llm_call_error_from_transport(failure.error)),
                Some(*failure.call_record),
            ),
        })
    }
}

/// Keep owner refusals typed across the transport's reservation callback.
fn reservation_refusal(error: lash_durable::DurableError) -> RuntimeEffectControllerError {
    let message = error.to_string();
    match error {
        lash_durable::DurableError::RevisionConflict {
            actor,
            epoch,
            expected,
            actual,
        } => {
            let mut refusal = RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::OwnerRevisionConflict,
                message,
            );
            refusal.cause = Some(crate::RuntimeErrorCause::OwnerRevisionConflict(Box::new(
                crate::OwnerRevisionConflict {
                    actor: actor.as_str().to_owned(),
                    epoch: epoch.0,
                    expected: expected.0,
                    actual: actual.0,
                },
            )));
            refusal
        }
        lash_durable::DurableError::OwnershipLost(_) => RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::SessionExecutionLeaseLost,
            message,
        ),
        _ => RuntimeEffectControllerError::new(crate::RuntimeErrorCode::StoreCommitFailed, message),
    }
}
