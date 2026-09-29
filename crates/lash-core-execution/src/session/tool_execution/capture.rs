//! One tool attempt's body, and the attempt as turn capture (ADR 0114 §2.2,
//! §4.1).
//!
//! The attempt's start persists before the tool runs, each progress chunk
//! before it publishes, and its settlement before the step returns. The
//! tool body reports progress through [`crate::AttemptContext::progress`].

use std::sync::Arc;

use super::RuntimeExecutionContext;

impl RuntimeExecutionContext<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_prepared_tool_attempt_body(
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
    ) -> Result<crate::ToolAttemptEffectOutcome, crate::RuntimeEffectControllerError> {
        let capture = self
            .open_tool_attempt_capture(&attempt_invocation, &prepared.call_id)
            .await?;
        let call_id = prepared.call_id.clone();
        let mut tool_context =
            crate::ToolContext::from_dispatch(std::sync::Arc::clone(&attempt_dispatch))
                .runtime_execution_context(attempt_context.clone())
                .prepared_call(&prepared)
                .cancellation_token(stop)
                .enclosing_process(self.process_id().cloned())
                .parent_invocation(Some(attempt_invocation))
                .child_execution_trace_hook(child_execution_trace_hook)
                .progress_reporter(capture.clone().map(|capture| capture as _));
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
        let mut outcome = Box::pin(crate::tool_dispatch::execute_prepared_tool_attempt_effect(
            attempt_dispatch.as_ref(),
            prepared,
            execution_grant,
            attempt,
            max_attempts,
            tool_context,
        ))
        .await?;
        settle_tool_attempt_capture(capture, &call_id, &mut outcome).await?;
        Ok(outcome)
    }

    /// Opens the attempt's capture writer, when the turn has a capture and
    /// the attempt a journaled identity.
    pub(super) async fn open_tool_attempt_capture(
        &self,
        attempt_invocation: &crate::RuntimeInvocation,
        call_id: &str,
    ) -> Result<Option<Arc<dyn crate::ToolAttemptCapture>>, crate::RuntimeEffectControllerError>
    {
        match (self.turn_capture(), attempt_invocation.replay_key()) {
            (Some(capture), Some(invocation)) => {
                Ok(Some(capture.open_attempt(invocation, call_id).await?))
            }
            _ => Ok(None),
        }
    }
}

/// Persists a finished attempt's settlement and stamps the watermark the
/// step's recorded outcome carries.
pub(super) async fn settle_tool_attempt_capture(
    capture: Option<Arc<dyn crate::ToolAttemptCapture>>,
    call_id: &str,
    outcome: &mut crate::ToolAttemptEffectOutcome,
) -> Result<(), crate::RuntimeEffectControllerError> {
    let Some(capture) = capture else {
        return Ok(());
    };
    if let crate::ToolAttemptLaunch::Done { record, .. } = &outcome.launch {
        capture
            .settled(call_id, &record.output)
            .await
            .map_err(|refused| {
                crate::RuntimeEffectControllerError::turn_capture_write_failed(format!(
                    "tool settlement capture failed: {refused}"
                ))
            })?;
    }
    outcome.capture_watermark = capture.watermark();
    Ok(())
}
