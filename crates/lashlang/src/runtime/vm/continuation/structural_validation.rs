//! Structural validation of a decoded `VmContinuation`: everything that can
//! be checked from the blob alone, before any compiled program is involved.

use super::*;

pub(super) fn continuation_forest_roots(continuation: &VmContinuation) -> PersistedRoots<'_> {
    let mut roots = PersistedRoots::default();
    visit_vm_roots(continuation, &mut roots);
    roots
}

fn validate_iterator_invariants(
    iterators: &[VmIteratorContinuation],
    slot_count: usize,
    frame: Option<usize>,
) -> Result<(), ContinuationError> {
    for (iterator_index, iterator) in iterators.iter().enumerate() {
        if iterator.binding_slot >= slot_count {
            return match frame {
                Some(frame) => Err(ContinuationError::FrameIteratorBindingOutOfBounds {
                    frame,
                    iterator: iterator_index,
                    binding_slot: iterator.binding_slot,
                    slot_count,
                }),
                None => Err(ContinuationError::IteratorBindingOutOfBounds {
                    iterator: iterator_index,
                    binding_slot: iterator.binding_slot,
                    slot_count,
                }),
            };
        }
        if matches!(iterator.cursor, VmIteratorCursor::Range { step: 0, .. }) {
            return match frame {
                Some(frame) => Err(ContinuationError::FrameZeroRangeStep {
                    frame,
                    iterator: iterator_index,
                }),
                None => Err(ContinuationError::ZeroRangeStep {
                    iterator: iterator_index,
                }),
            };
        }
    }
    Ok(())
}

struct ContinuationValidator<'a> {
    continuation: &'a VmContinuation,
    heap: &'a Heap,
}

impl<'a> ContinuationValidator<'a> {
    fn new(continuation: &'a VmContinuation) -> Self {
        Self {
            continuation,
            heap: &continuation.heap.heap,
        }
    }

    fn validate_stack_and_last(&self) -> Result<(), ContinuationError> {
        validate_values(&self.continuation.operand_stack, "operand stack")?;
        validate_heap_references(self.heap, &self.continuation.operand_stack)?;
        validate_optional_value(self.continuation.last_value.as_ref(), "last value")?;
        if let Some(value) = self.continuation.last_value.as_ref() {
            validate_heap_reference(self.heap, value)?;
        }
        Ok(())
    }

    fn validate_slots(&self) -> Result<(), ContinuationError> {
        for (index, (value, projected)) in self
            .continuation
            .slots
            .iter()
            .zip(&self.continuation.projected_slots)
            .enumerate()
        {
            if *projected {
                return Err(ContinuationError::UnserializableValue {
                    location: format!("slot {index}"),
                    variant: "Projected",
                });
            }
            validate_optional_value(value.as_ref(), &format!("slot {index}"))?;
            if let Some(value) = value.as_ref() {
                validate_heap_reference(self.heap, value)?;
            }
        }
        Ok(())
    }

    fn validate_globals(&self) -> Result<(), ContinuationError> {
        for (key, value) in self.continuation.globals.iter() {
            validate_value(value, &format!("global `{key}`"))?;
            validate_heap_reference(self.heap, value)?;
        }
        Ok(())
    }

    fn validate_iterators(&self) -> Result<(), ContinuationError> {
        for (depth, iterator) in self.continuation.iterator_stack.iter().enumerate() {
            validate_optional_value(
                iterator.restore_value.as_ref(),
                &format!("iterator {depth} restore value"),
            )?;
            if let Some(value) = iterator.restore_value.as_ref() {
                validate_heap_reference(self.heap, value)?;
            }
            if let VmIteratorCursor::List { values, .. } = &iterator.cursor {
                validate_values(values, &format!("iterator {depth} values"))?;
                validate_heap_references(self.heap, values)?;
            }
        }
        validate_iterator_invariants(
            &self.continuation.iterator_stack,
            self.continuation.slots.len(),
            None,
        )
    }

