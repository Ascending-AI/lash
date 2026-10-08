//! Retained scope boundaries in the execution's own journal.

use crate::ActorContext;

use lash_trace::{DurableTraceScope, TraceTransitionKind};

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
    retained: DurableTraceScope,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for BoundaryRunner {
    async fn execute(
        self: Box<Self>,
        _envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        Ok(RuntimeEffectOutcome::TraceBoundary {
            scope: Box::new(self.retained),
            at_ms: self.runtime.clock().timestamp_ms(),
        })
    }
}

impl super::TraceRuntime {
    /// Retains scope data under the owner's existing execution journal: the
    /// scope `retained` that a durable admission already retained, which
    /// proposed and settled its own candidate. The result retains data only;
    /// replay never restores the ephemeral permit.
    pub async fn record_boundary(
        &self,
        controller: &ActorContext,
        key: String,
        retained: DurableTraceScope,
        transition: TraceTransitionKind,
    ) -> Result<TraceBoundaryReceipt, RuntimeEffectControllerError> {
        let invocation = RuntimeEffectInvocation::new(
            crate::EffectAddress::new(controller.execution_scope().clone(), key.clone())?,
            crate::RuntimeAttribution::default(),
            key,
        );
        let scope = retained.scope.clone();
        let runner = BoundaryRunner {
            runtime: self.clone(),
            retained,
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
