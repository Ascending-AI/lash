//! One tool attempt as a turn's execution context runs it: a recorded step
//! whose body watches the turn's gate, with the attempt's capture written
//! before and after that watched body (ADR 0114 §2.2, FIG-4071).

use super::execution_context::RuntimeExecutionContext;

impl RuntimeExecutionContext<'_> {
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
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        let mut attempt_dispatch = (*self.dispatch).clone();
        attempt_dispatch.parent_invocation = Some(attempt_invocation.clone());
        // The attempt's invocation is now the observation base; an inherited
        // per-call key would key every retry of it under the caller's lane.
        attempt_dispatch.observation_call_key = None;
        attempt_dispatch.direct_completions = attempt_dispatch
            .direct_completions
            .with_tool_attempt_parent_invocation(attempt_invocation.clone())
            .with_usage_ledger(crate::runtime::ToolUsageLedger::for_attempt(attempt));
        attempt_dispatch.trigger_outcomes =
            crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        // Attempt-local: what this attempt commits is journaled on its
        // outcome's capture rather than read out of the shared buffer.
        attempt_dispatch.checkpoint_messages =
            crate::tool_dispatch::CheckpointMessageBuffer::default();
        let attempt_dispatch = std::sync::Arc::new(attempt_dispatch);
        let mut attempt_context = self.clone();
        attempt_context.dispatch = std::sync::Arc::clone(&attempt_dispatch);
        attempt_context.parent_invocation = Some(attempt_invocation.clone());

        // The attempt's capture is written outside the watched body, before
        // and after it (see `ToolAttemptTurnCapture`).
        let call_id = prepared.call_id.clone();
        let turn_capture =
            crate::tool_dispatch::ToolAttemptTurnCapture::open(attempt_dispatch.as_ref(), &call_id)
                .await?;
        // The attempt is a recorded step its engine cannot select away: its
        // body watches the turn's gate itself and gets the stop as its token,
        // so the recorded outcome says whether the stop won (FIG-3672 P9).
        let mut outcome = Box::pin(self.run_turn_step_body(|stop| {
            self.execute_prepared_tool_attempt_body(
                prepared,
                execution_grant,
                attempt,
                max_attempts,
                attempt_invocation,
                child_execution_trace_hook,
                completion_key,
                attempt_dispatch,
                attempt_context,
                stop,
                &turn_capture,
            )
        }))
        .await?;
        turn_capture.settle(&call_id, &mut outcome).await?;
        Ok(outcome)
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
        attempt_dispatch: std::sync::Arc<crate::tool_dispatch::ToolDispatchContext<'_>>,
        attempt_context: Self,
        stop: Option<tokio_util::sync::CancellationToken>,
        turn_capture: &crate::tool_dispatch::ToolAttemptTurnCapture,
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        let mut tool_context =
            crate::ToolContext::from_dispatch(std::sync::Arc::clone(&attempt_dispatch))
                .runtime_execution_context(attempt_context.clone())
                .prepared_call(&prepared)
                .cancellation_token(stop)
                .enclosing_process(self.process_id().cloned())
                .parent_invocation(Some(attempt_invocation))
                .child_execution_trace_hook(child_execution_trace_hook);
        if let Some(process_id) = self.process_id()
            && let Some(process_events) = self.process_event_context()
        {
            tool_context = tool_context.process_events(
                process_id,
                process_events.execution_write_authority.clone(),
                process_events.process_work.clone(),
                process_events.store.clone(),
                process_events.session_store_factory.clone(),
                std::sync::Arc::clone(&process_events.queued_work),
                process_events.process_wake_delivery_policy,
                std::sync::Arc::clone(&process_events.clock),
            );
        }
        let tool_context = tool_context.build();
        tool_context.install_prederived_completion_key(completion_key);
        Box::pin(crate::tool_dispatch::execute_prepared_tool_attempt_effect(
            attempt_dispatch.as_ref(),
            prepared,
            execution_grant,
            attempt,
            max_attempts,
            tool_context,
            turn_capture,
        ))
        .await
    }
}
