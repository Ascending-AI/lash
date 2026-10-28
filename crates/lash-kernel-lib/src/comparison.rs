//! Fallible structural equality, datum identity and lexicographic order.
use crate::numeric::{Numeric, number_cmp, scalar_cmp};
use crate::raised;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::native::SequenceView;
use lash_kernel_doc::{Args, NativeError, Nursery, Value};

use std::cmp::Ordering;

/// Prepared equality can resolve identical function handles without an arena.
#[inline]
pub(crate) fn scalar_equal(a: &Value, b: &Value) -> Option<bool> {
    lash_kernel_doc::scalar::immediate_equal(a, b).or_else(|| match (a, b) {
        (Value::Function(a), Value::Function(b)) if a == b => Some(true),
        _ => None,
    })
}

#[inline]
pub(crate) fn scalar_compare(
    a: &Value,
    b: &Value,
) -> Option<Result<Option<Ordering>, NativeError>> {
    use Value as V;
    Some(match (a, b) {
        (V::Int(a), V::Int(b)) => Ok(Some(a.cmp(b))),
        (V::Float(a), V::Float(b)) => Ok(crate::numeric::float_cmp(*a, *b)),
        (V::Int(_) | V::Float(_), V::Int(_) | V::Float(_)) => {
            scalar_cmp(&Numeric::resident(a)?, &Numeric::resident(b)?)
        }
        (V::Text(a), V::Text(b)) => Ok(Some(a.as_bytes().cmp(b.as_bytes()))),
        (V::Timestamp(a), V::Timestamp(b)) => Ok(Some(a.cmp(b))),
        (V::Bool(a), V::Bool(b)) => Ok(Some(a.cmp(b))),
        (V::Int(_) | V::BigInt(_) | V::Float(_), V::Int(_) | V::BigInt(_) | V::Float(_))
        | (V::Text(_) | V::LongText(_), V::Text(_) | V::LongText(_))
        | (V::Bytes(_), V::Bytes(_))
        | (V::Timestamp(_) | V::BigTimestamp(_), V::Timestamp(_) | V::BigTimestamp(_))
        | (V::Tuple(_), V::Tuple(_))
        | (V::List(_), V::List(_)) => return None,
        _ => Err(raised("type_error", "values have no shared ordering")),
    })
}

