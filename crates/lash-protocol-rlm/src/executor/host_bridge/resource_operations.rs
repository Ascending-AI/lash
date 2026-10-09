//! Every resource call of one cell: resolved alike when its operation is
//! admitted and when it is performed, and answered from the committed
//! outcome of the member its admission made of it (ADR 0132 §5, §8).

use lash_core::tool_dispatch::{CellCall, CellMember};
use lash_vm_broker::{Decide, MemberEnd, SettledMember};
use lash_vm_runtime::{AggregateAnswer, LeafStanding};

use super::*;

/// Where a resolved call stands in its cell, for its trace and its ledger.
struct LeafCall {
    source_operation: String,
    call_site: lash_vm::LashVmExecutionCallSite,
    logical_call_id: lash_core::ToolCallId,
}

/// A call performed in this pass: its place in the cell's call ledger.
struct DispatchedCall {
    call: LeafCall,
    execution_index: usize,
}

/// One resource call, resolved with no side effect: the same on admission
/// and on every perform of its operation.
enum ResolvedLeaf {
    /// A language-runtime read, performed in place on every perform.
    Runtime(String),
    /// A call admitted as its own member execution.
    Member(Box<ResolvedMember>),
}

/// A resolved call admitted as its own member execution.
struct ResolvedMember {
    call: LeafCall,
    member: CellMember,
    /// The refusal of a tool binding that drifted since the cell recorded
    /// it (FIG-3587).
    drift: Option<lash_core::RuntimeEffectControllerError>,
}

/// One leaf of an aggregate being performed.
enum Slot {
    /// Settled when the aggregate formed.
    Immediate(Option<Result<FlowValue, ExecutionHostError>>),
    /// A timer, due at its pinned deadline.
    Timer(Option<lash_vm_broker::DurableInstant>),
    /// An admitted member.
    Member(Box<MemberSlot>),
}

/// An aggregate's leaf that is an admitted member, as this pass performs
/// it.
struct MemberSlot {
    dispatched: DispatchedCall,
    member: CellMember,
}

/// The settled member of `ends` that answers `member`, if it settled.
fn settled<'a>(ends: &'a [MemberEnd], member: &CellMember) -> Option<&'a SettledMember> {
    ends.iter()
        .find(|end| &end.call == member.id())
        .and_then(|end| end.settled.as_ref())
}

