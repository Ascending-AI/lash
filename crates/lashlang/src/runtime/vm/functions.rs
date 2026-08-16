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
    /// `calls[0]` is the rooted URLSearchParams receiver and `next_index` is
    /// the next live list index. The list is re-read after every callback so
    /// appends and deletions follow WHATWG iteration semantics.
    live_url_search_params: bool,
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
    pub(super) fn map_set_live(
        &mut self,
        receiver: HeapId,
        key: &Value,
        value: &Value,
    ) -> Result<(), RuntimeError> {
        let existed = self.heap.map_has(receiver, key)?;
        self.heap.map_set(receiver, key.clone(), value.clone())?;
        let (stored_key, stored_value) = self
            .heap
            .map_entries(receiver)?
            .expect("Map receiver was checked")
            .into_iter()
            .find(|(candidate, _)| same_value_zero(candidate, key))
            .expect("Map.set stored the key");
        self.map_for_each_set(receiver, stored_key, stored_value, existed);
        Ok(())
    }

    pub(super) fn map_delete_live(
        &mut self,
        receiver: HeapId,
        key: &Value,
    ) -> Result<bool, RuntimeError> {
        let deleted = self.heap.map_delete(receiver, key)?;
        if deleted {
            self.map_for_each_delete(receiver, key);
        }
        Ok(deleted)
    }

    pub(super) fn set_add_live(
        &mut self,
        receiver: HeapId,
        value: &Value,
    ) -> Result<(), RuntimeError> {
        let existed = self.heap.set_has(receiver, value)?;
        self.heap.set_add(receiver, value.clone())?;
        let stored = self
            .heap
            .set_values(receiver)?
            .expect("Set receiver was checked")
            .into_iter()
            .find(|candidate| same_value_zero(candidate, value))
            .expect("Set.add stored the value");
        self.set_for_each_add(receiver, stored, existed);
        Ok(())
    }

    pub(super) fn set_delete_live(
        &mut self,
        receiver: HeapId,
        value: &Value,
    ) -> Result<bool, RuntimeError> {
        let deleted = self.heap.set_delete(receiver, value)?;
        if deleted {
            self.set_for_each_delete(receiver, value);
        }
        Ok(deleted)
    }

    pub(super) fn map_for_each_set(
        &mut self,
        receiver: HeapId,
        key: Value,
        value: Value,
        existed: bool,
    ) {
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            if existed {
                if let Some(call) = callback.calls[callback.next_index..]
                    .iter_mut()
                    .find(|call| callback_argument_matches(call, 1, &key))
                {
                    *call = collection_callback(vec![value.clone(), key.clone()], receiver);
                }
            } else {
                callback.calls.push(collection_callback(
                    vec![value.clone(), key.clone()],
                    receiver,
                ));
            }
        }
    }

    pub(super) fn map_for_each_delete(&mut self, receiver: HeapId, key: &Value) {
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            retain_pending_calls(callback, |call| !callback_argument_matches(call, 1, key));
        }
    }

    pub(super) fn map_for_each_clear(&mut self, receiver: HeapId) {
        clear_pending_calls(&mut self.frames, receiver);
    }

    pub(super) fn set_for_each_add(&mut self, receiver: HeapId, value: Value, existed: bool) {
        if existed {
            return;
        }
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            callback.calls.push(collection_callback(
                vec![value.clone(), value.clone()],
                receiver,
            ));
        }
    }

    pub(super) fn set_for_each_delete(&mut self, receiver: HeapId, value: &Value) {
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            retain_pending_calls(callback, |call| !callback_argument_matches(call, 0, value));
        }
    }

    pub(super) fn set_for_each_clear(&mut self, receiver: HeapId) {
        clear_pending_calls(&mut self.frames, receiver);
    }

    pub(super) fn begin_direct_function_call(
        &mut self,
        closure: Value,
        args: Vec<Value>,
    ) -> Result<(), RuntimeError> {
        self.begin_function_call(closure, args, ReturnTarget::Direct)
    }

    fn begin_function_call(
        &mut self,
        closure: Value,
        mut args: Vec<Value>,
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
        match function.parameter_model {
            ClosureParameterModel::Exact => {
                if args.len() != function.parameter_count {
                    return Err(RuntimeError::FunctionArgumentCount {
                        expected: function.parameter_count,
                        actual: args.len(),
                    });
                }
            }
            ClosureParameterModel::TypeScript {
                required_count,
                accepts_rest,
            } => {
                let fixed_count = function
                    .parameter_count
                    .saturating_sub(usize::from(accepts_rest));
                debug_assert!(required_count <= fixed_count);
                if accepts_rest {
                    let rest = if args.len() > fixed_count {
                        args.split_off(fixed_count)
                    } else {
                        Vec::new()
                    };
                    args.resize(fixed_count, Value::Undefined);
                    args.push(self.heap.allocate_list(rest)?);
                } else {
                    args.resize(function.parameter_count, Value::Undefined);
                    args.truncate(function.parameter_count);
                }
            }
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
                let call = if callback.live_url_search_params {
                    let Value::Ref(receiver) = callback.calls[0] else {
                        return Err(RuntimeError::ValidationFailed {
                            reason: "invalid live URLSearchParams callback receiver".to_string(),
                        });
                    };
                    self.heap
                        .url_search_params_entries(receiver)?
                        .and_then(|entries| entries.get(callback.next_index).cloned())
                        .map(|(name, value)| {
                            Value::Tuple(
                                vec![
                                    Value::String(value.into()),
                                    Value::String(name.into()),
                                    Value::Ref(receiver),
                                ]
                                .into(),
                            )
                        })
                } else {
                    callback.calls.get(callback.next_index).cloned()
                };
                if let Some(call) = call {
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
            live_url_search_params: false,
        };
        self.begin_function_call(function, first, ReturnTarget::Callback(callback))
    }

    pub(super) fn begin_url_search_params_for_each(
        &mut self,
        function: Value,
        receiver: HeapId,
    ) -> Result<(), RuntimeError> {
        let entries = self
            .heap
            .url_search_params_entries(receiver)?
            .expect("URLSearchParams receiver was checked");
        let Some((name, value)) = entries.first() else {
            self.stack.push(Value::Undefined);
            return Ok(());
        };
        let first = vec![
            Value::String(value.into()),
            Value::String(name.into()),
            Value::Ref(receiver),
        ];
        let callback = CallbackDriver {
            function: function.clone(),
            calls: vec![Value::Ref(receiver)],
            next_index: 1,
            results: Vec::new(),
            completion: CallbackCompletion::Discard,
            allow_effects: true,
            live_url_search_params: true,
        };
        self.begin_function_call(function, first, ReturnTarget::Callback(callback))
    }
}

