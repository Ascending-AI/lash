use super::*;

#[derive(Clone)]
pub(super) struct CallFrame {
    pub(super) return_ip: usize,
    pub(super) function: Option<usize>,
    pub(super) operand_stack_base: usize,
    pub(super) slots: SlotState,
    pub(super) iter_stack: Vec<IterState>,
    pub(super) return_target: ReturnTarget,
}

/// Arguments for one call. A callback driver keeps its pending calls as
/// argument tuples, so it lends the arguments of the call it is starting
/// instead of re-materializing them into a fresh vector per element.
pub(super) enum CallArguments<'a> {
    Owned(Vec<Value>),
    Borrowed(&'a [Value]),
}

impl CallArguments<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Owned(values) => values.len(),
            Self::Borrowed(values) => values.len(),
        }
    }

    fn into_owned(self) -> Vec<Value> {
        match self {
            Self::Owned(values) => values,
            Self::Borrowed(values) => values.to_vec(),
        }
    }
}

#[derive(Clone)]
pub(super) enum ReturnTarget {
    Direct,
    Callback(CallbackDriver),
    /// A guest `valueOf`/`toString` a coercion called (FIG-3652): its answer
    /// goes to the suspended instruction's log, not the operand stack.
    Coercion(CoercionDriver),
}

#[derive(Clone)]
pub(super) struct CallbackDriver {
    pub(super) function: Value,
    /// Each item is an inline tuple of arguments for one callback invocation.
    pub(super) calls: Vec<Value>,
    pub(super) next_index: usize,
    pub(super) results: Vec<Value>,
    pub(super) completion: CallbackCompletion,
    pub(super) allow_effects: bool,
    /// `calls[0]` is the rooted URLSearchParams receiver and `next_index` is
    /// the next live list index. The list is re-read after every callback so
    /// appends and deletions follow WHATWG iteration semantics.
    pub(super) live_url_search_params: bool,
}

/// A plain object's answer to a member call on a built-in method name.
pub(super) enum PlainObjectMethod {
    /// Its own closure.
    Own(Value),
    /// An own property that is not a function, or no member at all.
    NotCallable,
}

/// The methods `Object.prototype` gives every plain object.
const OBJECT_PROTOTYPE_METHODS: &[&str] = &[
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
];

#[derive(Clone, Copy)]
pub(super) enum CallbackCompletion {
    Collect,
    Discard,
}

pub(super) fn slot_names_for(chunk: &Chunk, active_function: Option<usize>) -> &[Name] {
    active_function
        .and_then(|index| chunk.functions.get(index))
        .map_or(chunk.slot_names.as_slice(), |function| {
            function.slot_names.as_ref()
        })
}