    fn validate_heap_objects(&self) -> Result<(), ContinuationError> {
        for (id, object) in self.heap.objects_in_id_order() {
            self.validate_heap_object(id, object)?;
        }
        Ok(())
    }

    fn validate_heap_object(
        &self,
        id: HeapId,
        object: &HeapObject,
    ) -> Result<(), ContinuationError> {
        match object {
            HeapObject::Tuple(values) | HeapObject::List(values) => {
                self.validate_heap_values(values, &format!("heap object {}", id.get()))
            }
            HeapObject::Record(record) => self.validate_heap_record(id, record),
            HeapObject::Closure { captures, .. } => {
                self.validate_heap_values(captures, &format!("heap closure {}", id.get()))
            }
            HeapObject::Map(map) => self.validate_heap_map(id, map),
            HeapObject::Set(set) => {
                self.validate_heap_values(&set.values, &format!("heap Set {}", id.get()))
            }
            HeapObject::RegExp(regexp) => self.validate_heap_regexp(id, regexp),
            HeapObject::RegExpMatch(result) => self.validate_heap_regexp_match(id, result),
            HeapObject::Date(_) | HeapObject::UrlSearchParams(_) => Ok(()),
            HeapObject::Error(error) => self.validate_heap_error(id, error),
            HeapObject::Url(url) => {
                validate_value(&url.search_params, &format!("heap URL {} params", id.get()))?;
                validate_heap_reference(self.heap, &url.search_params)
            }
        }
    }

    fn validate_heap_values(
        &self,
        values: &[Value],
        location: &str,
    ) -> Result<(), ContinuationError> {
        validate_values(values, location)?;
        validate_heap_references(self.heap, values)
    }

    fn validate_heap_record(&self, id: HeapId, record: &Record) -> Result<(), ContinuationError> {
        for (key, value) in record.iter() {
            validate_value(value, &format!("heap object {}.{key}", id.get()))?;
            validate_heap_reference(self.heap, value)?;
        }
        Ok(())
    }

    fn validate_heap_map(&self, id: HeapId, map: &MapObject) -> Result<(), ContinuationError> {
        for (index, (key, value)) in map.entries.iter().enumerate() {
            validate_value(key, &format!("heap Map {} key {index}", id.get()))?;
            validate_heap_reference(self.heap, key)?;
            validate_value(value, &format!("heap Map {} value {index}", id.get()))?;
            validate_heap_reference(self.heap, value)?;
        }
        Ok(())
    }

    fn validate_heap_regexp(
        &self,
        id: HeapId,
        regexp: &RegExpObject,
    ) -> Result<(), ContinuationError> {
        if regexp.last_index > crate::runtime::heap::MAX_JAVASCRIPT_LENGTH {
            return Err(ContinuationError::UnserializableValue {
                location: format!("heap RegExp {}", id.get()),
                variant: "lastIndex beyond JavaScript's maximum safe length",
            });
        }
        Ok(())
    }

    fn validate_heap_regexp_match(
        &self,
        id: HeapId,
        result: &RegExpMatchObject,
    ) -> Result<(), ContinuationError> {
        self.validate_heap_values(&result.items, &format!("heap RegExp match {}", id.get()))?;
        for (name, value) in [
            ("index", &result.index),
            ("input", &result.input),
            ("groups", &result.groups),
        ] {
            validate_value(value, &format!("heap RegExp match {} {name}", id.get()))?;
            validate_heap_reference(self.heap, value)?;
        }
        Ok(())
    }

    fn validate_heap_error(
        &self,
        id: HeapId,
        error: &ErrorObject,
    ) -> Result<(), ContinuationError> {
        for (value, location) in [
            (
                error.cause.as_ref(),
                format!("heap {} error cause", id.get()),
            ),
            (
                error.errors.as_ref(),
                format!("heap {} aggregate errors", id.get()),
            ),
        ] {
            if let Some(value) = value {
                validate_value(value, &location)?;
                validate_heap_reference(self.heap, value)?;
            }
        }
        Ok(())
    }

