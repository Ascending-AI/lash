use super::super::javascript::javascript_to_number;
use super::javascript::js_stdlib_error;
use super::javascript_stdlib::{
    array_includes, array_index_of, array_last_index_of, clamp_relative_index,
    last_index_exclusive, normalized_instance_arguments,
};
use super::*;

pub(super) fn javascript_array_method_for_value(
    heap: &Heap,
    method: &str,
    target: &Value,
    items: &[Value],
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if method == "valueOf" && args.is_empty() {
        // Defensive arm for non-heap dispatch paths; currently unreachable via
        // guest code.
        return Ok(target.clone());
    }
    super::javascript::javascript_array_method(heap, method, items, args)
}

/// A regexp match is array-shaped to JavaScript, so its method dispatch lives
/// beside ordinary array methods while this helper preserves heap identity for
/// `valueOf`.
pub(super) fn javascript_regexp_match_method(
    heap: &Heap,
    method: &str,
    receiver: HeapId,
    items: &[Value],
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if method == "valueOf" && args.is_empty() {
        return Ok(Value::Ref(receiver));
    }
    super::javascript::javascript_array_method(heap, method, items, args)
}

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn execute_javascript_array_heap_method(
        &mut self,
        method: &str,
        receiver: HeapId,
        args: &[Value],
    ) -> Result<bool, RuntimeError> {
        let HeapObject::List(current) = self.heap.get(receiver)? else {
            return Ok(false);
        };
        // The search trio only read the vector, so they run on the live heap
        // values: a needle or member that is a reference — a RegExp, say —
        // compares by heap identity here where the value path would have to
        // detach it across the host boundary (FIG-3658).
        if matches!(method, "indexOf" | "includes" | "lastIndexOf") {
            let argument_count = args.len();
            let args = normalized_instance_arguments(method, args);
            let (needle, from) = (args[0].clone(), args[1].clone());
            // An empty array answers before `fromIndex` is converted, so its
            // `valueOf` never runs (ECMA-262 steps 3-4).
            let result = if current.is_empty() {
                Ok(if method == "includes" {
                    Value::Bool(false)
                } else {
                    Value::Number(-1.0)
                })
            } else if method == "lastIndexOf" && argument_count < 2 {
                array_last_index_of(current, &needle, current.len())
            } else {
                let from = self.heap.javascript_to_number(&from)?;
                match method {
                    "includes" => {
                        array_includes(current, &needle, clamp_relative_index(from, current.len()))
                    }
                    "indexOf" => {
                        array_index_of(current, &needle, clamp_relative_index(from, current.len()))
                    }
                    _ => last_index_exclusive(from, current.len())
                        .map_or(Ok(Value::Number(-1.0)), |end| {
                            array_last_index_of(current, &needle, end)
                        }),
                }
            }?;
            self.stack.push(result);
            return Ok(true);
        }
        // `push` is the one array method a program runs once per loop
        // iteration, so it is the one that must not rebuild the array. It
        // grows the vector the heap already owns instead of cloning it, and
        // returns ECMA's new length like the rebuild did (FIG-3063).
        if method == "push" {
            let length = self.heap.append_javascript_list(receiver, args)?;
            self.stack.push(Value::Number(length as f64));
            return Ok(true);
        }
        let mut values = current.clone();
        let result = match method {
            "fill" => {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                let start = relative_bound(args.get(1), values.len(), 0);
                // `undefined` is the absent `end`: ToIntegerOrInfinity reads
                // it as NaN, but the parameter's default is the length.
                let end = match args.get(2) {
                    None | Some(Value::Undefined) => values.len(),
                    Some(_) => relative_bound(args.get(2), values.len(), values.len()),
                }
                .max(start);
                values[start..end].fill(value);
                self.heap.replace_javascript_list(receiver, values)?;
                Value::Ref(receiver)
            }
            "reverse" => {
                values.reverse();
                self.heap.replace_javascript_list(receiver, values)?;
                Value::Ref(receiver)
            }
            "copyWithin" => {
                copy_within(&mut values, args);
                self.heap.replace_javascript_list(receiver, values)?;
                Value::Ref(receiver)
            }
            "splice" => {
                let start = relative_bound(args.first(), values.len(), 0);
                let delete = if args.is_empty() {
                    0
                } else if args.len() == 1 {
                    values.len() - start
                } else {
                    clamp_delete_count(javascript_to_number(&args[1]), values.len() - start)
                };
                let removed = values
                    .splice(start..start + delete, args.iter().skip(2).cloned())
                    .collect::<Vec<_>>();
                self.heap.replace_javascript_list(receiver, values)?;
                self.heap.allocate_list(removed)?
            }
            // The ends-of-the-array mutators that still rebuild. They ride the
            // live-receiver path `splice` established: mutate the cloned
            // vector, hand it back through `replace_javascript_list` so the
            // byte accounting and the memory bound answer, and return what
            // ECMA returns — the new length for `unshift`, the removed element
            // (or `undefined`) for the shrinking pair. Each of them moves every
            // surviving member anyway, so the rebuild costs what the operation
            // costs; `push` is handled above because it does not.
            "unshift" => {
                values.splice(0..0, args.iter().cloned());
                let length = values.len();
                self.heap.replace_javascript_list(receiver, values)?;
                Value::Number(length as f64)
            }
            "pop" => {
                let removed = values.pop().unwrap_or(Value::Undefined);
                self.heap.replace_javascript_list(receiver, values)?;
                removed
            }
            "shift" => {
                let removed = if values.is_empty() {
                    Value::Undefined
                } else {
                    values.remove(0)
                };
                self.heap.replace_javascript_list(receiver, values)?;
                removed
            }
            "sort" if args.is_empty() || matches!(args, [Value::Undefined]) => {
                let mut keyed = values
                    .into_iter()
                    .map(|value| {
                        let key = if matches!(value, Value::Undefined) {
                            None
                        } else {
                            Some(self.heap.javascript_to_string(&value)?)
                        };
                        Ok((value, key))
                    })
                    .collect::<Result<Vec<_>, RuntimeError>>()?;
                keyed.sort_by(|(_, left), (_, right)| match (left, right) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(left), Some(right)) => left.encode_utf16().cmp(right.encode_utf16()),
                });
                self.heap.replace_javascript_list(
                    receiver,
                    keyed.into_iter().map(|(value, _)| value).collect(),
                )?;
                Value::Ref(receiver)
            }
            "toReversed" if args.is_empty() => {
                values.reverse();
                self.heap.allocate_list(values)?
            }
            "toSorted" if args.is_empty() || matches!(args, [Value::Undefined]) => {
                let mut keyed = values
                    .into_iter()
                    .map(|value| {
                        let key = if matches!(value, Value::Undefined) {
                            None
                        } else {
                            Some(self.heap.javascript_to_string(&value)?)
                        };
                        Ok((value, key))
                    })
                    .collect::<Result<Vec<_>, RuntimeError>>()?;
                keyed.sort_by(|(_, left), (_, right)| match (left, right) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(left), Some(right)) => left.encode_utf16().cmp(right.encode_utf16()),
                });
                self.heap
                    .allocate_list(keyed.into_iter().map(|(value, _)| value).collect())?
            }
            "toSpliced" => {
                let start = relative_bound(args.first(), values.len(), 0);
                let delete = if args.is_empty() {
                    0
                } else if args.len() == 1 {
                    values.len() - start
                } else {
                    clamp_delete_count(javascript_to_number(&args[1]), values.len() - start)
                };
                values.splice(start..start + delete, args.iter().skip(2).cloned());
                self.heap.allocate_list(values)?
            }
            "with" => {
                let [index, value] = args else {
                    return Err(js_stdlib_error("Array.with expects exactly two arguments"));
                };
                let relative = javascript_to_number(index);
                let Some(index) = relative_index(relative, values.len()) else {
                    return Err(RuntimeError::range_error(format!(
                        "Invalid index : {}",
                        crate::runtime::javascript_to_string(&Value::Number(relative.trunc()))
                    )));
                };
                values[index] = value.clone();
                self.heap.allocate_list(values)?
            }
            // `Array.prototype.valueOf` is the receiver itself, so it has to be
            // answered here where the heap id is in hand. Routing it through the
            // value path handed back a detached copy, and `a.valueOf().push(x)`
            // then wrote to something the original never sees.
            "valueOf" if args.is_empty() => Value::Ref(receiver),
            _ => return Ok(false),
        };
        self.stack.push(result);
        Ok(true)
    }
}

