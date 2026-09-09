use super::super::{
    CompiledAggregateAwaitShape, ExecutionHost, RuntimeError, Value, record_with_capacity, success,
};
use super::Vm;
use std::sync::Arc;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn create_pending_tool(
        &mut self,
        operation: usize,
        argc: usize,
    ) -> Result<(), RuntimeError> {
        let (receiver, args) = self.drain_receiver_call(argc)?;
        let id = self.pending_tools.len();
        self.pending_tools.push(Some(Value::List(
            [
                Value::Number(operation as f64),
                Value::Number(self.current_instruction_ip() as f64),
            ]
            .into_iter()
            .chain(std::iter::once(receiver))
            .chain(args)
            .collect(),
        )));
        let mut handle = record_with_capacity(2);
        handle.insert("__handle__".to_string(), Value::String("tool".into()));
        handle.insert("id".to_string(), Value::Number(id as f64));
        self.stack.push(Value::Record(Arc::new(handle)));
        Ok(())
    }

    pub(super) fn ensure_no_pending_tools(&self) -> Result<(), RuntimeError> {
        let count = self
            .pending_tools
            .iter()
            .filter(|entry| entry.is_some())
            .count();
        if count == 0 {
            Ok(())
        } else {
            Err(RuntimeError::PendingTool {
                problem: format!("{count} tool handle(s) were never awaited before cell end"),
            })
        }
    }

    pub(super) async fn await_pending_array(&mut self, settle: bool) -> Result<(), RuntimeError> {
        use super::super::{CompiledResourceOperationBatch, CompiledResourceOperationBatchLeaf};
        let Value::List(items) = self.pop_stack()? else {
            return Err(RuntimeError::PendingTool {
                problem: "Promise aggregate requires an array".into(),
            });
        };
        let mut values = Vec::new();
        let mut leaves = Vec::new();
        let mut shape = Vec::new();
        let mut seen = std::collections::BTreeMap::new();
        for item in items.iter() {
            if let Some(id) = pending_tool_id(item) {
                if let Some(index) = seen.get(&id) {
                    shape.push(CompiledAggregateAwaitShape::BatchLeaf(*index));
                    continue;
                }
                let Some(Some(Value::List(call))) = self.pending_tools.get_mut(id) else {
                    return Err(RuntimeError::PendingTool { problem: "await requires a pending handle; this handle is already settled or unknown".into() });
                };
                let Value::Number(operation) = call[0] else {
                    unreachable!()
                };
                let Value::Number(site) = call[1] else {
                    unreachable!()
                };
                let site = site as usize;
                let index = leaves.len();
                seen.insert(id, index);
                leaves.push(CompiledResourceOperationBatchLeaf {
                    operation: operation as usize,
                    argc: call.len() - 3,
                    receiver_stack_index: values.len(),
                    unwrap: !settle,
                    site: self
                        .chunk
                        .lashlang_execution_sites
                        .get(site)
                        .cloned()
                        .flatten(),
                    source_span: self.chunk.spans.get(site).copied().flatten(),
                });
                values.extend(call[2..].iter().cloned());
                shape.push(CompiledAggregateAwaitShape::BatchLeaf(index));
            } else {
                if matches!(item, Value::Record(record) if super::super::is_process_handle(record))
                {
                    return Err(RuntimeError::PendingTool {
                        problem:
                            "await process handles separately before aggregating their results"
                                .into(),
                    });
                }
                shape.push(CompiledAggregateAwaitShape::Value(values.len()));
                values.push(if settle {
                    success(item.clone())
                } else {
                    item.clone()
                });
            }
        }
        for id in seen.keys() {
            self.pending_tools[*id] = None;
        }
        if leaves.is_empty() {
            self.stack.push(Value::List(values.into()));
            return Ok(());
        }
        let batch = CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape: CompiledAggregateAwaitShape::List(shape.into_boxed_slice()),
            stack_value_count: values.len(),
            aggregate_unwrap: false,
            first_settled_rejection: !settle,
        };
        self.resolve_batch_spec(&batch, values).await
    }
}

pub(super) fn pending_tool_id(value: &Value) -> Option<usize> {
    let Value::Record(record) = value else {
        return None;
    };
    if !matches!(record.get("__handle__"), Some(Value::String(kind)) if kind.as_str() == "tool") {
        return None;
    }
    let Value::Number(id) = record.get("id")? else {
        return None;
    };
    (id.is_finite() && *id >= 0.0 && id.fract() == 0.0).then_some(*id as usize)
}
