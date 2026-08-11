use super::*;
use crate::facade_support::ScopedEffectControllerFacadeOps;

pub(in crate::runtime) struct ToolBatchRunOutcome {
    pub launches: Vec<crate::runtime::ToolCallLaunch>,
    pub triggers: Vec<crate::tool_dispatch::ToolTriggerEffectOutcome>,
}

impl RuntimeTurnDriver<'_> {
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
        let runtime_event_tx = event_tx.clone();
        let tool_event_forwarder = crate::task::spawn(async move {
            while let Some(event) = tool_event_rx.recv().await {
                send_session_event(&runtime_event_tx, event).await;
            }
        });
        let prepare_context = self
            .execution_context(
                tool_event_tx.clone(),
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
            match prepare_context.prepare_tool_call(call).await {
                crate::tool_dispatch::ToolPreparationOutcome::Prepared(prepared) => {
                    prepared_entries.push((index, prepared));
                }
                crate::tool_dispatch::ToolPreparationOutcome::Completed(outcome) => {
                    let completed = prepare_context
                        .complete_tool_call(
                            index,
                            call_id.clone(),
                            replay,
                            *outcome,
                            crate::TurnActivityId::new(format!("tool:{call_id}")),
                        )
                        .await
                        .completed;
                    results[index] = Some(completed);
                }
            }
        }

        if !prepared_entries.is_empty() {
            let parent_invocation =
                self.turn_effect_invocation(machine, id, RuntimeEffectKind::ToolBatch)?;
            let batch = crate::PreparedToolBatch::new(
                id.0.to_string(),
                prepared_entries
                    .iter()
                    .map(|(_, prepared)| prepared.clone())
                    .collect(),
            );
            let outcome = self
                .execute_typed_turn_effect(
                    machine,
                    event_tx,
                    cancel,
                    RuntimeEffectEnvelope::new(
                        parent_invocation,
                        RuntimeEffectCommand::ToolBatch { batch },
                    ),
                    RuntimeEffectOutcome::into_tool_batch_effect,
                )
                .await?;
            if outcome.launches.len() != prepared_entries.len() {
                return Err(RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ToolBatchResultCountMismatch,
                    format!(
                        "tool batch returned {} launches for {} prepared calls",
                        outcome.launches.len(),
                        prepared_entries.len()
                    ),
                ));
            }
            for ((source_index, prepared), launch) in
                prepared_entries.into_iter().zip(outcome.launches)
            {
                let call_id = prepared.call_id.clone();
                let replay = prepared.replay.clone();
                match launch {
                    crate::runtime::ToolCallLaunch::Done { result } => {
                        results[source_index] = Some(*result);
                    }
                    crate::runtime::ToolCallLaunch::Pending {
                        key,
                        pending,
                        duration_ms,
                    } => {
                        let resolution = self
                            .await_pending_tool_completion(
                                machine, id, &call_id, *key, &pending, event_tx, cancel,
                            )
                            .await?;
                        let dispatch_outcome = prepare_context
                            .pending_completion_dispatch_outcome(
                                prepared.tool_name.clone(),
                                prepared.args.clone(),
                                resolution,
                                duration_ms,
                                Vec::new(),
                            )
                            .await;
                        let completed = prepare_context
                            .complete_tool_call(
                                source_index,
                                call_id.clone(),
                                replay,
                                dispatch_outcome,
                                crate::TurnActivityId::new(format!("tool:{call_id}")),
                            )
                            .await
                            .completed;
                        send_turn_activity(
                            event_tx,
                            crate::TurnActivityId::new(format!("tool:{call_id}")),
                            crate::TurnEvent::ToolCallCompleted {
                                call_id: Some(call_id.clone()),
                                name: completed.tool_name.clone(),
                                args: completed.args.clone(),
                                output: completed.output.clone(),
                                duration_ms: completed.duration_ms,
                                graph_key: None,
                                parent_call_id: None,
                            },
                        )
                        .await;
                        results[source_index] = Some(completed);
                    }
                }
            }
        }
        drop(prepare_context);
        drop(tool_event_tx);
        let _ = tool_event_forwarder.await;
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
        let protocol_iteration = invocation.scope.protocol_iteration.unwrap_or_default();
        let context = match self.execution_context(
            tool_event_tx.clone(),
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
        let outcome = context
            .execute_prepared_tool_batch_launches(
                batch,
                invocation,
                std::collections::HashMap::new(),
            )
            .await?;
        drop(context);
        drop(tool_event_tx);
        drop(turn_event_tx);
        let _ = tool_event_forwarder.await;
        let _ = turn_event_forwarder.await;
        Ok(ToolBatchRunOutcome {
            launches: outcome.launches,
            triggers: outcome.triggers,
        })
    }

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
        let invocation = crate::runtime::causal::child_effect_invocation(
            &parent,
            format!("{}:{call_id}:await", parent_effect_id.0),
            RuntimeEffectKind::AwaitEvent,
            format!("{call_id}:await"),
        );
        let _ = event_tx;
        let scoped_effect_controller = self.scoped_effect_controller.clone();
        let deadline = _pending
            .deadline
            .map(|duration| self.host.core.clock.now() + duration);
        let outcome = scoped_effect_controller
            .controller()
            .execute_effect(
                RuntimeEffectEnvelope::new(invocation, RuntimeEffectCommand::AwaitEvent { key }),
                crate::RuntimeEffectLocalExecutor::await_event_with_clock(
                    cancel.clone(),
                    deadline,
                    Arc::clone(&self.host.core.clock),
                )
                .with_turn_cancel_scope(scoped_effect_controller.execution_scope().clone()),
            )
            .await?;
        RuntimeEffectOutcome::into_await_event(outcome)
    }
}
