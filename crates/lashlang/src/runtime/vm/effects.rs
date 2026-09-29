use std::sync::Arc;

use crate::LashlangExecutionCallSite;
use crate::span::Span;

use super::super::access::prototype_chain_data_key_error;
use super::super::host::{
    AbilityOp, AbilityOutcome, AggregateConsumer, ResourceOperation, ResourceOperationBatch,
    ResourceOperationBatchLeaf, ResourceOperationBatchOutcome, ResourceOperationOutcome, Sleep,
    SleepKind,
};
use super::super::ops::value_type_name;
use super::super::{
    CompiledAggregateAwaitShape, CompiledResourceOperationBatch,
    CompiledResourceOperationBatchLeaf, ErrorKind, ExecutionHost, ExecutionHostError, RuntimeError,
    Value, execution_host_error_value, is_process_handle, parse_handle_record,
    record_with_capacity, success, unwrap_tool_result,
};
use super::control::VmOutcome;
use super::pending_tools::{
    AwaitedValue, ensure_no_tool_handle_arguments, is_runtime_process_handle_id,
    plain_value_awaited,
};
use super::{ActiveLashlangExecutionNode, Vm};

#[derive(Clone, Copy)]
pub(super) enum VmEffect {
    ResourceCall { operation: usize, argc: usize },
    ResourceCallUnwrap { operation: usize, argc: usize },
    AwaitArray { consumer: AggregateConsumer },
    AwaitPending,
    ResourceOperationBatch(usize),
    AwaitHandle,
    Sleep(SleepKind),
    WaitSignal { name: usize },
    AwaitHandleUnwrap,
    Print,
    Finish,
    Fail,
}

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) async fn resolve_effect(
        &mut self,
        effect: VmEffect,
        instruction_ip: usize,
    ) -> Result<Option<VmOutcome>, RuntimeError> {
        let active = self.begin_lashlang_execution(instruction_ip);
        let result =
            Box::pin(self.resolve_effect_inner(effect, active.as_ref(), instruction_ip)).await;
        match (&result, active.as_ref()) {
            // A handed-over wait did not complete: its node is left for the
            // continuation that issues the wait again.
            (Ok(Some(VmOutcome::HandedOver)), _) => {}
            (Ok(Some(VmOutcome::ProcessFailed(value))), Some(active)) => {
                self.emit_lashlang_execution_failure(
                    active,
                    crate::LashlangExecutionFailure::Runtime {
                        code: RuntimeError::PROCESS_FAILED_CODE.to_owned(),
                        message: value.to_string(),
                    },
                );
            }
            (Ok(_), Some(active)) => {
                self.complete_lashlang_execution(active);
            }
            (Err(error), Some(active)) => {
                self.fail_lashlang_execution(active, error);
            }
            _ => {}
        }
        result
    }

    async fn resolve_effect_inner(
        &mut self,
        effect: VmEffect,
        active: Option<&ActiveLashlangExecutionNode>,
        instruction_ip: usize,
    ) -> Result<Option<VmOutcome>, RuntimeError> {
        match effect {
            VmEffect::ResourceCall { operation, argc } => {
                let (receiver, args) = self.drain_receiver_call(argc)?;
                ensure_no_tool_handle_arguments(&args)?;
                let operation_name = self.chunk.names[operation].text.to_string();
                let result = match self
                    .host
                    .perform(AbilityOp::ResourceOperation(Box::new(ResourceOperation {
                        receiver,
                        operation: operation_name.clone(),
                        args,
                        call_site: active.map(lashlang_execution_call_site),
                    })))
                    .await
                {
                    Ok(AbilityOutcome::Value(value)) => host_success(value, &operation_name),
                    Ok(AbilityOutcome::ResourceOperationBatch(_)) => execution_host_error_value(
                        ExecutionHostError::new(
                            "module operation returned a resource operation batch result",
                        ),
                        &operation_name,
                    ),
                    Ok(AbilityOutcome::Unit) => execution_host_error_value(
                        ExecutionHostError::new("module operation returned no value"),
                        &operation_name,
                    ),
                    Ok(AbilityOutcome::HandedOver) => execution_host_error_value(
                        ExecutionHostError::new("module operation returned a hand-over"),
                        &operation_name,
                    ),
                    Err(error) => execution_host_error_value(error, &operation_name),
                };
                self.stack.push(result);
            }
            VmEffect::ResourceCallUnwrap { operation, argc } => {
                let (receiver, args) = self.drain_receiver_call(argc)?;
                ensure_no_tool_handle_arguments(&args)?;
                let value = self
                    .host
                    .perform(AbilityOp::ResourceOperation(Box::new(ResourceOperation {
                        receiver,
                        operation: self.chunk.names[operation].text.to_string(),
                        args,
                        call_site: active.map(lashlang_execution_call_site),
                    })))
                    .await
                    .and_then(|result| result.into_value("module operation"))
                    .map_err(|source| RuntimeError::UnwrappedModuleOperationFailed { source })?;
                self.stack.push(value);
            }
            VmEffect::AwaitArray { consumer } => {
                self.await_pending_array(consumer, instruction_ip).await?;
            }
            VmEffect::AwaitPending => {
                let value = self.pop_stack()?;
                match self.classify_awaited(&value) {
                    // A process handle that reached the tool-await path (a
                    // runtime value the lowerer could not type) awaits the
                    // process the way the typed form does. Only an aggregate
                    // element position refuses it, because only there did the
                    // retired second phase settle it out of the batch order.
                    AwaitedValue::Leaf(id) if is_runtime_process_handle_id(&id) => {
                        let value = self.await_value_unwrap(value, active).await?;
                        self.stack.push(value);
                    }
                    AwaitedValue::Leaf(id) => {
                        if !self.live_pending_request(&id) {
                            return Err(self.unsettleable_handle(&id));
                        }
                        self.stack.push(Value::List(vec![value].into()));
                        self.await_pending_array(AggregateConsumer::All, instruction_ip)
                            .await?;
                        let Value::List(values) = self.pop_stack()? else {
                            unreachable!()
                        };
                        self.stack.push(values[0].clone());
                    }
                    AwaitedValue::Plain => {
                        return Err(RuntimeError::PendingTool {
                            problem: plain_value_awaited(&value),
                        });
                    }
                }
            }
            VmEffect::ResourceOperationBatch(batch) => {
                self.resolve_resource_operation_batch(batch, instruction_ip)
                    .await?;
            }
            VmEffect::AwaitHandle => {
                let handle = self.pop_stack()?;
                let result = self.await_value(handle, active).await?;
                self.stack.push(result);
            }
            VmEffect::Sleep(kind) => {
                let value = self.pop_stack()?;
                self.host
                    .perform(AbilityOp::Sleep(Sleep {
                        kind,
                        value,
                        call_site: active.map(lashlang_execution_call_site),
                    }))
                    .await
                    .and_then(|result| result.into_value("sleep"))
                    .map_err(|source| RuntimeError::SleepFailed { source })?;
                self.last_value = Some(Value::Null);
                self.stack.push(Value::Null);
            }
            VmEffect::WaitSignal { name } => {
                let result = self
                    .host
                    .perform(AbilityOp::WaitSignal {
                        name: self.chunk.names[name].text.to_string(),
                        call_site: active.map(lashlang_execution_call_site),
                    })
                    .await;
                if matches!(result, Ok(AbilityOutcome::HandedOver)) {
                    // The wait moved to a successor segment without
                    // completing. The instruction takes no operand, so
                    // standing on it again is the whole rewind: a
                    // continuation captured now names the wait and issues it
                    // again.
                    self.ip = instruction_ip;
                    self.resume_point = super::VmResumePoint::ReissueOperation {
                        operation: super::VmSuspendedOperation::WaitSignal {
                            name: self.chunk.names[name].text.to_string(),
                        },
                    };
                    return Ok(Some(VmOutcome::HandedOver));
                }
                let value = result
                    .and_then(|result| result.into_value("wait_signal"))
                    .map_err(|source| RuntimeError::WaitSignalFailed { source })?;
                self.stack.push(value);
            }
            VmEffect::AwaitHandleUnwrap => {
                let handle = self.pop_stack()?;
                let result = self.await_value_unwrap(handle, active).await?;
                self.stack.push(result);
            }
            VmEffect::Print => {
                let value = self.pop_stack()?;
                self.host
                    .perform(AbilityOp::Print(value))
                    .await
                    .map_err(|source| RuntimeError::PrintFailed { source })?;
                self.last_value = Some(Value::Null);
                self.stack.push(Value::Null);
            }
            VmEffect::Finish => {
                self.ensure_no_pending_tools()?;
                let value = self.pop_stack()?;
                let value = self
                    .host
                    .perform(AbilityOp::Finish(value))
                    .await
                    .and_then(|result| result.into_value("finish"))
                    .map_err(|source| RuntimeError::FinishFailed { source })?;
                return Ok(Some(match self.mode {
                    super::control::VmMode::Foreground => VmOutcome::Finished(value),
                    super::control::VmMode::Process => VmOutcome::ProcessFinished(value),
                }));
            }
            VmEffect::Fail => {
                let value = self.pop_stack()?;
                let value = self
                    .host
                    .perform(AbilityOp::Fail(value))
                    .await
                    .and_then(|result| result.into_value("fail"))
                    .map_err(|source| RuntimeError::FailFailed { source })?;
                return Ok(Some(VmOutcome::ProcessFailed(value)));
            }
        }
        Ok(None)
    }

    async fn resolve_resource_operation_batch(
        &mut self,
        batch: usize,
        instruction_ip: usize,
    ) -> Result<(), RuntimeError> {
        let batch = &self.chunk.resource_operation_batches[batch];
        let start = self.stack_drain_start(batch.stack_value_count)?;
        let values = self.stack.drain(start..).collect::<Vec<_>>();
        let mut leaves = Vec::new();
        let mut expanded_values = Vec::new();
        let shape = expand_aggregate_await_shape(
            &batch.shape,
            batch,
            &values,
            &mut leaves,
            &mut expanded_values,
        )?;
        let expanded = CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape,
            stack_value_count: expanded_values.len(),
            aggregate_unwrap: batch.aggregate_unwrap,
            consumer: batch.consumer,
        };
        self.resolve_batch_spec(&expanded, expanded_values, instruction_ip)
            .await
    }

    /// Settles one aggregate await as a single host batch under its consumer
    /// mode (ADR 0099 §10).
    ///
    /// Every leaf the aggregate has to settle is a pending operation, so there
    /// is one durable settlement order for the whole aggregate and no second
    /// phase to sequence against it. Non-leaf positions are plain values and
    /// are carried through untouched (ADR 0096: settlement is shallow, over
    /// element positions only).
    pub(super) async fn resolve_batch_spec(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: Vec<Value>,
        instruction_ip: usize,
    ) -> Result<(), RuntimeError> {
        // Element positions are the only ones that could have settled, so they
        // are the only ones where a handle is a mistake rather than data. A
        // handle here named the retired second phase; the repair names the tool
        // that parks on the durable wait instead.
        for index in element_value_positions(&batch.shape) {
            let Some(value) = values.get(index) else {
                return Err(RuntimeError::AggregateAwaitValueOutOfRange);
            };
            if let AwaitedValue::Leaf(id) = self.classify_awaited(value) {
                return Err(self.unsettleable_handle(&id));
            }
        }

        let mut value = match batch.consumer {
            AggregateConsumer::Race | AggregateConsumer::Any => {
                self.settle_selecting_aggregate(batch, &values, instruction_ip)
                    .await?
            }
            AggregateConsumer::All | AggregateConsumer::AllSettled => {
                let leaf_values = if batch.leaves.is_empty() {
                    Vec::new()
                } else {
                    self.settle_tool_leaves(batch, &values).await?
                };
                build_aggregate_await_shape(&batch.shape, &values, &leaf_values, self)?
            }
        };
        if batch.aggregate_unwrap {
            value = unwrap_tool_result(value)?;
        }
        self.stack.push(value);
        Ok(())
    }

    /// One host operation per unique leaf, in leaf order, with each leaf's
    /// execution node begun.
    fn batch_leaf_operations(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: &[Value],
    ) -> Result<
        (
            Vec<ResourceOperationBatchLeaf>,
            Vec<Option<ActiveLashlangExecutionNode>>,
        ),
        RuntimeError,
    > {
        let mut operations = Vec::with_capacity(batch.leaves.len());
        let mut active_nodes = Vec::with_capacity(batch.leaves.len());
        for leaf in batch.leaves.iter() {
            let active = leaf
                .site
                .clone()
                .map(|site| self.begin_lashlang_execution_site(site));
            let receiver = values
                .get(leaf.receiver_stack_index)
                .cloned()
                .ok_or(RuntimeError::ResourceBatchReceiverOutOfRange)?;
            let call_site = active.as_ref().map(lashlang_execution_call_site);
            if leaf.timer {
                operations.push(ResourceOperationBatchLeaf::Timer(Sleep {
                    kind: SleepKind::For,
                    value: receiver,
                    call_site,
                }));
            } else {
                let args_start = leaf.receiver_stack_index + 1;
                let args_end = args_start + leaf.argc;
                let args = values
                    .get(args_start..args_end)
                    .ok_or(RuntimeError::ResourceBatchArgumentOutOfRange)?
                    .to_vec();
                operations.push(ResourceOperationBatchLeaf::Operation(ResourceOperation {
                    receiver,
                    operation: self.chunk.names[leaf.operation].text.to_string(),
                    args,
                    call_site,
                }));
            }
            active_nodes.push(active);
        }
        Ok((operations, active_nodes))
    }

    /// Returns each leaf's value in leaf order for an all-results aggregate,
    /// or raises the rejection it reports: the first *consumed* rejection for
    /// `Promise.all`, which the host answers as soon as it is consumed rather
    /// than after every leaf settles, and the first *written* unwrapped
    /// rejection for a Lashlang-native aggregate (ADR 0099 §10 L2, L7).
    async fn settle_tool_leaves(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: &[Value],
    ) -> Result<Vec<Value>, RuntimeError> {
        let (operations, active_nodes) = self.batch_leaf_operations(batch, values)?;
        let reply = self
            .perform_resource_operation_batch(operations, &active_nodes, batch.consumer, None)
            .await?;
        match reply {
            ResourceOperationBatchOutcome::AllResults(results) => self
                .settle_resource_operation_leaves(
                    batch
                        .leaves
                        .iter()
                        .map(|leaf| (leaf.unwrap, leaf.source_span)),
                    results,
                    &active_nodes,
                ),
            ResourceOperationBatchOutcome::Selected {
                leaf,
                result: ResourceOperationOutcome::Error(source),
            } => {
                let error = RuntimeError::UnwrappedModuleOperationFailed { source };
                if let Some(Some(active)) = active_nodes.get(leaf) {
                    self.fail_lashlang_execution(active, &error);
                }
                self.pending_error_span = batch.leaves.get(leaf).and_then(|leaf| leaf.source_span);
                Err(error)
            }
            other => Err(self.fail_resource_operation_batch(
                &active_nodes,
                RuntimeError::ResourceBatchReply {
                    problem: format!(
                        "a {:?} aggregate cannot be answered with {}",
                        batch.consumer,
                        reply_shape_name(&other)
                    ),
                },
            )),
        }
    }

    /// Settles a `Promise.race` or `Promise.any` (ADR 0099 §10, §11).
    ///
    /// The aggregate resolves with one settlement; the losers keep running
    /// under the opener, exactly as losing promises do, and no loser's value
    /// is ever synthesized (L6). The immediate prefix — operands that are
    /// already plain values and leaves the host settled during preparation —
    /// answers in source order ahead of any dispatched settlement (L5), and
    /// every pending leaf is admitted before it may answer (§11 clause 3).
    async fn settle_selecting_aggregate(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: &[Value],
        instruction_ip: usize,
    ) -> Result<Value, RuntimeError> {
        let CompiledAggregateAwaitShape::List(elements) = &batch.shape else {
            return Err(RuntimeError::InvalidAggregateAwaitRecordShape);
        };
        let first_value = elements.iter().find_map(|element| match element {
            CompiledAggregateAwaitShape::Value(index) => Some(*index),
            _ => None,
        });
        let aggregate = match batch.consumer {
            AggregateConsumer::Race => "Promise.race",
            _ => "Promise.any",
        };
        if batch.leaves.is_empty() {
            // Nothing is pending, so nothing is admitted and no group opens
            // (ADR 0065 refuses an empty group). A plain operand decides; with
            // no operand at all, `race` can never settle (§11 clause 5) and
            // `any` rejects with an empty `AggregateError` (clause 6).
            return match first_value {
                Some(index) => values
                    .get(index)
                    .cloned()
                    .ok_or(RuntimeError::AggregateAwaitValueOutOfRange),
                None if batch.consumer == AggregateConsumer::Race => {
                    Err(RuntimeError::AggregateAwaitUnsettled {
                        aggregate: aggregate.to_string(),
                    })
                }
                None => Err(self.aggregate_error(elements, &[], instruction_ip)?),
            };
        }
        // How many leaves first appear before the first plain operand: leaves
        // are numbered in first-appearance order, so that count is the highest
        // leaf index seen before it, plus one.
        let mut leaves_before = 0usize;
        let mut settled_value_after = None;
        for element in elements.iter() {
            match element {
                CompiledAggregateAwaitShape::BatchLeaf(index) => {
                    leaves_before = leaves_before.max(index + 1);
                }
                CompiledAggregateAwaitShape::Value(_) => {
                    settled_value_after = Some(leaves_before);
                    break;
                }
                _ => return Err(RuntimeError::InvalidAggregateAwaitRecordShape),
            }
        }
        let (operations, active_nodes) = self.batch_leaf_operations(batch, values)?;
        let reply = self
            .perform_resource_operation_batch(
                operations,
                &active_nodes,
                batch.consumer,
                settled_value_after,
            )
            .await?;
        match reply {
            ResourceOperationBatchOutcome::Selected { leaf, result } => match result {
                ResourceOperationOutcome::Value(value) => {
                    if let Some(Some(active)) = active_nodes.get(leaf) {
                        self.complete_lashlang_execution(active);
                    }
                    Ok(value)
                }
                ResourceOperationOutcome::Error(source) => {
                    let error = RuntimeError::UnwrappedModuleOperationFailed { source };
                    if let Some(Some(active)) = active_nodes.get(leaf) {
                        self.fail_lashlang_execution(active, &error);
                    }
                    self.pending_error_span =
                        batch.leaves.get(leaf).and_then(|leaf| leaf.source_span);
                    Err(error)
                }
            },
            ResourceOperationBatchOutcome::SettledValue => first_value
                .and_then(|index| values.get(index).cloned())
                .ok_or(RuntimeError::AggregateAwaitValueOutOfRange),
            ResourceOperationBatchOutcome::ExhaustedRejections(errors) => {
                for (active, error) in active_nodes.iter().zip(&errors) {
                    if let Some(active) = active {
                        self.fail_lashlang_execution(
                            active,
                            &RuntimeError::UnwrappedModuleOperationFailed {
                                source: error.clone(),
                            },
                        );
                    }
                }
                Err(self.aggregate_error(elements, &errors, instruction_ip)?)
            }
            ResourceOperationBatchOutcome::AllResults(_) => Err(self
                .fail_resource_operation_batch(
                    &active_nodes,
                    RuntimeError::ResourceBatchReply {
                        problem: format!("a {aggregate} cannot be answered with every result"),
                    },
                )),
        }
    }

    /// The `AggregateError` a `Promise.any` rejects with when no operand
    /// fulfils. Its `errors` are in **input-position** order, one per position
    /// — a leaf written twice contributes its rejection twice — never in
    /// settlement order (ADR 0099 §10 L2, §11 clauses 6 and 8). Each element is
    /// the same value a caught rejection of that leaf would bind.
    fn aggregate_error(
        &mut self,
        elements: &[CompiledAggregateAwaitShape],
        errors: &[ExecutionHostError],
        instruction_ip: usize,
    ) -> Result<RuntimeError, RuntimeError> {
        let mut items = Vec::with_capacity(elements.len());
        for element in elements {
            let CompiledAggregateAwaitShape::BatchLeaf(index) = element else {
                return Err(RuntimeError::ResourceBatchReply {
                    problem: "an exhausted Promise.any holds a plain operand, which fulfils"
                        .to_string(),
                });
            };
            let source = errors
                .get(*index)
                .cloned()
                .ok_or(RuntimeError::AggregateAwaitLeafOutOfRange)?;
            items.push(self.runtime_error_value(
                &RuntimeError::UnwrappedModuleOperationFailed { source },
                instruction_ip,
            )?);
        }
        let errors = self.heap.allocate_list(items)?;
        let value = self.heap.allocate_error(
            ErrorKind::AggregateError,
            Some("All promises were rejected".to_string()),
            None,
            Some(errors),
        )?;
        Ok(RuntimeError::UncaughtException { value })
    }

    /// The one host call an aggregate makes, with its reply validated against
    /// the consumer mode that asked for it. Any refusal fails every leaf's
    /// execution node before it is returned: a malformed reply fails closed
    /// rather than being repaired into a plausible answer.
    async fn perform_resource_operation_batch(
        &mut self,
        leaves: Vec<ResourceOperationBatchLeaf>,
        active_nodes: &[Option<ActiveLashlangExecutionNode>],
        consumer: AggregateConsumer,
        settled_value_after: Option<usize>,
    ) -> Result<ResourceOperationBatchOutcome, RuntimeError> {
        let expected = leaves.len();
        let result = self
            .host
            .perform(AbilityOp::ResourceOperationBatch(ResourceOperationBatch {
                leaves,
                consumer,
                settled_value_after,
            }))
            .await;
        let reply = match result {
            Ok(AbilityOutcome::ResourceOperationBatch(reply)) => reply,
            Ok(AbilityOutcome::Value(_) | AbilityOutcome::Unit | AbilityOutcome::HandedOver) => {
                return Err(self.fail_resource_operation_batch(
                    active_nodes,
                    RuntimeError::InvalidResourceBatchResult,
                ));
            }
            Err(error) => {
                return Err(self.fail_resource_operation_batch(
                    active_nodes,
                    RuntimeError::AggregateHostControl { source: error },
                ));
            }
        };
        // The value-entry guard, on the one ability whose values arrive in
        // bulk. Like the malformed shapes below, a reply carrying a
        // prototype-chain data key fails the whole batch closed rather than
        // handing one leaf a value nothing can read.
        let carried = match &reply {
            ResourceOperationBatchOutcome::AllResults(results) => results.iter().collect(),
            ResourceOperationBatchOutcome::Selected { result, .. } => vec![result],
            ResourceOperationBatchOutcome::SettledValue
            | ResourceOperationBatchOutcome::ExhaustedRejections(_) => Vec::new(),
        };
        if let Some(rejection) = carried.into_iter().find_map(|result| match result {
            ResourceOperationOutcome::Value(value) => prototype_chain_data_key_error(value),
            ResourceOperationOutcome::Error(_) => None,
        }) {
            return Err(self.fail_resource_operation_batch(active_nodes, rejection));
        }
        let problem = match (&reply, consumer) {
            (ResourceOperationBatchOutcome::AllResults(results), _)
                if results.len() != expected =>
            {
                return Err(self.fail_resource_operation_batch(
                    active_nodes,
                    RuntimeError::ResourceBatchResultCount {
                        actual: results.len(),
                        expected,
                    },
                ));
            }
            (
                ResourceOperationBatchOutcome::AllResults(_),
                AggregateConsumer::All | AggregateConsumer::AllSettled,
            ) => None,
            (ResourceOperationBatchOutcome::Selected { leaf, .. }, _) if *leaf >= expected => Some(
                format!("selected leaf {leaf} is out of range for {expected} leaves"),
            ),
            (ResourceOperationBatchOutcome::Selected { .. }, AggregateConsumer::Race) => None,
            (
                ResourceOperationBatchOutcome::Selected {
                    result: ResourceOperationOutcome::Value(_),
                    ..
                },
                AggregateConsumer::Any,
            ) => None,
            (
                ResourceOperationBatchOutcome::Selected {
                    result: ResourceOperationOutcome::Error(_),
                    ..
                },
                AggregateConsumer::All,
            ) => None,
            (
                ResourceOperationBatchOutcome::SettledValue,
                AggregateConsumer::Race | AggregateConsumer::Any,
            ) if settled_value_after.is_some() => None,
            (
                ResourceOperationBatchOutcome::ExhaustedRejections(errors),
                AggregateConsumer::Any,
            ) if errors.len() == expected => None,
            (other, consumer) => Some(format!(
                "a {consumer:?} aggregate cannot be answered with {}",
                reply_shape_name(other)
            )),
        };
        if let Some(problem) = problem {
            return Err(self.fail_resource_operation_batch(
                active_nodes,
                RuntimeError::ResourceBatchReply { problem },
            ));
        }
        Ok(reply)
    }

    fn fail_resource_operation_batch(
        &mut self,
        active_nodes: &[Option<ActiveLashlangExecutionNode>],
        error: RuntimeError,
    ) -> RuntimeError {
        for active in active_nodes.iter().flatten() {
            self.fail_lashlang_execution(active, &error);
        }
        error
    }

    /// Turns every leaf's result into a value: an unwrapped leaf takes the
    /// value and records its rejection, a wrapped leaf becomes a result record.
    /// The first unwrapped rejection in **written** order fails the aggregate
    /// — the Lashlang-native rule (ADR 0099 §10 L7); `allSettled` never
    /// unwraps a leaf, and `Promise.all` has its rejection selected by the
    /// host before it gets here.
    fn settle_resource_operation_leaves(
        &mut self,
        leaves: impl Iterator<Item = (bool, Option<Span>)>,
        results: Vec<ResourceOperationOutcome>,
        active_nodes: &[Option<ActiveLashlangExecutionNode>],
    ) -> Result<Vec<Value>, RuntimeError> {
        let mut first_rejection = None;
        let mut leaf_values = Vec::with_capacity(results.len());
        for (((unwrap, source_span), result), active) in
            leaves.zip(results).zip(active_nodes.iter())
        {
            match result {
                ResourceOperationOutcome::Value(value) => {
                    leaf_values.push(if unwrap { value } else { success(value) });
                    if let Some(active) = active {
                        self.complete_lashlang_execution(active);
                    }
                }
                ResourceOperationOutcome::Error(error) => {
                    if unwrap {
                        if let Some(active) = active {
                            self.fail_lashlang_execution(
                                active,
                                &RuntimeError::UnwrappedModuleOperationFailed {
                                    source: error.clone(),
                                },
                            );
                        }
                        first_rejection.get_or_insert((error, source_span));
                        leaf_values.push(Value::Null);
                    } else {
                        leaf_values.push(execution_host_error_value(error, "resource_batch"));
                        if let Some(active) = active {
                            self.complete_lashlang_execution(active);
                        }
                    }
                }
            }
        }
        if let Some((source, span)) = first_rejection {
            self.pending_error_span = span;
            return Err(RuntimeError::UnwrappedModuleOperationFailed { source });
        }
        Ok(leaf_values)
    }

    /// Awaits a process handle, or every handle inside a tuple, list or
    /// record of them, into result records. A value with no handle in it is
    /// already resolved: awaiting it is a guest error, never a wrapped
    /// `{ ok: false }` that reads like a host failure.
    fn await_value<'vm>(
        &'vm self,
        handle: Value,
        active: Option<&'vm ActiveLashlangExecutionNode>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + 'vm>,
    > {
        Box::pin(async move {
            self.observe_child_process_wait(active, &handle);
            let result = self.await_value_at(handle, String::new()).await;
            if result.is_ok() && !self.host.is_cancelled() {
                self.observe_wait_resumed(active);
            }
            result
        })
    }

    fn await_value_at<'vm>(
        &'vm self,
        handle: Value,
        path: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + 'vm>,
    > {
        Box::pin(async move {
            match handle {
                Value::Tuple(handles) => {
                    let mut values = Vec::with_capacity(handles.len());
                    for (index, handle) in handles.iter().cloned().enumerate() {
                        values.push(
                            self.await_value_at(handle, format!("{path}[{index}]"))
                                .await?,
                        );
                    }
                    Ok(Value::Tuple(values.into()))
                }
                Value::List(handles) => {
                    let mut values = Vec::with_capacity(handles.len());
                    for (index, handle) in handles.iter().cloned().enumerate() {
                        values.push(
                            self.await_value_at(handle, format!("{path}[{index}]"))
                                .await?,
                        );
                    }
                    Ok(Value::List(values.into()))
                }
                Value::Record(handles) if is_process_handle(&handles) => {
                    let result = self
                        .host
                        .perform(AbilityOp::Await(Value::Record(handles)))
                        .await;
                    Ok(match result {
                        Ok(AbilityOutcome::Value(value)) => host_success(value, "await"),
                        Ok(AbilityOutcome::ResourceOperationBatch(_)) => {
                            execution_host_error_value(
                                ExecutionHostError::new(
                                    "await returned a resource operation batch result",
                                ),
                                "await",
                            )
                        }
                        Ok(AbilityOutcome::Unit) => execution_host_error_value(
                            ExecutionHostError::new("await returned no value"),
                            "await",
                        ),
                        Ok(AbilityOutcome::HandedOver) => execution_host_error_value(
                            ExecutionHostError::new("await returned a hand-over"),
                            "await",
                        ),
                        Err(error) => execution_host_error_value(error, "await"),
                    })
                }
                Value::Record(handles) => {
                    let mut record = record_with_capacity(handles.len());
                    for entry in handles.entries.iter() {
                        record.insert_symbolized(
                            &entry.symbol,
                            self.await_value_at(
                                entry.value.clone(),
                                if path.is_empty() {
                                    entry.symbol.as_str().to_string()
                                } else {
                                    format!("{path}.{}", entry.symbol.as_str())
                                },
                            )
                            .await?,
                        );
                    }
                    Ok(Value::Record(Arc::new(record)))
                }
                resolved => Err(RuntimeError::AwaitExpectsHandle {
                    found: if path.is_empty() {
                        value_type_name(&resolved).to_string()
                    } else {
                        format!("{} at `{path}`", value_type_name(&resolved))
                    },
                }),
            }
        })
    }

    async fn await_value_unwrap(
        &self,
        handle: Value,
        active: Option<&ActiveLashlangExecutionNode>,
    ) -> Result<Value, RuntimeError> {
        match handle {
            Value::Record(handles) if is_process_handle(&handles) => {
                self.observe_child_process_wait(active, &Value::Record(handles.clone()));
                let result = self
                    .host
                    .perform(AbilityOp::Await(Value::Record(handles)))
                    .await;
                if result.is_ok() && !self.host.is_cancelled() {
                    self.observe_wait_resumed(active);
                }
                result
                    .and_then(|result| result.into_value("await"))
                    .map_err(|error| {
                        if error.tool_failure_code().is_some() {
                            RuntimeError::UnwrappedHostToolResultFailed { source: error }
                        } else {
                            RuntimeError::UnwrappedToolResultFailed {
                                message: error.to_string(),
                            }
                        }
                    })
            }
            Value::Tuple(_) | Value::List(_) | Value::Record(_) => {
                unwrap_tool_result(self.await_value(handle, active).await?)
            }
            resolved => Err(RuntimeError::AwaitExpectsHandle {
                found: value_type_name(&resolved).to_string(),
            }),
        }
    }

    fn observe_child_process_wait(
        &self,
        active: Option<&ActiveLashlangExecutionNode>,
        value: &Value,
    ) {
        let Some(active) = active else { return };
        let mut process_ids = Vec::new();
        collect_awaited_process_ids(value, &mut process_ids);
        if process_ids.is_empty() {
            return;
        }
        self.observe(
            || crate::LashlangExecutionObservation::ChildProcessWaiting {
                site: active.site.clone(),
                occurrence: active.occurrence,
                process_ids,
            },
        );
    }

    fn observe_wait_resumed(&self, active: Option<&ActiveLashlangExecutionNode>) {
        if let Some(active) = active {
            self.observe(|| crate::LashlangExecutionObservation::NodeResumed {
                site: active.site.clone(),
                occurrence: active.occurrence,
            });
        }
    }
}

