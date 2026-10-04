use super::*;
use crate::session::tool_execution::{
    ToolAggregateConsumer, ToolRunAggregateCursor, ToolRunAggregatePoll,
};
use crate::session::tool_execution::{
    ToolAggregateLeaf, ToolAggregateLeafReply, ToolInvocationReply,
};

impl<'run> ProductionToolHandlers<'run> {
    pub(crate) async fn admit_aggregate<'owner>(
        self: &Arc<Self>,
        run: &mut RunCoordinator<'owner>,
        request: ToolAggregateRequest,
        parent: Option<crate::RuntimeInvocation>,
    ) -> Result<ToolRunAggregateCursor, SingletonRunError>
    where
        'run: 'owner,
    {
        let ToolAggregateRequest {
            leaves,
            consumer: _,
            settled_value_after,
            command,
        } = request;
        let key = self.context.command_group_key(&command);
        let mut plan = AggregatePlan {
            key: key.clone(),
            leaves: Vec::new(),
            operands: Vec::new(),
        };
        let mut calls = Vec::new();
        let mut positions = Vec::new();
        let mut invocations = BTreeMap::new();
        for (position, leaf) in leaves.into_iter().enumerate() {
            if settled_value_after == Some(position) {
                plan.leaves.push(AggregateLeaf::Settled { fulfilled: true });
                positions.push(None);
            }
            match leaf {
                ToolAggregateLeaf::Settled { fulfilled } => {
                    plan.leaves.push(AggregateLeaf::Settled { fulfilled })
                }
                ToolAggregateLeaf::Timer { duration_ms } => {
                    plan.leaves.push(AggregateLeaf::Timer { duration_ms })
                }
                ToolAggregateLeaf::Tool(invocation) => {
                    let recorded: Option<Prepared> = run
                        .prepared_value(&invocation.id)?
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(|error| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RecordEncodingFailed,
                                error.to_string(),
                            )
                        })?;
                    let definition = recorded
                        .as_ref()
                        .map(|prepared| prepared.input.definition.clone())
                        .or_else(|| invocation.recorded_binding.as_deref().cloned())
                        .or_else(|| {
                            invocation.execution_grant.as_deref().map(|grant| {
                                crate::ToolDefinition {
                                    manifest: grant.manifest().clone(),
                                    contract: grant.contract().clone(),
                                }
                            })
                        })
                        .or_else(|| {
                            self.context
                                .tool_catalog()
                                .tools
                                .iter()
                                .find(|definition| definition.manifest.id == invocation.tool_id)
                                .map(|entry| crate::ToolDefinition {
                                    manifest: entry.manifest.clone(),
                                    contract: entry.contract.as_ref().clone(),
                                })
                        })
                        .ok_or_else(|| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                                format!("tool `{}` is unavailable", invocation.tool_id),
                            )
                        })?;
                    let source = invocation
                        .execution_grant
                        .as_deref()
                        .and_then(|grant| grant.source_id.as_deref());
                    let binding = match &recorded {
                        Some(prepared) => prepared.input.binding.clone(),
                        None => self
                            .context
                            .dispatch()
                            .plugins
                            .tool_run_binding(&invocation.tool_id, source)
                            .map_err(crate::RuntimeEffectControllerError::from)?,
                    };
                    binding
                        .require_available(&self.context.dispatch().plugins.tool_run_revisions())
                        .map_err(|cause| AdmissionRefusal::BindingUnavailable {
                            member: position as u32,
                            cause: Box::new(cause),
                        })?;
                    if let Some(grant) = &invocation.execution_grant {
                        self.context
                            .dispatch()
                            .plugins
                            .validate_tool_owner(&grant.owner)
                            .map_err(crate::RuntimeEffectControllerError::from)?;
                    }
                    let input = CallInput {
                        definition: definition.clone(),
                        pending: invocation.pending.clone(),
                        grant: invocation.execution_grant.clone(),
                        parent: parent.clone(),
                        binding: binding.clone(),
                    };
                    self.calls
                        .lock_recover()
                        .insert(invocation.id.clone(), input);
                    if !run.contains_call(&invocation.id) {
                        calls.push(SingletonToolCall {
                            owner: run.owner().clone(),
                            segment: run.segment(),
                            call_id: invocation.id.clone(),
                            tool_name: definition.manifest.name,
                            arguments: invocation.args.clone(),
                            declaration: definition.manifest.declaration,
                            binding,
                            available: self.context.dispatch().plugins.tool_run_revisions(),
                            cancel: ExternalCancelPolicy::CancelExternalWork,
                            environment: self.environment.clone(),
                        });
                    }
                    plan.leaves.push(AggregateLeaf::Call {
                        call_id: invocation.id.clone(),
                    });
                    invocations.insert(position, invocation.id);
                }
            }
            positions.push(Some(position));
        }
        if settled_value_after == Some(positions.len()) {
            plan.leaves.push(AggregateLeaf::Settled { fulfilled: true });
            positions.push(None);
        }
        plan.operands = (0..plan.leaves.len()).map(|index| index as u32).collect();
        run.start_aggregate(
            &plan,
            &calls,
            self.clone(),
            RecordedRetryPolicy::Never,
            self.context.dispatch().clock.as_ref(),
        )
        .await?;
        Ok(ToolRunAggregateCursor {
            owner: run.owner().clone(),
            key,
            positions,
            calls: invocations,
        })
    }

    pub(crate) async fn consume_aggregate(
        &self,
        run: &mut RunCoordinator<'_>,
        cursor: ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
        wait: bool,
    ) -> Result<ToolRunAggregatePoll, SingletonRunError> {
        if &cursor.owner != run.owner() {
            return Err(ContinuationRefusal::ForeignOwner.into());
        }
        let ToolRunAggregateCursor {
            key,
            positions,
            calls: invocations,
            ..
        } = cursor;
        let plan = run.aggregate_plan(&key)?;
        if positions.len() != plan.leaves.len() || invocations.iter().any(|(position, id)| !positions.iter().enumerate().any(|(operand, slot)| slot == &Some(*position) && matches!(&plan.leaves[operand], AggregateLeaf::Call { call_id } if call_id == id))) { return Err(crate::tool_run::RunEventRefusal::AggregateShape { key }.into()); }
        let mode = match consumer {
            ToolAggregateConsumer::All => AggregateConsumer::All,
            ToolAggregateConsumer::AllSettled => AggregateConsumer::AllSettled,
            ToolAggregateConsumer::Race => AggregateConsumer::Race,
            ToolAggregateConsumer::Any => AggregateConsumer::Any,
        };
        let outcome = loop {
            let outcome = run
                .consume_aggregate_with_control(
                    &key,
                    mode,
                    consumer != ToolAggregateConsumer::AllSettled,
                )
                .await?;
            if !matches!(outcome, RunAggregateOutcome::Pending) {
                break outcome;
            }
            if !wait {
                return Ok(ToolRunAggregatePoll::Pending);
            }
            if plan.leaves.is_empty() {
                std::future::pending::<()>().await;
            }
            run.await_one_deferred().await?;
        };
        let reply = |operand: usize,
                     terminal: Option<SingletonTerminal>|
         -> Result<Option<ToolAggregateLeafReply>, SingletonRunError> {
            let Some(position) = positions[operand] else {
                return Ok(None);
            };
            let Some(call_id) = invocations.get(&position) else {
                return Ok(matches!(plan.leaves[operand], AggregateLeaf::Timer { .. })
                    .then_some(ToolAggregateLeafReply::Timer));
            };
            let terminal = terminal.ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                    "an aggregate tool has no terminal",
                )
            })?;
            let output = match terminal {
                SingletonTerminal::Final {
                    presentation,
                    capture,
                    ..
                } => {
                    let presented: Presented = decode(&presentation).map_err(|message| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            message,
                        )
                    })?;
                    let prepared = self
                        .prepared
                        .lock_recover()
                        .get(call_id)
                        .cloned()
                        .ok_or_else(|| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::EffectReplayDivergence,
                                "a presented call has no admission",
                            )
                        })?;
                    let captured: Captured = decode(capture.output().ok_or_else(|| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            "the final has no capture",
                        )
                    })?)
                    .map_err(|message| {
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::RecordEncodingFailed,
                            message,
                        )
                    })?;
                    let record = ToolCallRecord {
                        call_id: call_id.clone(),
                        provider_call_id: prepared.call.provider_call_id.clone(),
                        tool: prepared.call.tool_name.clone(),
                        args: prepared.call.args.clone(),
                        output: captured.output,
                    };
                    let mut reply = ToolInvocationReply::from_output(record.output.clone())
                        .with_record(record.clone());
                    reply.completed = Some(Box::new(crate::sansio::CompletedToolCall {
                        call_id: record.call_id,
                        provider_call_id: record.provider_call_id,
                        tool_name: record.tool,
                        args: record.args,
                        output: record.output,
                        model_return: presented.presentation.model_return,
                        intent_outcomes: presented.intent_outcomes,
                        replay: prepared.input.pending.and_then(|pending| pending.replay),
                    }));
                    reply
                }
                SingletonTerminal::Withheld { decision } => {
                    let recorded_cause = run.withheld_cause(call_id);
                    let output = match recorded_cause
                        .as_ref()
                        .map(|cause| cause.error_type.as_str())
                    {
                        Some("tool_failure") => ToolCallOutput::failure(
                            serde_json::from_value(recorded_cause.unwrap().payload).map_err(
                                |error| {
                                    crate::RuntimeEffectControllerError::new(
                                        crate::RuntimeErrorCode::RecordEncodingFailed,
                                        error.to_string(),
                                    )
                                },
                            )?,
                        ),
                        Some("tool_cancellation") => ToolCallOutput::cancelled(
                            serde_json::from_value(recorded_cause.unwrap().payload).map_err(
                                |error| {
                                    crate::RuntimeEffectControllerError::new(
                                        crate::RuntimeErrorCode::RecordEncodingFailed,
                                        error.to_string(),
                                    )
                                },
                            )?,
                        ),
                        _ => match decision {
                            CallDecision::Cancelled => {
                                ToolCallOutput::cancelled(crate::ToolCancellation::runtime(
                                    "the owning Run cancelled the call",
                                ))
                            }
                            _ => ToolCallOutput::failure(crate::ToolFailure::runtime(
                                crate::ToolFailureClass::InvalidRequest,
                                "tool_call_denied",
                                "a recorded tool check denied the call",
                            )),
                        },
                    };
                    let prepared = self
                        .prepared
                        .lock_recover()
                        .get(call_id)
                        .cloned()
                        .ok_or_else(|| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::EffectReplayDivergence,
                                "a withheld call has no recorded preparation",
                            )
                        })?;
                    let record = ToolCallRecord {
                        call_id: call_id.clone(),
                        provider_call_id: prepared.call.provider_call_id,
                        tool: prepared.call.tool_name,
                        args: prepared.call.args,
                        output,
                    };
                    let mut reply = ToolInvocationReply::from_output(record.output.clone())
                        .with_record(record.clone());
                    reply.completed = Some(Box::new(crate::sansio::CompletedToolCall {
                        model_return: crate::ModelToolReturn::from_output(
                            record.tool.clone(),
                            &record.output,
                        ),
                        call_id: record.call_id,
                        provider_call_id: record.provider_call_id,
                        tool_name: record.tool,
                        args: record.args,
                        output: record.output,
                        intent_outcomes: Vec::new(),
                        replay: prepared.input.pending.and_then(|pending| pending.replay),
                    }));
                    reply
                }
                SingletonTerminal::Deferred { .. } => {
                    return Err(crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                        "a Deferred descriptor is not a result",
                    )
                    .into());
                }
            };
            Ok(Some(ToolAggregateLeafReply::Tool(Box::new(output))))
        };
        let all = |terminals: Vec<Option<SingletonTerminal>>| -> Result<Vec<Option<ToolAggregateLeafReply>>, SingletonRunError> {
            terminals.into_iter().enumerate().filter(|(operand, _)| positions[*operand].is_some()).map(|(operand, terminal)| reply(operand, terminal)).collect()
        };
        let settlement_order = run
            .aggregate_settlement_order(&key)?
            .into_iter()
            .filter_map(|operand| positions[operand])
            .collect();
        let outcome = match outcome {
            RunAggregateOutcome::Selected {
                operand,
                reply: terminal,
                ..
            } => match positions[operand as usize] {
                Some(leaf) => ToolAggregateOutcome::Selected {
                    leaf,
                    reply: reply(operand as usize, terminal)?,
                },
                None => ToolAggregateOutcome::SettledValue,
            },
            RunAggregateOutcome::AllResults(terminals) => {
                ToolAggregateOutcome::AllResults(all(terminals)?)
            }
            RunAggregateOutcome::ExhaustedRejections(terminals) => {
                ToolAggregateOutcome::ExhaustedRejections(all(terminals)?)
            }
            RunAggregateOutcome::HostControl { call_id, decision } => {
                let mut error = crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    format!("tool {call_id}: {decision:?}"),
                );
                error.cause = Some(crate::RuntimeErrorCause::ToolRunControl {
                    cause: run.withheld_cause(&call_id).map(Box::new),
                    call_id,
                    aborted: decision == CallDecision::Aborted,
                });
                error.journaled = true;
                return Err(error.into());
            }
            RunAggregateOutcome::Pending => unreachable!("awaited until an observable result"),
        };
        Ok(ToolRunAggregatePoll::Ready {
            outcome,
            settlement_order,
        })
    }
}