impl<H: ExecutionHost> Vm<'_, H> {
    #[expect(
        clippy::expect_used,
        reason = "the Map receiver was checked and map_set stored the key above, so the stored pair resolves, per both messages"
    )]
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

    #[expect(
        clippy::expect_used,
        reason = "the Set receiver was checked and set_add stored the value above, per both messages"
    )]
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
        if !existed {
            update_live_cursors(
                self.all_iterators(),
                receiver,
                &CollectionMutation::Added(key.clone()),
            );
        }
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
        update_live_cursors(
            self.all_iterators(),
            receiver,
            &CollectionMutation::Deleted(key),
        );
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            retain_pending_calls(callback, |call| !callback_argument_matches(call, 1, key));
        }
    }

    pub(super) fn map_for_each_clear(&mut self, receiver: HeapId) {
        update_live_cursors(self.all_iterators(), receiver, &CollectionMutation::Cleared);
        clear_pending_calls(&mut self.frames, receiver);
    }

    pub(super) fn set_for_each_add(&mut self, receiver: HeapId, value: Value, existed: bool) {
        if existed {
            return;
        }
        update_live_cursors(
            self.all_iterators(),
            receiver,
            &CollectionMutation::Added(value.clone()),
        );
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            callback.calls.push(collection_callback(
                vec![value.clone(), value.clone()],
                receiver,
            ));
        }
    }

    pub(super) fn set_for_each_delete(&mut self, receiver: HeapId, value: &Value) {
        update_live_cursors(
            self.all_iterators(),
            receiver,
            &CollectionMutation::Deleted(value),
        );
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            retain_pending_calls(callback, |call| !callback_argument_matches(call, 0, value));
        }
    }

    pub(super) fn set_for_each_clear(&mut self, receiver: HeapId) {
        update_live_cursors(self.all_iterators(), receiver, &CollectionMutation::Cleared);
        clear_pending_calls(&mut self.frames, receiver);
    }

    /// How a member call `receiver.name(..)` on a built-in method name
    /// resolves when the receiver is a plain object (FIG-3700).
    ///
    /// A plain object has no array or string methods: its own property is the
    /// method, or, for the methods `Object.prototype` defines, the built-in
    /// answers when it has none. `None` means the receiver is not a plain object
    /// or the built-in answers.
    pub(super) fn plain_object_method(
        &self,
        receiver: &Value,
        name: &str,
    ) -> Result<Option<PlainObjectMethod>, RuntimeError> {
        let record = match receiver {
            Value::Record(record) => record.as_ref(),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => record.as_ref(),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        Ok(match record.get(name) {
            Some(member @ Value::Ref(id))
                if matches!(self.heap.get(*id)?, HeapObject::Closure { .. }) =>
            {
                Some(PlainObjectMethod::Own(member.clone()))
            }
            Some(_) => Some(PlainObjectMethod::NotCallable),
            None if OBJECT_PROTOTYPE_METHODS.contains(&name) => None,
            None => Some(PlainObjectMethod::NotCallable),
        })
    }

    /// Calls what [`Self::plain_object_method`] resolved, as a member call
    /// with `receiver` as `this`; a member that is not a function throws the
    /// `TypeError` ECMA-262's Call does.
    pub(super) fn call_plain_object_method(
        &mut self,
        method: PlainObjectMethod,
        name: &str,
        receiver: Value,
        arguments: Vec<Value>,
    ) -> Result<(), RuntimeError> {
        match method {
            PlainObjectMethod::Own(function) => self.begin_function_call(
                function,
                receiver,
                CallArguments::Owned(arguments),
                ReturnTarget::Direct,
            ),
            PlainObjectMethod::NotCallable => Err(
                match self.heap.allocate_error(
                    crate::runtime::heap::ErrorKind::TypeError,
                    Some(format!("{name} is not a function")),
                    None,
                    None,
                ) {
                    Ok(value) => RuntimeError::UncaughtException { value },
                    Err(error) => error,
                },
            ),
        }
    }

    /// Keeps a returning frame's slot state for the next call. Its contents
    /// are dropped here: the scratch slot is not a root set, and a finished
    /// frame's values must not outlive it.
    fn recycle_slot_state(&mut self, mut slots: SlotState) {
        slots.values.clear();
        slots.extras = Record::new();
        slots.extras_heapified = false;
        self.slot_scratch = Some(slots);
    }

    /// Slot state for a frame of `len` slots, reusing the scratch state when
    /// one is held. The values vector is reset to the fresh-frame state, so a
    /// reused state is indistinguishable from a newly allocated one.
    fn take_slot_state(&mut self, len: usize) -> SlotState {
        let Some(mut slots) = self.slot_scratch.take() else {
            return SlotState {
                values: vec![None; len],
                extras: Record::new(),
                extras_heapified: false,
            };
        };
        slots.values.clear();
        slots.values.resize(len, None);
        slots
    }

    /// The one entry point every guest call takes — a call instruction, a
    /// builtin's callback driver, a member call, a coercion hook — so each
    /// is held to the same frame-depth limit and instruction accounting.
    ///
    /// `receiver` is the call's receiver (`undefined` for a plain call). It
    /// lands in the callee's receiver slot when the callee declares one, and
    /// is dropped otherwise: an arrow reads its enclosing function's slot as a
    /// capture, so no frame carries a receiver it never reads.
    pub(super) fn begin_function_call(
        &mut self,
        closure: Value,
        receiver: Value,
        mut args: CallArguments<'_>,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        let limit = self.host.execution_bounds().max_frame_depth.get();
        if self.frames.len() as u64 >= limit {
            return Err(RuntimeError::FrameDepthExceeded { limit });
        }
        let Value::Ref(id) = closure else {
            return Err(RuntimeError::NonFunctionCall {
                actual: crate::runtime::value_type_name(&closure).to_string(),
            });
        };
        let (function_index, captures) = match self.heap.get(id)? {
            HeapObject::Closure {
                function, captures, ..
            } => (*function as usize, captures.clone()),
            HeapObject::BuiltinFunction(function) => {
                let function = *function;
                return self.call_detached_builtin(function, &args.into_owned(), return_target);
            }
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
                // An exact-arity call needs no adjustment, so the arguments
                // stay where they are instead of being copied into a vector.
                if accepts_rest || args.len() != function.parameter_count {
                    let mut values = args.into_owned();
                    if accepts_rest {
                        let rest = if values.len() > fixed_count {
                            values.split_off(fixed_count)
                        } else {
                            Vec::new()
                        };
                        values.resize(fixed_count, Value::Undefined);
                        values.push(self.heap.allocate_list(rest)?);
                    } else {
                        values.resize(function.parameter_count, Value::Undefined);
                        values.truncate(function.parameter_count);
                    }
                    args = CallArguments::Owned(values);
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

        let mut slots = self.take_slot_state(function.slot_names.len());
        if let Some(slot) = function.self_slot {
            slots.values[slot] = Some(Value::Ref(id));
        }
        if let Some(slot) = function.receiver_slot {
            slots.values[slot] = Some(receiver);
        }
        match args {
            CallArguments::Owned(values) => {
                for (slot, value) in function.parameter_slots.iter().copied().zip(values) {
                    slots.values[slot] = Some(value);
                }
            }
            CallArguments::Borrowed(values) => {
                for (slot, value) in function.parameter_slots.iter().copied().zip(values) {
                    slots.values[slot] = Some(value.clone());
                }
            }
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
            return_target,
        };
        if self.frames.is_empty() {
            // `CallFrame` is close to the 1 KiB raw-vec capacity step, so the
            // default first growth of four frames reserves 2.5 KiB up front for
            // every program that never calls again. One frame covers the
            // non-recursive case; growth from here follows the usual policy.
            self.frames.reserve_exact(1);
        }
        self.frames.push(frame);
        self.active_function = Some(function_index);
        self.ip = function.entry_ip;
        Ok(())
    }

    pub(super) fn return_from_function(&mut self) -> Result<(), RuntimeError> {
        let result = self.pop_stack()?;
        let frame = self.frames.pop().ok_or(RuntimeError::VmStackUnderflow)?;
        self.stack.truncate(frame.operand_stack_base);
        let finished = std::mem::replace(&mut self.slots, frame.slots);
        self.recycle_slot_state(finished);
        self.iter_stack = frame.iter_stack;
        self.active_function = frame.function;
        self.ip = frame.return_ip;
        self.complete_call(result, frame.return_target)
    }

    /// Hands a call's `result` to whatever started the call: the operand
    /// stack for a direct call, the callback driver, which records it and
    /// starts the next callback, or the coercion driver, which logs the
    /// primitive a guest `valueOf`/`toString` hook answered. A frame's return
    /// and a built-in that answers without a frame both finish here, so a
    /// built-in used as a callback or a coercion hook is driven exactly as a
    /// closure is.
    ///
    /// A built-in callee answers without a frame, so its callbacks are driven
    /// here in a loop rather than by recursing once per element.
    pub(super) fn complete_call(
        &mut self,
        mut result: Value,
        mut return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        loop {
            let mut callback = match return_target {
                ReturnTarget::Direct => {
                    self.stack.push(result);
                    return Ok(());
                }
                // A hook's answer goes to the suspended instruction's log,
                // not the operand stack.
                ReturnTarget::Coercion(driver) => {
                    return self.finish_guest_hook(driver, result);
                }
                ReturnTarget::Callback(callback) => callback,
            };
            {
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
                    let arguments = callback_arguments(call)?;
                    if let Some(builtin) = self.builtin_callee(&function)? {
                        result = self.detached_builtin_result(builtin, &arguments)?;
                        return_target = ReturnTarget::Callback(callback);
                        continue;
                    }
                    return self.begin_function_call(
                        function,
                        Value::Undefined,
                        CallArguments::Borrowed(&arguments),
                        ReturnTarget::Callback(callback),
                    );
                }
                self.stack.push(match callback.completion {
                    CallbackCompletion::Collect => Value::List(callback.results.into()),
                    CallbackCompletion::Discard => Value::Undefined,
                });
                return Ok(());
            }
        }
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
        self.begin_function_call(
            function,
            Value::Undefined,
            CallArguments::Borrowed(&first),
            ReturnTarget::Callback(callback),
        )
    }

    #[expect(
        clippy::expect_used,
        reason = "the URLSearchParams receiver was checked by heap guards above, per the message"
    )]
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
        self.begin_function_call(
            function,
            Value::Undefined,
            CallArguments::Owned(first),
            ReturnTarget::Callback(callback),
        )
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

fn callback_arguments(call: Value) -> Result<ListValue, RuntimeError> {
    let Value::Tuple(arguments) = call else {
        return Err(RuntimeError::ValidationFailed {
            reason: "invalid durable callback argument vector".to_string(),
        });
    };
    Ok(arguments)
}