fn collect_awaited_process_ids(value: &Value, process_ids: &mut Vec<lash_sansio::ProcessId>) {
    match value {
        Value::Tuple(values) | Value::List(values) => {
            for value in values.iter() {
                collect_awaited_process_ids(value, process_ids);
            }
        }
        Value::Record(record) => {
            if let Some(lash_sansio::handle::HandleTarget::Process { process_id, .. }) =
                parse_handle_record(record).and_then(|id| id.target())
            {
                if !process_ids.contains(&process_id) {
                    process_ids.push(process_id);
                }
            } else {
                for entry in record.entries.iter() {
                    collect_awaited_process_ids(&entry.value, process_ids);
                }
            }
        }
        _ => {}
    }
}

/// The name of a reply shape, for the refusal that names what arrived.
fn reply_shape_name(reply: &ResourceOperationBatchOutcome) -> &'static str {
    match reply {
        ResourceOperationBatchOutcome::AllResults(_) => "every result",
        ResourceOperationBatchOutcome::Selected { .. } => "a selected settlement",
        ResourceOperationBatchOutcome::SettledValue => "a settled plain value",
        ResourceOperationBatchOutcome::ExhaustedRejections(_) => "exhausted rejections",
    }
}

/// The stack-value positions written at the aggregate's own element positions.
///
/// Only the direct children of the awaited container are elements. Anything
/// deeper is a value *inside* an element, which settlement never reaches
/// (ADR 0096) and which this walk therefore must not descend into.
fn element_value_positions(shape: &CompiledAggregateAwaitShape) -> Vec<usize> {
    let elements = match shape {
        CompiledAggregateAwaitShape::List(values)
        | CompiledAggregateAwaitShape::Record { values, .. } => values,
        CompiledAggregateAwaitShape::BatchLeaf(_) | CompiledAggregateAwaitShape::Value(_) => {
            return Vec::new();
        }
    };
    elements
        .iter()
        .filter_map(|element| match element {
            CompiledAggregateAwaitShape::Value(index) => Some(*index),
            _ => None,
        })
        .collect()
}

