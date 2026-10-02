//! Direct completions retain response usage and sealed attempt history.
use super::*;

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
            .map(|live| self.tracing.effect_body(invocation, live))
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
        let (result, call_record) = self.run_direct_llm_request(provider, request).await;
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
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if let Some(refusal) = self.served_only_refusal() {
            return Err(refusal);
        }
        let outcome = Box::pin(self.run_body(envelope, effect_attempt.clone())).await;
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
