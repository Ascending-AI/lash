//! Values at the edges of a run: their charge sizes (`K-CHG-004` to
//! `K-CHG-006`) and copying out (`K-EFF-002`). Decoding in (`K-EFF-005` to
//! `K-EFF-007`) is [`lash_kernel_doc::Decoder`].

use std::collections::BTreeSet;

use lash_kernel_doc::{Datum, EntityId, ErrorDatum, Value};
use num_traits::ToPrimitive;

use lash_kernel_doc::NativeHeap;
use lash_kernel_doc::heap::{Chunk, HeapFault, IntRef, TextRef};
use std::ops::ControlFlow;

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

/// A value's size (`K-CHG-004`), using descriptor summaries only.
pub(crate) fn size(heap: &dyn NativeHeap, value: &Value) -> Result<u64, HeapFault> {
    let extent = match value {
        Value::Int(IntRef::Inline(i)) => i.bits().div_ceil(64),
        Value::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(64),
        Value::Text(t) => t.summary.utf8_bytes,
        Value::Bytes(b) => b.len,
        Value::Tuple(t) => t.summary.len,
        Value::Error(e) => e
            .kind
            .summary
            .utf8_bytes
            .saturating_add(e.message.summary.utf8_bytes),
        Value::List(id) | Value::Set(id) | Value::Record(id) => heap.len(*id)?,
        Value::Map(id) => heap.len(*id)?.saturating_mul(2),
        _ => 0,
    };
    Ok(extent.saturating_add(1))
}

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

/// A deep charge size counts each entity once (`K-CHG-005`). Immutable
/// values still count at each holding location, regardless of page sharing.
pub(crate) fn deep_size(heap: &dyn NativeHeap, value: &Value) -> Result<u64, HeapFault> {
    fn measure(
        heap: &dyn NativeHeap,
        value: &Value,
        seen: &mut BTreeSet<EntityId>,
        pending: &mut Vec<EntityId>,
    ) -> Result<u64, HeapFault> {
        match value {
            Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                if !seen.insert(*id) {
                    return Ok(0);
                }
                pending.push(*id);
            }
            _ => {}
        }
        let mut total = size(heap, value)?;
        immutable_members(heap, value, &mut |member| {
            total = total.saturating_add(measure(heap, member, seen, pending)?);
            Ok(())
        })?;
        Ok(total)
    }
    let mut seen = BTreeSet::new();
    let mut pending = Vec::new();
    let mut total = measure(heap, value, &mut seen, &mut pending)?;
    while let Some(id) = pending.pop() {
        let mut fault = None;
        heap.visit(id, &mut |element| {
            let mut add = |value| {
                total = total.saturating_add(measure(heap, value, &mut seen, &mut pending)?);
                Ok::<_, HeapFault>(())
            };
            let result = match element {
                lash_kernel_doc::Element::Item(v)
                | lash_kernel_doc::Element::Field { value: v, .. } => add(v),
                lash_kernel_doc::Element::Entry { key, value } => {
                    add(key).and_then(|()| add(value))
                }
            };
            match result {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => {
                    fault = Some(error);
                    ControlFlow::Break(())
                }
            }
        })?;
        if let Some(error) = fault {
            return Err(error);
        }
    }
    Ok(total)
}

fn immutable_members(
    heap: &dyn NativeHeap,
    value: &Value,
    visitor: &mut dyn FnMut(&Value) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    match value {
        Value::Tuple(tuple) => {
            let mut fault = None;
            heap.tuple_visit(tuple, &mut |member| match visitor(member) {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => {
                    fault = Some(error);
                    ControlFlow::Break(())
                }
            })?;
            if let Some(error) = fault {
                return Err(error);
            }
        }
        Value::Error(error) => visitor(&heap.error_data(error)?)?,
        _ => {}
    }
    Ok(())
}

pub(crate) fn nested_size(heap: &dyn NativeHeap, value: &Value) -> Result<u64, HeapFault> {
    let mut total = 0u64;
    immutable_members(heap, value, &mut |member| {
        let amount = match member {
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => 1,
            _ => size(heap, member)?.saturating_add(nested_size(heap, member)?),
        };
        total = total.saturating_add(amount);
        Ok(())
    })?;
    Ok(total)
}

pub(crate) fn magnitude(value: &Value) -> u64 {
    match value {
        Value::Int(IntRef::Inline(i)) if !i.is_negative() => i.to_u64().unwrap_or(u64::MAX),
        Value::Int(IntRef::Paged { summary, .. }) if !summary.negative => u64::MAX,
        Value::Float(f) if f.get().is_finite() && f.get() >= 0.0 => f.get() as u64,
        _ => 0,
    }
}

/// Boundary copy failure retains the infrastructure/guest distinction.
#[derive(Debug)]
pub(crate) enum CopyError {
    Raised(Raised),
    Heap(HeapFault),
}
impl From<Raised> for CopyError {
    fn from(error: Raised) -> Self {
        Self::Raised(error)
    }
}
impl From<HeapFault> for CopyError {
    fn from(error: HeapFault) -> Self {
        Self::Heap(error)
    }
}