    fn validate_frames(&self) -> Result<(), ContinuationError> {
        for (depth, frame) in self.continuation.frame_stack.iter().enumerate() {
            if frame.slots.len() != frame.projected_slots.len() {
                return Err(ContinuationError::SlotCountMismatch {
                    expected: frame.slots.len(),
                    actual: frame.projected_slots.len(),
                });
            }
            if frame.operand_stack_base > self.continuation.operand_stack.len() {
                return Err(ContinuationError::UnserializableValue {
                    location: format!("frame {depth} operand stack base"),
                    variant: "out-of-bounds stack base",
                });
            }
            let values = frame.slots.iter().flatten().cloned().collect::<Vec<_>>();
            validate_values(&values, &format!("frame {depth} slots"))?;
            validate_heap_references(self.heap, &values)?;
            for value in frame.globals.values() {
                validate_value(value, &format!("frame {depth} globals"))?;
                validate_heap_reference(self.heap, value)?;
            }
            validate_iterator_invariants(&frame.iterator_stack, frame.slots.len(), Some(depth))?;
            self.validate_frame_return_target(depth, frame)?;
        }
        Ok(())
    }

    fn validate_frame_return_target(
        &self,
        depth: usize,
        frame: &VmFrameContinuation,
    ) -> Result<(), ContinuationError> {
        let VmFrameReturnContinuation::Callback {
            function,
            calls,
            next_index,
            results,
            completion,
            allow_effects: _,
            live_url_search_params,
        } = &frame.return_target
        else {
            return Ok(());
        };
        self.validate_callback_cursors(
            depth,
            calls,
            *next_index,
            results,
            completion,
            *live_url_search_params,
        )?;
        validate_value(function, &format!("frame {depth} callback function"))?;
        validate_heap_reference(self.heap, function)?;
        validate_values(calls, &format!("frame {depth} callback calls"))?;
        validate_heap_references(self.heap, calls)?;
        validate_values(results, &format!("frame {depth} callback results"))?;
        validate_heap_references(self.heap, results)
    }

    fn validate_callback_cursors(
        &self,
        depth: usize,
        calls: &[Value],
        next_index: usize,
        results: &[Value],
        completion: &VmCallbackCompletion,
        live_url_search_params: bool,
    ) -> Result<(), ContinuationError> {
        let result_valid = match completion {
            VmCallbackCompletion::Collect => results.len().saturating_add(1) == next_index,
            VmCallbackCompletion::Discard => next_index >= 1 && results.is_empty(),
        };
        let cursor_valid = if live_url_search_params {
            calls.len() == 1
                && matches!(calls.first(), Some(Value::Ref(id)) if matches!(self.heap.get(*id), Ok(HeapObject::UrlSearchParams(_))))
                && matches!(completion, VmCallbackCompletion::Discard)
        } else {
            next_index <= calls.len() && calls.iter().all(|call| matches!(call, Value::Tuple(_)))
        };
        if !cursor_valid || !result_valid {
            return Err(ContinuationError::UnserializableValue {
                location: format!("frame {depth} callback"),
                variant: "invalid callback cursor",
            });
        }
        if !live_url_search_params && calls.iter().any(|call| !matches!(call, Value::Tuple(_))) {
            return Err(ContinuationError::UnserializableValue {
                location: format!("frame {depth} callback"),
                variant: "invalid callback arguments",
            });
        }
        Ok(())
    }