impl HostBridge<'_> {
    /// Resolve one resource call of operation `ordinal` (at `operand_index`
    /// of an aggregate): what it is and the identity it is called by. Pure:
    /// admission and every perform resolve it alike.
    async fn resolve_leaf(
        &self,
        operation: lash_vm::ResourceOperation,
        ordinal: u64,
        operand_index: Option<usize>,
    ) -> Result<ResolvedLeaf, ExecutionHostError> {
        let lash_vm::ResourceOperation {
            operation,
            receiver,
            args,
            call_site,
        } = operation;
        if let Some(checked) =
            lash_vm_runtime::language_runtime_operation(&receiver, &operation, &args)
        {
            return checked.map(|operation| ResolvedLeaf::Runtime(operation.to_owned()));
        }
        let FlowValue::Resource(receiver) = &receiver else {
            return Err(ExecutionHostError::from(
                lash_vm_runtime::LashVmHostError::ModuleAuthorityRequired { operation },
            ));
        };
        let host_operation = lash_vm_runtime::resolve_lash_vm_module_operation(
            &self.host_environment,
            receiver,
            &operation,
        )?;
        let source_operation = format!("{}.{}", receiver.alias, operation);
        let payload = operation_payload(&args).await?;
        let call_site =
            Self::require_call_site(&operation, &host_operation, call_site.as_ref())?.clone();
        let logical_call_id = self.resource_tool_call_id(ordinal, operand_index)?;
        let call = LeafCall {
            source_operation,
            call_site,
            logical_call_id,
        };

        let mut invocation = self.tool_invocation(
            call.logical_call_id.clone(),
            &host_operation,
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
        Ok(ResolvedLeaf::Member(Box::new(ResolvedMember {
            member: CellMember::Tool(CellCall::of(&invocation)),
            call,
            drift,
        })))
    }

    /// The members operation `ordinal`, which `request` asks for, admits:
    /// every call it makes, a tool call as its tool declares it. A cell
    /// over `max_tool_calls` admits none of the operation's calls: the
    /// operation is refused whole, and the calls it counted stay counted.
    pub(super) async fn admit_members(
        &self,
        ordinal: u64,
        request: &lash_vm::AbilityOp,
    ) -> Result<Vec<lash_vm_broker::MemberDraft>, String> {
        let operations = match request {
            AbilityOp::ResourceOperation(operation) => vec![(None, (**operation).clone())],
            AbilityOp::ResourceOperationBatch(batch) => batch
                .leaves
                .iter()
                .enumerate()
                .filter_map(|(index, leaf)| match leaf {
                    lash_vm::ResourceOperationBatchLeaf::Operation(operation) => {
                        Some((Some(index), operation.clone()))
                    }
                    lash_vm::ResourceOperationBatchLeaf::Timer(_) => None,
                })
                .collect(),
            _ => return Ok(Vec::new()),
        };
        let mut members = Vec::new();
        for (operand_index, operation) in operations {
            // A call that does not resolve is answered its refusal on
            // perform; a single call to a drifted binding stops the cell
            // there. Neither runs.
            if let Ok(ResolvedLeaf::Member(resolved)) =
                self.resolve_leaf(operation, ordinal, operand_index).await
                && (resolved.drift.is_none() || operand_index.is_some())
            {
                members.push(resolved.member);
            }
        }
        let requested = members
            .iter()
            .filter(|member| matches!(member, CellMember::Tool(_)))
            .count();
        {
            let mut counted = self.tool_calls.lock_recover();
            if requested > 0 && counted.saturating_add(requested) > self.ctx.max_tool_calls().get()
            {
                return Ok(Vec::new());
            }
            *counted += requested;
        }
        let now_ms = self
            .ctx
            .actor_context()
            .durable_now()
            .await
            .map_err(|error| error.to_string())?;
        let now_ms = u64::try_from(now_ms.0).unwrap_or(0);
        let opener = self
            .cell()
            .map_err(|error| error.to_string())?
            .identities()
            .code()
            .opener()
            .clone();
        members
            .into_iter()
            .map(|member| {
                let pin = self.members.pin(&member, now_ms);
                let request = lash_vm_protocol::EncodedPayload(member.encode()?);
                let draft =
                    lash_vm_broker::MemberDraft::pinned(member.id().clone(), request, &opener, pin)
                        .map_err(|fault| fault.0)?;
                // The cell's admission retains the call's trace scope, which
                // every attempt and every owner traces it under (FIG-5395).
                let trace = self.members.propose_trace(&member, now_ms);
                self.members.register(member);
                Ok(lash_vm_broker::MemberDraft {
                    draft: draft.draft.with_trace(trace),
                    ..draft
                })
            })
            .collect()
    }

    /// The `max_tool_calls` refusal of an operation making `requested` tool
    /// calls, recorded on the execution so the cell's failure names it.
    fn tool_call_limit(&self, requested: usize) -> lash_core::ToolCallLimitExceeded {
        let exceeded = lash_core::ToolCallLimitExceeded {
            scope: lash_core::ToolCallLimitScope::Cell,
            limit: self.ctx.max_tool_calls(),
            counted: *self.tool_calls.lock_recover(),
            requested,
        };
        self.ctx.record_tool_call_limit_refusal(exceeded);
        exceeded
    }

    /// A resolved call performed in this pass: its place in the cell's call
    /// ledger, and its node on the trace.
    fn dispatch(&self, call: LeafCall) -> DispatchedCall {
        if let Some(trace) = &self.lash_vm_execution_trace {
            trace.record_resource_call(&call.call_site, &call.logical_call_id);
            self.ctx.record_language_call_attribution(
                call.logical_call_id.clone(),
                trace.language,
                trace.identity().clone(),
                call.call_site.site.node_id.clone(),
                call.call_site.occurrence,
            );
        }
        DispatchedCall {
            call,
            execution_index: self.next_index(),
        }
    }

    /// Drive operation `ordinal`'s members until `decide` answers it from
    /// their committed outcomes. `None` once nothing runs and only rows are
    /// left to wait on: the cell suspends on its committed quiet point.
    pub(super) async fn drive_members<T>(
        &self,
        commands: &lash_vm_runtime::ReplayCommands<'_, '_>,
        ordinal: u64,
        decide: &mut (dyn FnMut(&[MemberEnd], lash_vm_broker::DurableInstant) -> Decide<T> + Send),
    ) -> Result<Option<T>, ExecutionHostError> {
        match self
            .snapshots
            .drive(ordinal, &self.ctx.cancellation(), decide)
            .await
        {
            Ok(lash_vm_broker::Driven::Answered(answer)) => Ok(Some(answer)),
            Ok(lash_vm_broker::Driven::Suspended) => Ok(None),
            Err(refusal) => Err(commands.abort(lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed,
                format!("the cell's calls could not be driven: {}", refusal.0),
            ))),
        }
    }

    /// The value the cell takes from `member`'s committed answer, kept in
    /// the cell's call ledger.
    fn member_value(
        &self,
        dispatched: &DispatchedCall,
        member: &CellMember,
        settled: &SettledMember,
        replay_key: &str,
    ) -> Result<FlowValue, ExecutionHostError> {
        let reply = self.members.reply(member, &settled.output);
        match member {
            CellMember::Tool(_) => self.consume_resource_reply(dispatched, reply, replay_key),
        }
    }

    /// Whether `member`'s committed answer fulfilled it, read without
    /// taking it.
    fn fulfilled(&self, member: &CellMember, settled: &SettledMember) -> bool {
        let reply = self.members.reply(member, &settled.output);
        match member {
            CellMember::Tool(_) => reply.output.is_success(),
        }
    }

    fn consume_resource_reply(
        &self,
        dispatched: &DispatchedCall,
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
            dispatched.execution_index,
            dispatched.call.source_operation.clone(),
            outcome,
            host_record,
        )
        .and(result)
    }

    pub(super) async fn resource_operation(
        &self,
        operation: String,
        receiver: FlowValue,
        args: Vec<FlowValue>,
        call_site: Option<lash_vm::LashVmExecutionCallSite>,
    ) -> Result<AbilityOutcome, ExecutionHostError> {
        let commands = self.commands()?;
        let performing = self.performing()?;
        let command = commands.issue(performing.ordinal)?;
        let resolved = self
            .resolve_leaf(
                lash_vm::ResourceOperation {
                    operation,
                    receiver,
                    args,
                    call_site,
                },
                command.ordinal,
                None,
            )
            .await?;
        let (call, member, drift) = match resolved {
            ResolvedLeaf::Runtime(operation) => {
                let in_flight = commands.enter(command, CommandShape::Value).await?;
                let result = lash_vm_runtime::journaled_language_runtime_value(
                    &in_flight.ctx,
                    in_flight.command.key.to_string(),
                    &operation,
                )
                .await;
                commands.finish(&in_flight)?;
                return match result {
                    Ok(value) => value.map(AbilityOutcome::Value),
                    Err(error) => Err(commands.journal_error(&in_flight, error, |error| {
                        ExecutionHostError::new(error.to_string())
                    })),
                };
            }
            ResolvedLeaf::Member(resolved) => {
                let ResolvedMember {
                    call,
                    member,
                    drift,
                } = *resolved;
                (call, member, drift)
            }
        };
        let shape = match member {
            CellMember::Tool(_) => CommandShape::ToolCall,
        };
        let in_flight = commands.enter_bound(command, shape, drift).await?;
        let key = in_flight.command.key.to_string();
        if performing.members.is_empty() {
            // Refused by `max_tool_calls` before anything was admitted.
            let exceeded = self.tool_call_limit(1);
            commands.finish(&in_flight)?;
            let reply = ToolInvocationReply::from_output(lash_core::ToolCallOutput::failure(
                lash_vm_runtime::tool_call_limit_failure(exceeded),
            ));
            return self.consume_reply(reply, &key).0.map(AbilityOutcome::Value);
        }
        let dispatched = self.dispatch(call);
        let answered = self
            .drive_members(&commands, performing.ordinal, &mut |ends, _| {
                settled(ends, &member).map_or(Decide::Wait { until: None }, |settled| {
                    Decide::Answer(settled.clone())
                })
            })
            .await?;
        let Some(settled) = answered else {
            commands.hand_over(&in_flight)?;
            return Ok(AbilityOutcome::HandedOver);
        };
        commands.finish(&in_flight)?;
        self.member_value(&dispatched, &member, &settled, &key)
            .map(AbilityOutcome::Value)
    }

    pub(super) async fn resource_operation_batch(
        &self,
        batch: lash_vm::ResourceOperationBatch,
    ) -> Result<AbilityOutcome, ExecutionHostError> {
        let lash_vm::ResourceOperationBatch {
            leaves,
            consumer,
            settled_value_after,
        } = batch;
        let commands = self.commands()?;
        let performing = self.performing()?;
        let command = commands.issue(performing.ordinal)?;
        let in_flight = commands.enter(command, CommandShape::Aggregate).await?;
        let leaf_key = |index| {
            format!(
                "{}:{}",
                in_flight.command.key,
                lash_core::CommandReplayKey::child_suffix(index),
            )
        };
        let mut timers = performing.waits.iter();
        let mut slots = Vec::with_capacity(leaves.len());
        let mut tool_calls = 0;
        for (index, leaf) in leaves.into_iter().enumerate() {
            let operation = match leaf {
                lash_vm::ResourceOperationBatchLeaf::Operation(operation) => operation,
                lash_vm::ResourceOperationBatchLeaf::Timer(sleep) => {
                    slots.push(match lash_vm_runtime::timer_duration_ms(&sleep) {
                        Ok(_) => {
                            let timer = timers.next().ok_or_else(|| {
                                ExecutionHostError::new(
                                    "a cell's aggregate was admitted without its timer",
                                )
                            })?;
                            let deadline =
                                lash_core::waits::deadline(self.ctx.actor_context(), timer)
                                    .await
                                    .map_err(|error| {
                                        commands.abort(
                                            lash_core::RuntimeEffectControllerError::new(
                                                lash_core::RuntimeErrorCode::EngineAwaitEventAwait,
                                                error.to_string(),
                                            ),
                                        )
                                    })?;
                            Slot::Timer(deadline)
                        }
                        Err(error) => Slot::Immediate(Some(Err(error))),
                    });
                    continue;
                }
            };
            let slot = match self
                .resolve_leaf(operation, in_flight.command.ordinal, Some(index))
                .await
            {
                Ok(ResolvedLeaf::Runtime(operation)) => {
                    let result = match lash_vm_runtime::journaled_language_runtime_value(
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
                    Slot::Immediate(Some(result))
                }
                Ok(ResolvedLeaf::Member(resolved)) => {
                    let ResolvedMember { call, member, .. } = *resolved;
                    if matches!(member, CellMember::Tool(_)) {
                        tool_calls += 1;
                    }
                    Slot::Member(Box::new(MemberSlot {
                        dispatched: self.dispatch(call),
                        member,
                    }))
                }
                Err(error) => Slot::Immediate(Some(Err(error))),
            };
            slots.push(slot);
        }
        if tool_calls > 0 && performing.members.is_empty() {
            // Refused whole by `max_tool_calls` before any call of it was
            // admitted: none of them runs.
            let exceeded = self.tool_call_limit(tool_calls);
            commands.finish(&in_flight)?;
            return Err(ExecutionHostError::from_tool_failure(
                &lash_vm_runtime::tool_call_limit_failure(exceeded),
                in_flight.command.key.to_string(),
            ));
        }
        let trace_calls = slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Member(slot) => Some(slot.dispatched.call.call_site.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Some(trace) = &self.lash_vm_execution_trace
            && trace_calls.len() > 1
        {
            for (position, call_site) in trace_calls.iter().enumerate() {
                trace.emit_waiting(
                    call_site,
                    lash_vm_runtime::TraceNodeAwaited::ToolBatch {
                        batch_id: in_flight.command.key.to_string(),
                        position,
                    },
                );
            }
        }
        let decided = self
            .drive_members(&commands, performing.ordinal, &mut |ends, now| {
                let standings = slots
                    .iter()
                    .enumerate()
                    .map(|(leaf, slot)| match slot {
                        Slot::Immediate(result) => LeafStanding::Immediate {
                            fulfilled: result.as_ref().is_some_and(Result::is_ok),
                        },
                        Slot::Timer(deadline) => match deadline {
                            // A timer that came due settles after every
                            // member that settled.
                            Some(due) if *due <= now => LeafStanding::Settled {
                                fulfilled: true,
                                order: (1, leaf as u64),
                            },
                            _ => LeafStanding::Open,
                        },
                        // A call ended cancelled (a check's cancel) is its
                        // operand's settlement; a turn's cancel answers the
                        // whole cell before any aggregate does.
                        Slot::Member(slot) => match settled(ends, &slot.member) {
                            None => LeafStanding::Open,
                            Some(settled) => LeafStanding::Settled {
                                fulfilled: self.fulfilled(&slot.member, settled),
                                order: (0, settled.order),
                            },
                        },
                    })
                    .collect::<Vec<_>>();
                match lash_vm_runtime::aggregate_answer(consumer, settled_value_after, &standings) {
                    Some(answer) => Decide::Answer((answer, ends.to_vec())),
                    None => Decide::Wait {
                        until: slots
                            .iter()
                            .filter_map(|slot| match slot {
                                Slot::Timer(Some(due)) if *due > now => Some(*due),
                                _ => None,
                            })
                            .min(),
                    },
                }
            })
            .await?;
        let Some((answer, ends)) = decided else {
            commands.hand_over(&in_flight)?;
            return Ok(AbilityOutcome::HandedOver);
        };
        commands.finish(&in_flight)?;
        let leaves = slots.len();
        let mut result_of = |leaf: usize| -> Result<FlowValue, ExecutionHostError> {
            match slots.get_mut(leaf) {
                Some(Slot::Immediate(result)) => result.take().unwrap_or_else(|| {
                    Err(ExecutionHostError::new(format!(
                        "aggregate leaf {leaf} was answered twice"
                    )))
                }),
                Some(Slot::Timer(_)) => Ok(FlowValue::Undefined),
                Some(Slot::Member(slot)) => match settled(&ends, &slot.member) {
                    Some(settled) => {
                        self.member_value(&slot.dispatched, &slot.member, settled, &leaf_key(leaf))
                    }
                    None => Err(ExecutionHostError::new(format!(
                        "aggregate leaf {leaf} was answered without a settlement"
                    ))),
                },
                None => Err(ExecutionHostError::new(format!(
                    "aggregate leaf {leaf} is not a leaf of its aggregate"
                ))),
            }
        };
        let outcome = match answer {
            AggregateAnswer::Leaf(leaf) => lash_vm::ResourceOperationBatchOutcome::Selected {
                leaf,
                result: lash_vm::ResourceOperationOutcome::from_result(result_of(leaf)),
            },
            AggregateAnswer::SettledValue => lash_vm::ResourceOperationBatchOutcome::SettledValue,
            AggregateAnswer::All => lash_vm::ResourceOperationBatchOutcome::AllResults(
                (0..leaves)
                    .map(|leaf| lash_vm::ResourceOperationOutcome::from_result(result_of(leaf)))
                    .collect(),
            ),
            AggregateAnswer::Exhausted => {
                let mut errors = Vec::with_capacity(leaves);
                for leaf in 0..leaves {
                    match result_of(leaf) {
                        Err(error) => errors.push(error),
                        Ok(_) => {
                            return Err(ExecutionHostError::new(format!(
                                "aggregate leaf {leaf} fulfilled, yet the aggregate reported \
                                 every leaf rejected"
                            )));
                        }
                    }
                }
                lash_vm::ResourceOperationBatchOutcome::ExhaustedRejections(errors)
            }
            AggregateAnswer::HostControl(leaf) => {
                return Err(result_of(leaf).err().unwrap_or_else(|| {
                    ExecutionHostError::new(format!("aggregate leaf {leaf} was cancelled"))
                }));
            }
        };
        if !self.is_cancelled()
            && trace_calls.len() > 1
            && let Some(trace) = &self.lash_vm_execution_trace
        {
            for call_site in &trace_calls {
                trace.emit_resumed(call_site, lash_vm_runtime::TraceNodeWaitResolution::Resumed);
            }
        }
        Ok(AbilityOutcome::ResourceOperationBatch(outcome))
    }
}
