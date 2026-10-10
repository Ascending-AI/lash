//! Exact integer arithmetic and explicitly selected IEEE division modes.

use std::cmp::Ordering;

use lash_kernel_doc::{Float, Integer, NativeError, NativeHeap, Value, WorkCounter};
use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive, Zero};

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
    heap: &mut dyn NativeHeap,
) -> Result<Value, NativeError> {
    if let (Value::Int(a), Value::Int(b)) = (a, b) {
        if let (Some(a), Some(b)) = (a.to_i64(), b.to_i64())
            && let Some(result) = in_word(op, a, b)
        {
            return Ok(Value::Int(Integer::from(result)));
        }
        let (a, b) = (a.as_bigint(), b.as_bigint());
        let (a, b) = (&*a, &*b);
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
            Binary::Pow => integer_pow(a, b, counter, heap)?,
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

fn integer_pow(
    a: &BigInt,
    b: &BigInt,
    counter: &mut WorkCounter,
    heap: &mut dyn NativeHeap,
) -> Result<BigInt, NativeError> {
    if b.is_negative() {
        return Err(raised(
            "number_range",
            "integer exponent must be non-negative",
        ));
    }
    let mut exponent = b.clone();
    let mut base = a.clone();
    let mut result = BigInt::one();
    let mut room = Room { heap, peak: 0 };
    while !exponent.is_zero() {
        if exponent.bit(0) {
            room.product(&result, &base, &result, &base, counter)?;
            result *= &base;
        }
        exponent >>= 1usize;
        if !exponent.is_zero() {
            room.product(&result, &base, &base, &base, counter)?;
            base = &base * &base;
        }
    }
    Ok(result)
}

/// The memory an integer power holds at its largest: the result so far, the
/// base and the product being taken.
struct Room<'a> {
    heap: &'a mut dyn NativeHeap,
    /// The most bytes any product so far has needed, all of it reserved.
    peak: u64,
}

impl Room<'_> {
    /// Admits the product `a * b`, taken while `result` and `base` are
    /// held. The guard counts its words as work; the bytes it adds to the
    /// most the call has held are reserved against the run's memory bound.
    /// Both come before the multiplication allocates.
    fn product(
        &mut self,
        result: &BigInt,
        base: &BigInt,
        a: &BigInt,
        b: &BigInt,
        counter: &mut WorkCounter,
    ) -> Result<(), NativeError> {
        // Upper bound on output magnitude in 64-bit words.
        let words = a.bits().saturating_add(b.bits()).div_ceil(64).max(1);
        counter.spend(words)?;
        let held = result.bits().saturating_add(base.bits()).div_ceil(8);
        let needed = held.saturating_add(words.saturating_mul(8));
        if needed > self.peak {
            self.heap.reserve(0, needed - self.peak)?;
            self.peak = needed;
        }
        Ok(())
    }
}

/// The integer operations whose operands and result fit in 64 bits, done
/// there. `None` is an operation the arbitrary-precision path does.
fn in_word(op: Binary, a: i64, b: i64) -> Option<i64> {
    // A floored quotient is one less than the truncated one when the
    // remainder's sign differs from the divisor's; the remainder then
    // takes the divisor once more.
    let floored = |r: i64| r != 0 && (r < 0) != (b < 0);
    match op {
        Binary::Add => a.checked_add(b),
        Binary::Sub => a.checked_sub(b),
        Binary::Mul => a.checked_mul(b),
        Binary::DivTrunc => a.checked_div(b),
        Binary::RemTrunc => a.checked_rem(b),
        Binary::DivFloor => {
            let (q, r) = (a.checked_div(b)?, a.checked_rem(b)?);
            if floored(r) {
                q.checked_sub(1)
            } else {
                Some(q)
            }
        }
        Binary::RemFloor => {
            let r = a.checked_rem(b)?;
            if floored(r) {
                r.checked_add(b)
            } else {
                Some(r)
            }
        }
        Binary::Min => Some(a.min(b)),
        Binary::Max => Some(a.max(b)),
        Binary::Div | Binary::Pow => None,
    }
}

pub(crate) fn unary(op: Unary, value: &Value) -> Result<Value, NativeError> {
    if let Value::Int(value) = value {
        let word = value.to_i64();
        return Ok(match op {
            Unary::Neg => match word.and_then(i64::checked_neg) {
                Some(negated) => Value::Int(Integer::from(negated)),
                None => Value::Int(Integer::new(-value.as_bigint().into_owned())),
            },
            Unary::Abs => match word.and_then(i64::checked_abs) {
                Some(magnitude) => Value::Int(Integer::from(magnitude)),
                None => Value::Int(Integer::new(value.as_bigint().abs())),
            },
            Unary::Floor
            | Unary::Ceil
            | Unary::Trunc
            | Unary::RoundEven
            | Unary::RoundAway
            | Unary::RoundUp => Value::Int(value.clone()),
            Unary::Sign => Value::Int(Integer::from(if value.is_negative() {
                -1
            } else {
                i64::from(value.bits() > 0)
            })),
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
