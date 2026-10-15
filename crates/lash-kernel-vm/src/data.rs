//! Values at the edges of a run: their charge sizes (`K-CHG-004` to
//! `K-CHG-006`) and copying out (`K-EFF-002`). Decoding in (`K-EFF-005` to
//! `K-EFF-007`) is [`lash_kernel_doc::Decoder`].

use std::collections::BTreeSet;

use lash_kernel_doc::{Datum, ErrorDatum, ObjectId, Value};
use num_traits::ToPrimitive;

use crate::heap::{Heap, Obj};

/// A raise the machine makes itself: one of the kinds of `K-ERR-002`.
#[derive(Debug)]
pub(crate) struct Raised {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

impl Raised {
    pub(crate) fn into_datum(self) -> Datum {
        Datum::Error(Box::new(ErrorDatum {
            kind: self.kind.to_string(),
            message: self.message,
            data: Datum::Null,
        }))
    }
}

pub(crate) fn raised(kind: &'static str, message: impl Into<String>) -> Raised {
    Raised {
        kind,
        message: message.into(),
    }
}

/// A value's size (`K-CHG-004`).
pub(crate) fn size(heap: &Heap, value: &Value) -> u64 {
    let extent = match value {
        Value::Int(integer) => integer.bits().div_ceil(64),
        Value::Text(text) => text.len() as u64,
        Value::Bytes(bytes) => bytes.as_slice().len() as u64,
        Value::Tuple(members) => members.len() as u64,
        Value::Error(error) => (error.kind.len() + error.message.len()) as u64,
        Value::List(id) | Value::Set(id) | Value::Record(id) | Value::Map(id) => {
            match heap.get(*id) {
                Some(Obj::List(items)) => items.len() as u64,
                Some(Obj::Set(table)) => table.len() as u64,
                Some(Obj::Record(fields)) => fields.len() as u64,
                Some(Obj::Map(table)) => table.len() as u64 * 2,
                _ => 0,
            }
        }
        _ => 0,
    };
    extent.saturating_add(1)
}

/// Whether a value holds no other value, so that its deep size is its
/// size (`K-CHG-005`).
pub(crate) fn holds_nothing(value: &Value) -> bool {
    !matches!(
        value,
        Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Record(_)
            | Value::Tuple(_)
            | Value::Error(_)
    )
}

/// Whether a value's size is 1 whatever it holds: it has no extent
/// (`K-CHG-004`).
pub(crate) fn size_one(value: &Value) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Absent
            | Value::Bool(_)
            | Value::Float(_)
            | Value::Timestamp(_)
            | Value::Closure(_)
            | Value::Task(_)
            | Value::Function(_)
            | Value::Handle(_)
            | Value::Ref(_)
    )
}

/// A value's size with everything it holds, a heap object counted the
/// first time it is reached (`K-CHG-005`).
pub(crate) fn deep_size(heap: &Heap, value: &Value) -> u64 {
    // A value that holds nothing is its own size: most measured values are.
    if holds_nothing(value) {
        return size(heap, value);
    }
    let mut total = 0u64;
    let mut seen = Seen::new();
    let mut pending = Vec::new();
    measure(heap, value, &mut total, &mut seen, &mut pending);
    while let Some(id) = pending.pop() {
        let mut inner = |value: &Value| measure(heap, value, &mut total, &mut seen, &mut pending);
        match heap.get(id) {
            Some(Obj::List(items)) => items.iter().for_each(&mut inner),
            Some(Obj::Map(table)) => table.iter().for_each(|(key, value)| {
                inner(key);
                inner(value);
            }),
            Some(Obj::Set(table)) => table.iter().for_each(|(member, _)| inner(member)),
            Some(Obj::Record(fields)) => fields.iter().for_each(|(_, value)| inner(value)),
            _ => {}
        }
    }
    total
}

/// How many objects a measurement remembers before it needs a set.
const FEW: usize = 8;

/// The objects a measurement has reached: the first few in place, the rest
/// in a set made only when they do not fit, so measuring a small graph
/// allocates and frees nothing for them.
struct Seen {
    few: [ObjectId; FEW],
    count: usize,
    rest: Option<BTreeSet<ObjectId>>,
}

impl Seen {
    fn new() -> Self {
        Self {
            few: [ObjectId(0); FEW],
            count: 0,
            rest: None,
        }
    }

    /// Adds `id`, and says whether it was new.
    fn insert(&mut self, id: ObjectId) -> bool {
        let few = self.count.min(FEW);
        if self.few[..few].contains(&id) {
            return false;
        }
        if self.count < FEW {
            self.few[self.count] = id;
        } else if !self.rest.get_or_insert_default().insert(id) {
            return false;
        }
        self.count += 1;
        true
    }
}

