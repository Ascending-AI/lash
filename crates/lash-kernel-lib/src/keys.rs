//! Validated keys. The canonical number representation is private: no guest
//! operation exposes a hash (`K-VAL-028`).

use std::hash::{Hash, Hasher};

use lash_kernel_doc::{NativeError, Value};
use num_bigint::BigInt;

use crate::numeric::dyadic;
use crate::raised;

/// An immutable, validated map key or set member (`K-KEY-001/002`).
///
/// Equality and hashing normalize mathematical numbers, signed zeros and
/// every NaN, recursively inside tuples. `value()` retains the first spelling
/// for a map's insertion-order semantics. Mutable values must use `ref(x)`.
#[derive(Clone, Debug)]
pub struct Key(Value);

impl Key {
    pub fn new(value: Value) -> Result<Self, NativeError> {
        validate(&value)?;
        Ok(Self(value))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }

    pub fn into_value(self) -> Value {
        self.0
    }
}

fn validate(value: &Value) -> Result<(), NativeError> {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bytes(_)
            | Value::Timestamp(_)
            | Value::Ref(_) => {}
            Value::Tuple(values) => pending.extend(values.iter()),
            _ => {
                return Err(raised(
                    "invalid_key",
                    "expected an immutable key; use ref for an object or task",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn key_equal(a: &Value, b: &Value) -> bool {
    let mut pending = vec![(a, b)];
    while let Some((a, b)) = pending.pop() {
        match (a, b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                if !matches!(
                    crate::numeric::number_cmp(a, b),
                    Ok(Some(std::cmp::Ordering::Equal))
                ) && !matches!((a,b),(Value::Float(a),Value::Float(b)) if a.get().is_nan() && b.get().is_nan())
                {
                    return false;
                }
            }
            (Value::Tuple(a), Value::Tuple(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                pending.extend(a.iter().zip(b.iter()));
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

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        key_equal(&self.0, &other.0)
    }
}
impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut pending = vec![&self.0];
        while let Some(value) = pending.pop() {
            if let Value::Tuple(values) = value {
                7u8.hash(state);
                values.len().hash(state);
                pending.extend(values.iter().rev());
            } else {
                hash_value(value, state);
            }
        }
    }
}

fn hash_number<H: Hasher>(mut coefficient: BigInt, mut exponent: i64, state: &mut H) {
    if let Some(zeros) = coefficient.trailing_zeros() {
        coefficient >>= zeros as usize;
        exponent += zeros as i64;
    }
    coefficient.hash(state);
    exponent.hash(state);
}

fn hash_value<H: Hasher>(value: &Value, state: &mut H) {
    match value {
        Value::Int(value) => {
            2u8.hash(state);
            0u8.hash(state);
            hash_number(value.as_bigint().into_owned(), 0, state);
        }
        Value::Float(value) => {
            2u8.hash(state);
            let value = value.get();
            if value.is_nan() {
                1u8.hash(state);
            } else if value.is_infinite() {
                2u8.hash(state);
                value.is_sign_negative().hash(state);
            } else {
                0u8.hash(state);
                let (coefficient, exponent) = dyadic(value);
                hash_number(coefficient, i64::from(exponent), state);
            }
        }
        Value::Null => 0u8.hash(state),
        Value::Bool(value) => {
            1u8.hash(state);
            value.hash(state);
        }
        Value::Text(value) => {
            3u8.hash(state);
            value.hash(state);
        }
        Value::Bytes(value) => {
            4u8.hash(state);
            value.hash(state);
        }
        Value::Timestamp(value) => {
            5u8.hash(state);
            value.hash(state);
        }
        Value::Ref(value) => {
            6u8.hash(state);
            value.hash(state);
        }
        // The constructor proves these variants are unreachable.
        _ => {}
    }
}
