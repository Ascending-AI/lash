//! A group child's tool attempt: a recorded step whose body races the
//! child's cancel fact.

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
        // A group child's attempt watches its child's cancel inside the
        // recorded body (ADR 0105 §4, FIG-3904): the watch fires the attempt's
        // stop and drops the body, and the typed cancel is the attempt's
        // recorded outcome, so a replay serves it and never re-runs the tool.
        let call_id = call.call_id.clone();
        let cancel_watch = self
            .dispatch
            .effect_controller
            .controller()
            .group_child_cancel_watch();
        let (stop, tool_context) = match &cancel_watch {
            None => (None, self.tool_context),
            Some(_) => {
                let stop = self
                    .tool_context
                    .cancellation_token()
                    .map(tokio_util::sync::CancellationToken::child_token)
                    .unwrap_or_default();
                (Some(stop.clone()), self.tool_context.with_step_stop(stop))
            }
        };
        let body = Box::pin(
            crate::tool_dispatch::AtomicToolAttempt::new(
                self.dispatch.as_ref(),
                tool_context,
                envelope.invocation.into_runtime_invocation(),
                self.completion_key,
                effect_attempt,
            )
            .execute(*call, execution_grant, attempt, max_attempts),
        );
        let outcome = match (cancel_watch, stop) {
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
        Ok(tool_attempt_outcome(outcome))
    }
}
