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
    Map(MapCallback),
}

#[derive(Clone)]
struct MapCallback {
    function: Value,
    items: Vec<Value>,
    next_index: usize,
    results: Vec<Value>,
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
            ReturnTarget::Map(mut callback) => {
                callback.results.push(self.heap.isolate_value(&result)?);
                if let Some(item) = callback.items.get(callback.next_index).cloned() {
                    callback.next_index += 1;
                    let item = self.heap.isolate_value(&item)?;
                    let function = callback.function.clone();
                    // Each builtin-initiated frame push has the same unit cost
                    // as an explicit `Call` opcode.
                    self.instructions_executed = self.instructions_executed.saturating_add(1);
                    self.begin_function_call(function, vec![item], ReturnTarget::Map(callback))?;
                } else {
                    self.stack.push(Value::List(callback.results.into()));
                }
            }
        }
        Ok(())
    }
}
