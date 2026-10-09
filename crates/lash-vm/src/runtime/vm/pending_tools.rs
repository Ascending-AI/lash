use super::super::{
    AggregateConsumer, CompiledAggregateAwaitShape, ExecutionHost, RuntimeError, Value,
    parse_handle_record, record_with_capacity, success, value_contains_tool_handle,
};
use super::{Instruction, PendingOperation, Vm};
use lash_sansio::handle::{HANDLE_FIELD, HANDLE_KIND, HandleId, HandleTarget};
use std::sync::Arc;

/// What a value handed to `await` turned out to be.
///
/// There is one handle kind (ADR 0095), so there is one question to ask of an
/// awaited value: is it a handle record at all. Whether the id it carries names
/// a live request of this execution is asked separately, by
/// [`Vm::unsettleable_handle`], because that is where the three repair texts
/// differ and collapsing them into the classification would spread the same
/// decision over two places again.
pub(super) enum AwaitedValue {
    /// A handle record: the id it names.
    Leaf(HandleId),
    /// Anything else: an ordinary value that needs no await.
    Plain,
}

/// Whether a handle id names a process rather than a tool request of this
/// execution. A process handle settles through the durable process-await seam,
/// never through the pending-tool batch.
pub(super) fn is_runtime_process_handle_id(id: &HandleId) -> bool {
    matches!(id.target(), Some(HandleTarget::Process { .. }))
}

