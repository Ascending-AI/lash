//! Values at the edges of a run: their charge sizes (`K-CHG-004` to
//! `K-CHG-006`) and copying out (`K-EFF-002`), over the arena; and copying
//! a page form out, for what reads a run's saved state without running it.
//! Decoding in (`K-EFF-005` to `K-EFF-007`) is [`lash_kernel_doc::Decoder`].

use std::collections::BTreeSet;

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::{Chunk, HeapFault, IntRef, PageReader, PageValue, TextRef};
use lash_kernel_doc::{Datum, EntityId, ErrorDatum, Integer, Measure, NativeError, Value};

use lash_kernel_state::arena::Arena;
use lash_kernel_state::kernel_internals::{PagedHeap, page_reads};
use num_traits::ToPrimitive;
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

fn integer_of(arena: &Arena, value: &Value) -> Result<Option<Integer>, NativeError> {
    arena.semantics().integer(value)
}

/// A value's size (`K-CHG-004`), from what the arena knows of it.
pub(crate) fn size(arena: &Arena, value: &Value) -> Result<u64, NativeError> {
    if let Some(size) = immediate_size(value) {
        return Ok(size);
    }
    let extent = match value {
        Value::BigInt(_) => {
            integer_of(arena, value)?.map_or(0, |integer| integer.bits().div_ceil(64))
        }
        Value::Text(short) => short.as_bytes().len() as u64,
        Value::LongText(handle) => match arena.body(*handle)? {
            Body::LongText(text) => text.summary.utf8_bytes,
            _ => 0,
        },
        Value::Bytes(handle) => match arena.body(*handle)? {
            Body::Bytes(bytes) => bytes.len,
            _ => 0,
        },
        Value::Tuple(handle) | Value::List(handle) | Value::Set(handle) | Value::Record(handle) => {
            return Ok(sequence_size(arena.len(*handle)?));
        }
        Value::Error(handle) => match arena.body(*handle)? {
            Body::Error(error) => {
                text_len(arena, &error.kind)?.saturating_add(text_len(arena, &error.message)?)
            }
            _ => 0,
        },
        Value::Map(handle) => return Ok(map_size(arena.len(*handle)?)),
        _ => 0,
    };
    Ok(extent.saturating_add(1))
}

/// The canonical shallow size of a sequence, resident or detached.
pub(crate) fn sequence_size(len: u64) -> u64 {
    len.saturating_add(1)
}

/// Maps count both keys and values in their canonical shallow extent.
pub(crate) fn map_size(len: u64) -> u64 {
    sequence_size(len.saturating_mul(2))
}

/// The size of a value the arena holds nothing of (`K-CHG-004`): a word,
/// a short text or a value of size 1; `None` for one the arena measures.
#[inline(always)]
pub(crate) fn immediate_size(value: &Value) -> Option<u64> {
    let extent = match value {
        // A word's magnitude takes at most one word; zero takes none.
        Value::Int(word) => u64::from(*word != 0),
        Value::Text(short) => short.as_bytes().len() as u64,
        value if size_one(value) => 0,
        _ => return None,
    };
    Some(extent + 1)
}

/// `measure` of a value the arena holds nothing of, as [`size`],
/// [`deep_size`], [`nested_size`] and [`magnitude`] measure it: such a
/// value holds no other, so its deep size is its size and its nested size
/// 0 (`K-CHG-005`, `K-CHG-006`). `None` for a value the arena measures.
#[inline(always)]
pub(crate) fn immediate_measure(value: &Value, measure: Measure) -> Option<u64> {
    Some(leaf_measure(
        immediate_size(value)?,
        word_magnitude(value),
        measure,
    ))
}

/// A detached native text has the same leaf measures as resident text.
pub(crate) fn text_measure(text: &str, measure: Measure) -> u64 {
    leaf_measure((text.len() as u64).saturating_add(1), 0, measure)
}

fn leaf_measure(size: u64, magnitude: u64, measure: Measure) -> u64 {
    match measure {
        Measure::Size | Measure::DeepSize => size,
        Measure::NestedSize => 0,
        Measure::Magnitude => magnitude,
    }
}

/// `measure` of a construction of no members, of any kind: what
/// [`size`], [`deep_size`], [`nested_size`] and [`magnitude`] measure of
/// an empty tuple, list, set, map or record, which holds no other value.
#[inline(always)]
pub(crate) fn empty_measure(measure: Measure) -> u64 {
    match measure {
        Measure::Size | Measure::DeepSize => 1,
        Measure::NestedSize | Measure::Magnitude => 0,
    }
}

