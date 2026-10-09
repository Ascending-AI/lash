//! Exact integer arithmetic and explicitly selected IEEE division modes.

use std::cmp::Ordering;

use lash_kernel_doc::{Float, Integer, NativeError, Value, WorkCounter};
use num_bigint::BigInt;
use num_traits::{One, Signed, Zero};

use crate::numeric::{as_float, number_cmp, ratio_to_float};
use crate::raised;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Binary {
    Add,
    Sub,
    Mul,
    Div,
    DivFloor,
    DivTrunc,
    RemFloor,
    RemTrunc,
    Pow,
    Min,
    Max,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Unary {
    Neg,
    Abs,
    Floor,
    Ceil,
    Trunc,
    RoundEven,
    RoundAway,
    RoundUp,
    Sign,
    IsFinite,
    IsInfinite,
    IsNan,
    IsInteger,
}

pub(crate) fn binary(
    op: Binary,
    a: &Value,
    b: &Value,
    counter: &mut WorkCounter,
) -> Result<Value, NativeError> {
    if let (Value::Int(a), Value::Int(b)) = (a, b) {
        let a = a.as_bigint();
        let b = b.as_bigint();
        let result = match op {
            Binary::Add => a + b,
            Binary::Sub => a - b,
            Binary::Mul => a * b,
            Binary::Div => {
                return ratio_to_float(a, b).map(|value| Value::Float(Float::new(value)));
            }
            Binary::DivFloor | Binary::DivTrunc | Binary::RemFloor | Binary::RemTrunc => {
                if b.is_zero() {
                    return Err(raised("division_by_zero", "integer divisor is zero"));
                }
                let mut q = a / b;
                let mut r = a % b;
                if matches!(op, Binary::DivFloor | Binary::RemFloor)
                    && !r.is_zero()
                    && r.sign() != b.sign()
                {
                    q -= 1;
                    r += b;
                }
                if matches!(op, Binary::DivFloor | Binary::DivTrunc) {
                    q
                } else {
                    r
                }
            }
            Binary::Pow => integer_pow(a, b, counter)?,
            Binary::Min => a.min(b).clone(),
            Binary::Max => a.max(b).clone(),
        };
        return Ok(Value::Int(Integer::new(result)));
    }
    if matches!(op, Binary::Min | Binary::Max) {
        let order = number_cmp(a, b)?;
        // Min/max propagate NaN and otherwise return an original operand.
        // Opposite floating zeros choose -0 for min and +0 for max.
        if order.is_none() {
            return Ok(Value::Float(Float::new(f64::NAN)));
        }
        if let (Value::Float(a), Value::Float(b)) = (a, b)
            && a.get() == 0.0
            && b.get() == 0.0
        {
            let negative = if matches!(op, Binary::Min) {
                a.get().is_sign_negative() || b.get().is_sign_negative()
            } else {
                a.get().is_sign_negative() && b.get().is_sign_negative()
            };
            return Ok(Value::Float(Float::new(if negative { -0.0 } else { 0.0 })));
        }
        return Ok(
            if matches!(
                (op, order),
                (Binary::Min, Some(Ordering::Greater)) | (Binary::Max, Some(Ordering::Less))
            ) {
                b.clone()
            } else {
                a.clone()
            },
        );
    }
    let a = as_float(a)?;
    let b = as_float(b)?;
    let result = match op {
        Binary::Add => a + b,
        Binary::Sub => a - b,
        Binary::Mul => a * b,
        Binary::Div => a / b,
        Binary::DivTrunc => (a / b).trunc(),
        Binary::RemTrunc => libm::fmod(a, b),
        Binary::RemFloor => floor_remainder(a, b),
        Binary::DivFloor => {
            if b == 0.0 {
                a / b
            } else {
                ((a - floor_remainder(a, b)) / b).round_ties_even()
            }
        }
        Binary::Pow => libm::pow(a, b),
        Binary::Min | Binary::Max => unreachable!("min/max returned before float arithmetic"),
    };
    Ok(Value::Float(Float::new(result)))
}

fn floor_remainder(a: f64, b: f64) -> f64 {
    let remainder = libm::fmod(a, b);
    if remainder == 0.0 {
        libm::copysign(0.0, b)
    } else if remainder.is_sign_negative() != b.is_sign_negative() {
        remainder + b
    } else {
        remainder
    }
}

fn integer_pow(a: &BigInt, b: &BigInt, counter: &mut WorkCounter) -> Result<BigInt, NativeError> {
    if b.is_negative() {
        return Err(raised(
            "number_range",
            "integer exponent must be non-negative",
        ));
    }
    let mut exponent = b.clone();
    let mut base = a.clone();
    let mut result = BigInt::one();
    while !exponent.is_zero() {
        if exponent.bit(0) {
            spend_product(&result, &base, counter)?;
            result *= &base;
        }
        exponent >>= 1usize;
        if !exponent.is_zero() {
            spend_product(&base, &base, counter)?;
            base = &base * &base;
        }
    }
    Ok(result)
}

fn spend_product(a: &BigInt, b: &BigInt, counter: &mut WorkCounter) -> Result<(), NativeError> {
    // Upper bound on output magnitude in 64-bit words; spent before allocating.
    counter.spend(a.bits().saturating_add(b.bits()).div_ceil(64).max(1))?;
    Ok(())
}

pub(crate) fn unary(op: Unary, value: &Value) -> Result<Value, NativeError> {
    if let Value::Int(value) = value {
        return Ok(match op {
            Unary::Neg => Value::Int(Integer::new(-value.as_bigint())),
            Unary::Abs => Value::Int(Integer::new(value.as_bigint().abs())),
            Unary::Floor
            | Unary::Ceil
            | Unary::Trunc
            | Unary::RoundEven
            | Unary::RoundAway
            | Unary::RoundUp => Value::Int(value.clone()),
            Unary::Sign => Value::Int(Integer::new(value.as_bigint().signum())),
            Unary::IsFinite | Unary::IsInteger => Value::Bool(true),
            Unary::IsInfinite | Unary::IsNan => Value::Bool(false),
        });
    }
    let value = as_float(value)?;
    Ok(match op {
        Unary::Neg => Value::Float(Float::new(-value)),
        Unary::Abs => Value::Float(Float::new(value.abs())),
        Unary::Floor => Value::Float(Float::new(value.floor())),
        Unary::Ceil => Value::Float(Float::new(value.ceil())),
        Unary::Trunc => Value::Float(Float::new(value.trunc())),
        Unary::RoundEven => Value::Float(Float::new(value.round_ties_even())),
        Unary::RoundAway => Value::Float(Float::new(value.round())),
        Unary::RoundUp => {
            // Avoid adding .5 to a large integer float or a value immediately
            // below a half: compare the fraction without introducing a tie.
            let floor = value.floor();
            let result = if value - floor < 0.5 {
                floor
            } else {
                value.ceil()
            };
            Value::Float(Float::new(if result == 0.0 {
                libm::copysign(0.0, value)
            } else {
                result
            }))
        }
        Unary::Sign => Value::Float(Float::new(if value.is_nan() || value == 0.0 {
            value
        } else if value < 0.0 {
            -1.0
        } else {
            1.0
        })),
        Unary::IsFinite => Value::Bool(value.is_finite()),
        Unary::IsInfinite => Value::Bool(value.is_infinite()),
        Unary::IsNan => Value::Bool(value.is_nan()),
        Unary::IsInteger => Value::Bool(value.is_finite() && value.trunc() == value),
    })
}
