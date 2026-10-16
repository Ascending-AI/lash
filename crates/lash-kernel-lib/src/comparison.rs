//! Fallible structural equality, datum identity and lexicographic order.
use crate::numeric::{Numeric, number_cmp, scalar_cmp};
use crate::raised;
use lash_kernel_doc::heap::{Chunk, HeapFault, Payload};
use lash_kernel_doc::{EntityId, NativeError, NativeHeap, Value, ValueKind};
use std::cmp::Ordering;
use std::collections::HashSet;

type Pair = (ValueKind, EntityId, EntityId);

/// A prepared comparison reads only operands held in its descriptors.
pub(crate) fn scalar_equal(a: &Value, b: &Value) -> Option<bool> {
    if matches!(
        (a, b),
        (
            Value::Int(_) | Value::Float(_),
            Value::Int(_) | Value::Float(_)
        )
    ) {
        return Some(
            scalar_cmp(&Numeric::resident(a)?, &Numeric::resident(b)?).ok()?
                == Some(Ordering::Equal),
        );
    }
    Some(match (a, b) {
        (Value::Null, Value::Null) | (Value::Absent, Value::Absent) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Text(a), Value::Text(b)) => match (&a.body, &b.body) {
            (Payload::Resident(a), Payload::Resident(b)) => a == b,
            _ => return None,
        },
        (Value::Bytes(a), Value::Bytes(b)) => match (&a.body, &b.body) {
            (Payload::Resident(a), Payload::Resident(b)) => a == b,
            _ => return None,
        },
        (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
        (Value::Task(a), Value::Task(b)) => a == b,
        (Value::Ref(a), Value::Ref(b)) => a == b,
        (Value::Function(a), Value::Function(b)) => match (&a.body, &b.body) {
            (Payload::Resident(a), Payload::Resident(b)) => a == b,
            _ => return None,
        },
        (Value::Handle(a), Value::Handle(b)) => a == b,
        (Value::Closure(a), Value::Closure(b)) => a == b,
        (Value::Tuple(_), Value::Tuple(_))
        | (Value::Error(_), Value::Error(_))
        | (Value::List(_), Value::List(_))
        | (Value::Map(_), Value::Map(_))
        | (Value::Set(_), Value::Set(_))
        | (Value::Record(_), Value::Record(_)) => return None,
        _ => false,
    })
}

pub(crate) fn scalar_compare(
    a: &Value,
    b: &Value,
) -> Option<Result<Option<Ordering>, NativeError>> {
    Some(match (a, b) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            scalar_cmp(&Numeric::resident(a)?, &Numeric::resident(b)?)
        }
        (Value::Text(a), Value::Text(b)) => match (&a.body, &b.body) {
            (Payload::Resident(a), Payload::Resident(b)) => Ok(Some(a.cmp(b))),
            _ => return None,
        },
        (Value::Bytes(a), Value::Bytes(b)) => match (&a.body, &b.body) {
            (Payload::Resident(a), Payload::Resident(b)) => Ok(Some(a.cmp(b))),
            _ => return None,
        },
        (Value::Timestamp(a), Value::Timestamp(b)) => Ok(Some(a.cmp(b))),
        (Value::Bool(a), Value::Bool(b)) => Ok(Some(a.cmp(b))),
        (Value::Tuple(_), Value::Tuple(_)) | (Value::List(_), Value::List(_)) => return None,
        _ => Err(raised("type_error", "values have no shared ordering")),
    })
}

pub fn equal(a: &Value, b: &Value, heap: &dyn NativeHeap) -> Result<bool, HeapFault> {
    heap.equal(a, b)
}

pub(crate) fn scalar_same(a: &Value, b: &Value) -> Option<bool> {
    Some(match (a, b) {
        (Value::Float(a), Value::Float(b)) => a.get().to_bits() == b.get().to_bits(),
        (Value::List(a), Value::List(b))
        | (Value::Map(a), Value::Map(b))
        | (Value::Set(a), Value::Set(b))
        | (Value::Record(a), Value::Record(b))
        | (Value::Closure(a), Value::Closure(b)) => a == b,
        _ if a.kind() != b.kind() => false,
        _ => scalar_equal(a, b)?,
    })
}

