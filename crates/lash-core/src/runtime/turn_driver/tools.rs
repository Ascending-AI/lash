use super::*;

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
    ) -> Result<Vec<crate::sansio::CompletedToolCall>, RuntimeEffectControllerError> {
        let prepare_context = self
            .execution_context(
                event_tx,
                Arc::new(crate::ChronologicalProjection::default()),
            )
            .map_err(|err| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                    err.to_string(),
                )
            })?
            .with_tracing(self.execution_tracing(machine.protocol_iteration()));
        let call_count = calls.len();
        let mut results = vec![None; call_count];
        let mut prepared_entries = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            let call_id = call.call_id.clone();
            let replay = call.replay.clone();
            // The turn-dispatched protocol path holds no invocation: key each
            // call's observation lanes on the iteration, its index within it
            // and the call id, so a call id the model repeats across
            // iterations or frames mints distinct observations (ADR 0105 §1).
            let call_key = format!("{}:{index}:{call_id}", machine.protocol_iteration());
            // A call on a tool that drifted from the turn's recorded surface
            // is prepared under its recorded definition, so the child it
            // forms is the one the journal recorded. The child judges its own
            // tool where it runs and is served only from its journal
            // (FIG-3725).
            let drift = self.recorded_surface_drift(&prepare_context, &call.tool_name)?;
            let prepare_started = prepare_context.dispatch().clock.now();
            let preparation = match &drift {
                Some(drift) => {
                    prepare_context
                        .prepare_recorded_tool_call(&drift.recorded_binding(), call, &call_key)
                        .await
                }
                None => prepare_context.prepare_tool_call(call, &call_key).await,
            };
            match preparation {
                crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => {
                    prepared_entries.push((index, *prepared));
                }
                crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                    let completed = prepare_context
                        .complete_undispatched_tool_call(
                            call_id.clone(),
                            replay,
                            *outcome,
                            &call_key,
                            prepare_context
                                .dispatch()
                                .clock
                                .now()
                                .duration_since(prepare_started)
                                .as_millis() as u64,
                        )
                        .await?
                        .completed;
                    results[index] = Some(completed);
                }
            }
        }

        if !prepared_entries.is_empty() {
            // ADR 0099: the turn's tool calls open as a durable effect group
            // of `ToolInvocation` children; a deferred leaf parks inside its
            // own child driver.
            let group_invocation = crate::runtime::causal::turn_tool_group_invocation(
                self.scoped_effect_controller.execution_scope(),
                &self.session_id,
                &self.turn_id,
                self.turn_index,
                machine.protocol_iteration(),
                id,
            );
            // The group's identity is its invocation's replay key, not the
            // sansio effect id alone: effect ids restart in every agent frame,
            // while the admitted scope stays the root turn's, so a follow-on
            // frame's first tool call would otherwise name the root frame's
            // group and reopen its settlements.
            let batch_id = group_invocation.replay_key().to_string();
            let completions = prepare_context
                .execute_prepared_tool_group(&batch_id, group_invocation, prepared_entries)
                .await?;
            for (source_index, completed) in completions {
                results[source_index] = Some(completed.completed);
            }
        }
        drop(prepare_context);
        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                        format!("the turn's tool group did not fill result slot {index}"),
                    )
                })
            })
            .collect()
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
        self.session
            .tool_surface_drift(&self.session_id, &tool_id)
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                    error.to_string(),
                )
            })
    }
}