/// Expands each leaf's packed receiver/argument values into the flat operand
/// list, preserving source traversal order for both the host batch and its
/// rejection policy. No operation executes here.
fn expand_aggregate_await_shape(
    shape: &CompiledAggregateAwaitShape,
    template: &CompiledResourceOperationBatch,
    captured: &[Value],
    leaves: &mut Vec<CompiledResourceOperationBatchLeaf>,
    values: &mut Vec<Value>,
) -> Result<CompiledAggregateAwaitShape, RuntimeError> {
    Ok(match shape {
        CompiledAggregateAwaitShape::BatchLeaf(index) => {
            let leaf = &template.leaves[*index];
            let start = leaf.receiver_stack_index;
            let packed = captured
                .get(start..start + leaf.argc + 1)
                .ok_or(RuntimeError::ResourceBatchArgumentOutOfRange)?;
            let index = leaves.len();
            leaves.push(CompiledResourceOperationBatchLeaf {
                receiver_stack_index: values.len(),
                ..leaf.clone()
            });
            values.extend_from_slice(packed);
            CompiledAggregateAwaitShape::BatchLeaf(index)
        }
        CompiledAggregateAwaitShape::Value(index) => {
            let value = captured
                .get(*index)
                .ok_or(RuntimeError::AggregateAwaitValueOutOfRange)?;
            let index = values.len();
            values.push(value.clone());
            CompiledAggregateAwaitShape::Value(index)
        }
        CompiledAggregateAwaitShape::List(items) => CompiledAggregateAwaitShape::List(
            items
                .iter()
                .map(|item| expand_aggregate_await_shape(item, template, captured, leaves, values))
                .collect::<Result<_, _>>()?,
        ),
        CompiledAggregateAwaitShape::Record {
            keys,
            values: items,
        } => CompiledAggregateAwaitShape::Record {
            keys: *keys,
            values: items
                .iter()
                .map(|item| expand_aggregate_await_shape(item, template, captured, leaves, values))
                .collect::<Result<_, _>>()?,
        },
    })
}

