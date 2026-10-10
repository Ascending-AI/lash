//! Structural equality, datum identity, and lexicographic order.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::ControlFlow;

use lash_kernel_doc::{Element, NativeError, NativeHeap, ObjectId, Value, ValueKind};

use crate::keys::key_equal;
use crate::numeric::number_cmp;
use crate::raised;

type Pair = (ValueKind, ObjectId, ObjectId);

/// Structural equality (`K-VAL-020..026`), including cyclic objects.
/// It has no guest error. Numeric NaNs are unequal, even inside one object.
pub fn equal(a: &Value, b: &Value, heap: &dyn NativeHeap) -> bool {
    // The pair in hand is not pushed, so that comparing two values that
    // hold nothing allocates nothing.
    let mut next = Some((a.clone(), b.clone()));
    let mut pending = Vec::new();
    let mut seen = HashSet::new();
    while let Some((a, b)) = next.take().or_else(|| pending.pop()) {
        let kind = a.kind();
        match (&a, &b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                if !matches!(number_cmp(&a, &b), Ok(Some(Ordering::Equal))) {
                    return false;
                }
            }
            (Value::Tuple(a), Value::Tuple(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                pending.extend(a.iter().cloned().zip(b.iter().cloned()));
            }
            (Value::Error(a), Value::Error(b)) => {
                if a.kind != b.kind || a.message != b.message {
                    return false;
                }
                pending.push((a.data.clone(), b.data.clone()));
            }
            (Value::List(a), Value::List(b))
            | (Value::Map(a), Value::Map(b))
            | (Value::Set(a), Value::Set(b))
            | (Value::Record(a), Value::Record(b)) => {
                if !seen.insert((kind, *a, *b)) {
                    continue;
                }
                let a = elements(heap, *a);
                let b = elements(heap, *b);
                if a.len() != b.len() {
                    return false;
                }
                for (index, element) in a.iter().enumerate() {
                    match element {
                        OwnedElement::Item(value) if kind == ValueKind::Set => {
                            if !b.iter().any(|other| matches!(other,OwnedElement::Item(other) if key_equal(value,other))) { return false; }
                        }
                        OwnedElement::Item(value) => {
                            let OwnedElement::Item(other) = &b[index] else { return false; };
                            pending.push((value.clone(),other.clone()));
                        }
                        OwnedElement::Entry(key,value) => {
                            let Some(other) = b.iter().find_map(|other| match other {
                                OwnedElement::Entry(other_key,other_value) if key_equal(key,other_key) => Some(other_value),
                                _ => None,
                            }) else { return false; };
                            pending.push((value.clone(),other.clone()));
                        }
                        OwnedElement::Field(name,value) => {
                            let Some(other) = b.iter().find_map(|other| match other {
                                OwnedElement::Field(other_name,other_value) if name == other_name => Some(other_value),
                                _ => None,
                            }) else { return false; };
                            pending.push((value.clone(),other.clone()));
                        }
                    }
                }
            }
            _ => {
                if a != b {
                    return false;
                }
            }
        }
    }
    true
}

#[derive(Clone)]
enum OwnedElement {
    Item(Value),
    Entry(Value, Value),
    Field(String, Value),
}

fn elements(heap: &dyn NativeHeap, object: ObjectId) -> Vec<OwnedElement> {
    let mut values = Vec::new();
    heap.visit(object, &mut |element| {
        values.push(match element {
            Element::Item(value) => OwnedElement::Item(value.clone()),
            Element::Entry { key, value } => OwnedElement::Entry(key.clone(), value.clone()),
            Element::Field { name, value } => OwnedElement::Field(name.to_owned(), value.clone()),
        });
        ControlFlow::Continue(())
    });
    values
}

/// Identity (`K-VAL-027`): immutable members retain their kinds and float bits.
pub fn same(a: &Value, b: &Value) -> bool {
    let mut next = Some((a, b));
    let mut pending = Vec::new();
    while let Some((a, b)) = next.take().or_else(|| pending.pop()) {
        match (a, b) {
            (Value::Tuple(a), Value::Tuple(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                pending.extend(a.iter().zip(b.iter()));
            }
            (Value::Error(a), Value::Error(b)) => {
                if a.kind != b.kind || a.message != b.message {
                    return false;
                }
                pending.push((&a.data, &b.data));
            }
            _ => {
                if a != b {
                    return false;
                }
            }
        }
    }
    true
}

/// Mathematical/lexicographic order (`K-VAL-029/030/033`). `None` means NaN
/// is unordered; unsupported pairings raise `type_error`. A sequence's equal
/// prefix is skipped, and the shorter sequence comes first. Repeated active
/// cyclic list pairs count as equal, just as for structural equality.
pub fn compare(
    a: &Value,
    b: &Value,
    heap: &dyn NativeHeap,
) -> Result<Option<Ordering>, NativeError> {
    enum Step {
        Values(Value, Value),
        Member(Value, Value),
        Length(usize, usize),
        Leave(Pair),
    }
    let mut next = Some(Step::Values(a.clone(), b.clone()));
    let mut pending = Vec::new();
    let mut active = HashSet::new();
    while let Some(step) = next.take().or_else(|| pending.pop()) {
        let (a, b) = match step {
            Step::Length(a, b) => {
                let order = a.cmp(&b);
                if order != Ordering::Equal {
                    return Ok(Some(order));
                }
                continue;
            }
            Step::Leave(pair) => {
                active.remove(&pair);
                continue;
            }
            Step::Member(a, b) if equal(&a, &b, heap) => continue,
            Step::Values(a, b) | Step::Member(a, b) => (a, b),
        };
        let order = match (&a, &b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                number_cmp(&a, &b)?
            }
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
            (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Tuple(a), Value::Tuple(b)) => {
                pending.push(Step::Length(a.len(), b.len()));
                for (a, b) in a.iter().zip(b.iter()).rev() {
                    pending.push(Step::Member(a.clone(), b.clone()));
                }
                continue;
            }
            (Value::List(a), Value::List(b)) => {
                let pair = (ValueKind::List, *a, *b);
                if !active.insert(pair) {
                    continue;
                }
                pending.push(Step::Leave(pair));
                let a_len = heap.len(*a);
                let b_len = heap.len(*b);
                pending.push(Step::Length(a_len, b_len));
                for index in (0..a_len.min(b_len)).rev() {
                    if let (Some(a), Some(b)) = (heap.list_get(*a, index), heap.list_get(*b, index))
                    {
                        pending.push(Step::Member(a, b));
                    }
                }
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
