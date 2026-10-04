use super::*;

impl RuntimeExecutionContext<'_> {
    pub async fn await_deferred_tool_completions(
        &self,
        step: &str,
        waits: Vec<crate::ToolCompletionWait>,
        dispatch: Option<crate::ToolDispatchCursor>,
        transferable: bool,
    ) -> Result<crate::ToolCompletionEvent, crate::RuntimeEffectControllerError> {
        let wait = self.turn_cancel_wait(self.cancellation_token.clone().unwrap_or_default());
        let mut invocation = self.language_runtime_invocation(step);
        if let Some(crate::ExecutionScope::Turn {
            session_id,
            turn_id,
        }) = wait.observed_scope()
        {
            invocation.attribution.session_id = Some(session_id.clone());
            invocation.attribution.turn_id = Some(turn_id.clone());
        }
        let outcome = self
            .dispatch
            .effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::AwaitToolCompletions {
                        waits,
                        dispatch,
                        transferable,
                    },
                ),
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &wait,
                    Arc::clone(&self.dispatch.clock),
                ),
            )
            .await?;
        match outcome {
            crate::RuntimeEffectOutcome::AwaitToolCompletions {
                event: crate::ToolCompletionEvent::HandedOver,
            } => Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::TurnWaitHandedOver,
                "the deferred tool round handed over",
            )),
            crate::RuntimeEffectOutcome::AwaitToolCompletions { event } => Ok(event),
            other => Err(crate::RuntimeEffectControllerError::wrong_outcome(
                crate::RuntimeEffectKind::AwaitToolCompletions,
                other.kind(),
            )),
        }
    }
}