fn build_aggregate_await_shape<H: ExecutionHost>(
    shape: &CompiledAggregateAwaitShape,
    stack_values: &[Value],
    leaf_values: &[Value],
    vm: &Vm<'_, H>,
) -> Result<Value, RuntimeError> {
    match shape {
        CompiledAggregateAwaitShape::BatchLeaf(index) => leaf_values
            .get(*index)
            .cloned()
            .ok_or(RuntimeError::AggregateAwaitLeafOutOfRange),
        CompiledAggregateAwaitShape::Value(index) => stack_values
            .get(*index)
            .cloned()
            .ok_or(RuntimeError::AggregateAwaitValueOutOfRange),
        CompiledAggregateAwaitShape::List(values) => values
            .iter()
            .map(|value| build_aggregate_await_shape(value, stack_values, leaf_values, vm))
            .collect::<Result<Vec<_>, _>>()
            .map(|values| Value::List(values.into())),
        CompiledAggregateAwaitShape::Record { keys, values } => {
            let key_indices = &vm.chunk.key_lists[*keys];
            if key_indices.len() != values.len() {
                return Err(RuntimeError::InvalidAggregateAwaitRecordShape);
            }
            let mut record = record_with_capacity(values.len());
            for (key, value_shape) in key_indices.iter().zip(values.iter()) {
                let name_entry = &vm.chunk.names[*key];
                let value =
                    build_aggregate_await_shape(value_shape, stack_values, leaf_values, vm)?;
                record.insert_symbolized(&name_entry.symbol, value);
            }
            Ok(Value::Record(Arc::new(record)))
        }
    }
}

fn lashlang_execution_call_site(active: &ActiveLashlangExecutionNode) -> LashlangExecutionCallSite {
    LashlangExecutionCallSite {
        site: active.site.clone(),
        occurrence: active.occurrence,
    }
}

/// A host value that clears the value-entry guard becomes a success result; one
/// that does not becomes the same error result a failed ability produces, so
/// the refusal reaches the guest by the route it already handles.
fn host_success(value: Value, operation: &str) -> Value {
    match prototype_chain_data_key_error(&value) {
        Some(error) => {
            execution_host_error_value(ExecutionHostError::new(error.to_string()), operation)
        }
        None => success(value),
    }
}