fn text_len(arena: &Arena, text: &Value) -> Result<u64, NativeError> {
    Ok(match text {
        Value::Text(short) => short.as_bytes().len() as u64,
        Value::LongText(handle) => match arena.body(*handle)? {
            Body::LongText(text) => text.summary.utf8_bytes,
            _ => 0,
        },
        _ => 0,
    })
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
            | Value::BigTimestamp(_)
            | Value::Closure(_)
            | Value::Task(_)
            | Value::Function(_)
            | Value::Handle(_)
            | Value::TaskRef(_)
            | Value::ObjectRef(_)
    )
}

/// The members of an immutable composite: a tuple's, an error's data.
fn immutable_members(
    arena: &Arena,
    value: &Value,
    visitor: &mut dyn FnMut(&Value) -> Result<(), NativeError>,
) -> Result<(), NativeError> {
    match value {
        Value::Tuple(tuple) => {
            for member in arena.sequence(*tuple)? {
                visitor(member)?;
            }
        }
        Value::Error(error) => match arena.body(*error)? {
            Body::Error(error) => visitor(&error.data)?,
            _ => return Err(NativeError::Malformed("an error handle names another body")),
        },
        _ => {}
    }
    Ok(())
}

/// A deep charge size counts each entity once (`K-CHG-005`). Immutable
/// values still count at each holding location.
pub(crate) fn deep_size(arena: &Arena, value: &Value) -> Result<u64, NativeError> {
    if holds_nothing(value) {
        return size(arena, value);
    }
    if let Some(total) = arena.flat_charge_size(value)? {
        return Ok(total);
    }
    graph_deep_size(arena, value)
}

/// The authoritative seen-set walk for graphs without a flat summary.
pub(crate) fn graph_deep_size(arena: &Arena, value: &Value) -> Result<u64, NativeError> {
    fn measure(
        arena: &Arena,
        value: &Value,
        seen: &mut BTreeSet<u32>,
        pending: &mut Vec<Value>,
    ) -> Result<u64, NativeError> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.flat_size_visits += 1);
        if let Value::List(handle)
        | Value::Map(handle)
        | Value::Set(handle)
        | Value::Record(handle) = value
        {
            if !seen.insert(handle.index()) {
                return Ok(0);
            }
            pending.push(*value);
        }
        let mut total = size(arena, value)?;
        immutable_members(arena, value, &mut |member| {
            total = total.saturating_add(measure(arena, member, seen, pending)?);
            Ok(())
        })?;
        Ok(total)
    }
    let mut seen = BTreeSet::new();
    let mut pending = Vec::new();
    let mut total = measure(arena, value, &mut seen, &mut pending)?;
    while let Some(object) = pending.pop() {
        let members: Vec<Value> = match object {
            Value::List(list) => arena.sequence(list)?.to_vec(),
            // A set's members are its keys; a record's fields their values.
            Value::Set(set) => arena.entries(set)?.iter().step_by(2).copied().collect(),
            Value::Record(record) => arena
                .entries(record)?
                .iter()
                .skip(1)
                .step_by(2)
                .copied()
                .collect(),
            Value::Map(map) => arena.entries(map)?.to_vec(),
            _ => Vec::new(),
        };
        for member in &members {
            total = total.saturating_add(measure(arena, member, &mut seen, &mut pending)?);
        }
    }
    Ok(total)
}

pub(crate) fn nested_size(arena: &Arena, value: &Value) -> Result<u64, NativeError> {
    if !matches!(value, Value::Tuple(_) | Value::Error(_)) {
        return Ok(0);
    }
    let mut total = 0u64;
    immutable_members(arena, value, &mut |member| {
        let amount = match member {
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => 1,
            _ => size(arena, member)?.saturating_add(nested_size(arena, member)?),
        };
        total = total.saturating_add(amount);
        Ok(())
    })?;
    Ok(total)
}

pub(crate) fn magnitude(arena: &Arena, value: &Value) -> u64 {
    match value {
        Value::BigInt(_) => match integer_of(arena, value) {
            Ok(Some(integer)) if !integer.is_negative() => integer.to_u64().unwrap_or(u64::MAX),
            _ => 0,
        },
        value => word_magnitude(value),
    }
}

