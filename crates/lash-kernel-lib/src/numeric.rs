//! Exact dyadic comparison and single-round conversion. No integer is first
//! rounded to a float while comparing (`K-VAL-021`, `K-NUM-004`).

use std::cmp::Ordering;

use lash_kernel_doc::{Float, Integer, NativeError, Value};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{FromPrimitive, One, ToPrimitive, Zero};

use crate::raised;

/// Converts an integer to the nearest binary64, ties to even. A result
/// that rounds to infinity raises `number_range` (`K-NUM-003`).
pub fn integer_to_float(value: &Integer) -> Result<Float, NativeError> {
    ratio_to_float(value.as_bigint(), &BigInt::one()).map(Float::new)
}

/// Converts a finite, integral binary64 exactly to an arbitrary integer.
/// Non-finite or fractional values raise `number_range`. Both zeros become 0.
pub fn float_to_integer(value: Float) -> Result<Integer, NativeError> {
    let value = value.get();
    if !value.is_finite() || value.trunc() != value {
        return Err(raised("number_range", "expected a finite integral float"));
    }
    BigInt::from_f64(value)
        .map(Integer::new)
        .ok_or_else(|| raised("number_range", "float cannot be represented as an integer"))
}

/// The finite float's exact value is `coefficient * 2^exponent`.
/// Every nonzero coefficient is odd; signed zero has coefficient zero.
pub(crate) fn dyadic(value: f64) -> (BigInt, i32) {
    let bits = value.to_bits();
    let encoded_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mut coefficient, mut exponent) = if encoded_exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), encoded_exponent - 1075)
    };
    if coefficient == 0 {
        return (BigInt::zero(), 0);
    }
    let zeros = coefficient.trailing_zeros();
    coefficient >>= zeros;
    exponent += zeros as i32;
    let coefficient = BigInt::from(coefficient);
    (
        if bits >> 63 == 0 {
            coefficient
        } else {
            -coefficient
        },
        exponent,
    )
}

pub(crate) fn int_float_cmp(integer: &BigInt, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float.is_infinite() {
        return Some(if float.is_sign_positive() {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let (coefficient, exponent) = dyadic(float);
    Some(if exponent >= 0 {
        integer.cmp(&(coefficient << exponent as usize))
    } else {
        (integer << (-exponent) as usize).cmp(&coefficient)
    })
}

pub(crate) fn number_cmp(a: &Value, b: &Value) -> Result<Option<Ordering>, NativeError> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Ok(Some(a.cmp(b))),
        (Value::Float(a), Value::Float(b)) => Ok(a.get().partial_cmp(&b.get())),
        (Value::Int(a), Value::Float(b)) => Ok(int_float_cmp(a.as_bigint(), b.get())),
        (Value::Float(a), Value::Int(b)) => {
            Ok(int_float_cmp(b.as_bigint(), a.get()).map(Ordering::reverse))
        }
        _ => Err(raised("type_error", "expected two numbers")),
    }
}

pub(crate) fn as_float(value: &Value) -> Result<f64, NativeError> {
    match value {
        Value::Int(value) => integer_to_float(value).map(Float::get),
        Value::Float(value) => Ok(value.get()),
        _ => Err(raised("type_error", "expected a number")),
    }
}

/// Rounds the rational exactly once, including subnormal ties and overflow.
pub(crate) fn ratio_to_float(a: &BigInt, b: &BigInt) -> Result<f64, NativeError> {
    if b.is_zero() {
        return Err(raised("division_by_zero", "integer divisor is zero"));
    }
    let negative = (a.sign() == Sign::Minus) != (b.sign() == Sign::Minus);
    let signed = |value: f64| if negative { -value } else { value };
    if a.is_zero() {
        return Ok(signed(0.0));
    }
    let a = a.magnitude();
    let b = b.magnitude();
    let mut exponent = i128::from(a.bits()) - i128::from(b.bits());
    // A ratio with this bit difference cannot round into the finite range.
    if exponent > 1024 {
        return Err(raised("number_range", "quotient rounds to infinity"));
    }
    if exponent < -1075 {
        return Ok(signed(0.0));
    }
    let below_power = if exponent >= 0 {
        a < &(b << exponent as usize)
    } else {
        &(a << (-exponent) as usize) < b
    };
    if below_power {
        exponent -= 1;
    }
    let step = (exponent - 52).max(-1074);
    let (numerator, denominator) = if step >= 0 {
        (a.clone(), b << step as usize)
    } else {
        (a << (-step) as usize, b.clone())
    };
    let mut quotient = &numerator / &denominator;
    let remainder = numerator % &denominator;
    let twice = remainder << 1usize;
    if twice > denominator || (twice == denominator && quotient.bit(0)) {
        quotient += BigUint::one();
    }
    let significand = quotient
        .to_u64()
        .ok_or_else(|| raised("number_range", "quotient rounds to infinity"))?;
    // This integer has at most 54 significant bits and is exactly representable
    // (the 54th bit can only be the carry 2^53).
    let value = libm::scalbn(significand as f64, step as i32);
    if value.is_infinite() {
        Err(raised("number_range", "quotient rounds to infinity"))
    } else {
        Ok(signed(value))
    }
}
