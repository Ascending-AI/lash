use super::*;

impl LashlangProcessHost<'_> {
    pub(super) async fn append_tool_effect_outcome(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        operation: &str,
        replay_key: &str,
        output: &lash_core::ToolCallOutput,
    ) -> Result<(), ExecutionHostError> {
        let (outcome_class, code) = match &output.outcome {
            lash_core::ToolCallOutcome::Success(_) => {
                (lash_core::ProcessEffectOutcomeClass::Success, None)
            }
            lash_core::ToolCallOutcome::Failure(failure) => (
                lash_core::ProcessEffectOutcomeClass::Failure,
                Some(lash_core::FailureCode::from_foreign_wire(&failure.code)),
            ),
            lash_core::ToolCallOutcome::Cancelled(_) => {
                (lash_core::ProcessEffectOutcomeClass::Cancelled, None)
            }
        };
        self.ctx
            .append_process_event(
                lash_core::ProcessEffectSummaryOccurrence::new(
                    call_site.site.node_id.clone(),
                    call_site.occurrence,
                    operation,
                    outcome_class,
                    code,
                    replay_key,
                )
                .append_request(),
            )
            .await
            .map_err(|error| LashlangHostError::AppendProcessEvent {
                message: error.to_string(),
            })?;
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "the TypeScript runtime receiver was checked above per site, and each batch result slot was filled by the same loop that reserved the Vec of slots"
    )]
    pub(super) async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> Result<lashlang::ResourceOperationBatchResult, ExecutionHostError> {
        let occurrence = batch.occurrence;
        let call_sites = batch
            .operations
            .iter()
            .map(|operation| operation.call_site.clone())
            .collect::<Vec<_>>();
        let mut results = vec![None; batch.operations.len()];
        let mut positions = Vec::new();
        let mut invocations = Vec::new();
        let mut outcome_metadata = Vec::new();
        for (index, operation) in batch.operations.into_iter().enumerate() {
            if crate::is_typescript_runtime_receiver(&operation.receiver) {
                let result = match operation.call_site.as_ref() {
                    Some(call_site) => {
                        let effect_id = self.resource_tool_call_id(
                            "typescript.runtime",
                            call_site,
                            Some(index),
                        );
                        crate::typescript_runtime::journaled_process_typescript_runtime_value(
                            &self.ctx,
                            effect_id,
                            &operation.receiver,
                            &operation.operation,
                            &operation.args,
                            call_site,
                        )
                        .await
                        .expect("TypeScript runtime receiver checked above")
                    }
                    None => Err(ExecutionHostError::new(
                        "TypeScript runtime operation is missing its call site",
                    )),
                };
                results[index] = Some(lashlang::ResourceOperationResult::from_result(result));
                continue;
            }
            match self.prepare_resource_invocation(
                operation.operation,
                operation.receiver,
                operation.args,
                operation.call_site,
                Some(index),
            ) {
                Ok(PreparedResourceInvocation::Trigger {
                    operation,
                    payload,
                    effect_id,
                    call_site,
                }) => {
                    let result = crate::trigger_commands::execute_process_trigger_operation(
                        &self.ctx,
                        self.artifact_store.as_ref(),
                        operation,
                        payload,
                        effect_id,
                        &call_site,
                    )
                    .await;
                    results[index] = Some(lashlang::ResourceOperationResult::from_result(result));
                }
                Ok(PreparedResourceInvocation::Tool {
                    invocation,
                    host_operation,
                    call_site,
                }) => {
                    positions.push(index);
                    outcome_metadata.push((host_operation, call_site, invocation.id.clone()));
                    invocations.push(invocation);
                }
                Err(error) => {
                    results[index] = Some(lashlang::ResourceOperationResult::Error(error));
                }
            }
        }

        let batch_id = lash_core::session::deterministic_tool_invocation_batch_id(
            &invocations,
            lash_core::session::ToolGroupOccurrence::Opener(occurrence),
        );
        if positions.len() > 1 {
            for (position, index) in positions.iter().copied().enumerate() {
                if let Some(Some(call_site)) = call_sites.get(index) {
                    self.lashlang_execution_trace.emit_waiting(
                        call_site,
                        TraceNodeAwaited::ToolBatch {
                            batch_id: batch_id.clone(),
                            position,
                        },
                    );
                }
            }
        }
        let batch = self
            .ctx
            .call_tool_batch(
                invocations,
                lash_core::session::ToolGroupOccurrence::Opener(occurrence),
            )
            .await;
        for ((index, reply), (host_operation, call_site, replay_key)) in positions
            .iter()
            .copied()
            .zip(batch.replies)
            .zip(outcome_metadata)
        {
            if let Some(record) = &reply.record {
                self.append_tool_effect_outcome(
                    &call_site,
                    &host_operation,
                    &replay_key,
                    &record.output,
                )
                .await?;
            }
            results[index] = Some(lashlang::ResourceOperationResult::from_result(
                protocol_tool_reply_to_lashlang_value(reply, &replay_key, &self.cancellation),
            ));
        }

        if !self.cancellation.is_cancelled() && positions.len() > 1 {
            for index in positions.iter().copied() {
                if let Some(Some(call_site)) = call_sites.get(index) {
                    self.lashlang_execution_trace
                        .emit_resumed(call_site, TraceNodeWaitResolution::Resumed);
                }
            }
        }

        // The batch counts settlement in its own invocation positions; the VM
        // counts in the aggregate's leaf positions. Leaves that failed before
        // the batch ran had already settled, so they lead.
        let mut settlement_order = (0..results.len())
            .filter(|index| !positions.contains(index))
            .collect::<Vec<_>>();
        // `call_tool_batch` refuses a malformed order at its boundary, so every
        // reported position is a real invocation position here. Filtering again
        // would only convert a future defect back into a silent repair.
        settlement_order.extend(
            batch
                .settlement_order
                .iter()
                .filter_map(|position| positions.get(*position).copied()),
        );

        Ok(lashlang::ResourceOperationBatchResult::settled_in_order(
            results
                .into_iter()
                .map(|result| result.expect("every batch result slot should be filled"))
                .collect(),
            settlement_order,
        ))
    }
}