fn collection_callback(mut arguments: Vec<Value>, receiver: HeapId) -> Value {
    arguments.push(Value::Ref(receiver));
    Value::Tuple(arguments.into())
}

fn callback_argument_matches(call: &Value, index: usize, expected: &Value) -> bool {
    matches!(call, Value::Tuple(arguments) if arguments.get(index).is_some_and(|actual| same_value_zero(actual, expected)))
}

fn callback_targets_receiver(callback: &CallbackDriver, receiver: HeapId) -> bool {
    !callback.live_url_search_params
        && callback.calls.iter().any(|call| {
            matches!(call, Value::Tuple(arguments) if matches!(arguments.last(), Some(Value::Ref(id)) if *id == receiver))
        })
}

fn live_collection_callbacks(
    frames: &mut [CallFrame],
    receiver: HeapId,
) -> impl Iterator<Item = &mut CallbackDriver> {
    frames.iter_mut().filter_map(move |frame| {
        let ReturnTarget::Callback(callback) = &mut frame.return_target else {
            return None;
        };
        callback_targets_receiver(callback, receiver).then_some(callback)
    })
}

fn retain_pending_calls(callback: &mut CallbackDriver, mut retain: impl FnMut(&Value) -> bool) {
    let mut pending = callback.calls.split_off(callback.next_index);
    pending.retain(|call| retain(call));
    callback.calls.extend(pending);
}

fn clear_pending_calls(frames: &mut [CallFrame], receiver: HeapId) {
    for callback in live_collection_callbacks(frames, receiver) {
        callback.calls.truncate(callback.next_index);
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
