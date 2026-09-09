use super::super::{
    CompiledAggregateAwaitShape, ExecutionHost, RuntimeError, Value, is_runtime_process_handle,
    is_tool_handle_record, record_with_capacity, success, value_contains_tool_handle,
};
use super::Vm;
use std::sync::Arc;

/// How the settled outcome of a process handle inside an aggregate is shaped.
///
/// `Promise.all` unwraps: a failed process rejects the aggregate. Every other
/// aggregate (`Promise.allSettled`, a Lashlang `await [...]`) reports each
/// outcome as the `{ok, value | error}` record the direct `await` of the same
/// handle would produce.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessLeafSettlement {
    Unwrap,
    Result,
}

/// What a value handed to `await` turned out to be, in the terms the repair
/// text has to use.
pub(super) enum AwaitedValue {
    /// A pending-tool handle this execution minted: its request slot.
    LocalToolHandle(usize),
    /// A pending-tool handle record minted by another execution, or written
    /// by hand.
    ForeignToolHandle,
    /// A process handle the host awaits.
    ProcessHandle,
    /// Anything else: an ordinary value that needs no await.
    Plain,
}

impl<H: ExecutionHost> Vm<'_, H> {
    /// Classify `value` for `await`: only a handle stamped with this execution's
    /// nonce reaches a request slot. The nonce is what stops a handle kept in a
    /// session global from aliasing the next execution's first request, and a
    /// literal `{__handle__: "tool", id: 0}` from stealing a live one.
    pub(super) fn classify_awaited(&self, value: &Value) -> AwaitedValue {
        let Value::Record(record) = value else {
            return AwaitedValue::Plain;
        };
        if is_runtime_process_handle(value) {
            return AwaitedValue::ProcessHandle;
        }
        if !is_tool_handle_record(record) {
            return AwaitedValue::Plain;
        }
        let stamped = matches!(
            record.get("execution"),
            Some(Value::String(nonce)) if nonce.as_str() == execution_nonce_text(self.execution_nonce)
        );
        let id = match record.get("id") {
            Some(Value::Number(id)) if id.is_finite() && *id >= 0.0 && id.fract() == 0.0 => {
                Some(*id as usize)
            }
            _ => None,
        };
        match id {
            Some(id) if stamped && id < self.pending_tools.len() => {
                AwaitedValue::LocalToolHandle(id)
            }
            _ => AwaitedValue::ForeignToolHandle,
        }
    }

    pub(super) fn create_pending_tool(
        &mut self,
        operation: usize,
        argc: usize,
    ) -> Result<(), RuntimeError> {
        let (receiver, args) = self.drain_receiver_call(argc)?;
        ensure_no_tool_handle_arguments(&args)?;
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
        let mut handle = record_with_capacity(3);
        handle.insert("__handle__".to_string(), Value::String("tool".into()));
        handle.insert("id".to_string(), Value::Number(id as f64));
        handle.insert(
            "execution".to_string(),
            Value::String(execution_nonce_text(self.execution_nonce).into()),
        );
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

    /// Settle an array of handles and values in two phases: every pending tool
    /// handle as one host batch (recorded settlement order decides which
    /// rejection `Promise.all` reports), then every process handle in array
    /// order through the host's process-await seam. A tool rejection therefore
    /// always wins over a process failure; among processes the first written
    /// wins (ADR 0086).
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
            match self.classify_awaited(item) {
                AwaitedValue::LocalToolHandle(id) => {
                    if let Some(index) = seen.get(&id) {
                        shape.push(CompiledAggregateAwaitShape::BatchLeaf(*index));
                        continue;
                    }
                    let Some(Some(Value::List(call))) = self.pending_tools.get_mut(id) else {
                        return Err(RuntimeError::PendingTool {
                            problem: SETTLED_HANDLE.into(),
                        });
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
                }
                AwaitedValue::ForeignToolHandle => {
                    return Err(RuntimeError::PendingTool {
                        problem: FOREIGN_HANDLE.into(),
                    });
                }
                // Phase two settles this one after the batch; the raw handle
                // holds its place in the stack values until then.
                AwaitedValue::ProcessHandle => {
                    shape.push(CompiledAggregateAwaitShape::Value(values.len()));
                    values.push(item.clone());
                }
                AwaitedValue::Plain => {
                    shape.push(CompiledAggregateAwaitShape::Value(values.len()));
                    values.push(if settle {
                        success(item.clone())
                    } else {
                        item.clone()
                    });
                }
            }
        }
        for id in seen.keys() {
            self.pending_tools[*id] = None;
        }
        let process_leaves = if settle {
            ProcessLeafSettlement::Result
        } else {
            ProcessLeafSettlement::Unwrap
        };
        let batch = CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape: CompiledAggregateAwaitShape::List(shape.into_boxed_slice()),
            stack_value_count: values.len(),
            aggregate_unwrap: false,
            first_settled_rejection: !settle,
        };
        self.resolve_batch_spec(&batch, values, process_leaves)
            .await
    }
}

/// The nonce as the handle record spells it.
pub(super) fn execution_nonce_text(nonce: u64) -> String {
    format!("{nonce:016x}")
}

/// A pending handle inside a tool's arguments would reach the host as its
/// marker record and the call would run with it; refused before dispatch so the
/// side effect never happens.
pub(super) fn ensure_no_tool_handle_arguments(args: &[Value]) -> Result<(), RuntimeError> {
    if args.iter().any(value_contains_tool_handle) {
        return Err(RuntimeError::PendingTool {
            problem: HANDLE_AS_ARGUMENT.into(),
        });
    }
    Ok(())
}

pub(super) const SETTLED_HANDLE: &str =
    "this tool handle was already awaited; await each tool call once and reuse its value";
pub(super) const FOREIGN_HANDLE: &str = "this tool handle was not minted by this execution (it is stale or hand-written); call the tool in this cell and await that call";
pub(super) const HANDLE_AS_ARGUMENT: &str = "a pending tool handle was passed as a tool argument; await it first and pass the awaited value";

pub(super) fn plain_value_awaited(value: &Value) -> String {
    format!(
        "`await` received a plain {} value, not a pending tool call; use the value directly",
        super::super::ops::value_type_name(value)
    )
}