/// The magnitude of a value that is no big integer: a word's or a
/// finite non-negative float's, truncated; 0 for any other.
#[inline(always)]
fn word_magnitude(value: &Value) -> u64 {
    match value {
        Value::Int(word) => u64::try_from(*word).unwrap_or(0),
        Value::Float(f) if f.get().is_finite() && f.get() >= 0.0 => f.get() as u64,
        _ => 0,
    }
}

impl From<NativeError> for CopyError {
    fn from(error: NativeError) -> Self {
        Self::Heap(HeapFault::Malformed(error.to_string()))
    }
}

/// Copies to the host's Datum tree without building or reserving
/// anything (`K-EFF-002`): the arena holds every object `value` reaches,
/// decoded.
pub(crate) fn copy_out(arena: &Arena, limit: usize, value: &Value) -> Result<Datum, CopyError> {
    copy(arena, limit, value, limit, &mut Vec::new())
}

fn text_of(arena: &Arena, value: &Value) -> Result<String, CopyError> {
    Ok(arena.semantics().text(value)?.to_owned())
}

fn copy(
    arena: &Arena,
    limit: usize,
    value: &Value,
    budget: usize,
    path: &mut Vec<ArenaHandle>,
) -> Result<Datum, CopyError> {
    let Some(budget) = budget.checked_sub(1) else {
        return Err(raised(
            "too_deep",
            format!(
                "a value copied out of the run exceeds the configured {limit}-level value depth"
            ),
        )
        .into());
    };
    Ok(match value {
        Value::Null => Datum::Null,
        Value::Absent => Datum::Absent,
        Value::Bool(v) => Datum::Bool(*v),
        Value::Float(v) => Datum::Float(*v),
        Value::Int(_) | Value::BigInt(_) => Datum::Int(
            integer_of(arena, value)?.ok_or(NativeError::Malformed("an integer was expected"))?,
        ),
        Value::Text(_) | Value::LongText(_) => Datum::Text(text_of(arena, value)?),
        Value::Bytes(handle) => Datum::Bytes(lash_kernel_doc::Bytes::new(
            arena.semantics().bytes(*handle)?.to_vec(),
        )),
        Value::Timestamp(_) | Value::BigTimestamp(_) => {
            Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: arena
                    .semantics()
                    .nanoseconds(value)?
                    .ok_or(NativeError::Malformed("a timestamp was expected"))?,
            })
        }
        Value::Function(handle) => Datum::Function(
            arena
                .callable(*handle)
                .map_err(|error| CopyError::Heap(HeapFault::Malformed(error.to_string())))?,
        ),
        Value::Handle(handle) => match arena.body(*handle)? {
            Body::Handle(handle) => Datum::Handle(handle.clone()),
            _ => return Err(NativeError::Malformed("a handle names another body").into()),
        },
        Value::Error(error) => match arena.body(*error)? {
            Body::Error(error) => Datum::Error(Box::new(ErrorDatum {
                kind: text_of(arena, &error.kind)?,
                message: text_of(arena, &error.message)?,
                data: copy(arena, limit, &error.data, budget, path)?,
            })),
            _ => return Err(NativeError::Malformed("an error names another body").into()),
        },
        Value::Tuple(tuple) => Datum::Tuple(
            arena
                .sequence(*tuple)?
                .iter()
                .map(|member| copy(arena, limit, member, budget, path))
                .collect::<Result<_, _>>()?,
        ),
        Value::Closure(_) | Value::Task(_) | Value::TaskRef(_) | Value::ObjectRef(_) => {
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
        Value::List(object) | Value::Map(object) | Value::Set(object) | Value::Record(object) => {
            if path.contains(object) {
                return Err(
                    raised("cycle", "a value that holds itself cannot leave the run").into(),
                );
            }
            path.push(*object);
            let copied = match value {
                Value::List(_) => Datum::List(
                    arena
                        .sequence(*object)?
                        .iter()
                        .map(|item| copy(arena, limit, item, budget, path))
                        .collect::<Result<_, _>>()?,
                ),
                Value::Set(_) => Datum::Set(
                    arena
                        .entries(*object)?
                        .iter()
                        .step_by(2)
                        .map(|member| copy(arena, limit, member, budget, path))
                        .collect::<Result<_, _>>()?,
                ),
                Value::Map(_) => {
                    let mut entries = Vec::new();
                    for pair in arena.entries(*object)?.as_chunks::<2>().0.iter() {
                        let key = copy(arena, limit, &pair[0], budget, path)?;
                        entries.push((key, copy(arena, limit, &pair[1], budget, path)?));
                    }
                    Datum::Map(entries)
                }
                _ => {
                    let mut fields = Vec::new();
                    for pair in arena.entries(*object)?.as_chunks::<2>().0.iter() {
                        let name = text_of(arena, &pair[0])?;
                        fields.push((name, copy(arena, limit, &pair[1], budget, path)?));
                    }
                    Datum::Record(fields)
                }
            };
            path.pop();
            copied
        }
    })
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

