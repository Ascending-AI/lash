use std::sync::Arc;

use crate::LashlangExecutionCallSite;
use crate::span::Span;

use super::super::access::prototype_chain_data_key_error;
use super::super::host::{
    AbilityOp, AbilityResult, ProcessEvent, ProcessEventKind, ResourceOperation,
    ResourceOperationBatch, ResourceOperationResult, Sleep, SleepKind,
};
use super::super::ops::value_type_name;
use super::super::{
    CompiledAggregateAwaitShape, CompiledResourceOperationBatch,
    CompiledResourceOperationBatchLeaf, ExecutionHost, ExecutionHostError, RuntimeError, Value,
    execution_host_error_value, is_process_handle, parse_handle_record, record_with_capacity,
    success, unwrap_tool_result,
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
    AwaitArray { settle: bool },
    AwaitPending,
    ResourceOperationBatch(usize),
    ResourceOperationListBatch(usize),
    AwaitHandle,
    Sleep(SleepKind),
    WaitSignal { name: usize },
    AwaitHandleUnwrap,
    Print,
    ProcessEvent(ProcessEventKind),
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
                    Ok(AbilityResult::Value(value)) => host_success(value, &operation_name),
                    Ok(AbilityResult::ResourceOperationBatch(_)) => execution_host_error_value(
                        ExecutionHostError::new(
                            "module operation returned a resource operation batch result",
                        ),
                        &operation_name,
                    ),
                    Ok(AbilityResult::Unit) => execution_host_error_value(
                        ExecutionHostError::new("module operation returned no value"),
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
            VmEffect::AwaitArray { settle } => {
                self.await_pending_array(settle, instruction_ip).await?;
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
                        self.await_pending_array(false, instruction_ip).await?;
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
            VmEffect::ResourceOperationListBatch(batch) => {
                self.resolve_resource_operation_list_batch(batch, instruction_ip)
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
                let value = self
                    .host
                    .perform(AbilityOp::WaitSignal {
                        name: self.chunk.names[name].text.to_string(),
                        call_site: active.map(lashlang_execution_call_site),
                    })
                    .await
                    .and_then(|result| result.into_value("wait_signal"))
                    .map_err(|source| RuntimeError::WaitSignalFailed { source })?;
                self.stack.push(value);
            }
            VmEffect::AwaitHandleUnwrap => {
                let handle = self.pop_stack()?;
                let result = self.await_value_unwrap(handle, active).await?;
                self.stack.push(result);
            }
            VmEffect::ProcessEvent(kind) => {
                let value = self.pop_stack()?;
                self.host
                    .perform(AbilityOp::ProcessEvent(ProcessEvent {
                        kind,
                        value: value.clone(),
                    }))
                    .await
                    .map_err(|source| RuntimeError::ProcessEventFailed { source })?;
                self.last_value = Some(value.clone());
                self.stack.push(value);
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
            first_settled_rejection: batch.first_settled_rejection,
        };
        self.resolve_batch_spec(&expanded, expanded_values, instruction_ip)
            .await
    }

    /// Settles one aggregate await as a single host batch.
    ///
    /// Every leaf the aggregate has to settle is a resource operation, so there
    /// is one recorded settlement order for the whole aggregate and no second
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

        let leaf_values = if batch.leaves.is_empty() {
            Vec::new()
        } else {
            self.settle_tool_leaves(batch, &values, instruction_ip)
                .await?
        };

        let mut value = build_aggregate_await_shape(&batch.shape, &values, &leaf_values, self)?;
        if batch.aggregate_unwrap {
            value = unwrap_tool_result(value)?;
        }
        self.stack.push(value);
        Ok(())
    }

    /// Returns each leaf's value in leaf order, or the rejection the batch reports (first
    /// settled for `Promise.all`, first written otherwise).
    async fn settle_tool_leaves(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: &[Value],
        instruction_ip: usize,
    ) -> Result<Vec<Value>, RuntimeError> {
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
            let args_start = leaf.receiver_stack_index + 1;
            let args_end = args_start + leaf.argc;
            let args = values
                .get(args_start..args_end)
                .ok_or(RuntimeError::ResourceBatchArgumentOutOfRange)?
                .to_vec();
            operations.push(ResourceOperation {
                receiver,
                operation: self.chunk.names[leaf.operation].text.to_string(),
                args,
                call_site: active.as_ref().map(lashlang_execution_call_site),
            });
            active_nodes.push(active);
        }

        let occurrence = self.next_aggregate_occurrence(instruction_ip);
        let settled = self
            .perform_resource_operation_batch(
                operations,
                occurrence,
                &active_nodes,
                batch.first_settled_rejection,
            )
            .await?;
        self.settle_resource_operation_leaves(
            batch
                .leaves
                .iter()
                .map(|leaf| (leaf.unwrap, leaf.source_span)),
            settled,
            &active_nodes,
        )
    }

    /// An empty comprehension never reaches the host.
    async fn resolve_resource_operation_list_batch(
        &mut self,
        batch: usize,
        instruction_ip: usize,
    ) -> Result<(), RuntimeError> {
        let batch = &self.chunk.resource_operation_list_batches[batch];
        let Value::List(calls) = self.pop_stack()? else {
            return Err(RuntimeError::ResourceListBatchMalformed);
        };
        let operation = self.chunk.names[batch.operation].text.to_string();
        let mut operations = Vec::with_capacity(calls.len());
        let mut active_nodes = Vec::with_capacity(calls.len());
        for call in calls.iter() {
            let Value::Tuple(items) = call else {
                return Err(RuntimeError::ResourceListBatchMalformed);
            };
            let mut items = items.iter();
            let Some(receiver) = items.next() else {
                return Err(RuntimeError::ResourceListBatchMalformed);
            };
            let args = items.cloned().collect::<Vec<_>>();
            if args.len() != batch.argc {
                return Err(RuntimeError::ResourceListBatchMalformed);
            }
            let active = batch
                .site
                .clone()
                .map(|site| self.begin_lashlang_execution_site(site));
            operations.push(ResourceOperation {
                receiver: receiver.clone(),
                operation: operation.clone(),
                args,
                call_site: active.as_ref().map(lashlang_execution_call_site),
            });
            active_nodes.push(active);
        }

        let leaf_values = if operations.is_empty() {
            Vec::new()
        } else {
            let occurrence = self.next_aggregate_occurrence(instruction_ip);
            let settled = self
                .perform_resource_operation_batch(operations, occurrence, &active_nodes, false)
                .await?;
            self.settle_resource_operation_leaves(
                std::iter::repeat_n((batch.unwrap, batch.source_span), calls.len()),
                settled,
                &active_nodes,
            )?
        };

        let mut value = Value::List(leaf_values.into());
        if batch.aggregate_unwrap {
            value = unwrap_tool_result(value)?;
        }
        self.stack.push(value);
        Ok(())
    }

    /// Any refusal fails every leaf's execution node before it is returned.
    async fn perform_resource_operation_batch(
        &mut self,
        operations: Vec<ResourceOperation>,
        occurrence: u64,
        active_nodes: &[Option<ActiveLashlangExecutionNode>],
        first_settled_rejection: bool,
    ) -> Result<SettledResourceOperationBatch, RuntimeError> {
        let expected = operations.len();
        let result = self
            .host
            .perform(AbilityOp::ResourceOperationBatch(ResourceOperationBatch {
                operations,
                occurrence,
                first_settled_rejection,
            }))
            .await;
        let result = match result {
            Ok(AbilityResult::ResourceOperationBatch(result)) => result,
            Ok(AbilityResult::Value(_)) | Ok(AbilityResult::Unit) => {
                return Err(self.fail_resource_operation_batch(
                    active_nodes,
                    RuntimeError::InvalidResourceBatchResult,
                ));
            }
            Err(error) => {
                return Err(self.fail_resource_operation_batch(
                    active_nodes,
                    RuntimeError::ResourceBatchFailed { source: error },
                ));
            }
        };
        // The value-entry guard, on the one ability whose values arrive in
        // bulk. Like the malformed shapes below, a batch carrying a
        // prototype-chain data key fails the whole batch closed rather than
        // handing one leaf a value nothing can read.
        if let Some(rejection) = result.results.iter().find_map(|result| match result {
            ResourceOperationResult::Value(value) => prototype_chain_data_key_error(value),
            ResourceOperationResult::Error(_) => None,
        }) {
            return Err(self.fail_resource_operation_batch(active_nodes, rejection));
        }
        if result.results.len() != expected {
            return Err(self.fail_resource_operation_batch(
                active_nodes,
                RuntimeError::ResourceBatchResultCount {
                    actual: result.results.len(),
                    expected,
                },
            ));
        }
        // A batch that selects by settlement order needs a usable order. A host
        // that reports a malformed one fails closed rather than being silently
        // read as input order, which is the behaviour this replaces.
        let selection = if first_settled_rejection {
            match result.settlement_sequence() {
                Ok(order) => order.to_vec(),
                Err(problem) => {
                    return Err(self.fail_resource_operation_batch(
                        active_nodes,
                        RuntimeError::ResourceBatchSettlementOrder { problem },
                    ));
                }
            }
        } else {
            (0..expected).collect()
        };
        Ok(SettledResourceOperationBatch {
            results: result.results,
            selection,
        })
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

    /// Turns settled results into per-leaf values: an unwrapped leaf takes the
    /// value and records its rejection, a wrapped leaf becomes a result record.
    /// The first rejection in the batch's selection order fails the aggregate.
    ///
    /// `Promise.all` reports the rejection that settled first (ADR 0096), so a
    /// batch that can propagate a rejection scans the host's settlement order;
    /// `allSettled` never propagates one and keeps leaf order.
    fn settle_resource_operation_leaves(
        &mut self,
        leaves: impl Iterator<Item = (bool, Option<Span>)>,
        settled: SettledResourceOperationBatch,
        active_nodes: &[Option<ActiveLashlangExecutionNode>],
    ) -> Result<Vec<Value>, RuntimeError> {
        let mut unwrapped_errors = vec![None; settled.results.len()];
        let mut leaf_values = Vec::with_capacity(settled.results.len());
        for (leaf_index, (((unwrap, source_span), result), active)) in leaves
            .zip(settled.results)
            .zip(active_nodes.iter())
            .enumerate()
        {
            match result {
                ResourceOperationResult::Value(value) => {
                    leaf_values.push(if unwrap { value } else { success(value) });
                    if let Some(active) = active {
                        self.complete_lashlang_execution(active);
                    }
                }
                ResourceOperationResult::Error(error) => {
                    if unwrap {
                        unwrapped_errors[leaf_index] = Some((error.clone(), source_span));
                        if let Some(active) = active {
                            self.fail_lashlang_execution(
                                active,
                                &RuntimeError::UnwrappedModuleOperationFailed {
                                    source: error.clone(),
                                },
                            );
                        }
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
        for index in settled.selection {
            if let Some((source, span)) = unwrapped_errors.get_mut(index).and_then(Option::take) {
                self.pending_error_span = span;
                return Err(RuntimeError::UnwrappedModuleOperationFailed { source });
            }
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
                        Ok(AbilityResult::Value(value)) => host_success(value, "await"),
                        Ok(AbilityResult::ResourceOperationBatch(_)) => execution_host_error_value(
                            ExecutionHostError::new(
                                "await returned a resource operation batch result",
                            ),
                            "await",
                        ),
                        Ok(AbilityResult::Unit) => execution_host_error_value(
                            ExecutionHostError::new("await returned no value"),
                            "await",
                        ),
                        Err(error) => execution_host_error_value(error, "await"),
                    })
                }
                Value::Record(handles) => {
                    let mut record = record_with_capacity(handles.len());
                    for entry in handles.entries.iter() {
                        record.insert_symbolized(
                            entry.symbol,
                            entry.name.clone(),
                            self.await_value_at(
                                entry.value.clone(),
                                if path.is_empty() {
                                    entry.name.to_string()
                                } else {
                                    format!("{path}.{}", entry.name)
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
        self.host.observe_lashlang_execution(
            crate::LashlangExecutionObservation::ChildProcessWaiting {
                site: active.site.clone(),
                occurrence: active.occurrence,
                process_ids,
            },
        );
    }

    fn observe_wait_resumed(&self, active: Option<&ActiveLashlangExecutionNode>) {
        if let Some(active) = active {
            self.host.observe_lashlang_execution(
                crate::LashlangExecutionObservation::NodeResumed {
                    site: active.site.clone(),
                    occurrence: active.occurrence,
                },
            );
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
                let process_id = lash_sansio::ProcessId::from(process_id);
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

/// One host batch reply after validation: the per-leaf results and the order
/// in which unwrapped rejections are considered.
struct SettledResourceOperationBatch {
    results: Vec<ResourceOperationResult>,
    selection: Vec<usize>,
}

/// The stack-value positions written at the aggregate's own element positions.
///
/// Only the direct children of the awaited container are elements. Anything
/// deeper is a value *inside* an element, which settlement never reaches
/// (ADR 0096) and which this walk therefore must not descend into.
fn element_value_positions(shape: &CompiledAggregateAwaitShape) -> Vec<usize> {
    let elements = match shape {
        CompiledAggregateAwaitShape::Tuple(values)
        | CompiledAggregateAwaitShape::List(values)
        | CompiledAggregateAwaitShape::Record { values, .. } => values,
        CompiledAggregateAwaitShape::Comprehension { .. }
        | CompiledAggregateAwaitShape::BatchLeaf(_)
        | CompiledAggregateAwaitShape::Value(_) => return Vec::new(),
    };
    elements
        .iter()
        .filter_map(|element| match element {
            CompiledAggregateAwaitShape::Value(index) => Some(*index),
            _ => None,
        })
        .collect()
}

/// Expand captured comprehension lists recursively, preserving source traversal
/// order for both the host batch and its rejection policy. Each element uses
/// its own packed receiver/argument values; no operation executes here.
fn expand_aggregate_await_shape(
    shape: &CompiledAggregateAwaitShape,
    template: &CompiledResourceOperationBatch,
    captured: &[Value],
    leaves: &mut Vec<CompiledResourceOperationBatchLeaf>,
    values: &mut Vec<Value>,
) -> Result<CompiledAggregateAwaitShape, RuntimeError> {
    Ok(match shape {
        CompiledAggregateAwaitShape::Comprehension {
            stack_index,
            template,
        } => {
            let Some(Value::List(elements)) = captured.get(*stack_index) else {
                return Err(RuntimeError::ResourceListBatchMalformed);
            };
            let mut shapes = Vec::with_capacity(elements.len());
            for element in elements.iter() {
                let Value::Tuple(packed) = element else {
                    return Err(RuntimeError::ResourceListBatchMalformed);
                };
                if packed.len() != template.stack_value_count {
                    return Err(RuntimeError::ResourceListBatchMalformed);
                }
                shapes.push(expand_aggregate_await_shape(
                    &template.shape,
                    template,
                    packed,
                    leaves,
                    values,
                )?);
            }
            CompiledAggregateAwaitShape::List(shapes.into_boxed_slice())
        }
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
        CompiledAggregateAwaitShape::Tuple(items) => CompiledAggregateAwaitShape::Tuple(
            items
                .iter()
                .map(|item| expand_aggregate_await_shape(item, template, captured, leaves, values))
                .collect::<Result<_, _>>()?,
        ),
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
        CompiledAggregateAwaitShape::Comprehension { .. } => {
            Err(RuntimeError::ResourceListBatchMalformed)
        }
        CompiledAggregateAwaitShape::BatchLeaf(index) => leaf_values
            .get(*index)
            .cloned()
            .ok_or(RuntimeError::AggregateAwaitLeafOutOfRange),
        CompiledAggregateAwaitShape::Value(index) => stack_values
            .get(*index)
            .cloned()
            .ok_or(RuntimeError::AggregateAwaitValueOutOfRange),
        CompiledAggregateAwaitShape::Tuple(values) => values
            .iter()
            .map(|value| build_aggregate_await_shape(value, stack_values, leaf_values, vm))
            .collect::<Result<Vec<_>, _>>()
            .map(|values| Value::Tuple(values.into())),
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
                record.insert_symbolized(name_entry.symbol, name_entry.text.clone(), value);
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
