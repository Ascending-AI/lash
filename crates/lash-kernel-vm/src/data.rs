//! Values at the edges of a run: their charge sizes (`K-CHG-004` to
//! `K-CHG-006`), copying out (`K-EFF-002`) and decoding in (`K-EFF-005` to
//! `K-EFF-007`).

use std::collections::BTreeSet;

use lash_kernel_doc::{
    Datum, ErrorDatum, Float, Integer, Name, NumberPolicy, NumberToken, ObjectId, Type, Value,
};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive, Zero};

use crate::heap::{Heap, MAX_VALUE_DEPTH, Obj};

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

/// A value's size with everything it holds, a heap object counted the
/// first time it is reached (`K-CHG-005`).
pub(crate) fn deep_size(heap: &Heap, value: &Value) -> u64 {
    // A value that holds nothing is its own size: most measured values are.
    if !matches!(
        value,
        Value::List(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Record(_)
            | Value::Tuple(_)
            | Value::Error(_)
    ) {
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
    let mut total = 0u64;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        let held = match value {
            Value::Tuple(members) => &members[..],
            Value::Error(error) => std::slice::from_ref(&error.data),
            _ => continue,
        };
        for member in held {
            let size = match member {
                Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => 1,
                _ => size(heap, member),
            };
            total = total.saturating_add(size);
            pending.push(member);
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
    copy(heap, value, MAX_VALUE_DEPTH, &mut Vec::new())
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
            format!("a value copied out of the run nests more than {MAX_VALUE_DEPTH} levels"),
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

/// How an inbound datum is read.
pub(crate) struct Decoder<'a> {
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
        self.fit(datum, ty, MAX_VALUE_DEPTH)
    }

    fn all(&self, items: &[Datum], ty: &Type, budget: usize) -> Result<Vec<Datum>, String> {
        items
            .iter()
            .map(|item| self.fit(item, ty, budget))
            .collect()
    }

    fn fit(&self, datum: &Datum, ty: &Type, budget: usize) -> Result<Datum, String> {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(format!("it nests more than {MAX_VALUE_DEPTH} levels"));
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
            return Err(format!("it nests more than {MAX_VALUE_DEPTH} levels"));
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