fn measure(
    heap: &Heap,
    value: &Value,
    total: &mut u64,
    seen: &mut Seen,
    pending: &mut Vec<ObjectId>,
) {
    match value {
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            if !seen.insert(*id) {
                return;
            }
            pending.push(*id);
        }
        Value::Tuple(members) => members
            .iter()
            .for_each(|member| measure(heap, member, total, seen, pending)),
        Value::Error(error) => measure(heap, &error.data, total, seen, pending),
        _ => {}
    }
    *total = total.saturating_add(size(heap, value));
}

/// The sizes of the immutable values nested in a value, down to the heap
/// objects it holds, each of which counts 1 (`K-CHG-005`).
pub(crate) fn nested_size(heap: &Heap, value: &Value) -> u64 {
    // The value in hand is not pushed, nor is a member that holds no
    // immutable value, so measuring a flat tuple, or a value that is
    // neither a tuple nor an error, allocates nothing.
    let mut total = 0u64;
    let mut next = Some(value);
    let mut pending = Vec::new();
    while let Some(value) = next.take().or_else(|| pending.pop()) {
        let held = match value {
            Value::Tuple(members) => &members[..],
            Value::Error(error) => std::slice::from_ref(&error.data),
            _ => continue,
        };
        for member in held {
            let size = match member {
                Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => 1,
                Value::Tuple(_) | Value::Error(_) => {
                    if next.is_none() {
                        next = Some(member);
                    } else {
                        pending.push(member);
                    }
                    size(heap, member)
                }
                _ => size(heap, member),
            };
            total = total.saturating_add(size);
        }
    }
    total
}

/// A value's magnitude (`K-CHG-006`).
pub(crate) fn magnitude(value: &Value) -> u64 {
    match value {
        Value::Int(integer) if !integer.is_negative() => integer.to_u64().unwrap_or(u64::MAX),
        Value::Float(float) if float.get().is_finite() && float.get() >= 0.0 => {
            // A float-to-integer cast rounds toward zero and saturates.
            float.get() as u64
        }
        _ => 0,
    }
}

/// Copies a value out of the run as a tree (`K-EFF-002`).
pub(crate) fn copy_out(heap: &Heap, value: &Value) -> Result<Datum, Raised> {
    copy(heap, value, heap.value_depth, &mut Vec::new())
}

fn copy(
    heap: &Heap,
    value: &Value,
    budget: usize,
    path: &mut Vec<ObjectId>,
) -> Result<Datum, Raised> {
    let Some(budget) = budget.checked_sub(1) else {
        return Err(raised(
            "too_deep",
            format!(
                "a value copied out of the run exceeds the configured {}-level value depth",
                heap.value_depth
            ),
        ));
    };
    let not_data = |what: &str| {
        Err(raised(
            "not_data",
            format!("{what} is not data and cannot leave the run"),
        ))
    };
    Ok(match value {
        Value::Null => Datum::Null,
        Value::Absent => Datum::Absent,
        Value::Bool(flag) => Datum::Bool(*flag),
        Value::Int(integer) => Datum::Int(integer.clone()),
        Value::Float(float) => Datum::Float(*float),
        Value::Text(text) => Datum::Text(text.to_string()),
        Value::Bytes(bytes) => Datum::Bytes(bytes.clone()),
        Value::Timestamp(timestamp) => Datum::Timestamp(timestamp.clone()),
        Value::Function(name) => Datum::Function(name.clone()),
        Value::Handle(handle) => Datum::Handle(handle.as_ref().clone()),
        Value::Error(error) => Datum::Error(Box::new(ErrorDatum {
            kind: error.kind.clone(),
            message: error.message.clone(),
            data: copy(heap, &error.data, budget, path)?,
        })),
        Value::Tuple(members) => Datum::Tuple(
            members
                .iter()
                .map(|member| copy(heap, member, budget, path))
                .collect::<Result<_, _>>()?,
        ),
        Value::Closure(_) => return not_data("a closure"),
        Value::Task(_) => return not_data("a task handle"),
        Value::Ref(_) => return not_data("a ref"),
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            if path.contains(id) {
                return Err(raised(
                    "cycle",
                    "a value that holds itself cannot leave the run",
                ));
            }
            path.push(*id);
            let mut inner = |value: &Value| copy(heap, value, budget, path);
            let datum = match heap.get(*id) {
                Some(Obj::List(items)) => {
                    Datum::List(items.iter().map(&mut inner).collect::<Result<_, _>>()?)
                }
                Some(Obj::Set(table)) => Datum::Set(
                    table
                        .iter()
                        .map(|(member, _)| inner(member))
                        .collect::<Result<_, _>>()?,
                ),
                Some(Obj::Map(table)) => Datum::Map(
                    table
                        .iter()
                        .map(|(key, value)| Ok((inner(key)?, inner(value)?)))
                        .collect::<Result<_, Raised>>()?,
                ),
                Some(Obj::Record(fields)) => Datum::Record(
                    fields
                        .iter()
                        .map(|(name, value)| Ok((name.clone(), inner(value)?)))
                        .collect::<Result<_, Raised>>()?,
                ),
                _ => Datum::Null,
            };
            path.pop();
            datum
        }
    })
}