/// `Array.prototype.copyWithin` on a materialized vector: the target, start
/// and end bounds are relative indexes, the count is `min(end - start, len -
/// target)`, and an overlapping range copies backward so a shifted read never
/// sees a value already moved. ECMA-262 23.1.3.3.
pub(super) fn copy_within(values: &mut [Value], args: &[Value]) {
    let len = values.len();
    let to = relative_bound(args.first(), len, 0);
    let from = relative_bound(args.get(1), len, 0);
    // `undefined` is the absent `end`: ToIntegerOrInfinity reads it as NaN,
    // but the parameter's default is the length, not the zero NaN implies.
    let end = match args.get(2) {
        None | Some(Value::Undefined) => len,
        Some(_) => relative_bound(args.get(2), len, len),
    };
    let count = end.saturating_sub(from).min(len.saturating_sub(to));
    if count == 0 {
        return;
    }
    if from < to && to < from + count {
        for index in (0..count).rev() {
            values[to + index] = values[from + index].clone();
        }
    } else {
        for index in 0..count {
            values[to + index] = values[from + index].clone();
        }
    }
}

fn relative_bound(value: Option<&Value>, len: usize, default: usize) -> usize {
    let Some(value) = value else { return default };
    let value = javascript_to_number(value);
    if value.is_nan() || value == f64::NEG_INFINITY {
        0
    } else if value == f64::INFINITY {
        len
    } else if value < 0.0 {
        (len as f64 + value.trunc()).clamp(0.0, len as f64) as usize
    } else {
        value.trunc().min(len as f64) as usize
    }
}

fn clamp_delete_count(value: f64, available: usize) -> usize {
    if value.is_nan() || value <= 0.0 {
        0
    } else if value == f64::INFINITY {
        available
    } else {
        (value.trunc() as usize).min(available)
    }
}

fn relative_index(value: f64, len: usize) -> Option<usize> {
    let value = if value.is_nan() { 0.0 } else { value.trunc() };
    let index = if value < 0.0 {
        len as f64 + value
    } else {
        value
    };
    (index >= 0.0 && index < len as f64).then_some(index as usize)
}
