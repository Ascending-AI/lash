//! A group child's tool attempt: a recorded step whose body races the
//! child's cancel fact, with the attempt's capture written before and after
//! that watched body (ADR 0114 §2.2, FIG-4071).

use super::*;

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for LocalPreparedToolAttemptEffectRunner<'_> {
    fn uses_task_boundary(&self, command: &RuntimeEffectCommand) -> bool {
        matches!(command, RuntimeEffectCommand::ToolAttempt { .. })
    }

    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
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
        let mut dispatch = (*self.dispatch).clone();
        dispatch.parent_invocation = Some(envelope.invocation.clone().into_runtime_invocation());
        // The attempt's invocation is now the observation base; an inherited
        // per-call key would key every retry of it under the caller's lane.
        dispatch.observation_call_key = None;
        dispatch.direct_completions = dispatch
            .direct_completions
            .with_tool_attempt_parent_invocation(
                envelope.invocation.clone().into_runtime_invocation(),
            )
            .with_usage_ledger(crate::runtime::ToolUsageLedger::for_attempt(attempt));
        dispatch.trigger_outcomes = crate::tool_dispatch::ToolTriggerOutcomeBuffer::default();
        // Attempt-local buffers: what this attempt commits is drained into the
        // journaled capture, never read out of a buffer it shares with
        // anything else.
        dispatch.checkpoint_messages = crate::tool_dispatch::CheckpointMessageBuffer::default();
        let dispatch = Arc::new(dispatch);
        let tool_context = self.tool_context.with_attempt_dispatch(
            Arc::clone(&dispatch),
            envelope.invocation.into_runtime_invocation(),
        );
        tool_context.install_prederived_completion_key(self.completion_key);
        // A group child's attempt watches its child's cancel inside the
        // recorded body (ADR 0105 §4, FIG-3904): the watch fires the attempt's
        // stop and drops the body, and the typed cancel is the attempt's
        // recorded outcome, so a replay serves it and never re-runs the tool.
        let call_id = call.call_id.clone();
        let cancel_watch = dispatch
            .effect_controller
            .controller()
            .group_child_cancel_watch();
        let (stop, tool_context) = match &cancel_watch {
            None => (None, tool_context),
            Some(_) => {
                let stop = tool_context
                    .cancellation_token()
                    .map(tokio_util::sync::CancellationToken::child_token)
                    .unwrap_or_default();
                (Some(stop.clone()), tool_context.with_step_stop(stop))
            }
        };
        // The attempt's capture is written outside the watched body, before
        // and after it (see `ToolAttemptTurnCapture`).
        let turn_capture = crate::tool_dispatch::ToolAttemptTurnCapture::open(
            dispatch.as_ref(),
            &crate::tool_dispatch::ToolCallIds::of(&call),
        )
        .await?;
        let body = Box::pin(crate::tool_dispatch::execute_prepared_tool_attempt_effect(
            dispatch.as_ref(),
            *call,
            execution_grant,
            attempt,
            max_attempts,
            tool_context,
            &turn_capture,
        ));
        let mut outcome = match (cancel_watch, stop) {
            (Some(watch), Some(stop)) => {
                crate::runtime::run_step_body_until_cancelled(
                    stop,
                    crate::runtime::retry_cancel_watch("an effect-group child's cancel", || {
                        watch.cancelled()
                    }),
                    |_| body,
                    || Err(crate::tool_dispatch::group_child_cancelled(&call_id)),
                )
                .await?
            }
            _ => body.await?,
        };
        turn_capture.settle(&call_id, &mut outcome).await?;
        Ok(tool_attempt_outcome(outcome))
    }
}