impl<H: ExecutionHost> Vm<'_, H> {
    /// Classify `value` for `await`: a handle record yields the id it carries,
    /// everything else is a plain value.
    pub(super) fn classify_awaited(&self, value: &Value) -> AwaitedValue {
        let Value::Record(record) = value else {
            return AwaitedValue::Plain;
        };
        match parse_handle_record(record) {
            Some(handle) => AwaitedValue::Leaf(handle),
            None => AwaitedValue::Plain,
        }
    }

    /// The live pending request `id` names, if it names one.
    ///
    /// Only a handle whose id carries this execution's nonce reaches a request
    /// slot. The nonce is folded into the id rather than stamped beside it, so
    /// there is one thing to check and nothing to keep in step. It is what stops
    /// a handle kept in a session global from aliasing the next execution's
    /// first request, and a literal `{__handle__: "lash", id: "t.0000000000000000.0"}`
    /// from stealing a live one.
    pub(super) fn live_pending_request(&self, id: &HandleId) -> bool {
        matches!(
            id.target(),
            Some(HandleTarget::Tool {
                execution_nonce,
                request: _,
            }) if execution_nonce == self.execution_nonce
        ) && matches!(self.pending_tools.get(id), Some(Some(_)))
    }

    /// Three different mistakes reach here and each has its own repair: a
    /// process handle that was written where a tool call belongs, a handle this
    /// execution minted and already awaited, and a handle from somewhere else
    /// entirely.
    pub(super) fn unsettleable_handle(&self, id: &HandleId) -> RuntimeError {
        let problem = match id.target() {
            Some(HandleTarget::Process { .. }) => PROCESS_HANDLE_LEAF,
            Some(HandleTarget::Tool {
                execution_nonce,
                request: _,
            }) if execution_nonce == self.execution_nonce
                && self.pending_tools.contains_key(id) =>
            {
                SETTLED_HANDLE
            }
            _ => FOREIGN_HANDLE,
        };
        RuntimeError::PendingTool {
            problem: problem.into(),
            pending: Vec::new(),
        }
    }

    pub(super) fn create_pending_tool(&mut self, argc: usize) -> Result<(), RuntimeError> {
        let (receiver, args) = self.drain_receiver_call(argc)?;
        ensure_no_tool_handle_arguments(&args)?;
        // Consumed requests stay in the map as `None`, so the entry count is
        // the next request number and a settled handle stays tellable from a
        // foreign one.
        let request =
            u32::try_from(self.pending_tools.len()).map_err(|_| RuntimeError::PendingTool {
                problem: "this cell launched more tool calls than one execution can hold".into(),
                pending: Vec::new(),
            })?;
        let id = HandleId::tool(self.execution_nonce, request);
        let site = self.current_instruction_ip();
        let occurrence = self.mint_pending_occurrence(site);
        self.pending_tools.insert(
            id.clone(),
            Some(PendingOperation::Tool {
                site,
                occurrence,
                receiver,
                args,
            }),
        );
        let mut handle = record_with_capacity(2);
        handle.insert(HANDLE_FIELD.to_string(), Value::String(HANDLE_KIND.into()));
        handle.insert("id".to_string(), Value::String(id.as_str().into()));
        self.stack.push(Value::Record(Arc::new(handle)));
        Ok(())
    }

    pub(super) fn ensure_no_pending_tools(&self) -> Result<(), RuntimeError> {
        let pending = self
            .pending_tools
            .values()
            .flatten()
            .map(|entry| {
                let site = entry.site();
                let call_path = match entry {
                    PendingOperation::Tool { receiver, .. } => {
                        let Some(Instruction::PendingTool { operation, .. }) =
                            self.chunk.code.get(site)
                        else {
                            return Err(RuntimeError::PendingTool {
                                problem: "pending tool does not match its instruction".into(),
                                pending: Vec::new(),
                            });
                        };
                        let operation = &self.chunk.names[*operation].text;
                        match receiver {
                            Value::Resource(receiver) => format!("{}.{operation}", receiver.alias),
                            _ => operation.to_string(),
                        }
                    }
                    PendingOperation::Timer { .. } => "sleep".to_string(),
                };
                Ok(super::super::UnawaitedToolCall {
                    call_path,
                    span: self.chunk.spans.get(site).copied().flatten(),
                })
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        if pending.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::PendingTool {
                problem: format!(
                    "{} tool handle(s) were never awaited before cell end",
                    pending.len()
                ),
                pending,
            })
        }
    }

    /// Mint a pending timer handle for an unawaited `sleep(ms)` (ADR 0099 §11).
    ///
    /// A timer is a pending operation like a tool call and shares its one
    /// handle encoding (clause 1: "One pending-operation handle for tools and
    /// timers"). Its entry captures the site and duration. The duration is only
    /// recorded here — the timer's start point is its **admission**, when the
    /// aggregate that awaits it is formed and the host records its deadline.
    /// What the entry does fix here is which occurrence of its site the timer
    /// is, and in which loops.
    pub(super) fn create_pending_timer(&mut self) -> Result<(), RuntimeError> {
        let duration = self.pop_stack()?;
        let request =
            u32::try_from(self.pending_tools.len()).map_err(|_| RuntimeError::PendingTool {
                problem: "this cell launched more pending operations than one execution can hold"
                    .into(),
                pending: Vec::new(),
            })?;
        let id = HandleId::tool(self.execution_nonce, request);
        let site = self.current_instruction_ip();
        let occurrence = self.mint_pending_occurrence(site);
        self.pending_tools.insert(
            id.clone(),
            Some(PendingOperation::Timer {
                site,
                occurrence,
                duration,
            }),
        );
        let mut handle = record_with_capacity(2);
        handle.insert(HANDLE_FIELD.to_string(), Value::String(HANDLE_KIND.into()));
        handle.insert("id".to_string(), Value::String(id.as_str().into()));
        self.stack.push(Value::Record(Arc::new(handle)));
        Ok(())
    }

    /// Settle an awaited array as **one** resource-operation batch under
    /// `consumer` (ADR 0099 §10, §11).
    ///
    /// Every leaf that has to settle is a pending operation — a tool call or a
    /// timer — so the host consumes one durable settlement order over all of
    /// them and that order is authoritative: it decides which settlement a
    /// `race` resolves with, which fulfilment an `any` resolves with, and which
    /// rejection an `all` reports. A durable process wait is a leaf like any
    /// other, because `processes.await` is a tool that parks on it (ADR 0095,
    /// ADR 0099 §12).
    ///
    /// A handle written at two positions is one leaf: execution deduplicates,
    /// positions never do (§10 L4, §11 clause 1). Operands were evaluated once,
    /// in source order, before this runs (clause 2).
    ///
    /// A host that parks the run on the batch (FIG-4275) settles none of its
    /// requests: each consumed request is live again, so the batch the resumed
    /// run issues is the same one.
    pub(super) async fn await_pending_array(
        &mut self,
        items: &Value,
        consumer: AggregateConsumer,
        instruction_ip: usize,
        reissued: bool,
    ) -> Result<super::effects::Awaited<Value>, RuntimeError> {
        use super::super::{CompiledResourceOperationBatch, CompiledResourceOperationBatchLeaf};
        let Value::List(items) = items else {
            return Err(RuntimeError::PendingTool {
                problem: "Promise aggregate requires an array".into(),
                pending: Vec::new(),
            });
        };
        let settle = consumer == AggregateConsumer::AllSettled;
        let mut values = Vec::new();
        let mut leaves = Vec::new();
        let mut shape = Vec::new();
        let mut seen = std::collections::BTreeMap::new();
        for item in items.iter() {
            match self.classify_awaited(item) {
                AwaitedValue::Leaf(id) => {
                    if let Some(index) = seen.get(&id) {
                        shape.push(CompiledAggregateAwaitShape::BatchLeaf(*index));
                        continue;
                    }
                    if !self.live_pending_request(&id) {
                        return Err(self.unsettleable_handle(&id));
                    }
                    let Some(Some(call)) = self.pending_tools.get(&id) else {
                        return Err(RuntimeError::PendingTool {
                            problem: SETTLED_HANDLE.into(),
                            pending: Vec::new(),
                        });
                    };
                    let site = call.site();
                    let index = leaves.len();
                    seen.insert(id.clone(), index);
                    let (timer, operation, argc) = match call {
                        PendingOperation::Tool { .. } => {
                            let Some(Instruction::PendingTool { operation, argc }) =
                                self.chunk.code.get(site)
                            else {
                                return Err(RuntimeError::PendingTool {
                                    problem: "pending tool does not match its instruction".into(),
                                    pending: Vec::new(),
                                });
                            };
                            (false, *operation, *argc)
                        }
                        PendingOperation::Timer { .. } => (true, 0, 0),
                    };
                    leaves.push(CompiledResourceOperationBatchLeaf {
                        timer,
                        operation,
                        argc,
                        receiver_stack_index: values.len(),
                        unwrap: !settle,
                        site: self
                            .chunk
                            .lash_vm_execution_sites
                            .get(site)
                            .cloned()
                            .flatten(),
                        minted: call.occurrence().cloned(),
                        source_span: self.chunk.spans.get(site).copied().flatten(),
                    });
                    values.extend(call.values().cloned());
                    shape.push(CompiledAggregateAwaitShape::BatchLeaf(index));
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
        let mut consumed = Vec::with_capacity(seen.len());
        for id in seen.keys() {
            // Consumed, not removed: the entry is what tells a second await of
            // the same handle from a handle this execution never minted.
            if let Some(entry) = self.pending_tools.get_mut(id)
                && let Some(request) = entry.take()
            {
                consumed.push((id.clone(), request));
            }
        }
        let batch = CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape: CompiledAggregateAwaitShape::List(shape.into_boxed_slice()),
            stack_value_count: values.len(),
            aggregate_unwrap: false,
            consumer,
        };
        let settled = self
            .resolve_batch_spec(&batch, values, instruction_ip, reissued)
            .await?;
        if let super::effects::Awaited::Parked(()) = settled {
            for (id, request) in consumed {
                self.pending_tools.insert(id, Some(request));
            }
        }
        Ok(settled)
    }
}

/// A pending handle inside a tool's arguments would reach the host as its
/// marker record and the call would run with it; refused before dispatch so the
/// side effect never happens.
pub(super) fn ensure_no_tool_handle_arguments(args: &[Value]) -> Result<(), RuntimeError> {
    if args.iter().any(value_contains_tool_handle) {
        return Err(RuntimeError::PendingTool {
            problem: HANDLE_AS_ARGUMENT.into(),
            pending: Vec::new(),
        });
    }
    Ok(())
}

pub(super) const SETTLED_HANDLE: &str =
    "this tool handle was already awaited; await each tool call once and reuse its value";
pub(super) const FOREIGN_HANDLE: &str = "this tool handle was not minted by this execution (it is stale or hand-written); call the tool in this cell and await that call";
pub(super) const PROCESS_HANDLE_LEAF: &str = "a process handle cannot be awaited directly; call `processes.await(handle)` and await that call, so the durable wait settles with the rest of the batch";
pub(super) const HANDLE_AS_ARGUMENT: &str = "a pending tool handle was passed as a tool argument; await it first and pass the awaited value";

pub(super) fn plain_value_awaited(value: &Value) -> String {
    format!(
        "`await` received a plain {} value, not a pending tool call; use the value directly",
        super::super::ops::value_type_name(value)
    )
}
