//! How a local executor accounts what its body spends (ADR 0125): the
//! body's [`UsageRun`](crate::UsageRun) is begun here, and its usage is
//! either handed to the engine to journal or projected at once.

use super::{LocalTarget, RuntimeEffectLocalExecutor, RuntimeEffectLocalExecutorState};
use super::{RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectOutcome};

/// The accounting of one `Direct` effect: the ledger its run is admitted
/// to, and the owner its call is attributed to. The source label and the
/// model key come from the effect's envelope.
pub struct DirectUsage {
    pub accounting: crate::UsageAccountingBinding,
    pub owner: crate::RuntimeOwner,
}

impl DirectUsage {
    /// The one call a `Direct` body makes, taken from its effect's run. A
    /// body outside any run refuses before it dispatches.
    pub(super) fn call(
        &self,
        usage_run: Option<&crate::UsageRun>,
        source: String,
        profile_key: crate::LlmProfileKey,
        requested_model: &str,
    ) -> Result<crate::UsageCall, RuntimeEffectControllerError> {
        usage_run
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::UsageRunMissing,
                    "a direct completion reached its provider outside any usage run",
                )
            })?
            .call(
                self.owner.clone(),
                source,
                profile_key,
                requested_model.to_string(),
            )
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::UsageRunMissing,
                    error.to_string(),
                )
            })
    }
}

impl super::LocalDirectEffectRunner {
    /// Runs the direct request as its run's one call, attributed to the
    /// envelope's `usage_source` and `profile_key`, and records the sealed call
    /// record, failed attempts included, as that call's facts.
    pub(super) async fn run_direct_in_usage_run(
        &mut self,
        provider: crate::ProviderHandle,
        request: crate::LlmRequest,
        usage_source: String,
        usage_run: Option<&crate::UsageRun>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let call = self.usage.call(
            usage_run,
            usage_source,
            request.model.key().clone(),
            request.model.wire_model(),
        )?;
        let (result, call_record) = self.run_direct_llm_request(provider, request, &call).await;
        if let Some(call_record) = &call_record {
            call.record(call_record);
        }
        Ok(RuntimeEffectOutcome::Direct {
            result: Box::new(result),
            call_record,
        })
    }
}

impl<'run> RuntimeEffectLocalExecutor<'run> {
    /// Where this executor's body accounts its provider calls: the binding
    /// its runner offers, when its body may dispatch one.
    pub(super) fn usage_accounting(&self) -> Option<crate::UsageAccountingBinding> {
        match &self.state {
            RuntimeEffectLocalExecutorState::Runner(runner) => runner.usage_accounting(),
            RuntimeEffectLocalExecutorState::Target(LocalTarget::OwnedRunner(runner)) => {
                runner.usage_accounting()
            }
            RuntimeEffectLocalExecutorState::Target(_) => None,
        }
    }