    fn validate_handlers(&self) -> Result<(), ContinuationError> {
        for (index, handler) in self.continuation.handler_stack.iter().enumerate() {
            let Some((owner_function, iterator_count)) =
                continuation_frame_owner(self.continuation, handler.frame_depth)
            else {
                return Err(ContinuationError::HandlerFrameDepthOutOfBounds {
                    handler: index,
                    frame_depth: handler.frame_depth,
                    frame_count: self.continuation.frame_stack.len(),
                });
            };
            if owner_function != handler.frame_function {
                return Err(ContinuationError::HandlerFrameIdentityMismatch {
                    handler: index,
                    frame_depth: handler.frame_depth,
                });
            }
            if handler.operand_stack_depth > self.continuation.operand_stack.len() {
                return Err(ContinuationError::HandlerStackDepthOutOfBounds {
                    handler: index,
                    stack_depth: handler.operand_stack_depth,
                    stack_size: self.continuation.operand_stack.len(),
                });
            }
            if handler.iterator_stack_depth > iterator_count {
                return Err(ContinuationError::HandlerIteratorDepthOutOfBounds {
                    handler: index,
                    iterator_depth: handler.iterator_stack_depth,
                    iterator_count,
                });
            }
        }
        for index in 1..self.continuation.handler_stack.len() {
            let outer = &self.continuation.handler_stack[index - 1];
            let inner = &self.continuation.handler_stack[index];
            let reason = if inner.frame_depth < outer.frame_depth {
                Some("its frame is shallower than the enclosing handler's")
            } else if inner.frame_depth == outer.frame_depth
                && inner.frame_function == outer.frame_function
                && (inner.operand_stack_depth < outer.operand_stack_depth
                    || inner.iterator_stack_depth < outer.iterator_stack_depth)
            {
                Some("it restores to a shallower depth than the enclosing handler")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(ContinuationError::HandlerNestingNotMonotonic {
                    handler: index,
                    outer: index - 1,
                    reason,
                });
            }
        }
        Ok(())
    }

    fn validate_finally(&self) -> Result<(), ContinuationError> {
        for index in 1..self.continuation.finally_stack.len() {
            let outer = &self.continuation.finally_stack[index - 1];
            let inner = &self.continuation.finally_stack[index];
            let reason = if inner.handler_stack_depth < outer.handler_stack_depth {
                Some("it claims fewer handlers than the enclosing finally")
            } else if inner.frame_depth < outer.frame_depth {
                Some("its frame is shallower than the enclosing finally's")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(ContinuationError::FinallyNestingNotMonotonic {
                    finally: index,
                    outer: index - 1,
                    reason,
                });
            }
        }
        for (index, finally) in self.continuation.finally_stack.iter().enumerate() {
            self.validate_finally_entry(index, finally)?;
        }
        Ok(())
    }

    fn validate_finally_entry(
        &self,
        index: usize,
        finally: &VmFinallyContinuation,
    ) -> Result<(), ContinuationError> {
        let Some((owner_function, _)) =
            continuation_frame_owner(self.continuation, finally.frame_depth)
        else {
            return Err(ContinuationError::FinallyFrameIdentityMismatch { finally: index });
        };
        if owner_function != finally.frame_function {
            return Err(ContinuationError::FinallyFrameIdentityMismatch { finally: index });
        }
        if finally.handler_stack_depth > self.continuation.handler_stack.len() {
            return Err(ContinuationError::FinallyHandlerDepthOutOfBounds {
                finally: index,
                handler_depth: finally.handler_stack_depth,
                handler_count: self.continuation.handler_stack.len(),
            });
        }
        if finally.operand_stack_depth > self.continuation.operand_stack.len() {
            return Err(ContinuationError::FinallyStackDepthOutOfBounds {
                finally: index,
                stack_depth: finally.operand_stack_depth,
                stack_size: self.continuation.operand_stack.len(),
            });
        }
        if let VmFinallyCompletionContinuation::Throw { value, origin } = &finally.completion {
            validate_value(value, &format!("finally {index} thrown value"))?;
            validate_heap_reference(self.heap, value)?;
            if let Some(origin) = origin
                && matches!(origin.error, RuntimeError::UncaughtException { .. })
            {
                return Err(ContinuationError::UnserializableValue {
                    location: format!("finally {index} pending error origin"),
                    variant: "uncaught exception is not a routed runtime failure",
                });
            }
        }
        Ok(())
    }

