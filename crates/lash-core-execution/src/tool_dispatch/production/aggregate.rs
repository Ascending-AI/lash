//! An aggregate's admission into the in-memory Run and its consumption.
use super::*;
use crate::session::tool_execution::{
    ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateLeafReply, ToolInvocationReply,
    ToolRunAggregateCursor, ToolRunAggregatePoll,
};
use crate::tool_dispatch::call_run::{Answer, CallEnd, Consumer, Leaf, ToolRun};

/// A call refused before the aggregate formed: its reply is its typed
/// failure.
#[derive(Clone)]
pub(super) struct RefusedInput {
    pending: crate::sansio::PendingToolCall,
    failure: crate::ToolFailure,
}

fn refused_reply(input: &RefusedInput) -> ToolAggregateLeafReply {
    let RefusedInput { pending, failure } = input.clone();
    let output = ToolCallOutput::failure(failure);
    let record = ToolCallRecord {
        call_id: pending.call_id.clone(),
        provider_call_id: pending.provider_call_id.clone(),
        tool: pending.tool_name.clone(),
        args: pending.args.clone(),
        output: output.clone(),
    };
    let mut reply = ToolInvocationReply::from_output(output.clone()).with_record(record);
    reply.completed = Some(Box::new(crate::sansio::CompletedToolCall {
        call_id: pending.call_id,
        provider_call_id: pending.provider_call_id,
        tool_name: pending.tool_name.clone(),
        args: pending.args,
        model_return: crate::ModelToolReturn::from_output(pending.tool_name, &output),
        output,
        intent_outcomes: Vec::new(),
        replay: pending.replay,
    }));
    ToolAggregateLeafReply::Tool(Box::new(reply))
}

fn drift(invocation: &crate::session::tool_execution::ToolInvocation) -> SingletonRunError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeToolRunShape,
        format!(
            "call {} appears twice in one aggregate with different requests",
            invocation.id
        ),
    )
    .into()
}

fn encoding(message: String) -> SingletonRunError {
    crate::RuntimeEffectControllerError::new(crate::RuntimeErrorCode::RecordEncodingFailed, message)
        .into()
}

