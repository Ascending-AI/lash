//! One atomic tool attempt recorded by its logical Run.

use super::*;

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalPreparedToolAttemptEffectRunner<'_> {
    fn plugin_state_session(&self) -> Option<Arc<crate::PluginSession>> {
        Some(Arc::clone(&self.dispatch.plugins))
    }

    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(command, RuntimeEffectCommand::ToolAttempt { .. })
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::ToolAttempt {
            call,
            execution_grant,
            attempt,
            max_attempts,
        } = envelope.command
        else {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "prepared tool attempt executor requires a tool_attempt command",
            ));
        };
        let outcome = crate::tool_dispatch::AtomicToolAttempt::new(
            self.dispatch.as_ref(),
            self.tool_context,
            envelope.invocation.into_runtime_invocation(),
            self.completion_key,
            effect_attempt,
        )
        .execute(*call, execution_grant, attempt, max_attempts)
        .await?;
        Ok(tool_attempt_outcome(outcome))
    }
}
