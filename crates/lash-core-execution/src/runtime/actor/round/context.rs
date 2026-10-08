//! The tool effects of the context.

use super::super::ActorContext;

impl ActorContext {
    /// The tool effects, run in place and recorded nowhere: a tool attempt
    /// and its presentation. Their
    /// durability is the admitted execution that runs the call (ADR 0132
    /// §5): a round member's `x_outcome`, or a code cell's snapshot (§8).
    /// Any other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal, or the body's.
    pub async fn tool_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match &envelope.command {
            crate::RuntimeEffectCommand::ToolAttempt { .. }
            | crate::RuntimeEffectCommand::PresentToolResult { .. } => {
                local.run_in_place(envelope).await
            }
            other => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!("{:?} is not a tool effect", other.kind()),
            )),
        }
    }
}
