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

    pub(super) async fn invoke_turn_tool_calls_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        calls: Vec<crate::sansio::PendingToolCall>,
        event_tx: &TurnObserver,
    ) -> Result<ToolRoundAdmission, RuntimeEffectControllerError> {
        let context = self
            .execution_context(
                event_tx,
                Arc::new(crate::ChronologicalProjection::default()),
            )
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                    error.to_string(),
                )
            })?
            .with_tracing(self.execution_tracing(machine.protocol_iteration()));
        let mut leaves = Vec::with_capacity(calls.len());
        for call in calls.iter().cloned() {
            let drift = self.recorded_surface_drift(&context, &call.tool_name)?;
            let tool_id = drift
                .as_ref()
                .map(|drift| drift.recorded_binding().manifest().id.clone())
                .or_else(|| context.callable_tool_id_by_name(&call.tool_name))
                .unwrap_or_else(|| crate::ToolId::new(call.tool_name.clone()));
            let invocation = crate::session::ToolInvocation::from_pending(call, tool_id);
            let invocation = match drift {
                Some(drift) => invocation.with_recorded_binding(drift.recorded_binding()),
                None => invocation,
            };
            leaves.push(crate::session::ToolAggregateLeaf::Tool(invocation));
        }
        let invocation = crate::runtime::causal::turn_tool_group_invocation(
            self.scoped_effect_controller.execution_scope(),
            &self.session_id,
            &self.turn_id,
            self.turn_index,
            machine.protocol_iteration(),
            id,
        );
        // Preparation, argument transforms and checks are part of A. The
        // cursor carries source slots; the logical Run owns attempts, finals,
        // protected presentation and Deferred sources across physical cuts.
        let cursor = match context
            .admit_tool_run_aggregate(crate::session::ToolAggregateRequest {
                leaves,
                consumer: crate::session::ToolAggregateConsumer::AllSettled,
                settled_value_after: None,
                command: crate::CommandReplayKey::new(invocation.effect_replay_key()),
            })
            .await
        {
            Ok(cursor) => cursor,
            Err(error) => {
                let Some(exceeded) = error.tool_call_limit_exceeded() else {
                    return Err(error);
                };
                // The recorded session limit refuses the round before A or
                // V exists. Preserve each original source call's identity and
                // typed refusal without invoking preparation or presentation.
                let output = crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::ResourceLimit,
                    crate::ToolCallLimitExceeded::CODE,
                    exceeded.to_string(),
                ));
                let results = calls
                    .into_iter()
                    .map(|call| crate::sansio::CompletedToolCall {
                        model_return: crate::ModelToolReturn::from_output(
                            call.tool_name.clone(),
                            &output,
                        ),
                        call_id: call.call_id,
                        provider_call_id: call.provider_call_id,
                        tool_name: call.tool_name,
                        args: call.args,
                        output: output.clone(),
                        intent_outcomes: Vec::new(),
                        replay: call.replay,
                    })
                    .collect();
                return Ok(ToolRoundAdmission::Refused(results));
            }
        };
        Ok(ToolRoundAdmission::Admitted(WaitingToolRound { cursor }))
    }
}

impl RuntimeTurnDriver<'_> {
    /// How the tool a model-issued call names drifted from the turn's
    /// recorded surface, judged on what decides how it links and dispatches.
    fn recorded_surface_drift(
        &self,
        context: &crate::RuntimeExecutionContext<'_>,
        tool_name: &str,
    ) -> Result<Option<crate::ToolSurfaceDrift>, RuntimeEffectControllerError> {
        let Some(tool_id) = context.callable_tool_id_by_name(tool_name) else {
            return Ok(None);
        };
        self.session.tool_surface_drift(&tool_id).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                error.to_string(),
            )
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitingToolRound {
    pub(super) cursor: crate::session::ToolRunAggregateCursor,
}

pub(super) enum ToolRoundAdmission {
    Admitted(WaitingToolRound),
    Refused(Vec<crate::sansio::CompletedToolCall>),
}
