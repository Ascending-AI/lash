use super::*;

impl RuntimeTurnDriver<'_> {
    pub(super) async fn report_undispatched_turn_tool_calls(
        &self,
        completed: Vec<crate::sansio::CompletedToolCall>,
        protocol_iteration: usize,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let (tool_event_tx, mut tool_event_rx) =
            tokio::sync::mpsc::channel::<SessionStreamEvent>(64);
        let (turn_event_tx, mut turn_event_rx) = tokio::sync::mpsc::channel::<TurnActivity>(64);
        let runtime_event_tx = event_tx.clone();
        let tool_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = tool_event_rx.recv().await {
                runtime_event_tx.session(event);
            }
        });
        let runtime_event_tx = event_tx.clone();
        let turn_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = turn_event_rx.recv().await {
                runtime_event_tx.publish(RuntimeStreamEvent::Turn(event));
            }
        });
        let context = match self.execution_context(
            tool_event_tx.clone(),
            event_tx,
            Arc::new(crate::ChronologicalProjection::default()),
        ) {
            Ok(context) => context
                .with_turn_event_sender(turn_event_tx.clone())
                .with_tracing(self.execution_tracing(protocol_iteration)),
            Err(err) => {
                drop(tool_event_tx);
                drop(turn_event_tx);
                let _ = tool_event_forwarder.await;
                let _ = turn_event_forwarder.await;
                return Err(RuntimeError::new(
                    RuntimeErrorCode::ToolCatalogResolutionFailed,
                    err.to_string(),
                ));
            }
        };
        for call in &completed {
            context.report_undispatched_tool_call(call).await;
        }
        drop(context);
        drop(tool_event_tx);
        drop(turn_event_tx);
        let _ = tool_event_forwarder.await;
        let _ = turn_event_forwarder.await;
        Ok(())
    }

    pub(super) async fn invoke_turn_tool_calls_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        calls: Vec<crate::sansio::PendingToolCall>,
        event_tx: &TurnObserver,
    ) -> Result<Vec<crate::sansio::CompletedToolCall>, RuntimeEffectControllerError> {
        let (tool_event_tx, mut tool_event_rx) =
            tokio::sync::mpsc::channel::<SessionStreamEvent>(64);
        let (turn_event_tx, mut turn_event_rx) = tokio::sync::mpsc::channel::<TurnActivity>(64);
        let runtime_event_tx = event_tx.clone();
        let tool_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = tool_event_rx.recv().await {
                runtime_event_tx.session(event);
            }
        });
        let runtime_event_tx = event_tx.clone();
        let turn_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = turn_event_rx.recv().await {
                runtime_event_tx.publish(RuntimeStreamEvent::Turn(event));
            }
        });
        let prepare_context = self
            .execution_context(
                tool_event_tx.clone(),
                event_tx,
                Arc::new(crate::ChronologicalProjection::default()),
            )
            .map_err(|err| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                    err.to_string(),
                )
            })?
            .with_turn_event_sender(turn_event_tx.clone())
            .with_tracing(self.execution_tracing(machine.protocol_iteration()));
        let call_count = calls.len();
        let mut results = vec![None; call_count];
        let mut prepared_entries = Vec::new();
        // The calls on tools whose live definition drifted from the turn's
        // recorded surface, by their position among the group's children.
        let mut drifted = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            let call_id = call.call_id.clone();
            let replay = call.replay.clone();
            let drift = self.recorded_surface_drift(&prepare_context, &call.tool_name)?;
            let preparation = match &drift {
                Some(drift) => {
                    prepare_context
                        .prepare_recorded_tool_call(&drift.recorded_binding(), call)
                        .await
                }
                None => prepare_context.prepare_tool_call(call).await,
            };
            match preparation {
                crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => {
                    if let Some(drift) = drift {
                        drifted.push((prepared_entries.len(), call_id.clone(), drift));
                    }
                    prepared_entries.push((index, *prepared));
                }
                crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                    let completed = prepare_context
                        .complete_undispatched_tool_call(call_id.clone(), replay, *outcome)
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
            // A call on a drifted tool is served only from its recorded
            // result: one the journal does not hold would reach the drifted
            // tool live, so the turn parks before anything is dispatched.
            if !drifted.is_empty() {
                let settled = prepare_context
                    .settled_tool_group_children(&batch_id)
                    .await?;
                if let Some((_, call_id, drift)) = drifted.iter().find(|(position, _, _)| {
                    !settled
                        .as_ref()
                        .is_some_and(|settled| settled.contains(position))
                }) {
                    return Err(drift.refusal(call_id));
                }
            }
            let completions = prepare_context
                .execute_prepared_tool_group(&batch_id, group_invocation, prepared_entries)
                .await?;
            for (source_index, completed) in completions {
                results[source_index] = Some(completed.completed);
            }
        }
        drop(prepare_context);
        drop(tool_event_tx);
        drop(turn_event_tx);
        let _ = tool_event_forwarder.await;
        let _ = turn_event_forwarder.await;
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
