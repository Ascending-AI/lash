use std::sync::Arc;

use crate::{LashlangExecutionCallSite, LashlangExecutionChild};

use super::super::access::prototype_chain_data_key_error;
use super::super::host::{
    AbilityOp, AbilityResult, ProcessEvent, ProcessEventKind, ProcessSignal, ProcessStart,
    ResourceOperation, ResourceOperationBatch, ResourceOperationResult, Sleep, SleepKind,
};
use super::super::{
    CompiledAggregateAwaitShape, CompiledResourceOperationBatch,
    CompiledResourceOperationBatchLeaf, ExecutionHost, RuntimeError, Value, error_value,
    is_process_handle, record_with_capacity, success, unwrap_tool_result, value_type_name,
};
use super::control::VmOutcome;
use super::{ActiveLashlangExecutionNode, Vm};

#[derive(Clone, Copy)]
pub(super) enum VmEffect {
    ResourceCall { operation: usize, argc: usize },
    ResourceCallUnwrap { operation: usize, argc: usize },
    AwaitArray { settle: bool },
    AwaitPending,
    ResourceOperationBatch(usize),
    ResourceOperationComprehensionBatch(usize),
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
                if super::pending_tools::pending_tool_id(&value).is_none() {
                    return Err(RuntimeError::PendingTool {
                        problem: "await requires a pending handle; this value is already settled"
                            .into(),
                    });
                }
                self.stack.push(Value::List(vec![value].into()));
                self.await_pending_array(false).await?;
                let Value::List(values) = self.pop_stack()? else {
                    unreachable!()
                };
                self.stack.push(values[0].clone());
            }
            VmEffect::ResourceOperationBatch(batch) => {
                self.resolve_resource_operation_batch(batch).await?;
            }
            VmEffect::ResourceOperationComprehensionBatch(batch) => {
                self.resolve_resource_operation_comprehension_batch(batch)
                    .await?;
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
                let result = self.await_value(handle, String::new()).await?;
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
        let batch = self.chunk.resource_operation_batches[batch].clone();
        let start = self.stack_drain_start(batch.stack_value_count)?;
        let values = self.stack.drain(start..).collect::<Vec<_>>();
        self.resolve_batch_spec(&batch, values).await
    }

    /// Settles a comprehension of operation calls as one batch. The loop left a
    /// list of packed tuples on the stack, one per element, each holding the
    /// receiver and argument values the per-element template expects; this
    /// flattens them into a single batch whose shape is a list of the template
    /// shape, offset per element, and hands it to the same settlement path the
    /// literal aggregate uses.
    async fn resolve_resource_operation_comprehension_batch(
        &mut self,
        batch: usize,
    ) -> Result<(), RuntimeError> {
        let template = self.chunk.resource_operation_batches[batch].clone();
        let Value::List(elements) = self.pop_stack()? else {
            return Err(RuntimeError::InvalidResourceComprehensionElement);
        };
        let mut values = Vec::with_capacity(elements.len() * template.stack_value_count);
        let mut leaves = Vec::with_capacity(elements.len() * template.leaves.len());
        let mut shapes = Vec::with_capacity(elements.len());
        for element in elements.iter() {
            let Value::Tuple(packed) = element else {
                return Err(RuntimeError::InvalidResourceComprehensionElement);
            };
            if packed.len() != template.stack_value_count {
                return Err(RuntimeError::InvalidResourceComprehensionElement);
            }
            let value_offset = values.len();
            let leaf_offset = leaves.len();
            values.extend(packed.iter().cloned());
            leaves.extend(
                template
                    .leaves
                    .iter()
                    .map(|leaf| CompiledResourceOperationBatchLeaf {
                        receiver_stack_index: leaf.receiver_stack_index + value_offset,
                        ..leaf.clone()
                    }),
            );
            shapes.push(offset_aggregate_await_shape(
                &template.shape,
                leaf_offset,
                value_offset,
            ));
        }
        let batch = CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape: CompiledAggregateAwaitShape::List(shapes.into_boxed_slice()),
            stack_value_count: values.len(),
            aggregate_unwrap: template.aggregate_unwrap,
            first_settled_rejection: template.first_settled_rejection,
        };
        if batch.leaves.is_empty() {
            // Nothing to settle: an empty comprehension is an empty list, and
            // the host is never asked to run a batch of zero operations.
            let mut value = Value::List(Vec::new().into());
            if batch.aggregate_unwrap {
                value = unwrap_tool_result(value)?;
            }
            self.stack.push(value);
            return Ok(());
        }
        self.resolve_batch_spec(&batch, values).await
    }

    pub(super) async fn resolve_batch_spec(
        &mut self,
        batch: &super::super::CompiledResourceOperationBatch,
        values: Vec<Value>,
    ) -> Result<(), RuntimeError> {
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

        let result = self
            .host
            .perform(AbilityOp::ResourceOperationBatch(ResourceOperationBatch {
                operations,
            }))
            .await;
        let result = match result {
            Ok(AbilityResult::ResourceOperationBatch(result)) => result,
            Ok(AbilityResult::Value(_)) | Ok(AbilityResult::Unit) => {
                let error = RuntimeError::InvalidResourceBatchResult;
                for active in active_nodes.iter().flatten() {
                    self.fail_lashlang_execution(active, error.to_string());
                }
                return Err(error);
            }
            Err(error) => {
                let runtime_error = RuntimeError::ResourceBatchFailed { source: error };
                for active in active_nodes.iter().flatten() {
                    self.fail_lashlang_execution(active, runtime_error.to_string());
                }
                return Err(runtime_error);
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
            for active in active_nodes.iter().flatten() {
                self.fail_lashlang_execution(active, rejection.to_string());
            }
            return Err(rejection);
        }
        if result.results.len() != batch.leaves.len() {
            let error = RuntimeError::ResourceBatchResultCount {
                actual: result.results.len(),
                expected: batch.leaves.len(),
            };
            for active in active_nodes.iter().flatten() {
                self.fail_lashlang_execution(active, error.to_string());
            }
            return Err(error);
        }
        // A batch that selects by settlement order needs a usable order. A host
        // that reports a malformed one fails closed rather than being silently
        // read as input order, which is the behaviour this replaces.
        let settlement_order = if batch.first_settled_rejection {
            let order = match result.settlement_sequence() {
                Ok(order) => order,
                Err(problem) => {
                    let error = RuntimeError::ResourceBatchSettlementOrder { problem };
                    for active in active_nodes.iter().flatten() {
                        self.fail_lashlang_execution(active, error.to_string());
                    }
                    return Err(error);
                }
            };
            Some(order.to_vec())
        } else {
            None
        };

        let mut unwrapped_errors = vec![None; batch.leaves.len()];
        let mut leaf_values = Vec::with_capacity(batch.leaves.len());
        for (leaf_index, ((leaf, result), active)) in batch
            .leaves
            .iter()
            .zip(result.results)
            .zip(active_nodes.iter())
            .enumerate()
        {
            match result {
                ResourceOperationResult::Value(value) => {
                    leaf_values.push(if leaf.unwrap { value } else { success(value) });
                    if let Some(active) = active {
                        self.complete_lashlang_execution(active);
                    }
                }
                ResourceOperationResult::Error(error) => {
                    if leaf.unwrap {
                        unwrapped_errors[leaf_index] = Some((error.clone(), leaf.source_span));
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

        // `Promise.all` reports the rejection that settled first; Lashlang's own
        // aggregates report the first one written. Both scan the same per-leaf
        // rejections, in different orders.
        let selection: Box<dyn Iterator<Item = usize>> = match &settlement_order {
            Some(order) => Box::new(order.iter().copied()),
            None => Box::new(0..unwrapped_errors.len()),
        };
        let mut selected = None;
        for index in selection {
            if let Some(error) = unwrapped_errors.get_mut(index).and_then(Option::take) {
                selected = Some(error);
                break;
            }
        }

        if let Some((source, span)) = selected {
            self.pending_error_span = span;
            return Err(RuntimeError::UnwrappedModuleOperationFailed { source });
        }

        let mut value = build_aggregate_await_shape(&batch.shape, &values, &leaf_values, self)?;
        if batch.aggregate_unwrap {
            value = unwrap_tool_result(value)?;
        }
        self.stack.push(value);
        Ok(())
    }

    /// Awaits a handle, or a list/tuple/record whose leaves are handles, by
    /// asking the host for each handle in written order. A value that is
    /// already settled - a scalar, or a collection holding no handle at all -
    /// is a typed error rather than a silent error record: the host is never
    /// asked to await something that cannot be awaited. `path` names the leaf
    /// inside the awaited value for that diagnostic (empty at the root).
    fn await_value(
        &self,
        handle: Value,
        path: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, RuntimeError>> + Send + '_>>
    {
        Box::pin(async move {
            match handle {
                Value::Tuple(handles) => {
                    reject_settled_aggregate(&handles, "tuple", &path)?;
                    let mut values = Vec::with_capacity(handles.len());
                    for (index, handle) in handles.iter().cloned().enumerate() {
                        values.push(self.await_value(handle, format!("{path}[{index}]")).await?);
                    }
                    Ok(Value::Tuple(values.into()))
                }
                Value::List(handles) => {
                    reject_settled_aggregate(&handles, "list", &path)?;
                    let mut values = Vec::with_capacity(handles.len());
                    for (index, handle) in handles.iter().cloned().enumerate() {
                        values.push(self.await_value(handle, format!("{path}[{index}]")).await?);
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
                    if !value_contains_handle(&Value::Record(handles.clone())) {
                        return Err(awaited_settled_value("record", &path));
                    }
                    let mut record = record_with_capacity(handles.len());
                    for entry in handles.entries.iter() {
                        record.insert_symbolized(
                            entry.symbol,
                            entry.name.clone(),
                            self.await_value(entry.value.clone(), format!("{path}.{}", entry.name))
                                .await?,
                        );
                    }
                    Ok(Value::Record(Arc::new(record)))
                }
                handle => Err(awaited_settled_value(value_type_name(&handle), &path)),
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
                unwrap_tool_result(self.await_value(handle, String::new()).await?)
            }
            handle => Err(awaited_settled_value(value_type_name(&handle), "")),
        }
    }
}

/// A collection with no handle anywhere inside it is a settled value
/// that `await` must refuse, naming the collection rather than its first leaf.
fn reject_settled_aggregate(
    items: &[Value],
    actual: &'static str,
    path: &str,
) -> Result<(), RuntimeError> {
    if !items.iter().any(value_contains_handle) {
        return Err(awaited_settled_value(actual, path));
    }
    Ok(())
}

fn value_contains_handle(value: &Value) -> bool {
    match value {
        Value::Record(record) if is_process_handle(record) => true,
        Value::Record(record) => record
            .entries
            .iter()
            .any(|entry| value_contains_handle(&entry.value)),
        Value::Tuple(items) | Value::List(items) => items.iter().any(value_contains_handle),
        _ => false,
    }
}

fn awaited_settled_value(actual: &str, path: &str) -> RuntimeError {
    RuntimeError::AwaitedSettledValue {
        actual: actual.to_string(),
        path: if path.is_empty() {
            String::new()
        } else {
            format!(" at `{path}`")
        },
    }
}

/// Re-indexes a per-element template shape into the flattened comprehension
/// batch: leaf indexes shift by the leaves settled before this element, value
/// indexes by the packed values before it.
fn offset_aggregate_await_shape(
    shape: &CompiledAggregateAwaitShape,
    leaf_offset: usize,
    value_offset: usize,
) -> CompiledAggregateAwaitShape {
    match shape {
        CompiledAggregateAwaitShape::BatchLeaf(index) => {
            CompiledAggregateAwaitShape::BatchLeaf(index + leaf_offset)
        }
        CompiledAggregateAwaitShape::Value(index) => {
            CompiledAggregateAwaitShape::Value(index + value_offset)
        }
        CompiledAggregateAwaitShape::Tuple(values) => CompiledAggregateAwaitShape::Tuple(
            values
                .iter()
                .map(|value| offset_aggregate_await_shape(value, leaf_offset, value_offset))
                .collect(),
        ),
        CompiledAggregateAwaitShape::List(values) => CompiledAggregateAwaitShape::List(
            values
                .iter()
                .map(|value| offset_aggregate_await_shape(value, leaf_offset, value_offset))
                .collect(),
        ),
        CompiledAggregateAwaitShape::Record { keys, values } => {
            CompiledAggregateAwaitShape::Record {
                keys: *keys,
                values: values
                    .iter()
                    .map(|value| offset_aggregate_await_shape(value, leaf_offset, value_offset))
                    .collect(),
            }
        }
    }
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
