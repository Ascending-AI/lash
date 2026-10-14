//! Values at the edges of a run: their charge sizes (`K-CHG-004` to
//! `K-CHG-006`), copying out (`K-EFF-002`) and decoding in (`K-EFF-005` to
//! `K-EFF-007`).

use std::collections::BTreeSet;

use lash_kernel_doc::{
    Datum, EntityId, ErrorDatum, Float, Integer, Name, NumberPolicy, NumberToken, Type, Value,
};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive, Zero};

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
    let mut out = String::new();
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

/// How an inbound datum is read.
pub(crate) struct Decoder<'a> {
    pub(crate) max_depth: usize,
    pub(crate) policy: NumberPolicy,
    /// Whether the document declares a function of this name.
    pub(crate) declared: &'a dyn Fn(&Name) -> bool,
}

/// The exact integer a JSON number spells, when its value is integral.
fn integral(token: &NumberToken) -> Option<BigInt> {
    let text = token.as_str();
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (text, 0),
    };
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = format!("{whole}{fraction}");
    let mut exponent = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;
    while exponent < 0 && digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
        exponent += 1;
    }
    let value: BigInt = digits.parse().ok()?;
    if value.is_zero() {
        return Some(value);
    }
    // A number this large is not one a run can hold.
    if !(0..=4096).contains(&exponent) {
        return None;
    }
    let value = value * BigInt::from(10u8).pow(u32::try_from(exponent).ok()?);
    Some(if negative { -value } else { value })
}

fn finite(value: f64, what: &str) -> Result<Datum, String> {
    if value.is_finite() {
        Ok(Datum::Float(Float::new(value)))
    } else {
        Err(format!("{what} has no finite nearest float"))
    }
}

fn nearest(token: &NumberToken) -> Result<Datum, String> {
    let value: f64 = token
        .as_str()
        .parse()
        .map_err(|_| format!("`{}` is not a number", token.as_str()))?;
    finite(value, token.as_str())
}

fn whole(float: f64) -> Option<Datum> {
    (float.is_finite() && float.fract() == 0.0)
        .then(|| BigInt::from_f64(float))
        .flatten()
        .map(|integer| Datum::Int(Integer::new(integer)))
}

fn kind(datum: &Datum) -> &'static str {
    match datum {
        Datum::Null => "null",
        Datum::Absent => "absent",
        Datum::Bool(_) => "a bool",
        Datum::Int(_) | Datum::Float(_) | Datum::Number(_) => "a number",
        Datum::Text(_) => "a text",
        Datum::Bytes(_) => "bytes",
        Datum::Timestamp(_) => "a timestamp",
        Datum::Tuple(_) => "a tuple",
        Datum::List(_) => "a list",
        Datum::Map(_) => "a map",
        Datum::Set(_) => "a set",
        Datum::Record(_) => "a record",
        Datum::Error(_) => "an error",
        Datum::Function(_) => "a function reference",
        Datum::Handle(_) => "a handle",
    }
}

