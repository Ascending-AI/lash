#[derive(Clone)]
struct CallFrame {
    return_ip: usize,
    function: Option<usize>,
    operand_stack_base: usize,
    slots: SlotState,
    iter_stack: Vec<IterState>,
    extras_heapified: bool,
    return_target: ReturnTarget,
}

#[derive(Clone)]
enum ReturnTarget {
    Direct,
    Callback(CallbackDriver),
}

#[derive(Clone)]
struct CallbackDriver {
    function: Value,
    /// Each item is an inline tuple of arguments for one callback invocation.
    calls: Vec<Value>,
    next_index: usize,
    results: Vec<Value>,
    completion: CallbackCompletion,
    allow_effects: bool,
}

#[derive(Clone, Copy)]
enum CallbackCompletion {
    Collect,
    Discard,
}

fn slot_names_for(chunk: &Chunk, active_function: Option<usize>) -> &[Name] {
    active_function
        .and_then(|index| chunk.functions.get(index))
        .map_or(chunk.slot_names.as_slice(), |function| {
            function.slot_names.as_ref()
        })
}

impl<H: ExecutionHost> Vm<'_, H> {
    fn begin_function_call(
        &mut self,
        closure: Value,
        args: Vec<Value>,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        let limit = self.host.execution_bounds().max_frame_depth.get();
        if self.frames.len() as u64 >= limit {
            return Err(RuntimeError::FrameDepthExceeded { limit });
        }
        let Value::Ref(id) = closure else {
            return Err(RuntimeError::NonFunctionCall {
                actual: super::value_type_name(&closure).to_string(),
            });
        };
        let (function_index, captures) = match self.heap.get(id)? {
            HeapObject::Closure { function, captures } => (*function as usize, captures.clone()),
            _ => {
                return Err(RuntimeError::NonFunctionCall {
                    actual: "value".to_string(),
                });
            }
        };
        let function =
            self.chunk
                .functions
                .get(function_index)
                .ok_or(RuntimeError::UnknownFunction {
                    index: function_index as u32,
                })?;
        if args.len() != function.parameter_count {
            return Err(RuntimeError::FunctionArgumentCount {
                expected: function.parameter_count,
                actual: args.len(),
            });
        }
        if captures.len() != function.capture_count {
            return Err(RuntimeError::ClosureCaptureCountMismatch {
                index: function_index as u32,
                expected: function.capture_count,
                actual: captures.len(),
            });
        }

        let mut slots = SlotState {
            values: vec![None; function.slot_names.len()],
            projected: vec![false; function.slot_names.len()],
            extras: Record::new(),
        };
        if let Some(slot) = function.self_slot {
            slots.values[slot] = Some(Value::Ref(id));
        }
        for (slot, value) in function.parameter_slots.iter().copied().zip(args) {
            slots.values[slot] = Some(value);
        }
        for (slot, value) in function.capture_slots.iter().copied().zip(captures) {
            slots.values[slot] = Some(value);
        }
        let frame = CallFrame {
            return_ip: self.ip,
            function: self.active_function,
            operand_stack_base: self.stack.len(),
            slots: std::mem::replace(&mut self.slots, slots),
            iter_stack: std::mem::take(&mut self.iter_stack),
            extras_heapified: self.extras_heapified,
            return_target,
        };
        self.frames.push(frame);
        self.active_function = Some(function_index);
        self.ip = function.entry_ip;
        self.extras_heapified = false;
        Ok(())
    }

    fn return_from_function(&mut self) -> Result<(), RuntimeError> {
        let result = self.pop_stack()?;
        let frame = self.frames.pop().ok_or(RuntimeError::VmStackUnderflow)?;
        self.stack.truncate(frame.operand_stack_base);
        self.slots = frame.slots;
        self.iter_stack = frame.iter_stack;
        self.extras_heapified = frame.extras_heapified;
        self.active_function = frame.function;
        self.ip = frame.return_ip;
        match frame.return_target {
            ReturnTarget::Direct => self.stack.push(result),
            ReturnTarget::Callback(mut callback) => {
                if matches!(callback.completion, CallbackCompletion::Collect) {
                    callback.results.push(self.heap.isolate_value(&result)?);
                }
                if let Some(call) = callback.calls.get(callback.next_index).cloned() {
                    callback.next_index += 1;
                    let function = callback.function.clone();
                    // Each builtin-initiated frame push has the same unit cost
                    // as an explicit `Call` opcode.
                    self.instructions_executed = self.instructions_executed.saturating_add(1);
                    self.begin_function_call(
                        function,
                        callback_arguments(call)?,
                        ReturnTarget::Callback(callback),
                    )?;
                } else {
                    self.stack.push(match callback.completion {
                        CallbackCompletion::Collect => Value::List(callback.results.into()),
                        CallbackCompletion::Discard => Value::Undefined,
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) fn begin_callback_driver(
        &mut self,
        function: Value,
        calls: Vec<Vec<Value>>,
        collect_results: bool,
        allow_effects: bool,
    ) -> Result<(), RuntimeError> {
        let calls = calls
            .into_iter()
            .map(|arguments| Value::Tuple(arguments.into()))
            .collect::<Vec<_>>();
        if calls.is_empty() {
            self.stack.push(if collect_results {
                Value::List(Vec::new().into())
            } else {
                Value::Undefined
            });
            return Ok(());
        }
        let first = callback_arguments(calls[0].clone())?;
        let callback = CallbackDriver {
            function: function.clone(),
            calls,
            next_index: 1,
            results: Vec::new(),
            completion: if collect_results {
                CallbackCompletion::Collect
            } else {
                CallbackCompletion::Discard
            },
            allow_effects,
        };
        self.begin_function_call(function, first, ReturnTarget::Callback(callback))
    }
}

fn callback_arguments(call: Value) -> Result<Vec<Value>, RuntimeError> {
    let Value::Tuple(arguments) = call else {
        return Err(RuntimeError::ValidationFailed {
            reason: "invalid durable callback argument vector".to_string(),
        });
    };
    Ok(arguments.into_vec())
}
