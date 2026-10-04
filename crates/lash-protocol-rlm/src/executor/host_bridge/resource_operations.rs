//! Preparation and source pairing for every resource call of one cell.

use super::*;

/// Metadata travels together even when another operand fails preparation.
struct PreparedCall {
    source_operation: String,
    host_operation: String,
    call_site: lashlang::LashlangExecutionCallSite,
    operand_index: Option<usize>,
    execution_index: usize,
    logical_call_id: lash_core::ToolCallId,
}

/// Preparation chooses the operation; callers only choose error disposition.
enum PreparedOperation {
    Runtime(String),
    Trigger {
        call: PreparedCall,
        operation: lashlang::TriggerHostOperation,
        payload: Value,
    },
    Tool {
        call: PreparedCall,
        invocation: Box<ToolInvocation>,
        drift: Option<lash_core::RuntimeEffectControllerError>,
    },
}

impl HostBridge<'_> {
    async fn prepare_resource_operation(
        &self,
        operation: lashlang::ResourceOperation,
        ordinal: u64,
        operand_index: Option<usize>,
    ) -> Result<PreparedOperation, ExecutionHostError> {
        let lashlang::ResourceOperation {
            operation,
            receiver,
            args,
            call_site,
        } = operation;
        if let Some(checked) =
            lash_lashlang_runtime::language_runtime_operation(&receiver, &operation, &args)
        {
            return checked.map(|operation| PreparedOperation::Runtime(operation.to_owned()));
        }
        let FlowValue::Resource(receiver) = &receiver else {
            return Err(ExecutionHostError::from(
                lash_lashlang_runtime::LashlangHostError::ModuleAuthorityRequired { operation },
            ));
        };
        let host_operation = lash_lashlang_runtime::resolve_lashlang_module_operation(
            &self.host_environment,
            receiver,
            &operation,
        )?;
        let source_operation = format!("{}.{}", receiver.alias, operation);
        let payload = operation_payload(&args).await?;
        let call_site =
            Self::require_call_site(&operation, &host_operation, call_site.as_ref())?.clone();
        let execution_index = self.next_index();
        let logical_call_id = self.resource_tool_call_id(ordinal, &call_site, operand_index)?;
        let call = PreparedCall {
            source_operation,
            host_operation,
            call_site,
            operand_index,
            execution_index,
            logical_call_id,
        };
        if let Some(operation) =
            lashlang::TriggerHostOperation::from_host_operation(&call.host_operation)
        {
            return Ok(PreparedOperation::Trigger {
                call,
                operation,
                payload,
            });
        }
        let mut invocation = self.tool_invocation(
            call.logical_call_id.clone(),
            &call.host_operation,
            payload,
            Some(&call.call_site),
        );
        let drift = self
            .cell_bindings
            .drift_for(&invocation.tool_id)
            .map(|drift| {
                invocation = invocation
                    .clone()
                    .with_recorded_binding(drift.recorded_binding());
                drift.refusal()
            });
        Ok(PreparedOperation::Tool {
            call,
            invocation: Box::new(invocation),
            drift,
        })
    }

    fn consume_resource_reply(
        &self,
        call: &PreparedCall,
        reply: ToolInvocationReply,
        replay_key: &str,
    ) -> Result<FlowValue, ExecutionHostError> {
        let outcome = match &reply.output.outcome {
            lash_core::ToolCallOutcome::Success(_) => lash_core::ExecutedCallOutcome::Ok,
            lash_core::ToolCallOutcome::Failure(_) | lash_core::ToolCallOutcome::Cancelled(_) => {
                lash_core::ExecutedCallOutcome::Err
            }
        };
        let (result, host_record) = self.consume_reply(reply, replay_key);
        self.record_executed_call(
            call.execution_index,
            call.source_operation.clone(),
            outcome,
            host_record,
        )
        .and(result)
    }

    fn consume_resource_trigger(
        &self,
        call: PreparedCall,
        result: Result<FlowValue, ExecutionHostError>,
    ) -> Result<FlowValue, ExecutionHostError> {
        let outcome = if result.is_ok() {
            lash_core::ExecutedCallOutcome::Ok
        } else {
            lash_core::ExecutedCallOutcome::Err
        };
        self.record_executed_call(call.execution_index, call.source_operation, outcome, None)
            .and(result)
    }

    pub(super) async fn resource_operation(
        &self,
        operation: String,
        receiver: FlowValue,
        args: Vec<FlowValue>,
        call_site: Option<lashlang::LashlangExecutionCallSite>,
    ) -> Result<AbilityOutcome, ExecutionHostError> {
        let commands = self.commands()?;
        let command = commands.issue()?;
        let prepared = match self
            .prepare_resource_operation(
                lashlang::ResourceOperation {
                    operation,
                    receiver,
                    args,
                    call_site,
                },
                command.ordinal,
                None,
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                commands.skipped(&command)?;
                return Err(error);
            }
        };
        match prepared {
            PreparedOperation::Runtime(operation) => {
                let in_flight = commands.enter(command, CommandShape::Value).await?;
                let result = lash_lashlang_runtime::journaled_language_runtime_value(
                    &in_flight.ctx,
                    in_flight.command.key.to_string(),
                    &operation,
                )
                .await;
                commands.finish(&in_flight)?;
                match result {
                    Ok(value) => value.map(AbilityOutcome::Value),
                    Err(error) => Err(commands.journal_error(&in_flight, error, |error| {
                        ExecutionHostError::new(error.to_string())
                    })),
                }
            }
            PreparedOperation::Trigger {
                call,
                operation,
                payload,
            } => {
                let in_flight = commands.enter(command, CommandShape::Value).await?;
                let result = lash_lashlang_runtime::execute_trigger_operation(
                    &self.workers,
                    &in_flight.ctx,
                    &self.artifact_store,
                    operation,
                    payload,
                    in_flight.command.key.to_string(),
                )
                .await;
                commands.finish(&in_flight)?;
                self.consume_resource_trigger(call, result)
                    .map(AbilityOutcome::Value)
            }
            PreparedOperation::Tool {
                call,
                invocation,
                drift,
            } => {
                let in_flight = commands
                    .enter_bound(command, CommandShape::ToolCall, drift)
                    .await?;
                let reply = Box::pin(
                    in_flight
                        .ctx
                        .call_command_tool(&in_flight.command.key, *invocation),
                )
                .await;
                if in_flight.ctx.take_wait_handed_over() {
                    commands.hand_over(&in_flight)?;
                    return Ok(AbilityOutcome::HandedOver);
                }
                commands.finish(&in_flight)?;
                self.consume_resource_reply(&call, reply, in_flight.command.key.as_str())
                    .map(AbilityOutcome::Value)
            }
        }
    }

    pub(super) async fn resource_operation_batch(
        &self,
        batch: lashlang::ResourceOperationBatch,
    ) -> Result<AbilityOutcome, ExecutionHostError> {
        let lashlang::ResourceOperationBatch {
            leaves,
            consumer,
            settled_value_after,
        } = batch;
        let commands = self.commands()?;
        let command = commands.issue()?;
        let in_flight = commands.enter(command, CommandShape::Aggregate).await?;
        let leaf_key = |index| {
            format!(
                "{}:{}",
                in_flight.command.key,
                lash_core::CommandReplayKey::child_suffix(index),
            )
        };
        let mut bridge_leaves = Vec::with_capacity(leaves.len());
        let mut dispatched = BTreeMap::new();
        for (index, leaf) in leaves.into_iter().enumerate() {
            let operation = match leaf {
                lashlang::ResourceOperationBatchLeaf::Operation(operation) => operation,
                lashlang::ResourceOperationBatchLeaf::Timer(sleep) => {
                    bridge_leaves.push(match lash_lashlang_runtime::timer_duration_ms(&sleep) {
                        Ok(duration_ms) => {
                            lash_lashlang_runtime::BridgeAggregateLeaf::Timer { duration_ms }
                        }
                        Err(error) => {
                            lash_lashlang_runtime::BridgeAggregateLeaf::Settled(Err(error))
                        }
                    });
                    continue;
                }
            };
            let prepared = self
                .prepare_resource_operation(operation, in_flight.command.ordinal, Some(index))
                .await;
            let leaf = match prepared {
                Ok(PreparedOperation::Runtime(operation)) => {
                    let result = match lash_lashlang_runtime::journaled_language_runtime_value(
                        &in_flight.ctx,
                        leaf_key(index),
                        &operation,
                    )
                    .await
                    {
                        Ok(value) => value,
                        Err(error) => Err(commands.journal_error(&in_flight, error, |error| {
                            ExecutionHostError::new(error.to_string())
                        })),
                    };
                    lash_lashlang_runtime::BridgeAggregateLeaf::Settled(result)
                }
                Ok(PreparedOperation::Trigger {
                    call,
                    operation,
                    payload,
                }) => {
                    let result = lash_lashlang_runtime::execute_trigger_operation(
                        &self.workers,
                        &in_flight.ctx,
                        &self.artifact_store,
                        operation,
                        payload,
                        leaf_key(index),
                    )
                    .await;
                    lash_lashlang_runtime::BridgeAggregateLeaf::Settled(
                        self.consume_resource_trigger(call, result),
                    )
                }
                Ok(PreparedOperation::Tool {
                    call, invocation, ..
                }) => {
                    let operand = call.operand_index.ok_or_else(|| {
                        ExecutionHostError::new(
                            "a batch call was prepared without its operand index",
                        )
                    })?;
                    dispatched.insert(operand, call);
                    lash_lashlang_runtime::BridgeAggregateLeaf::Tool(*invocation)
                }
                Err(error) => lash_lashlang_runtime::BridgeAggregateLeaf::Settled(Err(error)),
            };
            bridge_leaves.push(leaf);
        }
        if let Some(trace) = &self.lashlang_execution_trace
            && dispatched.len() > 1
        {
            for (position, call) in dispatched.values().enumerate() {
                trace.emit_waiting(
                    &call.call_site,
                    lash_lashlang_runtime::TraceNodeAwaited::ToolBatch {
                        batch_id: in_flight.command.key.to_string(),
                        position,
                    },
                );
            }
        }
        let reply = lash_lashlang_runtime::settle_bridge_aggregate(
            &in_flight.ctx,
            &in_flight.command.key,
            consumer,
            settled_value_after,
            bridge_leaves,
            |leaf, reply| {
                let call = dispatched.get(&leaf).ok_or_else(|| {
                    ExecutionHostError::new(format!(
                        "aggregate leaf {leaf} was answered as a tool call it never dispatched",
                    ))
                })?;
                self.consume_resource_reply(call, reply, &leaf_key(leaf))
            },
        )
        .await;
        if in_flight.ctx.take_wait_handed_over() {
            commands.hand_over(&in_flight)?;
            return Ok(AbilityOutcome::HandedOver);
        }
        commands.finish(&in_flight)?;
        if !self.is_cancelled()
            && dispatched.len() > 1
            && let Some(trace) = &self.lashlang_execution_trace
        {
            for call in dispatched.values() {
                trace.emit_resumed(
                    &call.call_site,
                    lash_lashlang_runtime::TraceNodeWaitResolution::Resumed,
                );
            }
        }
        reply.map(AbilityOutcome::ResourceOperationBatch)
    }
}
