use super::*;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

impl RuntimeTurnDriver<'_> {
    pub(super) async fn report_undispatched_turn_tool_calls(
        &self,
        completed: Vec<crate::sansio::CompletedToolCall>,
        protocol_iteration: usize,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let context = match self.execution_context(
            event_tx,
            Arc::new(crate::ChronologicalProjection::default()),
        ) {
            Ok(context) => context.with_tracing(self.execution_tracing(protocol_iteration)),
            Err(err) => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::ToolCatalogResolutionFailed,
                    err.to_string(),
                ));
            }
        };
        for (index, call) in completed.iter().enumerate() {
            // No invocation exists on this presentation path: the lane keys
            // on the protocol iteration, the call's index and its id, so a
            // repeated call id still mints a unique key (ADR 0105 §1).
            let call_key = format!("{protocol_iteration}:{index}:{}", call.call_id);
            context.report_undispatched_tool_call(call, &call_key).await;
        }
        Ok(())
    }
}

impl RuntimeTurnDriver<'_> {}
