use super::*;

/// What one run's effect-summary writer carries between incorporations: the
/// occurrences it counted past the cap (restored from and snapshotted into
/// segment state, so a redrive re-derives the same counts) and the first
/// incorporation that failed.
#[derive(Default)]
pub(super) struct EffectSummaryWriter {
    omissions: std::sync::Mutex<BTreeMap<String, lash_core::ProcessEffectOmittedCounts>>,
    incorporation_fault: std::sync::Mutex<Option<lash_core::PluginError>>,
}

impl EffectSummaryWriter {
    pub(super) fn restore(
        omissions: BTreeMap<String, lash_core::ProcessEffectOmittedCounts>,
    ) -> Self {
        Self {
            omissions: std::sync::Mutex::new(omissions),
            incorporation_fault: std::sync::Mutex::new(None),
        }
    }

    pub(super) fn omissions(&self) -> BTreeMap<String, lash_core::ProcessEffectOmittedCounts> {
        self.omissions.lock_recover().clone()
    }

    pub(super) fn take_incorporation_fault(&self) -> Option<lash_core::PluginError> {
        self.incorporation_fault.lock_recover().take()
    }
}

impl LashlangProcessHost<'_> {
    /// Incorporates one recorded effect outcome into the durable summary.
    ///
    /// An occurrence within the cap is appended under the effect's replay
    /// key; a later one is counted for the run's omission record. A failed
    /// append is an incorporation failure, never an effect result: the run's
    /// scope is cancelled so the guest stops at its next step, and the run
    /// reports the failure as retryable infrastructure so a redrive
    /// re-incorporates the recorded result.
    pub(super) async fn record_effect_outcome(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        operation: &str,
        outcome_class: lash_core::ProcessEffectOutcomeClass,
        code: Option<lash_core::FailureCode>,
        replay_key: &str,
    ) {
        if !lash_core::ProcessEffectSummaryOccurrence::is_within_cap(call_site.occurrence) {
            self.effect_summary
                .omissions
                .lock_recover()
                .entry(call_site.site.node_id.clone())
                .or_default()
                .record(outcome_class);
            return;
        }
        let request = lash_core::ProcessEffectSummaryOccurrence::new(
            call_site.site.node_id.clone(),
            call_site.occurrence,
            operation,
            outcome_class,
            code,
            replay_key,
        )
        .append_request();
        if let Err(error) = self.ctx.append_process_event(request).await {
            self.fail_incorporation(error);
        }
    }

    /// Appends the run's omission record, once, before its terminal output.
    pub(super) async fn record_effect_omissions(&self) {
        let omissions = self.effect_summary.omissions();
        if omissions.is_empty() {
            return;
        }
        let request = lash_core::ProcessEffectOmissions::new(omissions)
            .append_request(self.identities.effect_omissions());
        if let Err(error) = self.ctx.append_process_event(request).await {
            self.fail_incorporation(error);
        }
    }

    fn fail_incorporation(&self, error: lash_core::PluginError) {
        self.effect_summary
            .incorporation_fault
            .lock_recover()
            .get_or_insert(error);
        self.cancellation.cancel();
    }

    pub(super) async fn record_tool_reply(
        &self,
        call_site: &lashlang::LashlangExecutionCallSite,
        host_operation: &str,
        replay_key: &str,
        reply: &lash_core::facade_support::ToolInvocationReply,
    ) {
        let Some(record) = &reply.record else {
            return;
        };
        let (outcome_class, code) = match &record.output.outcome {
            lash_core::ToolCallOutcome::Success(_) => {
                (lash_core::ProcessEffectOutcomeClass::Success, None)
            }
            lash_core::ToolCallOutcome::Failure(failure) => (
                lash_core::ProcessEffectOutcomeClass::Failure,
                Some(lash_core::tool_failure_code(failure)),
            ),
            lash_core::ToolCallOutcome::Cancelled(_) => {
                (lash_core::ProcessEffectOutcomeClass::Cancelled, None)
            }
        };
        self.record_effect_outcome(call_site, host_operation, outcome_class, code, replay_key)
            .await;
    }

    #[expect(
        clippy::expect_used,
        reason = "callers dispatch here only for a TypeScript runtime receiver"
    )]
    pub(super) async fn typescript_runtime_value(
        &self,
        receiver: &lashlang::Value,
        operation: &str,
        args: &[lashlang::Value],
        call_site: &lashlang::LashlangExecutionCallSite,
        batch_index: Option<usize>,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let host_operation = crate::typescript_runtime::TYPESCRIPT_RUNTIME_HOST_OPERATION;
        let effect_id = self.resource_tool_call_id(host_operation, call_site, batch_index);
        let mut journaled = false;
        let result = crate::typescript_runtime::journaled_typescript_runtime_value_recording(
            &self.ctx,
            effect_id.clone(),
            receiver,
            operation,
            args,
            &mut journaled,
        )
        .await
        .expect("TypeScript runtime receiver checked by the caller");
        if journaled {
            self.record_effect_outcome(
                call_site,
                host_operation,
                lash_core::ProcessEffectOutcomeClass::Success,
                None,
                &effect_id,
            )
            .await;
        }
        result
    }

    pub(super) async fn trigger_operation(
        &self,
        operation: lashlang::TriggerHostOperation,
        payload: serde_json::Value,
        effect_id: String,
        host_operation: &str,
        call_site: &lashlang::LashlangExecutionCallSite,
    ) -> Result<lashlang::Value, ExecutionHostError> {
        let mut recorded = None;
        let result = crate::trigger_commands::execute_trigger_operation_recording(
            &self.ctx,
            self.artifact_store.as_ref(),
            operation,
            payload,
            effect_id.clone(),
            &mut recorded,
        )
        .await;
        if let Some((outcome_class, code)) = recorded {
            self.record_effect_outcome(call_site, host_operation, outcome_class, code, &effect_id)
                .await;
        }
        result
    }

    #[expect(
        clippy::expect_used,
        reason = "the TypeScript runtime receiver was checked above per site, and each batch result slot was filled by the same loop that reserved the Vec of slots"
    )]
    pub(super) async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> lashlang::ResourceOperationBatchResult {
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
                        self.typescript_runtime_value(
                            &operation.receiver,
                            &operation.operation,
                            &operation.args,
                            call_site,
                            Some(index),
                        )
                        .await
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
                    host_operation,
                    call_site,
                }) => {
                    let result = self
                        .trigger_operation(
                            operation,
                            payload,
                            effect_id,
                            &host_operation,
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
            self.record_tool_reply(&call_site, &host_operation, &replay_key, &reply)
                .await;
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

        lashlang::ResourceOperationBatchResult::settled_in_order(
            results
                .into_iter()
                .map(|result| result.expect("every batch result slot should be filled"))
                .collect(),
            settlement_order,
        )
    }
}