pub fn equal(a: &Value, b: &Value, args: &Args<'_>) -> Result<bool, NativeError> {
    args.equal(a, b)
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

pub fn same(a: &Value, b: &Value, args: &Args<'_>) -> Result<bool, NativeError> {
    let mut pending = vec![(*a, *b)];
    while let Some((a, b)) = pending.pop() {
        if let Some(same) = scalar_same(&a, &b) {
            if !same {
                return Ok(false);
            }
            continue;
        }
        match (&a, &b) {
            (Value::Tuple(_), Value::Tuple(_)) => {
                let (Some(left), Some(right)) = (args.sequence(&a)?, args.sequence(&b)?) else {
                    return Err(NativeError::Malformed("a tuple names no sequence"));
                };
                if left.len() != right.len() {
                    return Ok(false);
                }
                // Members in order: the last pushed is compared first.
                for index in (0..left.len()).rev() {
                    let (Some(x), Some(y)) = (left.get(index)?, right.get(index)?) else {
                        return Err(NativeError::Malformed("a tuple member is absent"));
                    };
                    pending.push((*x, *y));
                }
            }
            (Value::Error(_), Value::Error(_)) => {
                let (Some(left), Some(right)) = (args.error(&a)?, args.error(&b)?) else {
                    return Err(NativeError::Malformed("an error names no parts"));
                };
                if !args.equal(&left.kind, &right.kind)?
                    || !args.equal(&left.message, &right.message)?
                {
                    return Ok(false);
                }
                pending.push((left.data, right.data));
            }
            _ if a.kind() == b.kind() => {
                if !args.equal(&a, &b)? {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Each sequence advances one member at a time, and never snapshots a
/// list.
/// A paged integer compared whole holds its bytes in `new`.
#[inline]
pub fn compare(
    a: &Value,
    b: &Value,
    args: &Args<'_>,
    new: &mut Nursery<'_>,
) -> Result<Option<Ordering>, NativeError> {
    compare_reading(a, b, args, |a, b| number_cmp(a, b, args, new))
}

/// Ordering with an input-only numeric reader. The native caller retains its
/// physical read reservations; a logical description supplies mathematical
/// numbers without installing values or reserving nursery room.
pub(crate) fn compare_reading(
    a: &Value,
    b: &Value,
    args: &Args<'_>,
    mut numbers: impl FnMut(&Value, &Value) -> Result<Option<Ordering>, NativeError>,
) -> Result<Option<Ordering>, NativeError> {
    if let Some(answer) = scalar_compare(a, b) {
        return answer;
    }
    enum Step<'a> {
        Values(Value, Value, bool),
        Members(SequenceView<'a>, SequenceView<'a>, u64),
        Leave,
    }
    let mut pending = vec![Step::Values(*a, *b, false)];
    // The list pairs being compared, innermost last.
    let mut active: Vec<(ArenaHandle, ArenaHandle)> = Vec::new();
    while let Some(step) = pending.pop() {
        let (a, b, member) = match step {
            Step::Leave => {
                active.pop();
                continue;
            }
            Step::Members(a, b, mut index) => {
                while index < a.len().min(b.len()) {
                    let left = a
                        .get(index)?
                        .ok_or(NativeError::Malformed("an ordered member is absent"))?;
                    let right = b
                        .get(index)?
                        .ok_or(NativeError::Malformed("an ordered member is absent"))?;
                    if let (Value::Int(left), Value::Int(right)) = (left, right) {
                        let order = left.cmp(right);
                        if order != Ordering::Equal {
                            return Ok(Some(order));
                        }
                        index += 1;
                        continue;
                    }
                    pending.push(Step::Members(a, b, index + 1));
                    pending.push(Step::Values(*left, *right, true));
                    break;
                }
                if index == a.len().min(b.len()) {
                    let order = a.len().cmp(&b.len());
                    if order != Ordering::Equal {
                        return Ok(Some(order));
                    }
                }
                continue;
            }
            Step::Values(a, b, member) => (a, b, member),
        };
        if member && args.equal(&a, &b)? {
            continue;
        }
        if let Some(order) = scalar_compare(&a, &b) {
            let order = order?;
            if order != Some(Ordering::Equal) {
                return Ok(order);
            }
            continue;
        }
        let order = match (&a, &b) {
            (
                Value::Int(_) | Value::BigInt(_) | Value::Float(_),
                Value::Int(_) | Value::BigInt(_) | Value::Float(_),
            ) => numbers(&a, &b)?,
            (Value::Text(_) | Value::LongText(_), Value::Text(_) | Value::LongText(_)) => {
                let (Some(left), Some(right)) = (args.text(&a)?, args.text(&b)?) else {
                    return Err(NativeError::Malformed("a text names no text"));
                };
                Some(left.whole()?.as_bytes().cmp(right.whole()?.as_bytes()))
            }
            (Value::Bytes(_), Value::Bytes(_)) => {
                let (Some(left), Some(right)) = (args.bytes(&a)?, args.bytes(&b)?) else {
                    return Err(NativeError::Malformed("bytes name no bytes"));
                };
                Some(left.whole()?.cmp(&right.whole()?))
            }
            (
                Value::Timestamp(_) | Value::BigTimestamp(_),
                Value::Timestamp(_) | Value::BigTimestamp(_),
            ) => match (args.timestamp(&a)?, args.timestamp(&b)?) {
                (Some(left), Some(right)) => Some(left.cmp(&right)),
                _ => return Err(NativeError::Malformed("a timestamp names no time")),
            },
            (Value::Tuple(_), Value::Tuple(_)) => {
                let (Some(left), Some(right)) = (args.sequence(&a)?, args.sequence(&b)?) else {
                    return Err(NativeError::Malformed("a tuple names no sequence"));
                };
                pending.push(Step::Members(left, right, 0));
                continue;
            }
            (Value::List(a_id), Value::List(b_id)) => {
                let pair = (*a_id, *b_id);
                if active.contains(&pair) {
                    continue;
                }
                let (Some(left), Some(right)) = (args.sequence(&a)?, args.sequence(&b)?) else {
                    return Err(NativeError::Malformed("a list names no sequence"));
                };
                active.push(pair);
                pending.push(Step::Leave);
                pending.push(Step::Members(left, right, 0));
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

/// UTF-16 order reads resident text chunks without materializing either text.
pub(crate) fn utf16_compare(
    a: &Value,
    b: &Value,
    args: &Args<'_>,
) -> Result<Ordering, NativeError> {
    let (Some(a), Some(b)) = (args.text(a)?, args.text(b)?) else {
        return Err(raised("type_error", "expected text keys"));
    };
    if let (Some(a), Some(b)) = (a.as_str(), b.as_str()) {
        return Ok(if a.is_ascii() && b.is_ascii() {
            a.cmp(b)
        } else {
            a.encode_utf16().cmp(b.encode_utf16())
        });
    }
    fn next<'a>(
        chunks: &mut impl Iterator<Item = Result<&'a str, NativeError>>,
        units: &mut std::str::EncodeUtf16<'a>,
    ) -> Result<Option<u16>, NativeError> {
        loop {
            if let Some(unit) = units.next() {
                return Ok(Some(unit));
            }
            let Some(chunk) = chunks.next() else {
                return Ok(None);
            };
            *units = chunk?.encode_utf16();
        }
    }
    let (mut a, mut b) = (a.chunks(), b.chunks());
    let (mut x, mut y) = ("".encode_utf16(), "".encode_utf16());
    loop {
        let (a, b) = (next(&mut a, &mut x)?, next(&mut b, &mut y)?);
        let order = a.cmp(&b);
        if order != Ordering::Equal || a.is_none() {
            return Ok(order);
        }
    }
}
