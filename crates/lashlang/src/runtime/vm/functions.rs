use super::*;
use std::collections::BTreeSet;

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

    fn to_vec(&self) -> Vec<Value> {
        match self {
            Self::Owned(values) => values.clone(),
            Self::Borrowed(values) => values.to_vec(),
        }
    }
}

#[derive(Clone)]
pub(super) enum ReturnTarget {
    Direct,
    /// Boxed: a callback driver carries the walk and the sort window, where
    /// the common `Direct` frame is empty.
    Callback(Box<CallbackDriver>),
    /// A guest `valueOf`/`toString` a coercion called (FIG-3652): its answer
    /// goes to the suspended instruction's log, not the operand stack.
    Coercion(CoercionDriver),
}

#[derive(Clone)]
pub(super) struct CallbackDriver {
    pub(super) function: Value,
    /// Each item is an inline tuple of arguments for one callback
    /// invocation. For a `Queued` plan the still-pending tuples sit at
    /// `next_index..`; an `array_like` walk materializes each tuple only as
    /// it is issued, so `calls[next_index - 1]` is always the call that
    /// just answered.
    pub(super) calls: Vec<Value>,
    pub(super) next_index: usize,
    pub(super) results: Vec<Value>,
    pub(super) completion: CallbackCompletion,
    pub(super) allow_effects: bool,
    /// The receiver each callback runs with — the array methods' `thisArg`.
    /// `undefined` for the collection methods, which pass no receiver.
    pub(super) this_arg: Value,
    /// `calls[0]` is the rooted URLSearchParams receiver and `next_index` is
    /// the next live list index. The list is re-read after every callback so
    /// appends and deletions follow WHATWG iteration semantics.
    pub(super) live_url_search_params: bool,
    /// `Some` when the pending invocations come from a live index walk over
    /// a generic array-like receiver rather than a materialized `calls`
    /// queue (FIG-3787). The walk resolves each index against the receiver
    /// as it stands *then* — a callback's own writes are observed and its
    /// deletes are skipped, ECMA's per-index `Has`/`Get` ordering — and a
    /// `length` near `2**53` costs only the calls actually made.
    pub(super) array_like: Option<ArrayLikeWalk>,
}

/// A lazy index walk over a generic array-like receiver — the pending-call
/// source the callback methods use instead of a materialized tuple queue.
#[derive(Clone)]
pub(super) struct ArrayLikeWalk {
    /// The receiver each index read runs against — and each callback's
    /// final argument.
    pub(super) receiver: Value,
    /// Ascending: the first unvisited index. Descending: the next candidate,
    /// visited then decremented; `u64::MAX` marks the walk exhausted once
    /// index 0 has been visited (or the receiver had `length` 0).
    pub(super) next: u64,
    /// The receiver's `length`, read once when the walk began — ECMA's `len`
    /// snapshot.
    pub(super) length: u64,
    /// Descending order (`findLast`/`findLastIndex`/`reduceRight`).
    pub(super) descending: bool,
    /// `true` gates each visit on a live `Has` — every/some/map/filter/
    /// flatMap/forEach/reduce*; `false` visits every index in range (the
    /// find family, and `Array.from`'s mapfn).
    pub(super) gated: bool,
    /// `Array.from`'s mapfn is `Call(mapfn, thisArg, «element, index»)` — two
    /// arguments, where the prototype methods append the receiver.
    pub(super) omit_receiver: bool,
}

/// A `function` index no compiled chunk provides: the marker a bound
/// function's closure carries. `Function.prototype.bind` answers a
/// `HeapObject::Closure` whose captures are `[target, receiver, boundArgs]`
/// and whose function index is this sentinel — `begin_function_call` unwraps
/// it into a call of `target` rather than pushing a frame. The wire shape is
/// an ordinary closure, so a bound function snapshots, restores and is
/// garbage-collected exactly as a closure is.
pub(crate) const BOUND_FUNCTION_INDEX: u32 = u32::MAX;