pub(crate) fn text_copy(heap: &dyn NativeHeap, text: &TextRef) -> Result<String, HeapFault> {
    // The descriptor certifies the full UTF-8 extent; check String's host
    // capacity limit before reserving it once, rather than growing per leaf.
    let capacity = usize::try_from(text.summary.utf8_bytes)
        .ok()
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or_else(|| HeapFault::Malformed("text length exceeds host String capacity".into()))?;
    let mut out = String::with_capacity(capacity);
    heap.payload_visit(
        &Value::Text(text.clone()),
        0..text.summary.utf8_bytes,
        &mut |chunk| {
            if let Chunk::Text(text) = chunk {
                out.push_str(text);
            }
            ControlFlow::Continue(())
        },
    )?;
    Ok(out)
}

/// Copies to the host's Datum tree without allocating guest entities or
/// reserving guest memory (`K-EFF-002`). Reads stop at the first fault.
pub(crate) fn copy_out(heap: &dyn NativeHeap, value: &Value) -> Result<Datum, CopyError> {
    copy(heap, value, heap.value_depth_limit(), &mut Vec::new())
}
fn copy(
    heap: &dyn NativeHeap,
    value: &Value,
    budget: usize,
    path: &mut Vec<EntityId>,
) -> Result<Datum, CopyError> {
    let Some(budget) = budget.checked_sub(1) else {
        return Err(raised(
            "too_deep",
            format!(
                "a value copied out of the run exceeds the configured {}-level value depth",
                heap.value_depth_limit()
            ),
        )
        .into());
    };
    Ok(match value {
        Value::Null => Datum::Null,
        Value::Absent => Datum::Absent,
        Value::Bool(v) => Datum::Bool(*v),
        Value::Float(v) => Datum::Float(*v),
        Value::Int(IntRef::Inline(i)) => Datum::Int(i.clone()),
        Value::Int(int @ IntRef::Paged { summary, .. }) => {
            let mut bytes = Vec::new();
            heap.payload_visit(
                &Value::Int(int.clone()),
                0..summary.bits.div_ceil(8),
                &mut |chunk| {
                    if let Chunk::Bytes(b) = chunk {
                        bytes.extend_from_slice(b);
                    }
                    ControlFlow::Continue(())
                },
            )?;
            Datum::Int(lash_kernel_doc::heap_values::integer_from_digits(
                summary.negative,
                &bytes,
            ))
        }
        Value::Text(t) => Datum::Text(text_copy(heap, t)?),
        Value::Bytes(b) => {
            let mut bytes = Vec::new();
            heap.payload_visit(value, 0..b.len, &mut |chunk| {
                if let Chunk::Bytes(b) = chunk {
                    bytes.extend_from_slice(b);
                }
                ControlFlow::Continue(())
            })?;
            Datum::Bytes(lash_kernel_doc::Bytes::new(bytes))
        }
        Value::Timestamp(t) => Datum::Timestamp(t.clone()),
        Value::Function(n) => Datum::Function(lash_kernel_doc::Name::new(text_copy(heap, n)?)),
        Value::Handle(h) => Datum::Handle(h.as_ref().clone()),
        Value::Error(e) => Datum::Error(Box::new(ErrorDatum {
            kind: text_copy(heap, &e.kind)?,
            message: text_copy(heap, &e.message)?,
            data: copy(heap, &heap.error_data(e)?, budget, path)?,
        })),
        Value::Tuple(t) => {
            let mut items = Vec::new();
            for index in 0..t.summary.len {
                let member = heap
                    .tuple_get(t, index)?
                    .ok_or_else(|| HeapFault::Malformed("tuple extent has no member".into()))?;
                items.push(copy(heap, &member, budget, path)?);
            }
            Datum::Tuple(items)
        }
        Value::Closure(_) | Value::Task(_) | Value::Ref(_) => {
            let what = match value {
                Value::Closure(_) => "a closure",
                Value::Task(_) => "a task handle",
                _ => "a ref",
            };
            return Err(raised(
                "not_data",
                format!("{what} is not data and cannot leave the run"),
            )
            .into());
        }
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            if path.contains(id) {
                return Err(
                    raised("cycle", "a value that holds itself cannot leave the run").into(),
                );
            }
            path.push(*id);
            let mut items = Vec::new();
            let mut entries = Vec::new();
            let mut fields = Vec::new();
            let mut error = None;
            heap.visit(*id, &mut |element| {
                let result = match element {
                    lash_kernel_doc::Element::Item(v) => {
                        copy(heap, v, budget, path).map(|v| items.push(v))
                    }
                    lash_kernel_doc::Element::Entry { key, value } => copy(heap, key, budget, path)
                        .and_then(|k| {
                            copy(heap, value, budget, path).map(|v| entries.push((k, v)))
                        }),
                    lash_kernel_doc::Element::Field { name, value } => text_copy(heap, name)
                        .map_err(CopyError::from)
                        .and_then(|n| copy(heap, value, budget, path).map(|v| fields.push((n, v)))),
                };
                match result {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => {
                        error = Some(e);
                        ControlFlow::Break(())
                    }
                }
            })?;
            if let Some(error) = error {
                return Err(error);
            }
            path.pop();
            match value {
                Value::List(_) => Datum::List(items),
                Value::Set(_) => Datum::Set(items),
                Value::Map(_) => Datum::Map(entries),
                _ => Datum::Record(fields),
            }
        }
    })
}
