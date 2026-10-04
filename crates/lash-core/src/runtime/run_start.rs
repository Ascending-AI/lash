//! The start marker of one execution of an admitted run (ADR 0105 §2, L-S8,
//! FIG-3815).
//!
//! A run's first recorded step, before its seal, draws a fresh nonce in the
//! run's own journal. The draw is random on purpose: it tells executions
//! apart, and only an execution that can read the journal replays it. It
//! runs only as the body of that recorded step, never on a replay, which is
//! why this module sits outside the shift's determinism scope.

use crate::engine::RunStartNonce;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode,
};

/// The first execution of one `DrawRunStart` step.
pub(in crate::runtime) struct DrawRunStartRunner {
    pub(in crate::runtime) admission: crate::engine::AdmissionId,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for DrawRunStartRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::DrawRunStart { admission } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "run start executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *admission != self.admission {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "run start executor was bound to another run",
            ));
        }
        Ok(RuntimeEffectOutcome::DrawRunStart {
            run_start: RunStartNonce::new(uuid::Uuid::new_v4().simple().to_string()),
        })
    }
}
