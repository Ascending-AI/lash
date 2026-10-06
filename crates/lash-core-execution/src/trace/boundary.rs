//! Retained scope boundaries in the execution's own journal.

use crate::ActorContext;
use std::sync::{Arc, Mutex};

use lash_trace::{
    DurableTraceScope, TraceCandidateOutcome, TraceCause, TraceScopeId, TraceTransitionKind,
};

use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectOutcome,
};

/// Scope data retained by the execution journal; it grants no emission right.
pub struct TraceBoundaryReceipt {
    pub scope: DurableTraceScope,
    pub at_ms: u64,
}

struct BoundaryRunner {
    runtime: super::TraceRuntime,
    scope: TraceScopeId,
    cause: TraceCause,
    retained: Option<DurableTraceScope>,
    candidate: Arc<Mutex<Option<Box<dyn lash_trace::TraceAdmissionCandidate>>>>,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for BoundaryRunner {
    async fn execute(
        self: Box<Self>,
        _envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let at_ms = self.runtime.clock().timestamp_ms();
        let scope = match self.retained {
            Some(scope) => scope,
            None => {
                let candidate = self.runtime.scopes().propose(&self.scope, &self.cause);
                let scope = DurableTraceScope {
                    scope: self.scope,
                    cause: self.cause,
                    anchor: candidate.anchor(),
                    started_at_ms: at_ms,
                };
                *self
                    .candidate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(candidate);
                scope
            }
        };
        Ok(RuntimeEffectOutcome::TraceBoundary {
            scope: Box::new(scope),
            at_ms,
        })
    }
}

impl super::TraceRuntime {
    /// Retains scope data under the owner's existing execution journal.
    /// The result retains data only; replay never restores the ephemeral permit.
    pub async fn record_boundary(
        &self,
        controller: &ActorContext,
        key: String,
        scope: TraceScopeId,
        cause: TraceCause,
        retained: Option<DurableTraceScope>,
        transition: TraceTransitionKind,
    ) -> Result<TraceBoundaryReceipt, RuntimeEffectControllerError> {
        let candidate = Arc::new(Mutex::new(None));
        let invocation = RuntimeEffectInvocation::new(
            crate::EffectAddress::new(controller.execution_scope().clone(), key.clone())?,
            crate::RuntimeAttribution::default(),
            key,
        );
        let runner = BoundaryRunner {
            runtime: self.clone(),
            scope: scope.clone(),
            cause,
            retained,
            candidate: Arc::clone(&candidate),
        };
        let result = controller
            .turn_effect(
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::TraceBoundary { scope, transition },
                ),
                crate::runtime::effect::executor::owned_runner_executor(Box::new(runner), None),
            )
            .await;
        let outcome = if result.is_ok() {
            TraceCandidateOutcome::Selected
        } else {
            TraceCandidateOutcome::Refused
        };
        let candidate = candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(candidate) = candidate {
            candidate.settle(outcome);
        }
        let RuntimeEffectOutcome::TraceBoundary { scope, at_ms } = result? else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                "scope boundary returned another outcome",
            ));
        };
        Ok(TraceBoundaryReceipt {
            scope: *scope,
            at_ms,
        })
    }
}