    /// Executes the effect body for an engine that journals what it spent
    /// (ADR 0125).
    ///
    /// A spending body runs under a fresh [`UsageRun`](crate::UsageRun); the
    /// answer carries the run's [`EffectUsage`](crate::EffectUsage), which the
    /// engine journals beside the outcome and delivers through
    /// [`project_usage_settlement`](crate::project_usage_settlement) once it
    /// is recorded. An admission fault is the attempt's, never the effect's:
    /// the engine ends the attempt retryably and journals nothing.
    ///
    /// The body is raced against its run's attempt fault (FIG-4632): a call
    /// of the run that latches one ends the attempt there, and the body is
    /// dropped instead of running on. Nothing will journal that run, so the
    /// usage of the calls it dispatched before the fault is projected here.
    pub async fn execute_recording_usage(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> crate::RecordedEffectExecution {
        // An engine that reaches a served-only effect's live execution
        // without asking still dispatches nothing (FIG-3719).
        if let Some(refusal) = self.served_only_refusal() {
            return crate::RecordedEffectExecution {
                outcome: Err(refusal),
                usage: None,
                admission_fault: None,
                attempt_fault: None,
            };
        }
        let binding = self.usage_accounting();
        let usage_run = binding
            .as_ref()
            .and_then(|binding| binding.begin(&envelope));
        let effect = crate::UsageEffectKey::for_effect(envelope.invocation.address());
        // Boxed: the body's future is the executor's largest, and every
        // engine path awaits this one inside its own journaling future.
        let body = Box::pin(self.run_body(envelope, usage_run.clone()));
        let (Some(usage_run), Some(binding)) = (usage_run, binding) else {
            return crate::RecordedEffectExecution {
                outcome: body.await,
                usage: None,
                admission_fault: None,
                attempt_fault: None,
            };
        };
        let outcome = tokio::select! {
            biased;
            fault = usage_run.attempt_faulted() => Err(fault),
            outcome = body => outcome,
        };
        let admission_fault = usage_run.admission_fault();
        let attempt_fault = usage_run.attempt_fault();
        let usage = usage_run.finish();
        let Some(fault) = attempt_fault else {
            return crate::RecordedEffectExecution {
                outcome,
                usage,
                admission_fault,
                attempt_fault: None,
            };
        };
        if let Some(usage) = &usage {
            // A projection the store refuses leaves the run's admitted row
            // open, which its execution's end resolves as an explicit unknown
            // liability; the attempt ends with its fault either way.
            let _ = crate::project_unrecorded_usage(
                binding.store.as_ref(),
                usage,
                &effect,
                binding.clock.timestamp_ms(),
            )
            .await;
        }
        crate::RecordedEffectExecution {
            outcome: Err(fault.clone()),
            usage: None,
            admission_fault,
            attempt_fault: Some(fault),
        }
    }

    /// Executes a spending body inside another spending effect's run: a
    /// direct completion a recorded tool attempt makes journals nothing of its
    /// own, so its provider call is a call of the attempt's run (ADR 0125).
    /// `None` refuses the call before dispatch.
    ///
    /// A recorded model this worker cannot bind ends the enclosing attempt
    /// (FIG-4404): the body journals nothing of its own that could stay
    /// unsealed, so the fault is latched on that run and this call never
    /// returns. The run's executor drops the attempt's body here
    /// ([`Self::execute_recording_usage`]), so the tool is handed no error it
    /// could swallow and runs no further (FIG-4632).
    pub async fn execute_within_run(
        self,
        envelope: RuntimeEffectEnvelope,
        usage_run: Option<crate::UsageRun>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if let Some(refusal) = self.served_only_refusal() {
            return Err(refusal);
        }
        let outcome = Box::pin(self.run_body(envelope, usage_run.clone())).await;
        if let (Err(fault), Some(usage_run)) = (&outcome, &usage_run)
            && fault.code == crate::RuntimeErrorCode::LlmProfileUnavailable
            && fault.is_attempt_fault()
        {
            usage_run.fault_attempt(fault.clone());
            return std::future::pending().await;
        }
        outcome
    }

    /// Executes the effect body for a controller that journals nothing of its
    /// own (the test and conformance doubles): the body's usage, if it spent
    /// any, is projected as soon as the body returns, because nothing else
    /// will ever deliver it.
    pub async fn execute(
        self,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let binding = self.usage_accounting();
        let effect = crate::UsageEffectKey::for_effect(envelope.invocation.address());
        let crate::RecordedEffectExecution {
            outcome,
            usage,
            admission_fault,
            attempt_fault,
        } = Box::pin(self.execute_recording_usage(envelope)).await;
        // The attempt's own fault stays typed: the controller's caller
        // retries on it as an engine would.
        if let Some(fault) = attempt_fault {
            return Err(fault);
        }
        if let Some(fault) = admission_fault {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::UsageAdmissionFault,
                fault,
            )
            .retryable_uncommitted_derivation());
        }
        if let (Some(usage), Some(binding)) = (usage, binding) {
            crate::project_usage_settlement(
                binding.store.as_ref(),
                &usage.settlement(&effect),
                binding.clock.timestamp_ms(),
            )
            .await
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::UsageAdmissionFault,
                    format!("usage settlement projection failed: {error}"),
                )
                .retryable_uncommitted_derivation()
            })?;
        }
        outcome
    }
}