/// The in-flight ordering a comparator `sort` runs: a binary-search
/// insertion sort whose comparisons are guest calls. Each completed answer
/// narrows `current`'s window in `sorted`; when the window closes the next
/// `pending` element starts a fresh search.
#[derive(Clone)]
pub(super) struct SortState {
    /// The elements still to be inserted, in reverse visit order (the next
    /// one pops off the back). `undefined` never reaches the comparator —
    /// `undefined_count` appends it after the sorted defined elements.
    pub(super) pending: Vec<Value>,
    /// The sorted prefix.
    pub(super) sorted: Vec<Value>,
    /// The element the open binary search is placing.
    pub(super) current: Value,
    /// The `sorted` index the last comparator call probed.
    pub(super) probe: usize,
    /// The binary-search window in `sorted`: `lo..hi`.
    pub(super) lo: usize,
    pub(super) hi: usize,
    /// How many present elements were `undefined`.
    pub(super) undefined_count: u64,
    /// The receiver the ordering writes back to (`sort`) or clones
    /// (`toSorted`).
    pub(super) receiver: Value,
    /// The receiver's `length`.
    pub(super) length: u64,
    /// `sort` writes back; `toSorted` allocates a fresh dense array.
    pub(super) in_place: bool,
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

#[derive(Clone)]
pub(super) enum CallbackCompletion {
    Collect,
    Discard,
    /// `every`: the first falsy predicate result answers `false`; exhaustion
    /// answers `true`.
    Every,
    /// `some`: the first truthy predicate result answers `true`; exhaustion
    /// answers `false`.
    Some,
    /// `filter`: a truthy predicate result keeps its element — the first
    /// argument of the call that produced it — in `results`.
    Filter,
    /// `find`/`findLast`: the first truthy predicate answers its element.
    Find,
    /// `findIndex`/`findLastIndex`: the first truthy predicate answers its
    /// index — the second argument of the call that produced it.
    FindIndex,
    /// `map`: results fill a fresh dense array at their own indices; holes
    /// stay holes.
    Map {
        length: u64,
    },
    /// `flatMap`: the collected results flatten one level into the answer.
    FlatMap,
    /// `reduce`/`reduceRight`: each result becomes the next call's
    /// accumulator (its first argument); the last result is the answer.
    Reduce {
        accumulator: Value,
    },
    /// `sort`/`toSorted` with a comparator function: the insertion-sort
    /// state machine above.
    Sort(SortState),
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
        // `has`, `set`, `entries` and the `find` below each scan the entries.
        self.charge_intrinsic_work(self.heap.map_len(receiver)?.saturating_mul(4));
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
        // The delete scans the entries once, then shifts the tail it removes.
        self.charge_intrinsic_work(self.heap.map_len(receiver)?.saturating_mul(2));
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
        // `has`, `add`, `values` and the `find` below each scan the members.
        self.charge_intrinsic_work(self.heap.set_len(receiver)?.saturating_mul(4));
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
        // The delete scans the members once, then shifts the tail it removes.
        self.charge_intrinsic_work(self.heap.set_len(receiver)?.saturating_mul(2));
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
            // Every live cursor over this map rewrites its pending tail.
            let work = update_live_cursors(
                self.all_iterators(),
                receiver,
                &CollectionMutation::Added(key.clone()),
            );
            self.charge_intrinsic_work(work);
        }
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            // Maintaining the durable queue scans its pending calls once.
            charge_collection_work(&mut self.instructions_executed, callback.calls.len());
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
        let work = update_live_cursors(
            self.all_iterators(),
            receiver,
            &CollectionMutation::Deleted(key),
        );
        self.charge_intrinsic_work(work);
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            charge_collection_work(&mut self.instructions_executed, callback.calls.len());
            retain_pending_calls(callback, |call| !callback_argument_matches(call, 1, key));
        }
    }

    pub(super) fn map_for_each_clear(&mut self, receiver: HeapId) {
        let work =
            update_live_cursors(self.all_iterators(), receiver, &CollectionMutation::Cleared)
                .saturating_add(clear_pending_calls(&mut self.frames, receiver));
        self.charge_intrinsic_work(work);
    }

    pub(super) fn set_for_each_add(&mut self, receiver: HeapId, value: Value, existed: bool) {
        if existed {
            return;
        }
        let work = update_live_cursors(
            self.all_iterators(),
            receiver,
            &CollectionMutation::Added(value.clone()),
        );
        self.charge_intrinsic_work(work);
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            charge_collection_work(&mut self.instructions_executed, callback.calls.len());
            callback.calls.push(collection_callback(
                vec![value.clone(), value.clone()],
                receiver,
            ));
        }
    }

    pub(super) fn set_for_each_delete(&mut self, receiver: HeapId, value: &Value) {
        let work = update_live_cursors(
            self.all_iterators(),
            receiver,
            &CollectionMutation::Deleted(value),
        );
        self.charge_intrinsic_work(work);
        for callback in live_collection_callbacks(&mut self.frames, receiver) {
            charge_collection_work(&mut self.instructions_executed, callback.calls.len());
            retain_pending_calls(callback, |call| !callback_argument_matches(call, 0, value));
        }
    }

    pub(super) fn set_for_each_clear(&mut self, receiver: HeapId) {
        let work =
            update_live_cursors(self.all_iterators(), receiver, &CollectionMutation::Cleared)
                .saturating_add(clear_pending_calls(&mut self.frames, receiver));
        self.charge_intrinsic_work(work);
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
            Some(member @ Value::Ref(id)) if self.heap.get(*id)?.is_function() => {
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
        mut callee: Value,
        mut receiver: Value,
        mut args: CallArguments<'_>,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        let limit = self.host.execution_bounds().max_frame_depth.get();
        if self.frames.len() as u64 >= limit {
            return Err(RuntimeError::FrameDepthExceeded { limit });
        }
        // `Function.prototype.bind` products are closures on the sentinel
        // index: a call unwraps them into a call of the bound target with
        // the bound receiver and the bound arguments ahead of the call's
        // own — iteratively, so `f.bind(a).bind(b)` costs no extra frames
        // and `call`/`apply` chains of them stay in the one instruction.
        while let Value::Ref(id) = &callee {
            let HeapObject::Closure {
                function, captures, ..
            } = self.heap.get(*id)?
            else {
                break;
            };
            if *function != BOUND_FUNCTION_INDEX {
                break;
            }
            let [target, bound_receiver, bound_args] = captures.as_slice() else {
                return Err(RuntimeError::ValidationFailed {
                    reason: "invalid bound-function captures".to_string(),
                });
            };
            let mut bound = match bound_args {
                Value::List(values) | Value::Tuple(values) => values.to_vec(),
                Value::Ref(bound_id) => match self.heap.get(*bound_id)? {
                    HeapObject::List(values) | HeapObject::Tuple(values) => values.clone(),
                    _ => {
                        return Err(RuntimeError::ValidationFailed {
                            reason: "invalid bound-function argument list".to_string(),
                        });
                    }
                },
                _ => {
                    return Err(RuntimeError::ValidationFailed {
                        reason: "invalid bound-function argument list".to_string(),
                    });
                }
            };
            bound.extend(args.to_vec());
            args = CallArguments::Owned(bound);
            callee = target.clone();
            receiver = bound_receiver.clone();
        }
        let Value::Ref(id) = callee else {
            return Err(RuntimeError::NonFunctionCall {
                actual: crate::runtime::value_type_name(&callee).to_string(),
            });
        };
        // `arguments` materializes lazily through `Lash.Arguments` from the
        // raw argv and the callee, stashed on the frame's extras under names
        // no guest binding can name. The snapshot precedes parameter
        // adjustment because ECMA's `arguments` sees what was passed, not
        // what the signature filled.
        let arguments_argv = args.to_vec();
        let (function_index, captures) = match self.heap.get(id)? {
            HeapObject::Closure {
                function, captures, ..
            } => (*function as usize, captures.clone()),
            HeapObject::BuiltinFunction(function) => {
                let function = *function;
                // A prototype-owned built-in is a method value: the receiver
                // the call site carried is its `this` — a member call's own
                // object, a `call`/`apply`/`bind` product's bound receiver,
                // or `undefined` for a detached plain call (FIG-3787).
                if function.prototype().is_some() {
                    return self.call_detached_builtin(
                        function,
                        receiver,
                        args.into_owned(),
                        return_target,
                    );
                }
                return self.call_builtin(function, args, return_target);
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
        slots
            .extras
            .insert_str("lash:argv", Value::List(arguments_argv.into()));
        slots.extras.insert_str("lash:callee", Value::Ref(id));
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
                // The just-finished call's result folds into the completion
                // state first: an early answer (`every`/`some`/`find*`), a
                // kept element (`filter`), the next accumulator (`reduce`),
                // or the sort's next comparison.
                if let Some(done) = self.callback_result(&mut callback, &result)? {
                    self.stack.push(done);
                    return Ok(());
                }
                let call = if callback.live_url_search_params {
                    let Value::Ref(receiver) = callback.calls[0] else {
                        return Err(RuntimeError::ValidationFailed {
                            reason: "invalid live URLSearchParams callback receiver".to_string(),
                        });
                    };
                    let entries = self.heap.url_search_params_entries(receiver)?;
                    if let Some(entries) = &entries {
                        // Each step re-reads the live entry list whole.
                        self.charge_intrinsic_work(entries.len());
                    }
                    entries
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
                } else if callback.array_like.is_some() {
                    // The generic array-like walk resolves each index live
                    // and appends the produced tuple to `calls`, keeping the
                    // `calls[next_index - 1]` convention the element-keyed
                    // completions read.
                    match self.array_like_next_call(&mut callback)? {
                        Some(arguments) => {
                            let call = Value::Tuple(arguments.into());
                            callback.calls.push(call.clone());
                            Some(call)
                        }
                        None => None,
                    }
                } else {
                    callback.calls.get(callback.next_index).cloned()
                };
                if let Some(call) = call {
                    callback.next_index += 1;
                    let function = callback.function.clone();
                    let this_arg = callback.this_arg.clone();
                    // Each builtin-initiated frame push has the same unit cost
                    // as an explicit `Call` opcode.
                    self.instructions_executed = self.instructions_executed.saturating_add(1);
                    let arguments = callback_arguments(call)?;
                    if let Some(builtin) = self.builtin_callee(&function)? {
                        // A built-in callback answers synchronously — the
                        // method's receiver is the callback's `this`.
                        result = self.detached_builtin_result(builtin, &this_arg, &arguments)?;
                        return_target = ReturnTarget::Callback(callback);
                        continue;
                    }
                    return self.begin_function_call(
                        function,
                        this_arg,
                        CallArguments::Borrowed(&arguments),
                        ReturnTarget::Callback(callback),
                    );
                }
                let finished = self.callback_finish(&callback)?;
                self.stack.push(finished);
                return Ok(());
            }
        }
    }

    /// A call whose callee is a built-in object — a global, a static or a
    /// `Owner.prototype` object `builtin_value` minted. The conversion
    /// functions, `Array` and the error constructors answer directly; an
    /// `Owner.method` value runs the synchronous stdlib table under its
    /// qualified name; `Function` and its friends refuse rather than
    /// evaluate source; a `[[Construct]]`-only constructor and a
    /// non-callable namespace throw the TypeError Node throws.
    fn call_builtin(
        &mut self,
        function: BuiltinFunction,
        args: CallArguments<'_>,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        let name = function.qualified_name();
        let args = args.into_owned();
        let result = self.global_builtin_result(&name, &args)?;
        self.complete_call(result, return_target)
    }

    /// What a global or object-scope built-in answers to a call —
    /// `call_builtin`'s dispatch, shared with the callback driver's detached
    /// path.
    pub(super) fn global_builtin_result(
        &mut self,
        name: &str,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let result = match name {
            "String" => Value::String(
                self.heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?
                    .into(),
            ),
            "Number" => Value::Number(match args.first() {
                None => 0.0,
                Some(value) => self.heap.javascript_to_number(value)?,
            }),
            "Boolean" => Value::Bool(match args.first() {
                None => false,
                Some(value) => self.is_truthy_for_dialect(value)?,
            }),
            "Array" => match args {
                [Value::Number(length)]
                    if length.fract() == 0.0 && *length >= 0.0 && *length <= u32::MAX as f64 =>
                {
                    let length = *length as usize;
                    self.heap.ensure_list_allocation_len(length)?;
                    let list = self.heap.allocate_list(vec![Value::Undefined; length])?;
                    if let Value::Ref(list_id) = list {
                        self.heap.mark_list_holes(list_id, (0..length).collect());
                    }
                    list
                }
                _ => {
                    self.heap.ensure_list_allocation_len(args.len())?;
                    self.heap.allocate_list(args.to_vec())?
                }
            },
            "Error" | "AggregateError" | "EvalError" | "RangeError" | "ReferenceError"
            | "SyntaxError" | "TypeError" | "URIError" => {
                let kind = crate::runtime::ErrorKind::from_name(name).ok_or_else(|| {
                    RuntimeError::ValidationFailed {
                        reason: format!("unknown error kind `{name}`"),
                    }
                })?;
                let (message_arg, options) = if kind == crate::runtime::ErrorKind::AggregateError {
                    (args.get(1), args.get(2))
                } else {
                    (args.first(), args.get(1))
                };
                let message = match message_arg {
                    None | Some(Value::Undefined) => None,
                    Some(value) => Some(self.heap.javascript_to_string(value)?),
                };
                let cause = match options {
                    Some(Value::Ref(id)) => match self.heap.get(*id)? {
                        HeapObject::Record(record) => record.get("cause").cloned(),
                        _ => None,
                    },
                    Some(Value::Record(record)) => record.get("cause").cloned(),
                    _ => None,
                };
                let errors = if kind == crate::runtime::ErrorKind::AggregateError {
                    let (isolated, staged) = self
                        .heap
                        .isolate_value_with_work(args.first().unwrap_or(&Value::Undefined))?;
                    // The `errors` copy walks every object it reaches.
                    self.charge_intrinsic_work(staged);
                    Some(isolated)
                } else {
                    None
                };
                self.heap.allocate_error(kind, message, cause, errors)?
            }
            // `parseInt`/`parseFloat` are the same functions as the
            // `Number.*` statics; the global `isNaN`/`isFinite` coerce first.
            "parseInt" | "parseFloat" => {
                let mut call = Vec::with_capacity(args.len() + 1);
                call.push(Value::String(format!("Number.{name}").into()));
                call.extend(args.iter().cloned());
                super::javascript::javascript_stdlib(
                    &self.heap,
                    &call,
                    &mut self.instructions_executed,
                )?
            }
            "isNaN" | "isFinite" => {
                let value = args.first().unwrap_or(&Value::Undefined);
                // Coercing a string argument scans it whole.
                self.charge_intrinsic_work(proportional_units(value));
                let number = self.heap.javascript_to_number(value)?;
                Value::Bool(if name == "isNaN" {
                    number.is_nan()
                } else {
                    number.is_finite()
                })
            }
            "encodeURIComponent" | "encodeURI" | "decodeURIComponent" | "decodeURI" => {
                let input = self
                    .heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?;
                // Encoding or decoding reads every input byte once.
                self.charge_intrinsic_work(input.len());
                let result = match name {
                    "encodeURIComponent" => Ok(super::javascript_codec::encode(&input, false)),
                    "encodeURI" => Ok(super::javascript_codec::encode(&input, true)),
                    "decodeURIComponent" => super::javascript_codec::decode(&input, false),
                    _ => super::javascript_codec::decode(&input, true),
                };
                match result {
                    Ok(value) => {
                        crate::runtime::ensure_javascript_string_size(value.len())?;
                        self.charge_intrinsic_work(value.len());
                        Value::String(value.into())
                    }
                    Err(()) => {
                        let error = self.heap.allocate_error(
                            crate::runtime::ErrorKind::URIError,
                            Some("URI malformed".to_string()),
                            None,
                            None,
                        )?;
                        return Err(RuntimeError::UncaughtException { value: error });
                    }
                }
            }
            // `RegExp(p)` called as a function constructs, like `new RegExp`.
            "RegExp" => self.construct_regexp(args)?,
            // `Object()` with no or a nullish argument is a fresh `{}`; an
            // object argument is the object itself. A primitive would need a
            // wrapper object the value model does not have.
            "Object" => match args.first() {
                None | Some(Value::Null) | Some(Value::Undefined) => {
                    self.heap.allocate_record(Record::new())?
                }
                Some(
                    value @ (Value::Record(_) | Value::List(_) | Value::Tuple(_) | Value::Ref(_)),
                ) => value.clone(),
                _ => {
                    return Err(RuntimeError::ValidationFailed {
                        reason: "TS_METHOD_UNSUPPORTED: Object() on a primitive needs a wrapper object this value model does not have".to_string(),
                    });
                }
            },
            // `eval`/`Function` evaluate source the dialect never admits; the
            // rejected globals refuse for the same reasons their direct-call
            // lowerings do.
            "eval" => {
                return Err(RuntimeError::ValidationFailed {
                    reason:
                        "TS_EVAL_UNSUPPORTED: eval evaluates source, which this dialect does not"
                            .to_string(),
                });
            }
            "structuredClone" | "btoa" | "atob" | "escape" | "unescape" => {
                return Err(RuntimeError::ValidationFailed {
                    reason: format!(
                        "TS_METHOD_UNSUPPORTED: {name} is not in the TypeScript runtime surface"
                    ),
                });
            }
            "BigInt" => {
                return Err(RuntimeError::ValidationFailed {
                    reason: "TS_BIGINT_UNSUPPORTED: BigInt values are not in the dialect"
                        .to_string(),
                });
            }
            "Symbol" => {
                return Err(RuntimeError::ValidationFailed {
                    reason: "TS_METHOD_UNSUPPORTED: Symbol values are not in the dialect"
                        .to_string(),
                });
            }
            // A bare `Date()` call answers the current date-time string; only
            // the lowerer can reach the journaled clock, so a materialized
            // `Date` value invoked indirectly refuses here.
            "Date" => {
                return Err(RuntimeError::ValidationFailed {
                    reason: "TS_DATE_NOW_EFFECT_REQUIRED: Date() must be lowered through the journaled clock effect".to_string(),
                });
            }
            _ if name.contains('.') && !name.ends_with(".prototype") => {
                // An `Owner.method` value: run the synchronous stdlib table
                // under its qualified name.
                let mut call = Vec::with_capacity(args.len() + 1);
                call.push(Value::String(name.into()));
                call.extend(args.iter().cloned());
                super::javascript::javascript_stdlib(
                    &self.heap,
                    &call,
                    &mut self.instructions_executed,
                )?
            }
            _ if name == "Function" || name.ends_with("Function") => {
                return Err(RuntimeError::ValidationFailed {
                    reason: format!(
                        "TS_FUNCTION_CONSTRUCTOR_UNSUPPORTED: `{name}()` evaluates source, which this dialect does not"
                    ),
                });
            }
            _ => {
                let message = if name.ends_with(".prototype")
                    || matches!(name, "Math" | "JSON" | "Reflect" | "Intl" | "Atomics")
                {
                    format!("{name} is not a function")
                } else {
                    format!("Constructor {name} requires 'new'")
                };
                let error = self.heap.allocate_error(
                    crate::runtime::ErrorKind::TypeError,
                    Some(message),
                    None,
                    None,
                )?;
                return Err(RuntimeError::UncaughtException { value: error });
            }
        };
        Ok(result)
    }

    /// Folds a completed callback's `result` into the driver's completion
    /// state. `Some` answers the whole driven call early; `None` lets the
    /// next pending call run.
    fn callback_result(
        &mut self,
        callback: &mut CallbackDriver,
        result: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        match &mut callback.completion {
            CallbackCompletion::Collect
            | CallbackCompletion::Map { .. }
            | CallbackCompletion::FlatMap => {
                // A heap reference is the value the callback returned: a copy
                // would be a different object (ECMA identity) and would copy
                // the binding cells a returned closure shares. Only an inline
                // compound is given its own object.
                let result = match result {
                    Value::Ref(_) => result.clone(),
                    result => {
                        let (isolated, staged) = self.heap.isolate_value_with_work(result)?;
                        charge_collection_work(&mut self.instructions_executed, staged);
                        isolated
                    }
                };
                callback.results.push(result);
                Ok(None)
            }
            CallbackCompletion::Discard => Ok(None),
            CallbackCompletion::Every => {
                Ok((!self.is_truthy_for_dialect(result)?).then_some(Value::Bool(false)))
            }
            CallbackCompletion::Some => {
                Ok((self.is_truthy_for_dialect(result)?).then_some(Value::Bool(true)))
            }
            CallbackCompletion::Filter => {
                if self.is_truthy_for_dialect(result)? {
                    callback.results.push(callback_call_arg(
                        &callback.calls,
                        callback.next_index,
                        0,
                    ));
                }
                Ok(None)
            }
            CallbackCompletion::Find => Ok(self
                .is_truthy_for_dialect(result)?
                .then(|| callback_call_arg(&callback.calls, callback.next_index, 0))),
            CallbackCompletion::FindIndex => Ok(self
                .is_truthy_for_dialect(result)?
                .then(|| callback_call_arg(&callback.calls, callback.next_index, 1))),
            CallbackCompletion::Reduce { accumulator } => {
                // The result is the next call's accumulator — the first
                // argument of `reduce`'s `(acc, item, i, o)` shape — and the
                // answer once the walk exhausts.
                *accumulator = result.clone();
                Ok(None)
            }
            CallbackCompletion::Sort(_) => self.sort_callback_result(callback, result),
        }
    }

    /// A comparator `sort`'s step: narrow `current`'s insertion window by the
    /// comparison the callback answered, and when it closes place `current`
    /// and open the next element's search. The answer arrives when every
    /// pending element has been placed.
    fn sort_callback_result(
        &mut self,
        callback: &mut CallbackDriver,
        result: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        let CallbackCompletion::Sort(state) = &mut callback.completion else {
            unreachable!("sort_callback_result only runs for a Sort completion")
        };
        // The comparator's answer coerces by ToNumber; a value that needs a
        // guest hook cannot suspend from a callback return, so it refuses
        // rather than silently ordering by `NaN`.
        let primitive = matches!(
            result,
            Value::Null | Value::Undefined | Value::Bool(_) | Value::Number(_) | Value::String(_)
        );
        let comparison = match self.heap.javascript_to_number(result) {
            Ok(number) => number,
            Err(error) => {
                if primitive {
                    return Err(error);
                }
                return Err(RuntimeError::ValidationFailed {
                    reason: "TS_SORT_COMPARATOR_HOOK_UNSUPPORTED: a comparator answer needing a guest hook cannot run inside a callback return".to_string(),
                });
            }
        };
        if !matches!(comparison.partial_cmp(&0.0), Some(std::cmp::Ordering::Less)) {
            // `>= 0` and `NaN` both place `current` after the probe — ECMA
            // keeps an unstable-but-defined order for both.
            state.lo = state.probe + 1;
        } else {
            state.hi = state.probe;
        }
        if state.lo < state.hi {
            // The search continues: enqueue the next probe as the driver's
            // next call.
            state.probe = state.lo + (state.hi - state.lo) / 2;
            let call =
                Value::Tuple(vec![state.current.clone(), state.sorted[state.probe].clone()].into());
            callback.calls.truncate(callback.next_index);
            callback.calls.push(call);
            return Ok(None);
        }
        state.sorted.insert(state.lo, state.current.clone());
        let Some(next) = state.pending.pop() else {
            // The ordering is complete: sorted defined elements, then the
            // `undefined` elements, then the holes the receiver keeps.
            let mut ordered = std::mem::take(&mut state.sorted);
            ordered.extend(std::iter::repeat_n(
                Value::Undefined,
                state.undefined_count as usize,
            ));
            let receiver = state.receiver.clone();
            let length = state.length;
            let in_place = state.in_place;
            return if in_place {
                self.array_like_write_back(&receiver, length, ordered)?;
                let receiver = self.array_like_receiver_object(&receiver);
                Ok(Some(receiver))
            } else {
                ordered.resize(length.min(usize::MAX as u64) as usize, Value::Undefined);
                self.heap.allocate_list(ordered).map(Some)
            };
        };
        state.current = next;
        state.lo = 0;
        state.hi = state.sorted.len();
        state.probe = state.hi / 2;
        let call =
            Value::Tuple(vec![state.current.clone(), state.sorted[state.probe].clone()].into());
        callback.calls.truncate(callback.next_index);
        callback.calls.push(call);
        Ok(None)
    }

    /// The driver's answer when its pending calls run out — each completion's
    /// exhaustion value.
    fn callback_finish(&mut self, callback: &CallbackDriver) -> Result<Value, RuntimeError> {
        Ok(match &callback.completion {
            CallbackCompletion::Collect => Value::List(callback.results.clone().into()),
            CallbackCompletion::Discard => Value::Undefined,
            CallbackCompletion::Every => Value::Bool(true),
            CallbackCompletion::Some | CallbackCompletion::Find => Value::Undefined,
            // `findIndex`'s miss and `some`'s miss differ: one is `-1`.
            CallbackCompletion::FindIndex => Value::Number(-1.0),
            CallbackCompletion::Filter | CallbackCompletion::FlatMap => {
                let mut items = Vec::new();
                for result in &callback.results {
                    match result {
                        Value::List(values) | Value::Tuple(values)
                            if matches!(callback.completion, CallbackCompletion::FlatMap) =>
                        {
                            items.extend(values.iter().cloned());
                        }
                        Value::Ref(id)
                            if matches!(callback.completion, CallbackCompletion::FlatMap) =>
                        {
                            match self.heap.get(*id)? {
                                HeapObject::List(values) | HeapObject::Tuple(values) => {
                                    items.extend(values.iter().cloned());
                                }
                                _ => items.push(result.clone()),
                            }
                        }
                        _ => items.push(result.clone()),
                    }
                }
                self.heap.allocate_list(items)?
            }
            CallbackCompletion::Map { length } => {
                // Each result lands at the index its call carried; positions
                // no call visited stay holes.
                let mut written: Vec<(u64, Value)> = Vec::with_capacity(callback.results.len());
                for (position, result) in callback.results.iter().enumerate() {
                    let index = match callback.calls.get(position) {
                        Some(Value::Tuple(arguments)) => match arguments.get(1) {
                            Some(Value::Number(index)) => *index as u64,
                            _ => continue,
                        },
                        _ => continue,
                    };
                    written.push((index, result.clone()));
                }
                if *length > u32::MAX as u64 {
                    return Err(RuntimeError::range_error("Invalid array length"));
                }
                self.heap.ensure_list_allocation_len(*length as usize)?;
                let mut items = vec![Value::Undefined; *length as usize];
                let mut holes: BTreeSet<usize> = (0..*length as usize).collect();
                for (index, value) in written {
                    if index < *length {
                        items[index as usize] = value;
                        holes.remove(&(index as usize));
                    }
                }
                let list = self.heap.allocate_list(items)?;
                if let Value::Ref(id) = list {
                    self.heap.mark_list_holes(id, holes);
                }
                list
            }
            // `reduce`'s answer is the accumulator the last callback left —
            // or the seed the walk started with when no index was present.
            CallbackCompletion::Reduce { accumulator } => accumulator.clone(),
            CallbackCompletion::Sort(state) => {
                // All elements were placed before the queue ran dry — the
                // one-element fast path that never issued a comparison.
                let mut ordered = state.sorted.clone();
                ordered.extend(std::iter::repeat_n(
                    Value::Undefined,
                    state.undefined_count as usize,
                ));
                if state.in_place {
                    self.array_like_write_back(&state.receiver, state.length, ordered)?;
                    self.array_like_receiver_object(&state.receiver)
                } else {
                    ordered.resize(
                        state.length.min(usize::MAX as u64) as usize,
                        Value::Undefined,
                    );
                    self.heap.allocate_list(ordered)?
                }
            }
        })
    }

    /// Starts the callback driver over a comparator `sort`'s first probe.
    /// `state` is the ready search over the receiver's present defined
    /// elements.
    pub(super) fn begin_sort_driver(
        &mut self,
        comparator: Value,
        mut state: SortState,
    ) -> Result<(), RuntimeError> {
        // The first element needs no comparison; each later element opens a
        // binary-search probe against the sorted prefix. A closed window
        // places `current` and takes the next pending element until either a
        // probe is issued or the ordering is complete.
        loop {
            if state.lo < state.hi {
                state.probe = state.lo + (state.hi - state.lo) / 2;
                let first = vec![state.current.clone(), state.sorted[state.probe].clone()];
                // `calls[0]` is the call being issued: the driver convention
                // reads the completed call as `calls[next_index - 1]` and the
                // next one as `calls[next_index]`.
                let callback = CallbackDriver {
                    function: comparator.clone(),
                    calls: vec![Value::Tuple(first.clone().into())],
                    next_index: 1,
                    results: Vec::new(),
                    completion: CallbackCompletion::Sort(state),
                    allow_effects: true,
                    this_arg: Value::Undefined,
                    live_url_search_params: false,
                    array_like: None,
                };
                return self.begin_function_call(
                    comparator,
                    Value::Undefined,
                    CallArguments::Owned(first),
                    ReturnTarget::Callback(Box::new(callback)),
                );
            }
            state.sorted.insert(state.lo, state.current.clone());
            let Some(next) = state.pending.pop() else {
                break;
            };
            state.current = next;
            state.lo = 0;
            state.hi = state.sorted.len();
        }
        // A sorted-empty or single-element receiver answers without a single
        // comparator call.
        let mut ordered = state.sorted;
        ordered.extend(std::iter::repeat_n(
            Value::Undefined,
            state.undefined_count as usize,
        ));
        let result = if state.in_place {
            self.array_like_write_back(&state.receiver, state.length, ordered)?;
            state.receiver
        } else {
            ordered.resize(
                state.length.min(usize::MAX as u64) as usize,
                Value::Undefined,
            );
            self.heap.allocate_list(ordered)?
        };
        self.stack.push(result);
        Ok(())
    }

    pub(super) fn begin_callback_driver(
        &mut self,
        function: Value,
        calls: Vec<Vec<Value>>,
        completion: CallbackCompletion,
        allow_effects: bool,
        this_arg: Value,
    ) -> Result<(), RuntimeError> {
        let calls = calls
            .into_iter()
            .map(|arguments| Value::Tuple(arguments.into()))
            .collect::<Vec<_>>();
        if calls.is_empty() {
            let finished = self.callback_finish(&CallbackDriver {
                function: function.clone(),
                calls: Vec::new(),
                next_index: 0,
                results: Vec::new(),
                completion,
                allow_effects,
                this_arg: this_arg.clone(),
                live_url_search_params: false,
                array_like: None,
            })?;
            self.stack.push(finished);
            return Ok(());
        }
        // The queue writes one call per element it was built from.
        self.charge_intrinsic_work(calls.len());
        let first = callback_arguments(calls[0].clone())?;
        let callback = CallbackDriver {
            function: function.clone(),
            calls,
            next_index: 1,
            results: Vec::new(),
            completion,
            allow_effects,
            this_arg: this_arg.clone(),
            live_url_search_params: false,
            array_like: None,
        };
        self.begin_function_call(
            function,
            this_arg,
            CallArguments::Borrowed(&first),
            ReturnTarget::Callback(Box::new(callback)),
        )
    }

    /// Starts the callback driver over a generic array-like `walk` — the
    /// generic `Array.prototype` methods' pending-call source (FIG-3787).
    /// The first call is materialized the same way every later one is: the
    /// walk resolves its index against the live receiver and the produced
    /// tuple joins `calls` as the driver's history.
    pub(super) fn begin_array_like_driver(
        &mut self,
        function: Value,
        walk: ArrayLikeWalk,
        completion: CallbackCompletion,
        this_arg: Value,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        if !matches!(return_target, ReturnTarget::Direct) {
            return Err(RuntimeError::ValidationFailed {
                reason: "TS_NESTED_DRIVER_UNSUPPORTED: a callback method reached as a callback cannot suspend".to_string(),
            });
        }
        let mut callback = CallbackDriver {
            function: function.clone(),
            calls: Vec::new(),
            next_index: 0,
            results: Vec::new(),
            completion,
            allow_effects: true,
            this_arg: this_arg.clone(),
            live_url_search_params: false,
            array_like: Some(walk),
        };
        let Some(arguments) = self.array_like_next_call(&mut callback)? else {
            let finished = self.callback_finish(&callback)?;
            self.stack.push(finished);
            return Ok(());
        };
        callback.calls.push(Value::Tuple(arguments.into()));
        callback.next_index = 1;
        let call = callback_arguments(callback.calls[0].clone())?;
        self.begin_function_call(
            function,
            this_arg,
            CallArguments::Borrowed(&call),
            ReturnTarget::Callback(Box::new(callback)),
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
        // Snapshotting the live list reads every stored entry once.
        self.charge_intrinsic_work(entries.len());
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
            this_arg: Value::Undefined,
            live_url_search_params: true,
            array_like: None,
        };
        self.begin_function_call(
            function,
            Value::Undefined,
            CallArguments::Owned(first),
            ReturnTarget::Callback(Box::new(callback)),
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
        callback_targets_receiver(callback, receiver).then_some(callback.as_mut())
    })
}