pub fn same(a: &Value, b: &Value, heap: &dyn NativeHeap) -> Result<bool, HeapFault> {
    enum Step {
        Pair(Value, Value),
        Members(
            std::sync::Arc<lash_kernel_doc::heap::TupleRef>,
            std::sync::Arc<lash_kernel_doc::heap::TupleRef>,
            u64,
        ),
    }
    let mut pending = vec![Step::Pair(a.clone(), b.clone())];
    while let Some(step) = pending.pop() {
        let (a, b) = match step {
            Step::Pair(a, b) => (a, b),
            Step::Members(a, b, index) => {
                if index == a.summary.len {
                    continue;
                }
                let left = required(heap.tuple_get(&a, index)?)?;
                let right = required(heap.tuple_get(&b, index)?)?;
                pending.push(Step::Members(a, b, index + 1));
                pending.push(Step::Pair(left, right));
                continue;
            }
        };
        if let Some(same) = scalar_same(&a, &b) {
            if !same {
                return Ok(false);
            }
            continue;
        }
        match (&a, &b) {
            (Value::Tuple(a), Value::Tuple(b)) => {
                if a.summary.len != b.summary.len {
                    return Ok(false);
                }
                pending.push(Step::Members(a.clone(), b.clone(), 0));
            }
            (Value::Error(a), Value::Error(b)) => {
                if !heap.equal(&Value::Text(a.kind.clone()), &Value::Text(b.kind.clone()))?
                    || !heap.equal(
                        &Value::Text(a.message.clone()),
                        &Value::Text(b.message.clone()),
                    )?
                {
                    return Ok(false);
                }
                pending.push(Step::Pair(heap.error_data(a)?, heap.error_data(b)?));
            }
            (Value::Function(_), Value::Function(_))
            | (Value::Int(_), Value::Int(_))
            | (Value::Text(_), Value::Text(_))
            | (Value::Bytes(_), Value::Bytes(_)) => {
                if !heap.equal(&a, &b)? {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}
fn required(value: Option<Value>) -> Result<Value, HeapFault> {
    value.ok_or_else(|| HeapFault::Malformed("an admitted sequence member is absent".into()))
}

/// Each sequence advances one member at a time. It never snapshots a list
/// or loads the pages of later members before the equal prefix needs them.
pub fn compare(
    a: &Value,
    b: &Value,
    heap: &mut dyn NativeHeap,
) -> Result<Option<Ordering>, NativeError> {
    enum Step {
        Values(Value, Value, bool),
        Members(Value, Value, u64, u64, u64),
        Leave(Pair),
    }
    let mut pending = vec![Step::Values(a.clone(), b.clone(), false)];
    let mut active = HashSet::new();
    while let Some(step) = pending.pop() {
        let (a, b, member) = match step {
            Step::Leave(pair) => {
                active.remove(&pair);
                continue;
            }
            Step::Members(a, b, index, a_len, b_len) => {
                if index == a_len.min(b_len) {
                    let order = a_len.cmp(&b_len);
                    if order != Ordering::Equal {
                        return Ok(Some(order));
                    }
                    continue;
                }
                let get = |value: &Value, heap: &dyn NativeHeap| match value {
                    Value::Tuple(t) => heap.tuple_get(t, index),
                    Value::List(id) => heap.list_get(*id, index),
                    _ => Err(HeapFault::Malformed(
                        "ordering cursor has another kind".into(),
                    )),
                };
                let left = required(get(&a, heap)?)?;
                let right = required(get(&b, heap)?)?;
                pending.push(Step::Members(a, b, index + 1, a_len, b_len));
                pending.push(Step::Values(left, right, true));
                continue;
            }
            Step::Values(a, b, member) => (a, b, member),
        };
        if member && heap.equal(&a, &b)? {
            continue;
        }
        let order = match (&a, &b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                number_cmp(&a, &b, heap)?
            }
            (Value::Text(a), Value::Text(b)) => Some(payload_order(
                &Value::Text(a.clone()),
                &Value::Text(b.clone()),
                heap,
            )?),
            (Value::Bytes(a), Value::Bytes(b)) => Some(payload_order(
                &Value::Bytes(a.clone()),
                &Value::Bytes(b.clone()),
                heap,
            )?),
            (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Tuple(a_tuple), Value::Tuple(b_tuple)) => {
                let lengths = (a_tuple.summary.len, b_tuple.summary.len);
                pending.push(Step::Members(a, b, 0, lengths.0, lengths.1));
                continue;
            }
            (Value::List(a_id), Value::List(b_id)) => {
                let pair = (ValueKind::List, *a_id, *b_id);
                if !active.insert(pair) {
                    continue;
                }
                let lengths = (heap.len(*a_id)?, heap.len(*b_id)?);
                pending.push(Step::Leave(pair));
                pending.push(Step::Members(a, b, 0, lengths.0, lengths.1));
                continue;
            }
            _ => return Err(raised("type_error", "values have no shared ordering")),
        };
        if order != Some(Ordering::Equal) {
            return Ok(order);
        }
    }
    Ok(Some(Ordering::Equal))
}

/// UTF-8 lexicographic order agrees with scalar order. Compare only the
/// prefix required, with at most one bounded chunk from either payload.
pub(crate) fn payload_order(
    a: &Value,
    b: &Value,
    heap: &dyn NativeHeap,
) -> Result<Ordering, HeapFault> {
    let length = |v: &Value| match v {
        Value::Text(t) => t.summary.utf8_bytes,
        Value::Bytes(b) => b.len,
        _ => 0,
    };
    let (a_len, b_len) = (length(a), length(b));
    let chunk = |value: &Value, offset: u64, len: u64| -> Result<Vec<u8>, HeapFault> {
        let mut bytes = Vec::new();
        heap.payload_visit(value, offset..len, &mut |chunk| {
            let data = match chunk {
                Chunk::Text(text) => {
                    let mut end = text.len().min(4096);
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    &text.as_bytes()[..end]
                }
                Chunk::Bytes(bytes) => &bytes[..bytes.len().min(4096)],
            };
            bytes.extend_from_slice(data);
            std::ops::ControlFlow::Break(())
        })?;
        if bytes.is_empty() && offset != len {
            return Err(HeapFault::Malformed(
                "payload ended before its descriptor length".into(),
            ));
        }
        Ok(bytes)
    };
    let mut offset = 0;
    while offset < a_len.min(b_len) {
        let left = chunk(a, offset, a_len)?;
        let right = chunk(b, offset, b_len)?;
        let count = left.len().min(right.len());
        let order = left[..count].cmp(&right[..count]);
        if order != Ordering::Equal {
            return Ok(order);
        }
        offset += count as u64;
    }
    Ok(a_len.cmp(&b_len))
}
