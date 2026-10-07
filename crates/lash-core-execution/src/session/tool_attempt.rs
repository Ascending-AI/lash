//! One tool attempt as a turn's execution context runs it: a recorded step
//! whose body watches the turn's gate.

use super::execution_context::RuntimeExecutionContext;

impl<'run> RuntimeExecutionContext<'run> {
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_tool_attempt_effect(
        &self,
        prepared: crate::PreparedToolCall,
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
        attempt_invocation: crate::RuntimeInvocation,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
        completion_key: Option<crate::AwaitEventKey>,
        effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        // The attempt is a recorded step its engine cannot select away: its
        // body watches the turn's gate itself and gets the stop as its token,
        // so the recorded outcome says whether the stop won (FIG-3672 P9).
        Box::pin(self.run_turn_step_body(|stop| {
            self.execute_prepared_tool_attempt_body(
                prepared,
                execution_grant,
                attempt,
                max_attempts,
                attempt_invocation,
                child_execution_trace_hook,
                completion_key,
                effect_attempt,
                stop,
            )
        }))
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_prepared_tool_attempt_body(
        &self,
        prepared: crate::PreparedToolCall,
        execution_grant: Option<Box<crate::ToolExecutionGrant>>,
        attempt: u32,
        max_attempts: u32,
        attempt_invocation: crate::RuntimeInvocation,
        child_execution_trace_hook: Option<crate::ToolChildExecutionTraceHook>,
        completion_key: Option<crate::AwaitEventKey>,
        effect_attempt: Option<crate::EffectAttempt>,
        stop: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        let mut tool_context =
            crate::ToolContext::from_dispatch(std::sync::Arc::clone(&self.dispatch), &prepared)
                .runtime_execution_context(self.clone())
                .cancellation_token(stop)
                .enclosing_process(self.process_id().cloned())
                .parent_invocation(Some(attempt_invocation.clone()))
                .child_execution_trace_hook(child_execution_trace_hook);
        if let Some(process_id) = self.process_id()
            && let Some(process_events) = self.process_event_context()
        {
            tool_context = tool_context.inside_process(crate::ProcessToolCallWiring::new(
                process_id.clone(),
                process_events.execution_write_authority.clone(),
                process_events.process_work.clone(),
            ));
        }
        Box::pin(
            crate::tool_dispatch::AtomicToolAttempt::new(
                self.dispatch.as_ref(),
                tool_context.build(),
                attempt_invocation,
                completion_key,
                effect_attempt,
            )
            .execute(prepared, execution_grant, attempt, max_attempts),
        )
        .await
    }

    pub(crate) fn for_tool_attempt(
        mut self,
        dispatch: std::sync::Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.dispatch = dispatch;
        self.parent_invocation = Some(invocation);
        self
    }
}