impl Decoder<'_> {
    /// Reads `datum` as `ty`: the same tree with every number decoded and
    /// every part in the shape the type names, or why it does not fit.
    pub(crate) fn decode(&self, datum: &Datum, ty: &Type) -> Result<Datum, String> {
        self.fit(datum, ty, self.max_depth)
    }

    fn all(&self, items: &[Datum], ty: &Type, budget: usize) -> Result<Vec<Datum>, String> {
        items
            .iter()
            .map(|item| self.fit(item, ty, budget))
            .collect()
    }

    fn fit(&self, datum: &Datum, ty: &Type, budget: usize) -> Result<Datum, String> {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(format!(
                "it exceeds the configured {}-level value depth",
                self.max_depth
            ));
        };
        let mismatch = |expected: &str| Err(format!("expected {expected}, found {}", kind(datum)));
        match (ty, datum) {
            (Type::Union(members), _) => members
                .iter()
                .find_map(|member| self.fit(datum, member, budget).ok())
                .ok_or_else(|| format!("{} fits no member of the union", kind(datum))),
            (Type::Any, _) => self.any(datum, budget),
            (Type::Null, Datum::Null)
            | (Type::Absent, Datum::Absent)
            | (Type::Bool, Datum::Bool(_))
            | (Type::Text, Datum::Text(_))
            | (Type::Bytes, Datum::Bytes(_))
            | (Type::Timestamp, Datum::Timestamp(_))
            | (Type::Int, Datum::Int(_))
            | (Type::Float, Datum::Float(_))
            | (Type::Number, Datum::Int(_) | Datum::Float(_)) => Ok(datum.clone()),
            (Type::Int, Datum::Float(float)) => {
                whole(float.get()).ok_or_else(|| format!("{float} is not an integer"))
            }
            (Type::Int, Datum::Number(token)) => integral(token)
                .map(|integer| Datum::Int(Integer::new(integer)))
                .ok_or_else(|| format!("{} is not an integer", token.as_str())),
            (Type::Float, Datum::Int(integer)) => {
                finite(integer.to_f64().unwrap_or(f64::INFINITY), "the integer")
            }
            (Type::Float, Datum::Number(token)) => nearest(token),
            (Type::Number, Datum::Number(token)) => self.bare(token),
            (Type::Error, Datum::Error(error)) => Ok(Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self.any(&error.data, budget)?,
            }))),
            (Type::Enum(members), Datum::Text(text)) => {
                if members.contains(text) {
                    Ok(datum.clone())
                } else {
                    Err(format!("`{text}` is not a member of the enum"))
                }
            }
            (Type::Handle(kind), Datum::Handle(handle)) => {
                if handle.kind == *kind {
                    Ok(datum.clone())
                } else {
                    Err(format!(
                        "expected a `{kind}` handle, found a `{}` handle",
                        handle.kind
                    ))
                }
            }
            (Type::Function(_), Datum::Function(name)) => self.function(name, datum),
            (Type::Tuple(types), Datum::Tuple(items) | Datum::List(items)) => {
                if types.len() != items.len() {
                    return Err(format!(
                        "expected a tuple of {}, found {} member(s)",
                        types.len(),
                        items.len()
                    ));
                }
                Ok(Datum::Tuple(
                    items
                        .iter()
                        .zip(types)
                        .map(|(item, ty)| self.fit(item, ty, budget))
                        .collect::<Result<_, _>>()?,
                ))
            }
            (Type::List(item), Datum::List(items)) => {
                Ok(Datum::List(self.all(items, item, budget)?))
            }
            (Type::Set(item), Datum::Set(items) | Datum::List(items)) => {
                Ok(Datum::Set(self.all(items, item, budget)?))
            }
            (Type::Map(map), Datum::Map(entries)) => Ok(Datum::Map(
                entries
                    .iter()
                    .map(|(key, value)| {
                        Ok((
                            self.fit(key, &map.key, budget)?,
                            self.fit(value, &map.value, budget)?,
                        ))
                    })
                    .collect::<Result<_, String>>()?,
            )),
            // A JSON object read as a map: its member names are the keys.
            (Type::Map(map), Datum::Record(fields)) => Ok(Datum::Map(
                fields
                    .iter()
                    .map(|(name, value)| {
                        Ok((
                            self.fit(&Datum::Text(name.clone()), &map.key, budget)?,
                            self.fit(value, &map.value, budget)?,
                        ))
                    })
                    .collect::<Result<_, String>>()?,
            )),
            (Type::Record(record), Datum::Record(fields)) => {
                for field in &record.fields {
                    if !field.optional && !fields.iter().any(|(name, _)| *name == field.name) {
                        return Err(format!("the required field `{}` is missing", field.name));
                    }
                }
                let mut decoded = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let declared = record.fields.iter().find(|field| field.name == *name);
                    let ty = match (declared, &record.rest) {
                        (Some(field), _) => &field.ty,
                        (None, Some(rest)) => rest.as_ref(),
                        (None, None) => {
                            return Err(format!("the record type has no field `{name}`"));
                        }
                    };
                    let value = self
                        .fit(value, ty, budget)
                        .map_err(|problem| format!("field `{name}`: {problem}"))?;
                    decoded.push((name.clone(), value));
                }
                Ok(Datum::Record(decoded))
            }
            (Type::Null, _) => mismatch("null"),
            (Type::Absent, _) => mismatch("absent"),
            (Type::Bool, _) => mismatch("a bool"),
            (Type::Int, _) => mismatch("an integer"),
            (Type::Float, _) => mismatch("a float"),
            (Type::Number, _) => mismatch("a number"),
            (Type::Text | Type::Enum(_), _) => mismatch("a text"),
            (Type::Bytes, _) => mismatch("bytes"),
            (Type::Timestamp, _) => mismatch("a timestamp"),
            (Type::Tuple(_), _) => mismatch("a tuple"),
            (Type::List(_), _) => mismatch("a list"),
            (Type::Map(_), _) => mismatch("a map"),
            (Type::Set(_), _) => mismatch("a set"),
            (Type::Record(_), _) => mismatch("a record"),
            (Type::Function(_), _) => mismatch("a function reference"),
            (Type::Task(_), _) => mismatch("a task handle, which no effect returns"),
            (Type::Error, _) => mismatch("an error"),
            (Type::Handle(_), _) => mismatch("a handle"),
        }
    }

    fn bare(&self, token: &NumberToken) -> Result<Datum, String> {
        match self.policy {
            NumberPolicy::BySpelling if token.is_integer_spelling() => integral(token)
                .map(|integer| Datum::Int(Integer::new(integer)))
                .ok_or_else(|| format!("{} is not an integer", token.as_str())),
            _ => nearest(token),
        }
    }

    fn function(&self, name: &Name, datum: &Datum) -> Result<Datum, String> {
        if (self.declared)(name) {
            Ok(datum.clone())
        } else {
            Err(format!("the document declares no function `{name}`"))
        }
    }

    fn any(&self, datum: &Datum, budget: usize) -> Result<Datum, String> {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(format!(
                "it exceeds the configured {}-level value depth",
                self.max_depth
            ));
        };
        let all = |items: &[Datum]| -> Result<Vec<Datum>, String> {
            items.iter().map(|item| self.any(item, budget)).collect()
        };
        Ok(match datum {
            Datum::Number(token) => self.bare(token)?,
            Datum::Function(name) => self.function(name, datum)?,
            Datum::Tuple(items) => Datum::Tuple(all(items)?),
            Datum::List(items) => Datum::List(all(items)?),
            Datum::Set(items) => Datum::Set(all(items)?),
            Datum::Map(entries) => Datum::Map(
                entries
                    .iter()
                    .map(|(key, value)| Ok((self.any(key, budget)?, self.any(value, budget)?)))
                    .collect::<Result<_, String>>()?,
            ),
            Datum::Record(fields) => Datum::Record(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), self.any(value, budget)?)))
                    .collect::<Result<_, String>>()?,
            ),
            Datum::Error(error) => Datum::Error(Box::new(ErrorDatum {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self.any(&error.data, budget)?,
            })),
            other => other.clone(),
        })
    }
}