    fn validate_heap_graph(&self) -> Result<(), ContinuationError> {
        let roots = continuation_forest_roots(self.continuation);
        let validation = if self.continuation.reference_semantics {
            self.heap.validate_persisted_graph(&roots)
        } else {
            self.heap.validate_persisted_forest(&roots)
        };
        validation.map_err(|reason| ContinuationError::UnserializableValue {
            location: format!("continuation heap: {reason}"),
            variant: "invalid heap object graph",
        })
    }
}

pub(super) fn validate_continuation(
    continuation: &VmContinuation,
) -> Result<(), ContinuationError> {
    if continuation.format_version != VM_CONTINUATION_FORMAT_VERSION {
        return Err(ContinuationError::FormatVersionMismatch {
            expected: VM_CONTINUATION_FORMAT_VERSION,
            found: continuation.format_version,
        });
    }
    if continuation.slots.len() != continuation.projected_slots.len() {
        return Err(ContinuationError::SlotCountMismatch {
            expected: continuation.slots.len(),
            actual: continuation.projected_slots.len(),
        });
    }
    if continuation.active_function.is_none() && !continuation.frame_stack.is_empty() {
        return Err(ContinuationError::UnserializableValue {
            location: "frame stack".to_string(),
            variant: "frames without an active function",
        });
    }
    if continuation.active_function.is_some()
        && continuation
            .frame_stack
            .first()
            .is_none_or(|frame| frame.function.is_some())
    {
        return Err(ContinuationError::MissingRootFrame);
    }
    let validator = ContinuationValidator::new(continuation);
    validator.validate_stack_and_last()?;
    validator.validate_slots()?;
    validator.validate_globals()?;
    validator.validate_iterators()?;
    validator.validate_heap_objects()?;
    validator.validate_frames()?;
    validator.validate_handlers()?;
    validator.validate_finally()?;
    validator.validate_heap_graph()
}

fn continuation_frame_owner(
    continuation: &VmContinuation,
    frame_depth: usize,
) -> Option<(Option<u32>, usize)> {
    if frame_depth == continuation.frame_stack.len() {
        return Some((
            continuation.active_function,
            continuation.iterator_stack.len(),
        ));
    }
    continuation
        .frame_stack
        .get(frame_depth)
        .map(|frame| (frame.function, frame.iterator_stack.len()))
}

pub(super) fn validate_heap_references(
    heap: &Heap,
    values: &[Value],
) -> Result<(), ContinuationError> {
    for value in values {
        validate_heap_reference(heap, value)?;
    }
    Ok(())
}

pub(super) fn validate_heap_reference(heap: &Heap, value: &Value) -> Result<(), ContinuationError> {
    heap.validate_resolvable_refs(value)
        .map_err(|_| ContinuationError::UnserializableValue {
            location: "continuation heap root".to_string(),
            variant: "dangling heap reference",
        })
}

pub(super) fn validate_values(values: &[Value], location: &str) -> Result<(), ContinuationError> {
    for (index, value) in values.iter().enumerate() {
        validate_value(value, &format!("{location}[{index}]"))?;
    }
    Ok(())
}

pub(super) fn validate_optional_value(
    value: Option<&Value>,
    location: &str,
) -> Result<(), ContinuationError> {
    if let Some(value) = value {
        validate_value(value, location)?;
    }
    Ok(())
}

pub(super) fn validate_value(value: &Value, location: &str) -> Result<(), ContinuationError> {
    match value {
        Value::Projected(_) => Err(ContinuationError::UnserializableValue {
            location: location.to_string(),
            variant: "Projected",
        }),
        Value::Tuple(values) | Value::List(values) => {
            for (index, value) in values.iter().enumerate() {
                validate_value(value, &format!("{location}[{index}]"))?;
            }
            Ok(())
        }
        Value::Record(record) => {
            for (key, value) in record.iter() {
                validate_value(value, &format!("{location}.{key}"))?;
            }
            Ok(())
        }
        Value::Null
        | Value::Undefined
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Image(_)
        | Value::Resource(_)
        | Value::Ref(_) => Ok(()),
    }
}
