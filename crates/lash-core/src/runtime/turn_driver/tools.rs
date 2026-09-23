use super::*;

pub(in crate::runtime) struct ToolBatchRunOutcome {
    pub launches: Vec<crate::runtime::ToolCallLaunch>,
    pub triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
    /// Input indices in the order the batch's leaves settled.
    pub settlement_order: Vec<usize>,
}

impl RuntimeTurnDriver<'_> {
    pub(super) async fn report_undispatched_turn_tool_calls(
        &self,
        completed: Vec<crate::sansio::CompletedToolCall>,
        protocol_iteration: usize,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    ) -> Result<(), RuntimeError> {
        let (tool_event_tx, mut tool_event_rx) =
            tokio::sync::mpsc::channel::<SessionStreamEvent>(64);
        let (turn_event_tx, mut turn_event_rx) = tokio::sync::mpsc::channel::<TurnActivity>(64);
        let runtime_event_tx = event_tx.clone();
        let tool_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = tool_event_rx.recv().await {
                send_session_event(&runtime_event_tx, event).await;
            }
        });
        let runtime_event_tx = event_tx.clone();
        let turn_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = turn_event_rx.recv().await {
                let _ = runtime_event_tx.send(RuntimeStreamEvent::Turn(event)).await;
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<Vec<crate::sansio::CompletedToolCall>, RuntimeEffectControllerError> {
        let (tool_event_tx, mut tool_event_rx) =
            tokio::sync::mpsc::channel::<SessionStreamEvent>(64);
        let (turn_event_tx, mut turn_event_rx) = tokio::sync::mpsc::channel::<TurnActivity>(64);
        let runtime_event_tx = event_tx.clone();
        let tool_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = tool_event_rx.recv().await {
                send_session_event(&runtime_event_tx, event).await;
            }
        });
        let runtime_event_tx = event_tx.clone();
        let turn_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = turn_event_rx.recv().await {
                let _ = runtime_event_tx.send(RuntimeStreamEvent::Turn(event)).await;
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
            .with_tracing(self.execution_tracing(machine.protocol_iteration()))
            .with_cancellation_token(cancel.clone());
        let call_count = calls.len();
        let mut results = vec![None; call_count];
        let mut prepared_entries = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            let call_id = call.call_id.clone();
            let replay = call.replay.clone();
            match prepare_context.prepare_tool_call(call).await {
                crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => {
                    prepared_entries.push((index, *prepared));
                }
                crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                    let completed = prepare_context
                        .complete_undispatched_tool_call(call_id.clone(), replay, *outcome)
                        .await
                        .completed;
                    results[index] = Some(completed);
                }
            }
        }

        if !prepared_entries.is_empty() {
            // ADR 0099: the batch opens as a durable effect group of
            // `ToolInvocation` children under the invocation the `ToolBatch`
            // effect would have claimed; a deferred leaf parks inside its own
            // child driver, so no `ToolCallLaunch::Pending` reaches here.
            let group_invocation =
                self.turn_effect_invocation(machine, id, RuntimeEffectKind::ToolBatch)?;
            // The group's identity is the invocation it replaces, not the
            // sansio effect id alone: effect ids restart in every agent frame,
            // while the admitted scope stays the root turn's, so a follow-on
            // frame's first tool call would otherwise name the root frame's
            // group and reopen its settlements. The invocation's replay key
            // carries the physical turn and protocol iteration.
            let batch_id = group_invocation.replay_key().to_string();
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
                        crate::RuntimeErrorCode::ToolBatchMissingResult,
                        format!("tool batch did not fill result slot {index}"),
                    )
                })
            })
            .collect()
    }

    pub(in crate::runtime) async fn run_tool_batch(
        &mut self,
        batch: crate::PreparedToolBatch,
        invocation: crate::RuntimeInvocation,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<ToolBatchRunOutcome, crate::RuntimeEffectControllerError> {
        let (tool_event_tx, mut tool_event_rx) =
            tokio::sync::mpsc::channel::<SessionStreamEvent>(64);
        let (turn_event_tx, mut turn_event_rx) = tokio::sync::mpsc::channel::<TurnActivity>(64);
        let runtime_event_tx = event_tx.clone();
        let tool_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = tool_event_rx.recv().await {
                send_session_event(&runtime_event_tx, event).await;
            }
        });
        let runtime_event_tx = event_tx.clone();
        let turn_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = turn_event_rx.recv().await {
                let _ = runtime_event_tx.send(RuntimeStreamEvent::Turn(event)).await;
            }
        });
        let protocol_iteration = invocation
            .attribution
            .protocol_iteration
            .unwrap_or_default();
        let context = match self.execution_context(
            tool_event_tx.clone(),
            event_tx,
            Arc::new(crate::ChronologicalProjection::default()),
        ) {
            Ok(context) => context
                .with_turn_event_sender(turn_event_tx.clone())
                .with_tracing(self.execution_tracing(protocol_iteration))
                .with_cancellation_token(cancel.clone()),
            Err(err) => {
                drop(tool_event_tx);
                drop(turn_event_tx);
                let _ = tool_event_forwarder.await;
                let _ = turn_event_forwarder.await;
                return Err(crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
                    err.to_string(),
                ));
            }
        };
        let outcome = Box::pin(context.execute_prepared_tool_batch_launches(
            batch,
            invocation,
            std::collections::HashMap::new(),
            std::sync::Arc::new(std::collections::HashMap::new()),
        ))
        .await?;
        drop(context);
        drop(tool_event_tx);
        drop(turn_event_tx);
        let _ = tool_event_forwarder.await;
        let _ = turn_event_forwarder.await;
        Ok(ToolBatchRunOutcome {
            launches: outcome.launches,
            triggers: outcome.triggers,
            settlement_order: outcome.settlement_order,
        })
    }

    // Unused on the group path — a deferred leaf parks inside its own child
    // driver and no `ToolCallLaunch::Pending` reaches the turn driver — but
    // retained until PR B removes the batch machinery that shares it.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    async fn await_pending_tool_completion(
        &mut self,
        machine: &mut TurnMachine,
        parent_effect_id: crate::sansio::EffectId,
        call_id: &str,
        key: crate::AwaitEventKey,
        _pending: &crate::PendingCompletion,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<crate::Resolution, RuntimeEffectControllerError> {
        let parent =
            self.turn_effect_invocation(machine, parent_effect_id, RuntimeEffectKind::ToolBatch)?;
        let invocation = crate::runtime::causal::child_effect_invocation_from_effect(
            self.scoped_effect_controller.execution_scope(),
            &parent,
            format!("{}:{call_id}:await", parent_effect_id.0),
            format!("{call_id}:await"),
        );
        let _ = event_tx;
        let scoped_effect_controller = self.scoped_effect_controller.clone();
        let turn_cancel_wait = self.turn_cancel_wait(cancel.clone());
        let deadline = _pending
            .deadline
            .map(|duration| self.host.core.clock.now() + duration);
        let outcome = scoped_effect_controller
            .execute_effect(
                RuntimeEffectEnvelope::new(invocation, RuntimeEffectCommand::AwaitEvent { key }),
                crate::RuntimeEffectLocalExecutor::await_event_under(
                    &turn_cancel_wait,
                    deadline,
                    Arc::clone(&self.host.core.clock),
                ),
            )
            .await?;
        RuntimeEffectOutcome::into_await_event(outcome)
    }
}
