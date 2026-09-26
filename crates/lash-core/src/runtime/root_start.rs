//! The start marker of one execution of an admitted root (ADR 0105 §2, L-S8,
//! FIG-3815).
//!
//! A root's first recorded step, before its seal, draws a fresh nonce in the
//! root's own journal. The draw is random on purpose: it tells executions
//! apart, and only an execution that can read the journal replays it. It
//! runs only as the body of that recorded step, never on a replay, which is
//! why this module sits outside the drive's determinism scope.

use crate::engine::RootStartNonce;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode, TurnId,
};

/// The first execution of one `DrawRootStart` step.
pub(in crate::runtime) struct DrawRootStartRunner {
    pub(in crate::runtime) root: TurnId,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for DrawRootStartRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::DrawRootStart { root } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root start executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *root != self.root {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "root start executor was bound to another root",
            ));
        }
        Ok(RuntimeEffectOutcome::DrawRootStart {
            root_start: RootStartNonce::new(uuid::Uuid::new_v4().simple().to_string()),
        })
    }
}