impl<'run> ProductionToolHandlers<'run> {
    /// The definition a tool leaf is admitted under: its recorded binding,
    /// its grant's, or the catalog's.
    pub(super) fn leaf_definition(
        &self,
        invocation: &crate::session::tool_execution::ToolInvocation,
    ) -> Option<crate::ToolDefinition> {
        invocation
            .recorded_binding
            .as_deref()
            .cloned()
            .or_else(|| {
                invocation
                    .execution_grant
                    .as_deref()
                    .map(|grant| crate::ToolDefinition {
                        manifest: grant.manifest().clone(),
                        contract: grant.contract().clone(),
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
    }

    /// The call `invocation` runs as, its input registered with these
    /// handlers.
    #[expect(
        clippy::too_many_arguments,
        reason = "a leaf's admission reads its owner's frame, parent, environment and attribution"
    )]
    pub(super) async fn admit_leaf(
        &self,
        owner: &crate::EffectOpener,
        invocation: &crate::session::tool_execution::ToolInvocation,
        definition: crate::ToolDefinition,
        parent: Option<crate::RuntimeInvocation>,
        environment_spec: &crate::ProcessExecutionEnvSpec,
        attribution: &crate::session::ToolObservationAttribution,
        environment: &mut Option<crate::ProcessExecutionEnvRef>,
    ) -> Result<SingletonToolCall, SingletonRunError> {
        let source = invocation
            .execution_grant
            .as_deref()
            .and_then(|grant| grant.source_id.as_deref());
        let binding = self
            .context
            .dispatch()
            .plugins
            .tool_run_binding(&invocation.tool_id, source)
            .map_err(crate::RuntimeEffectControllerError::from)?;
        if let Some(grant) = &invocation.execution_grant {
            self.context
                .dispatch()
                .plugins
                .validate_tool_owner(&grant.owner)
                .map_err(crate::RuntimeEffectControllerError::from)?;
        }
        let environment = match environment {
            Some(reference) => reference.clone(),
            None => {
                let context = self
                    .context
                    .clone()
                    .with_execution_env_spec(environment_spec.clone());
                let claim = crate::session::execution_claim_of(
                    context.dispatch().effect_controller.execution_scope(),
                )
                .map_err(crate::RuntimeEffectControllerError::from)?;
                let reference = context
                    .captured_process_execution_env_ref(&claim)
                    .await
                    .map_err(crate::RuntimeEffectControllerError::from)?;
                *environment = Some(reference.clone());
                reference
            }
        };
        let mut attribution = attribution.clone();
        if invocation.issuing_language_node_id.is_some() {
            attribution.issuing_node_id = invocation.issuing_language_node_id.clone();
        }
        self.calls.lock_recover().insert(
            invocation.id.clone(),
            CallInput {
                attribution,
                definition: definition.clone(),
                pending: invocation.pending.clone(),
                grant: invocation.execution_grant.clone(),
                parent,
                binding: binding.clone(),
                environment: environment.clone(),
                render: environment_spec.render.clone(),
            },
        );
        Ok(SingletonToolCall {
            owner: owner.clone(),
            call_id: invocation.id.clone(),
            tool_name: definition.manifest.name,
            arguments: invocation.args.clone(),
            declaration: definition.manifest.declaration,
            binding,
            available: self.context.dispatch().plugins.tool_run_revisions(),
            cancel: ExternalCancelPolicy::CancelExternalWork,
            environment: Some(environment),
        })
    }

    /// Form one aggregate of `request` in `run`: its new calls start at
    /// once, each on its own, and its refused calls answer their typed
    /// failure. One refused member of a round refuses every member with it.
    pub(crate) async fn admit_aggregate<'owner>(
        self: &Arc<Self>,
        run: &mut ToolRun<'owner>,
        owner: &crate::EffectOpener,
        request: ToolAggregateRequest,
        parent: Option<crate::RuntimeInvocation>,
        environment_spec: crate::ProcessExecutionEnvSpec,
        attribution: crate::session::ToolObservationAttribution,
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
        let mut requested =
            BTreeMap::<crate::ToolCallId, &crate::session::tool_execution::ToolInvocation>::new();
        for leaf in &leaves {
            if let ToolAggregateLeaf::Tool(invocation) = leaf {
                match requested.get(&invocation.id) {
                    Some(original)
                        if original.tool_id != invocation.tool_id
                            || original.args != invocation.args =>
                    {
                        return Err(drift(invocation));
                    }
                    Some(_) => {}
                    None => {
                        requested.insert(invocation.id.clone(), invocation);
                    }
                }
            }
        }
        let key = self.context.command_group_key(&command);
        // A process holds its calls at once; a cell counts every call it
        // makes, and a protocol step without a cell is its own group.
        let capacity = if self.context.process_id().is_some() {
            crate::tool_run::CapacityScope::Held
        } else {
            crate::tool_run::CapacityScope::Cell {
                key: parent
                    .as_ref()
                    .and_then(crate::RuntimeInvocation::effect_replay_key)
                    .map_or_else(|| key.clone(), str::to_owned),
            }
        };
        let new_calls = requested
            .keys()
            .filter(|call_id| !run.contains_call(call_id))
            .cloned()
            .collect();
        run.admit_capacity(&capacity, new_calls, self.context.max_tool_calls())
            .map_err(crate::RuntimeEffectControllerError::max_tool_calls_exceeded)?;

        // The request's new members are admitted together, before any of
        // them prepares or starts. An isolated member is admitted only bound
        // to a registered process by its provider. One refused member
        // settles every member with its typed refusal: no body, hook or
        // process runs for any of them.
        let mut definitions = BTreeMap::new();
        let mut bound = Vec::new();
        let mut admitted = Vec::new();
        for leaf in &leaves {
            let ToolAggregateLeaf::Tool(invocation) = leaf else {
                continue;
            };
            if run.contains_call(&invocation.id) || definitions.contains_key(&invocation.id) {
                continue;
            }
            let definition = self.leaf_definition(invocation);
            let mut isolation_bound = false;
            if let Some(definition) = &definition
                && definition.manifest.declaration.isolated
            {
                let source = invocation
                    .execution_grant
                    .as_deref()
                    .and_then(|grant| grant.source_id.as_deref());
                let binding = self
                    .context
                    .dispatch()
                    .plugins
                    .tool_run_binding(&invocation.tool_id, source)
                    .map_err(crate::RuntimeEffectControllerError::from)?;
                if let Some(start) = self.bind_isolated(
                    owner,
                    &invocation.id,
                    &definition.manifest.id,
                    &invocation.args,
                    &binding.executable,
                ) {
                    bound.push((invocation.id.clone(), start));
                    isolation_bound = true;
                }
            }
            admitted.push((invocation.id.clone(), isolation_bound));
            definitions.insert(invocation.id.clone(), definition);
        }
        let round_refusal =
            super::super::admit_tool_round(admitted.iter().map(|(call_id, isolation_bound)| {
                definitions
                    .get(call_id)
                    .and_then(Option::as_ref)
                    .map(|definition| (&definition.manifest, *isolation_bound))
            }))
            .err();
        if round_refusal.is_none() {
            self.isolated.lock_recover().extend(bound);
        }

        let mut plan_leaves: Vec<Leaf> = Vec::new();
        let mut operand_leaves: Vec<usize> = Vec::new();
        let mut positions: Vec<Option<usize>> = Vec::new();
        let mut cursor_calls = BTreeMap::new();
        let mut refused = BTreeMap::new();
        let mut call_leaves = BTreeMap::<crate::ToolCallId, usize>::new();
        let mut calls = Vec::new();
        let mut environment = self.environment.clone();
        let settled_value = |plan_leaves: &mut Vec<Leaf>,
                             operand_leaves: &mut Vec<usize>,
                             positions: &mut Vec<Option<usize>>| {
            plan_leaves.push(Leaf::Settled { fulfilled: true });
            operand_leaves.push(plan_leaves.len() - 1);
            positions.push(None);
        };
        for (position, leaf) in leaves.into_iter().enumerate() {
            if settled_value_after == Some(position) {
                settled_value(&mut plan_leaves, &mut operand_leaves, &mut positions);
            }
            match leaf {
                ToolAggregateLeaf::Settled { fulfilled } => {
                    plan_leaves.push(Leaf::Settled { fulfilled });
                    operand_leaves.push(plan_leaves.len() - 1);
                }
                ToolAggregateLeaf::Timer { duration_ms } => {
                    plan_leaves.push(Leaf::Timer { duration_ms });
                    operand_leaves.push(plan_leaves.len() - 1);
                }
                ToolAggregateLeaf::Tool(invocation) => {
                    if let Some(leaf) = call_leaves.get(&invocation.id) {
                        // An alias of a call this aggregate already reads.
                        operand_leaves.push(*leaf);
                        cursor_calls.insert(position, invocation.id.clone());
                        positions.push(Some(position));
                        continue;
                    }
                    let definition = definitions.get(&invocation.id).cloned().flatten();
                    let pending = invocation.pending.as_deref().cloned().unwrap_or_else(|| {
                        crate::sansio::PendingToolCall {
                            call_id: invocation.id.clone(),
                            provider_call_id: None,
                            tool_name: invocation.tool_id.to_string(),
                            args: invocation.args.clone(),
                            replay: None,
                        }
                    });
                    let known = run.contains_call(&invocation.id);
                    if !known && (definition.is_none() || round_refusal.is_some()) {
                        let member = calls.len();
                        let failure = match (&definition, &round_refusal) {
                            (Some(_), Some(refusal)) => {
                                refusal.failure_for(member, &pending.tool_name)
                            }
                            _ => crate::ToolFailure::runtime(
                                crate::ToolFailureClass::InvalidRequest,
                                "tool_unavailable",
                                "Tool is unavailable in this session",
                            ),
                        };
                        plan_leaves.push(Leaf::Settled { fulfilled: false });
                        operand_leaves.push(plan_leaves.len() - 1);
                        refused.insert(operand_leaves.len() - 1, RefusedInput { pending, failure });
                        positions.push(Some(position));
                        continue;
                    }
                    if !known && let Some(definition) = definition {
                        calls.push(
                            run.alongside(self.admit_leaf(
                                owner,
                                &invocation,
                                definition,
                                parent.clone(),
                                &environment_spec,
                                &attribution,
                                &mut environment,
                            ))
                            .await?,
                        );
                    }
                    plan_leaves.push(Leaf::Call(invocation.id.clone()));
                    operand_leaves.push(plan_leaves.len() - 1);
                    call_leaves.insert(invocation.id.clone(), plan_leaves.len() - 1);
                    cursor_calls.insert(position, invocation.id.clone());
                }
            }
            positions.push(Some(position));
        }
        if settled_value_after == Some(positions.len()) {
            settled_value(&mut plan_leaves, &mut operand_leaves, &mut positions);
        }
        for input in refused.values() {
            if let ToolAggregateLeafReply::Tool(reply) = refused_reply(input)
                && let Some(completed) = &reply.completed
            {
                let context = self.context.with_tool_observation_attribution(&attribution);
                run.alongside(
                    context.report_undispatched_tool_call(completed, completed.call_id.as_str()),
                )
                .await;
            }
        }
        self.refused.lock_recover().insert(key.clone(), refused);
        for call in calls {
            run.start(
                call,
                Arc::clone(self) as Arc<dyn SingletonToolHandlers + 'owner>,
            )?;
        }
        run.form(key.clone(), plan_leaves, operand_leaves)?;
        Ok(ToolRunAggregateCursor {
            owner: owner.clone(),
            key,
            positions,
            calls: cursor_calls,
        })
    }

    /// What the machine or a consumer is answered with for `call_id`, ended
    /// as `end`.
    ///
    /// # Errors
    ///
    /// A capture or presentation that does not decode.
    pub(crate) fn completed_call(
        &self,
        call_id: &crate::ToolCallId,
        end: &CallEnd,
    ) -> Result<crate::sansio::CompletedToolCall, SingletonRunError> {
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or_else(|| encoding(format!("call {call_id} has no admitted preparation")))?;
        let (output, model_return, intent_outcomes) = match end {
            // An isolated final presents the descriptor of the process it
            // started; no ordinary body produced an output.
            CallEnd::Final {
                capture: SingletonCapture::Isolated { .. },
                presentation,
                ..
            } => {
                let output = ToolCallOutput::success(
                    decode::<serde_json::Value>(presentation).map_err(encoding)?,
                );
                let model_return =
                    crate::ModelToolReturn::from_output(prepared.call.tool_name.clone(), &output);
                (output, model_return, Vec::new())
            }
            CallEnd::Final {
                capture,
                presentation,
                ..
            } => {
                let presented: Presented = decode(presentation).map_err(encoding)?;
                let captured: Captured = decode(
                    capture
                        .output()
                        .ok_or_else(|| encoding("the final has no capture".to_owned()))?,
                )
                .map_err(encoding)?;
                let mut output = captured.output;
                super::super::attempt_coordinator::project_recorded_intent_outcomes(
                    &mut output,
                    &presented.intent_outcomes,
                );
                // The realized declarations are reported after the presented
                // value; a declared start's launch receipt is host-facing only.
                let mut model_return = presented.presentation.model_return;
                model_return.parts.extend(
                    super::super::pending_resolver::model_visible_outcomes(
                        &captured.intents,
                        &presented.intent_outcomes,
                    )
                    .iter()
                    .map(|outcome| crate::ModelToolReturnPart::text(outcome.model_addendum())),
                );
                (output, model_return, presented.intent_outcomes)
            }
            CallEnd::Withheld { decision, cause } => {
                let output = super::observations::terminal_output(decision, cause.as_ref(), None)
                    .map_err(encoding)?;
                let model_return =
                    crate::ModelToolReturn::from_output(prepared.call.tool_name.clone(), &output);
                (output, model_return, Vec::new())
            }
        };
        Ok(crate::sansio::CompletedToolCall {
            call_id: call_id.clone(),
            provider_call_id: prepared.call.provider_call_id,
            tool_name: prepared.call.tool_name,
            args: prepared.call.args,
            output,
            model_return,
            intent_outcomes,
            replay: prepared.call.replay,
        })
    }

    fn tool_reply(
        &self,
        call_id: &crate::ToolCallId,
        end: &CallEnd,
    ) -> Result<ToolAggregateLeafReply, SingletonRunError> {
        let completed = self.completed_call(call_id, end)?;
        let record = ToolCallRecord {
            call_id: completed.call_id.clone(),
            provider_call_id: completed.provider_call_id.clone(),
            tool: completed.tool_name.clone(),
            args: completed.args.clone(),
            output: completed.output.clone(),
        };
        let mut reply = ToolInvocationReply::from_output(record.output.clone()).with_record(record);
        reply.completed = Some(Box::new(completed));
        Ok(ToolAggregateLeafReply::Tool(Box::new(reply)))
    }

    /// Take `consumer`'s answer from the aggregate `cursor` names: at once,
    /// or once it has one when `wait`.
    pub(crate) async fn consume_aggregate(
        &self,
        run: &mut ToolRun<'_>,
        owner: &crate::EffectOpener,
        cursor: ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
        wait: bool,
        host_control: bool,
    ) -> Result<ToolRunAggregatePoll, SingletonRunError> {
        if &cursor.owner != owner {
            return Err(encoding(format!(
                "aggregate {} belongs to another Run",
                cursor.key
            )));
        }
        let ToolRunAggregateCursor {
            key,
            positions,
            calls,
            ..
        } = cursor;
        let mode = match consumer {
            ToolAggregateConsumer::All => Consumer::All,
            ToolAggregateConsumer::AllSettled => Consumer::AllSettled,
            ToolAggregateConsumer::Race => Consumer::Race,
            ToolAggregateConsumer::Any => Consumer::Any,
        };
        let answer = if wait {
            run.consume(&key, mode, host_control).await?
        } else {
            match run.answer(&key, mode, host_control)? {
                Some(answer) => answer,
                None => return Ok(ToolRunAggregatePoll::Pending),
            }
        };
        let refused = self
            .refused
            .lock_recover()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let reply = |operand: usize| -> Result<Option<ToolAggregateLeafReply>, SingletonRunError> {
            let Some(position) = positions[operand] else {
                return Ok(None);
            };
            if let Some(input) = refused.get(&operand) {
                return Ok(Some(refused_reply(input)));
            }
            let Some(call_id) = calls.get(&position) else {
                return Ok(match run.leaf(&key, operand)? {
                    Some(Leaf::Timer { .. }) => Some(ToolAggregateLeafReply::Timer),
                    _ => None,
                });
            };
            let end = run
                .end(call_id)?
                .ok_or_else(|| encoding(format!("call {call_id} has not ended")))?;
            self.tool_reply(call_id, end).map(Some)
        };
        let all = || -> Result<Vec<Option<ToolAggregateLeafReply>>, SingletonRunError> {
            (0..positions.len())
                .filter(|operand| positions[*operand].is_some())
                .map(reply)
                .collect()
        };
        let settlement_order = run
            .settlement_order(&key)?
            .into_iter()
            .filter_map(|operand| positions[operand])
            .collect();
        let outcome = match answer {
            Answer::Selected(operand) => match positions[operand] {
                Some(leaf) => ToolAggregateOutcome::Selected {
                    leaf,
                    reply: reply(operand)?,
                },
                None => ToolAggregateOutcome::SettledValue,
            },
            Answer::All => ToolAggregateOutcome::AllResults(all()?),
            Answer::Exhausted => ToolAggregateOutcome::ExhaustedRejections(all()?),
            Answer::HostControl { call_id, decision } => {
                let cause = match run.end(&call_id)? {
                    Some(CallEnd::Withheld { cause, .. }) => {
                        cause.as_ref().map(|cause| Box::new(cause.verdict.clone()))
                    }
                    _ => None,
                };
                return Err(crate::tool_dispatch::call_run::run_control(
                    call_id, &decision, cause,
                ));
            }
        };
        Ok(ToolRunAggregatePoll::Ready {
            outcome,
            settlement_order,
        })
    }
}