fn retain_pending_calls(callback: &mut CallbackDriver, mut retain: impl FnMut(&Value) -> bool) {
    let mut pending = callback.calls.split_off(callback.next_index);
    pending.retain(|call| retain(call));
    callback.calls.extend(pending);
}

fn clear_pending_calls(frames: &mut [CallFrame], receiver: HeapId) -> usize {
    let mut work = 0usize;
    for callback in live_collection_callbacks(frames, receiver) {
        work = work.saturating_add(callback.calls.len());
        callback.calls.truncate(callback.next_index);
    }
    work
}

/// The `arg`-th argument of the call that just completed — `calls[next_index
/// - 1]` — for the element-keyed completions.
fn callback_call_arg(calls: &[Value], next_index: usize, arg: usize) -> Value {
    match calls.get(next_index.saturating_sub(1)) {
        Some(Value::Tuple(arguments)) => arguments.get(arg).cloned().unwrap_or(Value::Undefined),
        _ => Value::Undefined,
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

impl<H: ExecutionHost> Vm<'_, H> {
    /// Makes, reads or writes a binding cell (FIG-3707): the one storage a
    /// captured, assigned binding has, shared by its frame and every closure
    /// over it.
    pub(super) fn execute_binding_cell(&mut self, op: IntrinsicOp) -> Result<(), RuntimeError> {
        match op {
            IntrinsicOp::BindingCellNew => {
                let popped = self.pop_stack()?;
                let value = self.binding_cell_member(popped)?;
                let cell = self.heap.allocate_cell(value)?;
                self.stack.push(cell);
            }
            IntrinsicOp::BindingCellGet => {
                let cell = self.pop_stack()?;
                let value = self.heap.cell_value(&cell)?;
                self.stack.push(value);
            }
            IntrinsicOp::BindingCellSet => {
                let popped = self.pop_stack()?;
                let value = self.binding_cell_member(popped)?;
                let cell = self.pop_stack()?;
                self.heap.set_cell(&cell, value.clone())?;
                self.stack.push(value);
            }
            _ => unreachable!("only the binding cell operations dispatch here"),
        }
        Ok(())
    }

    /// The value a binding cell stores, admitted the way a slot admits one: a
    /// host projection is read, and an inline compound goes through the same
    /// heap import a slot's value does (`heapify_vm_state`), so a member write
    /// through the binding reaches the object every reader of the binding
    /// sees.
    fn binding_cell_member(&mut self, value: Value) -> Result<Value, RuntimeError> {
        match materialize_value(value)? {
            value @ (Value::Tuple(_) | Value::List(_) | Value::Record(_)) => {
                // Importing the captured value walks its whole graph once.
                self.charge_intrinsic_work(deep_proportional_units(&value));
                let imported = self.heap.import_values(vec![value], 1)?;
                let Ok([value]) = <[Value; 1]>::try_from(imported) else {
                    unreachable!("an import returns one value per value it imports")
                };
                Ok(value)
            }
            value => Ok(value),
        }
    }
}
