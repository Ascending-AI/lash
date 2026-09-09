use std::sync::Arc;

use crate::lexer::Span;
use crate::{LashlangExecutionCallSite, LashlangExecutionChild};

use super::super::access::prototype_chain_data_key_error;
use super::super::host::{
    AbilityOp, AbilityResult, ProcessEvent, ProcessEventKind, ProcessSignal, ProcessStart,
    ResourceOperation, ResourceOperationBatch, ResourceOperationResult, Sleep, SleepKind,
};
use super::super::ops::value_type_name;
use super::super::{
    CompiledAggregateAwaitShape, CompiledResourceOperationBatch,
    CompiledResourceOperationBatchLeaf, ExecutionHost, RuntimeError, Value, error_value,
    is_process_handle, is_runtime_process_handle, record_with_capacity, success,
    unwrap_tool_result,
};
use super::control::VmOutcome;
use super::pending_tools::{
    AwaitedValue, FOREIGN_HANDLE, ProcessLeafSettlement, SETTLED_HANDLE,
    ensure_no_tool_handle_arguments, plain_value_awaited,
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
    StartProcess { process: usize, keys: usize },
    AwaitHandle,
    Sleep(SleepKind),
    WaitSignal { name: usize },
    SignalRun { name: usize },
    AwaitHandleUnwrap,
    CancelHandle,
    PrintValues(usize),
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
        let result = Box::pin(self.resolve_effect_inner(effect, active.as_ref())).await;
        match (&result, active.as_ref()) {
            (Ok(Some(VmOutcome::ProcessFailed(value))), Some(active)) => {
                self.fail_lashlang_execution(active, value.to_string());
            }
            (Ok(_), Some(active)) => {
                self.complete_lashlang_execution(active);
            }
            (Err(error), Some(active)) => {
                self.fail_lashlang_execution(active, error.to_string());
            }
            _ => {}
        }
        result
    }

    async fn resolve_effect_inner(
        &mut self,
        effect: VmEffect,
        active: Option<&ActiveLashlangExecutionNode>,
    ) -> Result<Option<VmOutcome>, RuntimeError> {
        match effect {
            VmEffect::ResourceCall { operation, argc } => {
                let (receiver, args) = self.drain_receiver_call(argc)?;
                ensure_no_tool_handle_arguments(&args)?;
                let result = match self
                    .host
                    .perform(AbilityOp::ResourceOperation(ResourceOperation {
                        receiver,
                        operation: self.chunk.names[operation].text.to_string(),
                        args,
                        call_site: active.map(lashlang_execution_call_site),
                    }))
                    .await
                {
                    Ok(AbilityResult::Value(value)) => host_success(value),
                    Ok(AbilityResult::ResourceOperationBatch(_)) => error_value(
                        "module operation returned a resource operation batch result".to_string(),
                    ),
                    Ok(AbilityResult::Unit) => {
                        error_value("module operation returned no value".to_string())
                    }
                    Err(error) => error_value(error.to_string()),
                };
                self.stack.push(result);
            }
            VmEffect::ResourceCallUnwrap { operation, argc } => {
                let (receiver, args) = self.drain_receiver_call(argc)?;
                ensure_no_tool_handle_arguments(&args)?;
                let value = self
                    .host
                    .perform(AbilityOp::ResourceOperation(ResourceOperation {
                        receiver,
                        operation: self.chunk.names[operation].text.to_string(),
                        args,
                        call_site: active.map(lashlang_execution_call_site),
                    }))
                    .await
                    .and_then(|result| result.into_value("module operation"))
                    .map_err(|source| RuntimeError::UnwrappedModuleOperationFailed { source })?;
                self.stack.push(value);
            }
            VmEffect::AwaitArray { settle } => {
                self.await_pending_array(settle).await?;
            }
            VmEffect::AwaitPending => {
                let value = self.pop_stack()?;
                match self.classify_awaited(&value) {
                    AwaitedValue::LocalToolHandle(id) => {
                        if self.pending_tools.get(id).is_none_or(Option::is_none) {
                            return Err(RuntimeError::PendingTool {
                                problem: SETTLED_HANDLE.into(),
                            });
                        }
                        self.stack.push(Value::List(vec![value].into()));
                        self.await_pending_array(false).await?;
                        let Value::List(values) = self.pop_stack()? else {
                            unreachable!()
                        };
                        self.stack.push(values[0].clone());
                    }
                    AwaitedValue::ForeignToolHandle => {
                        return Err(RuntimeError::PendingTool {
                            problem: FOREIGN_HANDLE.into(),
                        });
                    }
                    // A process handle that reached the tool-await path (a
                    // runtime value the lowerer could not type) awaits the
                    // process the way the typed form does.
                    AwaitedValue::ProcessHandle => {
                        let value = self.await_value_unwrap(value).await?;
                        self.stack.push(value);
                    }
                    AwaitedValue::Plain => {
                        return Err(RuntimeError::PendingTool {
                            problem: plain_value_awaited(&value),
                        });
                    }
                }
            }
            VmEffect::ResourceOperationBatch(batch) => {
                self.resolve_resource_operation_batch(batch).await?;
            }
            VmEffect::ResourceOperationListBatch(batch) => {
                self.resolve_resource_operation_list_batch(batch).await?;
            }
            VmEffect::StartProcess { process, keys } => {
                let args = self.drain_record_from_stack(keys)?;
                let start_site = active
                    .map(lashlang_execution_call_site)
                    .ok_or(RuntimeError::StartSiteMissing)?;
                let process_name = self.chunk.names[process].text.to_string();
                let module_context = self
                    .chunk
                    .module_context
                    .as_ref()
                    .ok_or(RuntimeError::LinkedArtifactMissing)?;
                let process_ref = module_context
                    .process_refs
                    .get(&process_name)
                    .cloned()
                    .ok_or_else(|| RuntimeError::LinkedProcessNotExported {
                        module_ref: module_context.module_ref.clone(),
                        name: process_name.clone(),
                    })?;
                let child_module_ref = module_context.module_ref.clone();
                let child_host_requirements_ref = module_context.host_requirements_ref.clone();
                let value = self
                    .host
                    .perform(AbilityOp::StartProcess(Box::new(ProcessStart {
                        module_ref: child_module_ref.clone(),
                        process_ref: process_ref.clone(),
                        host_requirements_ref: child_host_requirements_ref,
                        start_site,
                        process_name: process_name.clone(),
                        args,
                    })))
                    .await
                    .and_then(|result| result.into_value("process start"))
                    .map_err(|source| RuntimeError::ProcessStartFailed { source })?;
                if let (Some(active), Some(process_id)) =
                    (active, process_handle_id_from_value(&value))
                {
                    self.observe_child_started(
                        active,
                        LashlangExecutionChild {
                            process_id,
                            module_ref: child_module_ref,
                            process_ref,
                            process_name,
                        },
                    );
                }
                self.stack.push(value);
            }
            VmEffect::AwaitHandle => {
                let handle = self.pop_stack()?;
                let result = self.await_value(handle).await?;
                self.stack.push(result);
            }
            VmEffect::Sleep(kind) => {
                let value = self.pop_stack()?;
                self.host
                    .perform(AbilityOp::Sleep(Sleep { kind, value }))
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
                    })
                    .await
                    .and_then(|result| result.into_value("wait_signal"))
                    .map_err(|source| RuntimeError::WaitSignalFailed { source })?;
                self.stack.push(value);
            }
            VmEffect::SignalRun { name } => {
                let payload = self.pop_stack()?;
                let run = self.pop_stack()?;
                self.host
                    .perform(AbilityOp::SignalRun(ProcessSignal {
                        run,
                        name: self.chunk.names[name].text.to_string(),
                        payload,
                    }))
                    .await
                    .and_then(|result| result.into_value("signal_run"))
                    .map_err(|source| RuntimeError::SignalRunFailed { source })?;
                self.last_value = Some(Value::Null);
                self.stack.push(Value::Null);
            }
            VmEffect::AwaitHandleUnwrap => {
                let handle = self.pop_stack()?;
                let result = self.await_value_unwrap(handle).await?;
                self.stack.push(result);
            }
            VmEffect::CancelHandle => {
                let handle = self.pop_stack()?;
                let value = self
                    .host
                    .perform(AbilityOp::Cancel(handle))
                    .await
                    .and_then(|result| result.into_value("cancel"))
                    .map_err(|source| RuntimeError::CancelFailed { source })?;
                self.last_value = Some(value.clone());
                self.stack.push(value);
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
            VmEffect::PrintValues(argc) => {
                use super::super::{BudgetedJsonProjector, ValueProjectionContext, ValueProjector};
                let start = self.stack_drain_start(argc)?;
                let values = self.stack.drain(start..).collect::<Vec<_>>();
                let mut rendered = Vec::with_capacity(values.len());
                for value in values {
                    rendered.push(
                        BudgetedJsonProjector::unbounded()
                            .project(ValueProjectionContext::new(&value))
                            .await,
                    );
                }
                self.host
                    .perform(AbilityOp::Print(Value::String(rendered.join(" ").into())))
                    .await
                    .map_err(|source| RuntimeError::PrintFailed { source })?;
                self.last_value = Some(Value::Null);
                self.stack.push(Value::Null);
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

    async fn resolve_resource_operation_batch(&mut self, batch: usize) -> Result<(), RuntimeError> {
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
        self.resolve_batch_spec(&expanded, expanded_values, ProcessLeafSettlement::Result)
            .await
    }

    /// Settles one aggregate await in two phases: every tool leaf as one host
    /// batch, then every process handle among the plain values in written
    /// order (ADR 0087). The shape is built once both phases are in.
    pub(super) async fn resolve_batch_spec(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        mut values: Vec<Value>,
        process_leaves: ProcessLeafSettlement,
    ) -> Result<(), RuntimeError> {
        let leaf_values = if batch.leaves.is_empty() {
            Vec::new()
        } else {
            self.settle_tool_leaves(batch, &values).await?
        };

        // Phase two: process handles written into the aggregate settle after
        // the tool batch, in written order, through the same durable
        // process-await seam a direct `await` uses. Lashlang values may be
        // bound containers, so walk those recursively; TypeScript deliberately
        // retains Promise's shallow element semantics. Reaching this point
        // means no tool leaf rejected, so the first failing process is the
        // rejection an unwrapping aggregate reports.
        let mut process_positions = Vec::new();
        collect_value_positions(&batch.shape, &mut process_positions);
        for index in process_positions {
            let Some(value) = values.get(index) else {
                return Err(RuntimeError::AggregateAwaitValueOutOfRange);
            };
            let handle = value.clone();
            values[index] = if self.reference_semantics {
                if !is_runtime_process_handle(&handle) {
                    continue;
                }
                match process_leaves {
                    ProcessLeafSettlement::Unwrap => self.await_value_unwrap(handle).await?,
                    ProcessLeafSettlement::Result => self.await_value(handle).await?,
                }
            } else {
                self.settle_lashlang_process_leaves(handle, process_leaves)
                    .await?
            };
        }

        let mut value = build_aggregate_await_shape(&batch.shape, &values, &leaf_values, self)?;
        if batch.aggregate_unwrap {
            value = unwrap_tool_result(value)?;
        }
        self.stack.push(value);
        Ok(())
    }

    /// Settles only process handles inside a Lashlang aggregate value. Unlike
    /// a direct `await`, ordinary leaves are retained because a runtime-bound
    /// container may deliberately mix handles with already-settled values.
    fn settle_lashlang_process_leaves(
        &self,
        value: Value,
        settlement: ProcessLeafSettlement,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + '_>>
    {
        Box::pin(async move {
            match value {
                handle if is_runtime_process_handle(&handle) => match settlement {
                    ProcessLeafSettlement::Unwrap => self.await_value_unwrap(handle).await,
                    ProcessLeafSettlement::Result => self.await_value(handle).await,
                },
                Value::Tuple(items) => {
                    let mut settled = Vec::with_capacity(items.len());
                    for item in items.iter().cloned() {
                        settled.push(
                            self.settle_lashlang_process_leaves(item, settlement)
                                .await?,
                        );
                    }
                    Ok(Value::Tuple(settled.into()))
                }
                Value::List(items) => {
                    let mut settled = Vec::with_capacity(items.len());
                    for item in items.iter().cloned() {
                        settled.push(
                            self.settle_lashlang_process_leaves(item, settlement)
                                .await?,
                        );
                    }
                    Ok(Value::List(settled.into()))
                }
                Value::Record(record) => {
                    let mut settled = record_with_capacity(record.len());
                    for entry in record.entries.iter() {
                        settled.insert_symbolized(
                            entry.symbol,
                            entry.name.clone(),
                            self.settle_lashlang_process_leaves(entry.value.clone(), settlement)
                                .await?,
                        );
                    }
                    Ok(Value::Record(Arc::new(settled)))
                }
                settled => Ok(settled),
            }
        })
    }

    /// Phase one of an aggregate await: every tool leaf as one host batch.
    /// Returns each leaf's value in leaf order, or the rejection the batch
    /// reports (first settled for `Promise.all`, first written otherwise).
    async fn settle_tool_leaves(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: &[Value],
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

        let settled = self
            .perform_resource_operation_batch(
                operations,
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

    /// Starts every call an awaited list comprehension collected as one host
    /// batch: the comprehension left `(receiver, args...)` tuples in a list on
    /// the stack, one per accepted element, all sharing the compiled leaf's
    /// operation and `?`. An empty comprehension never reaches the host.
    async fn resolve_resource_operation_list_batch(
        &mut self,
        batch: usize,
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
            let settled = self
                .perform_resource_operation_batch(operations, &active_nodes, false)
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

    /// Runs one host batch and validates the reply: a batch result of the
    /// right arity that clears the value-entry guard, together with the order
    /// leaf rejections are selected in. Any refusal fails every leaf's
    /// execution node before it is returned.
    async fn perform_resource_operation_batch(
        &mut self,
        operations: Vec<ResourceOperation>,
        active_nodes: &[Option<ActiveLashlangExecutionNode>],
        first_settled_rejection: bool,
    ) -> Result<SettledResourceOperationBatch, RuntimeError> {
        let expected = operations.len();
        let result = self
            .host
            .perform(AbilityOp::ResourceOperationBatch(ResourceOperationBatch {
                operations,
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
            self.fail_lashlang_execution(active, error.to_string());
        }
        error
    }

    /// Turns settled results into per-leaf values: an unwrapped leaf takes the
    /// value and records its rejection, a wrapped leaf becomes a result record.
    /// The first rejection in the batch's selection order fails the aggregate.
    ///
    /// `Promise.all` reports the rejection that settled first; Lashlang's own
    /// aggregates report the first one written. Both scan the same per-leaf
    /// rejections, in different orders.
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
                            self.fail_lashlang_execution(active, error.to_string());
                        }
                        leaf_values.push(Value::Null);
                    } else {
                        leaf_values.push(error_value(error.to_string()));
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
    fn await_value(
        &self,
        handle: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + '_>>
    {
        self.await_value_at(handle, String::new())
    }

    fn await_value_at(
        &self,
        handle: Value,
        path: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + '_>>
    {
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
                Value::Record(handles) if is_process_handle(&handles) => Ok(
                    match self
                        .host
                        .perform(AbilityOp::Await(Value::Record(handles)))
                        .await
                    {
                        Ok(AbilityResult::Value(value)) => host_success(value),
                        Ok(AbilityResult::ResourceOperationBatch(_)) => error_value(
                            "await returned a resource operation batch result".to_string(),
                        ),
                        Ok(AbilityResult::Unit) => {
                            error_value("await returned no value".to_string())
                        }
                        Err(error) => error_value(error.to_string()),
                    },
                ),
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

    async fn await_value_unwrap(&self, handle: Value) -> Result<Value, RuntimeError> {
        match handle {
            Value::Record(handles) if is_process_handle(&handles) => self
                .host
                .perform(AbilityOp::Await(Value::Record(handles)))
                .await
                .and_then(|result| result.into_value("await"))
                .map_err(|error| RuntimeError::UnwrappedToolResultFailed {
                    message: error.to_string(),
                }),
            Value::Tuple(_) | Value::List(_) | Value::Record(_) => {
                unwrap_tool_result(self.await_value(handle).await?)
            }
            resolved => Err(RuntimeError::AwaitExpectsHandle {
                found: value_type_name(&resolved).to_string(),
            }),
        }
    }
}

/// One host batch reply after validation: the per-leaf results and the order
/// in which unwrapped rejections are considered.
struct SettledResourceOperationBatch {
    results: Vec<ResourceOperationResult>,
    selection: Vec<usize>,
}

/// Every stack-value position an aggregate shape reads, in written order.
fn collect_value_positions(shape: &CompiledAggregateAwaitShape, positions: &mut Vec<usize>) {
    match shape {
        CompiledAggregateAwaitShape::Comprehension { .. } => {
            unreachable!("batch shape expands before settlement")
        }
        CompiledAggregateAwaitShape::BatchLeaf(_) => {}
        CompiledAggregateAwaitShape::Value(index) => positions.push(*index),
        CompiledAggregateAwaitShape::Tuple(values)
        | CompiledAggregateAwaitShape::List(values)
        | CompiledAggregateAwaitShape::Record { values, .. } => {
            for value in values.iter() {
                collect_value_positions(value, positions);
            }
        }
    }
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

fn process_handle_id_from_value(value: &Value) -> Option<String> {
    let record = value.as_record()?;
    let Value::String(kind) = record.get("__handle__")? else {
        return None;
    };
    if kind.as_str() != "process" {
        return None;
    }
    let Value::String(id) = record.get("id")? else {
        return None;
    };
    Some(id.to_string())
}

/// A host value that clears the value-entry guard becomes a success result; one
/// that does not becomes the same error result a failed ability produces, so
/// the refusal reaches the guest by the route it already handles.
fn host_success(value: Value) -> Value {
    match prototype_chain_data_key_error(&value) {
        Some(error) => error_value(error.to_string()),
        None => success(value),
    }
}