pub(crate) fn text_copy(heap: &PagedHeap, text: &TextRef) -> Result<String, HeapFault> {
    // The descriptor certifies the full UTF-8 extent; check String's host
    // capacity limit before copying it.
    let capacity = usize::try_from(text.summary.utf8_bytes)
        .ok()
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or_else(|| HeapFault::Malformed("text length exceeds host String capacity".into()))?;
    match &text.body {
        #[expect(
            clippy::expect_used,
            reason = "the matched resident or borrowed variant always contains text"
        )]
        lash_kernel_doc::heap::TextPayload::Resident(_)
        | lash_kernel_doc::heap::TextPayload::Borrowed(_) => {
            let text = text.body.resident().expect("resident text");

            text.get(..capacity).map(str::to_owned).ok_or_else(|| {
                HeapFault::Malformed("text range cuts a scalar or exceeds length".into())
            })
        }
        // The host consumes the entire text. Its authenticated siblings are
        // independent demands, unlike a visitor that can stop at a prefix.
        lash_kernel_doc::heap::TextPayload::Paged(_) => {
            Ok(page_reads::text(heap.pages(), text)?.to_string())
        }
    }
}

/// Copies to the host's Datum tree without allocating guest entities or
/// reserving guest memory (`K-EFF-002`). Reads stop at the first fault.
pub(crate) fn copy_page_out(heap: &PagedHeap, value: &PageValue) -> Result<Datum, CopyError> {
    copy_page(heap, value, heap.value_depth_limit(), &mut Vec::new())
}
fn copy_page(
    heap: &PagedHeap,
    value: &PageValue,
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
        PageValue::Null => Datum::Null,
        PageValue::Absent => Datum::Absent,
        PageValue::Bool(v) => Datum::Bool(*v),
        PageValue::Float(v) => Datum::Float(*v),
        PageValue::Int(IntRef::Inline(i)) => Datum::Int(i.clone()),
        PageValue::Int(int @ IntRef::Paged { summary, .. }) => {
            let mut bytes = Vec::new();
            heap.payload_visit(
                &PageValue::Int(int.clone()),
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
        PageValue::Text(t) => Datum::Text(text_copy(heap, t)?),
        PageValue::Bytes(b) => {
            let mut bytes = Vec::new();
            heap.payload_visit(value, 0..b.len, &mut |chunk| {
                if let Chunk::Bytes(b) = chunk {
                    bytes.extend_from_slice(b);
                }
                ControlFlow::Continue(())
            })?;
            Datum::Bytes(lash_kernel_doc::Bytes::new(bytes))
        }
        PageValue::Timestamp(t) => Datum::Timestamp(t.clone()),
        PageValue::Function(n) => Datum::Function(**n),
        PageValue::Handle(h) => Datum::Handle(h.as_ref().clone()),
        PageValue::Error(e) => Datum::Error(Box::new(ErrorDatum {
            kind: text_copy(heap, &e.kind)?,
            message: text_copy(heap, &e.message)?,
            data: copy_page(heap, &heap.error_data(e)?, budget, path)?,
        })),
        PageValue::Tuple(t) => {
            let mut items = Vec::new();
            for index in 0..t.summary.len {
                let member = heap
                    .tuple_get(t, index)?
                    .ok_or_else(|| HeapFault::Malformed("tuple extent has no member".into()))?;
                items.push(copy_page(heap, &member, budget, path)?);
            }
            Datum::Tuple(items)
        }
        PageValue::Closure(_) | PageValue::Task(_) | PageValue::Ref(_) => {
            let what = match value {
                PageValue::Closure(_) => "a closure",
                PageValue::Task(_) => "a task handle",
                _ => "a ref",
            };
            return Err(raised(
                "not_data",
                format!("{what} is not data and cannot leave the run"),
            )
            .into());
        }
        PageValue::List(id) | PageValue::Map(id) | PageValue::Set(id) | PageValue::Record(id) => {
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
                    lash_kernel_doc::heap::Element::Item(v) => {
                        copy_page(heap, v, budget, path).map(|v| items.push(v))
                    }
                    lash_kernel_doc::heap::Element::Entry { key, value } => {
                        copy_page(heap, key, budget, path).and_then(|k| {
                            copy_page(heap, value, budget, path).map(|v| entries.push((k, v)))
                        })
                    }
                    lash_kernel_doc::heap::Element::Field { name, value } => text_copy(heap, name)
                        .map_err(CopyError::from)
                        .and_then(|n| {
                            copy_page(heap, value, budget, path).map(|v| fields.push((n, v)))
                        }),
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
                PageValue::List(_) => Datum::List(items),
                PageValue::Set(_) => Datum::Set(items),
                PageValue::Map(_) => Datum::Map(entries),
                _ => Datum::Record(fields),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use lash_kernel_doc::{Datum, Float, Integer, Measure, Value};

    use lash_kernel_state::arena::Bench;

    use super::{
        deep_size, empty_measure, immediate_measure, magnitude, nested_size, size, text_measure,
    };

    const MEASURES: [Measure; 4] = [
        Measure::Size,
        Measure::DeepSize,
        Measure::NestedSize,
        Measure::Magnitude,
    ];

    /// K-CHG-004 to K-CHG-006: what a value carries of its measurements is
    /// what the arena measures of it. A construction of no members, of
    /// every kind, measures as [`empty_measure`] says; a value the arena
    /// holds nothing of measures as [`immediate_measure`] says, and every
    /// other value is the arena's to measure.
    #[test]
    fn carried_measurements_are_the_arenas() {
        let mut bench = Bench::new();
        let measured = |bench: &Bench, value: &Value, measure: Measure| {
            let arena = bench.arena();
            match measure {
                Measure::Size => size(arena, value),
                Measure::DeepSize => deep_size(arena, value),
                Measure::NestedSize => nested_size(arena, value),
                Measure::Magnitude => Ok(magnitude(arena, value)),
            }
            .unwrap()
        };
        for empty in [
            Datum::Tuple(Vec::new()),
            Datum::List(Vec::new()),
            Datum::Set(Vec::new()),
            Datum::Map(Vec::new()),
            Datum::Record(Vec::new()),
        ] {
            let value = bench.build(&empty).unwrap();
            for measure in MEASURES {
                assert_eq!(
                    empty_measure(measure),
                    measured(&bench, &value, measure),
                    "{empty:?} {measure:?}"
                );
            }
        }
        for datum in [
            Datum::Null,
            Datum::Absent,
            Datum::Bool(true),
            Datum::Int(Integer::from(0)),
            Datum::Int(Integer::from(-7)),
            Datum::Int(Integer::from(i64::MAX)),
            Datum::Int(Integer::try_from(String::from("9223372036854775808")).unwrap()),
            Datum::Float(Float::new(2.75)),
            Datum::Float(Float::new(-1.5)),
            Datum::Float(Float::new(f64::INFINITY)),
            Datum::Text(String::new()),
            Datum::Text("fourteen bytes".into()),
            Datum::Text("fifteen bytes!!".into()),
            Datum::Text("é🙂".into()),
            Datum::Text("\0é🙂終 text that exceeds the inline representation".into()),
            Datum::Tuple(vec![Datum::Int(Integer::from(1))]),
            Datum::List(vec![Datum::Text("a".into())]),
        ] {
            let value = bench.build(&datum).unwrap();
            for measure in MEASURES {
                if let Datum::Text(text) = &datum {
                    assert_eq!(
                        text_measure(text, measure),
                        measured(&bench, &value, measure),
                        "detached native text {datum:?} {measure:?}"
                    );
                }
                if let Some(carried) = immediate_measure(&value, measure) {
                    assert_eq!(
                        carried,
                        measured(&bench, &value, measure),
                        "{datum:?} {measure:?}"
                    );
                } else {
                    assert!(
                        value.handle().is_some(),
                        "{datum:?} {measure:?}: only an arena object is the arena's to measure"
                    );
                }
            }
        }
    }
}
