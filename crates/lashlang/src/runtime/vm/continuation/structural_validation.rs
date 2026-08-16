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
    validate_values(&continuation.operand_stack, "operand stack")?;
    validate_heap_references(&continuation.heap.heap, &continuation.operand_stack)?;
    validate_optional_value(continuation.last_value.as_ref(), "last value")?;
    if let Some(value) = continuation.last_value.as_ref() {
        validate_heap_reference(&continuation.heap.heap, value)?;
    }
    for (index, (value, projected)) in continuation
        .slots
        .iter()
        .zip(&continuation.projected_slots)
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
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
    }
    for (key, value) in continuation.globals.iter() {
        validate_value(value, &format!("global `{key}`"))?;
        validate_heap_reference(&continuation.heap.heap, value)?;
    }
    for (depth, iterator) in continuation.iterator_stack.iter().enumerate() {
        validate_optional_value(
            iterator.restore_value.as_ref(),
            &format!("iterator {depth} restore value"),
        )?;
        if let Some(value) = iterator.restore_value.as_ref() {
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
        if let VmIteratorCursor::List { values, .. } = &iterator.cursor {
            validate_values(values, &format!("iterator {depth} values"))?;
            validate_heap_references(&continuation.heap.heap, values)?;
        }
    }
    validate_iterator_invariants(&continuation.iterator_stack, continuation.slots.len(), None)?;
    for (id, object) in continuation.heap.heap.objects_in_id_order() {
        match object {
            HeapObject::Tuple(values) | HeapObject::List(values) => {
                validate_values(values, &format!("heap object {}", id.get()))?;
                validate_heap_references(&continuation.heap.heap, values)?;
            }
            HeapObject::Record(record) => {
                for (key, value) in record.iter() {
                    validate_value(value, &format!("heap object {}.{key}", id.get()))?;
                    validate_heap_reference(&continuation.heap.heap, value)?;
                }
            }
            HeapObject::Closure { captures, .. } => {
                validate_values(captures, &format!("heap closure {}", id.get()))?;
                validate_heap_references(&continuation.heap.heap, captures)?;
            }
            HeapObject::Map(map) => {
                for (index, (key, value)) in map.entries.iter().enumerate() {
                    validate_value(key, &format!("heap Map {} key {index}", id.get()))?;
                    validate_heap_reference(&continuation.heap.heap, key)?;
                    validate_value(value, &format!("heap Map {} value {index}", id.get()))?;
                    validate_heap_reference(&continuation.heap.heap, value)?;
                }
            }
            HeapObject::Set(set) => {
                validate_values(&set.values, &format!("heap Set {}", id.get()))?;
                validate_heap_references(&continuation.heap.heap, &set.values)?;
            }
            HeapObject::RegExp(regexp) => {
                if regexp.last_index > crate::runtime::heap::MAX_JAVASCRIPT_LENGTH {
                    return Err(ContinuationError::UnserializableValue {
                        location: format!("heap RegExp {}", id.get()),
                        variant: "lastIndex beyond JavaScript's maximum safe length",
                    });
                }
            }
            HeapObject::Date(_) => {}
            HeapObject::Error(error) => {
                if let Some(cause) = &error.cause {
                    validate_value(cause, &format!("heap {} error cause", id.get()))?;
                    validate_heap_reference(&continuation.heap.heap, cause)?;
                }
                if let Some(errors) = &error.errors {
                    validate_value(errors, &format!("heap {} aggregate errors", id.get()))?;
                    validate_heap_reference(&continuation.heap.heap, errors)?;
                }
            }
            HeapObject::Url(url) => {
                validate_value(&url.search_params, &format!("heap URL {} params", id.get()))?;
                validate_heap_reference(&continuation.heap.heap, &url.search_params)?;
            }
            HeapObject::UrlSearchParams(_) => {}
        }
    }
    for (depth, frame) in continuation.frame_stack.iter().enumerate() {
        if frame.slots.len() != frame.projected_slots.len() {
            return Err(ContinuationError::SlotCountMismatch {
                expected: frame.slots.len(),
                actual: frame.projected_slots.len(),
            });
        }
        if frame.operand_stack_base > continuation.operand_stack.len() {
            return Err(ContinuationError::UnserializableValue {
                location: format!("frame {depth} operand stack base"),
                variant: "out-of-bounds stack base",
            });
        }
        validate_values(
            &frame.slots.iter().flatten().cloned().collect::<Vec<_>>(),
            &format!("frame {depth} slots"),
        )?;
        validate_heap_references(
            &continuation.heap.heap,
            &frame.slots.iter().flatten().cloned().collect::<Vec<_>>(),
        )?;
        for value in frame.globals.values() {
            validate_value(value, &format!("frame {depth} globals"))?;
            validate_heap_reference(&continuation.heap.heap, value)?;
        }
        validate_iterator_invariants(&frame.iterator_stack, frame.slots.len(), Some(depth))?;
        if let VmFrameReturnContinuation::Callback {
            function,
            calls,
            next_index,
            results,
            completion,
            allow_effects: _,
            live_url_search_params,
        } = &frame.return_target
        {
            let result_cursor_is_valid = match completion {
                VmCallbackCompletion::Collect => results.len().saturating_add(1) == *next_index,
                VmCallbackCompletion::Discard => *next_index >= 1 && results.is_empty(),
            };
            let live_cursor_is_valid = if *live_url_search_params {
                calls.len() == 1
                    && matches!(calls.first(), Some(Value::Ref(id)) if matches!(continuation.heap.heap.get(*id), Ok(HeapObject::UrlSearchParams(_))))
                    && matches!(completion, VmCallbackCompletion::Discard)
            } else {
                *next_index <= calls.len()
                    && calls.iter().all(|call| matches!(call, Value::Tuple(_)))
            };
            if !live_cursor_is_valid || !result_cursor_is_valid {
                return Err(ContinuationError::UnserializableValue {
                    location: format!("frame {depth} callback"),
                    variant: "invalid callback cursor",
                });
            }
            if !*live_url_search_params && calls.iter().any(|call| !matches!(call, Value::Tuple(_)))
            {
                return Err(ContinuationError::UnserializableValue {
                    location: format!("frame {depth} callback"),
                    variant: "invalid callback arguments",
                });
            }
            validate_value(function, &format!("frame {depth} callback function"))?;
            validate_heap_reference(&continuation.heap.heap, function)?;
            validate_values(calls, &format!("frame {depth} callback calls"))?;
            validate_heap_references(&continuation.heap.heap, calls)?;
            validate_values(results, &format!("frame {depth} callback results"))?;
            validate_heap_references(&continuation.heap.heap, results)?;
        }
    }
    for (index, handler) in continuation.handler_stack.iter().enumerate() {
        let Some((owner_function, iterator_count)) =
            continuation_frame_owner(continuation, handler.frame_depth)
        else {
            return Err(ContinuationError::HandlerFrameDepthOutOfBounds {
                handler: index,
                frame_depth: handler.frame_depth,
                frame_count: continuation.frame_stack.len(),
            });
        };
        if owner_function != handler.frame_function {
            return Err(ContinuationError::HandlerFrameIdentityMismatch {
                handler: index,
                frame_depth: handler.frame_depth,
            });
        }
        if handler.operand_stack_depth > continuation.operand_stack.len() {
            return Err(ContinuationError::HandlerStackDepthOutOfBounds {
                handler: index,
                stack_depth: handler.operand_stack_depth,
                stack_size: continuation.operand_stack.len(),
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
    // Ordering carries semantics the VM trusts absolutely, so the parts of it
    // that need no compiled program are checked here: a handler stack unwinds
    // outwards, never inwards.
    for index in 1..continuation.handler_stack.len() {
        let outer = &continuation.handler_stack[index - 1];
        let inner = &continuation.handler_stack[index];
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
    for index in 1..continuation.finally_stack.len() {
        let outer = &continuation.finally_stack[index - 1];
        let inner = &continuation.finally_stack[index];
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
    for (index, finally) in continuation.finally_stack.iter().enumerate() {
        let Some((owner_function, _)) = continuation_frame_owner(continuation, finally.frame_depth)
        else {
            return Err(ContinuationError::FinallyFrameIdentityMismatch { finally: index });
        };
        if owner_function != finally.frame_function {
            return Err(ContinuationError::FinallyFrameIdentityMismatch { finally: index });
        }
        if finally.handler_stack_depth > continuation.handler_stack.len() {
            return Err(ContinuationError::FinallyHandlerDepthOutOfBounds {
                finally: index,
                handler_depth: finally.handler_stack_depth,
                handler_count: continuation.handler_stack.len(),
            });
        }
        if finally.operand_stack_depth > continuation.operand_stack.len() {
            return Err(ContinuationError::FinallyStackDepthOutOfBounds {
                finally: index,
                stack_depth: finally.operand_stack_depth,
                stack_size: continuation.operand_stack.len(),
            });
        }
        if let VmFinallyCompletionContinuation::Throw { value, origin } = &finally.completion {
            validate_value(value, &format!("finally {index} thrown value"))?;
            validate_heap_reference(&continuation.heap.heap, value)?;
            if let Some(origin) = origin {
                // A pending origin is a routed runtime failure, never a guest
                // value: `UncaughtException` is the one variant carrying a
                // `Value`, and accepting it here would smuggle an unrooted
                // heap reference past the forest rule.
                if matches!(origin.error, RuntimeError::UncaughtException { .. }) {
                    return Err(ContinuationError::UnserializableValue {
                        location: format!("finally {index} pending error origin"),
                        variant: "uncaught exception is not a routed runtime failure",
                    });
                }
            }
        }
    }
    let roots = continuation_forest_roots(continuation);
    let validation = if continuation.reference_semantics {
        continuation.heap.heap.validate_persisted_graph(&roots)
    } else {
        continuation.heap.heap.validate_persisted_forest(&roots)
    };
    validation.map_err(|reason| ContinuationError::UnserializableValue {
        location: format!("continuation heap: {reason}"),
        variant: "invalid heap object graph",
    })?;
    Ok(())
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
