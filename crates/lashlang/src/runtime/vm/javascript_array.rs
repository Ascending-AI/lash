use super::super::javascript::javascript_to_number;
use super::javascript::js_stdlib_error;
use super::*;

pub(super) fn javascript_array_method_for_value(
    method: &str,
    target: &Value,
    items: &[Value],
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if method == "valueOf" && args.is_empty() {
        return Ok(target.clone());
    }
    super::javascript::javascript_array_method(method, items, args)
}

pub(super) fn javascript_regexp_match_method(
    method: &str,
    receiver: HeapId,
    items: &[Value],
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if method == "valueOf" && args.is_empty() {
        return Ok(Value::Ref(receiver));
    }
    super::javascript::javascript_array_method(method, items, args)
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
        let mut values = current.clone();
        let result = match method {
            "fill" => {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                let start = relative_bound(args.get(1), values.len(), 0);
                let end = relative_bound(args.get(2), values.len(), values.len()).max(start);
                values[start..end].fill(value);
                self.heap.replace_javascript_list(receiver, values)?;
                Value::Ref(receiver)
            }
            "reverse" => {
                values.reverse();
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
            // The four ends-of-the-array mutators. They ride the same
            // live-receiver path `splice` established: mutate the cloned
            // vector, hand it back through `replace_javascript_list` so the
            // byte accounting and the memory bound answer, and return what
            // ECMA returns — the new length for the growing pair, the removed
            // element (or `undefined`) for the shrinking one.
            "push" => {
                values.extend(args.iter().cloned());
                let length = values.len();
                self.heap.replace_javascript_list(receiver, values)?;
                Value::Number(length as f64)
            }
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
                let Some(index) = relative_index(javascript_to_number(index), values.len()) else {
                    return Err(js_stdlib_error(
                        "RangeError: Array.with index is out of range",
                    ));
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
