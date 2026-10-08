//! Direct completions retain response usage and sealed attempt history.
use super::super::envelope::RuntimeDirectLlmOutcome;
use super::super::outcome::llm_call_error_from_transport;
use super::*;
use crate::LlmRequest as CoreLlmRequest;
use crate::provider::ProviderHandle;

impl LocalDirectEffectRunner {
    pub(super) async fn run_direct(
        &mut self,
        invocation: &crate::RuntimeEffectInvocation,
        provider: crate::ProviderHandle,
        request: crate::LlmRequest,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        // The request is this body's own work: its records are made here,
        // under the body's live step.
        let traced = self
            .live
            .as_ref()
            .map(|live| self.tracing.effect_body(live))
            .filter(crate::trace::TraceStanding::is_observed)
            .map(|standing| {
                let context = super::super::direct_trace_context(
                    &self.owner,
                    Some(&uuid::Uuid::new_v4().to_string()),
                    invocation.caused_by.as_ref(),
                );
                super::super::emit_llm_trace_started(&standing, context.clone(), &request);
                (standing, context, request.model.wire_model().to_string())
            });
        let (result, call_record) = self
            .run_direct_llm_request(provider, request, traced.as_ref())
            .await;
        if let Some((standing, context, request_model)) = traced {
            match &result {
                Ok(response) => super::super::emit_llm_trace_completed(
                    &standing,
                    context,
                    response,
                    &request_model,
                    0,
                    None,
                    call_record.as_ref(),
                ),
                Err(error) => super::super::emit_llm_trace_failed(
                    &standing,
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
        if let (Err(fault), Some(attempt)) = (&outcome, &effect_attempt)
            && fault.code == crate::RuntimeErrorCode::LlmProfileUnavailable
            && fault.is_attempt_fault()
        {
            attempt.fault_attempt(fault.clone());
            return std::future::pending().await;
        }
        outcome
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
        traced: Option<&(
            crate::trace::TraceStanding,
            lash_trace::TraceContext,
            String,
        )>,
    ) -> RuntimeDirectLlmOutcome {
        let mut request = request;
        let sideband = lash_core_llm::core_internal::prepare_completion(&provider, &mut request);
        let sideband = traced.map_or_else(
            || sideband.clone(),
            |(standing, context, _)| standing.provider_attempts(sideband.clone(), context.clone()),
        );
        match lash_core_llm::core_internal::complete_prepared(
            &mut provider,
            request,
            &self.template,
            self.deliveries.as_ref(),
            sideband,
            self.charge_safety.clone(),
            self.tracing.metrics(),
            traced.and_then(|(standing, _, _)| standing.body_permit()),
            self.bounds.clone(),
        )
        .await
        {
            Ok(completion) => (Ok(completion.response), Some(completion.call_record)),
            Err(failure) => (
                Err(llm_call_error_from_transport(failure.error)),
                Some(*failure.call_record),
            ),
        }
    }
}
