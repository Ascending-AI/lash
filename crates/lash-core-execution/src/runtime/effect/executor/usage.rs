//! How a local executor accounts what its body spends (ADR 0125): the
//! body's [`UsageRun`](crate::UsageRun) is begun here, and its usage is
//! either handed to the engine to journal or projected at once.

use super::{LocalTarget, RuntimeEffectLocalExecutor, RuntimeEffectLocalExecutorState};
use super::{RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectOutcome};

/// The accounting of one `Direct` effect: the ledger its run is admitted
/// to, and the owner and source label its call is attributed to.
pub struct DirectUsage {
    pub accounting: crate::UsageAccountingBinding,
    pub owner: crate::RuntimeOwner,
    pub source: String,
}

impl DirectUsage {
    /// The one call a `Direct` body makes, taken from its effect's run. A
    /// body outside any run refuses before it dispatches.
    pub(super) fn call(
        &self,
        usage_run: Option<&crate::UsageRun>,
        model: &str,
    ) -> Result<crate::UsageCall, RuntimeEffectControllerError> {
        usage_run
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::UsageRunMissing,
                    "a direct completion reached its provider outside any usage run",
                )
            })?
            .call(self.owner.clone(), self.source.clone(), model.to_string())
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::UsageRunMissing,
                    error.to_string(),
                )
            })
    }
}

impl super::LocalDirectEffectRunner {
    /// Runs the direct request as its run's one call, and records the sealed
    /// call record, failed attempts included, as that call's facts.
    pub(super) async fn run_direct_in_usage_run(
        &mut self,
        request: crate::LlmRequest,
        usage_run: Option<&crate::UsageRun>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let call = self.usage.call(usage_run, &request.model)?;
        let (result, call_record) = self.run_direct_llm_request(request, &call).await;
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
            };
        }
        let usage_run = self
            .usage_accounting()
            .and_then(|binding| binding.begin(&envelope));
        // Boxed: the body's future is the executor's largest, and every
        // engine path awaits this one inside its own journaling future.
        let outcome = Box::pin(self.run_body(envelope, usage_run.clone())).await;
        match usage_run {
            None => crate::RecordedEffectExecution {
                outcome,
                usage: None,
                admission_fault: None,
            },
            Some(usage_run) => crate::RecordedEffectExecution {
                outcome,
                admission_fault: usage_run.admission_fault(),
                usage: usage_run.finish(),
            },
        }
    }

    /// Executes a spending body inside another spending effect's run: a
    /// direct completion a recorded tool attempt makes journals nothing of its
    /// own, so its provider call is a call of the attempt's run (ADR 0125).
    /// `None` refuses the call before dispatch.
    pub async fn execute_within_run(
        self,
        envelope: RuntimeEffectEnvelope,
        usage_run: Option<crate::UsageRun>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if let Some(refusal) = self.served_only_refusal() {
            return Err(refusal);
        }
        Box::pin(self.run_body(envelope, usage_run)).await
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
        } = Box::pin(self.execute_recording_usage(envelope)).await;
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
