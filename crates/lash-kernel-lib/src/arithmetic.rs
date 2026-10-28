//! Exact integer arithmetic and explicitly selected IEEE division modes.

use std::cmp::Ordering;

use lash_kernel_doc::{Args, Float, Integer, NativeError, Nursery, Value, WorkCounter};

use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive, Zero};

use crate::numeric::{Numeric, ratio_to_float, scalar_cmp as number_cmp, scalar_float as as_float};
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
    args: &Args<'_>,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    let a = Numeric::read(a, args, new)?;
    let b = Numeric::read(b, args, new)?;
    let result = match (plain(op, &a, &b)?, &a, &b) {
        (Some(result), _, _) => result,
        (None, Numeric::Int(a), Numeric::Int(b)) => Numeric::Int(Integer::new(integer_pow(
            &a.as_bigint(),
            &b.as_bigint(),
            counter,
            new,
        )?)),
        (None, _, _) => unreachable!("only an integer power counts its work"),
    };
    Ok(result.store(new))
}

/// [`binary`] for every operation but an integer power past the 64-bit
/// word, which counts its work and reserves its room: `None` for that one.
#[inline]
pub(crate) fn plain(op: Binary, a: &Numeric, b: &Numeric) -> Result<Option<Numeric>, NativeError> {
    if let (Numeric::Int(a), Numeric::Int(b)) = (a, b) {
        if let (Some(a), Some(b)) = (a.to_i64(), b.to_i64())
            && let Some(result) = in_word(op, a, b)
        {
            return Ok(Some(Numeric::Int(Integer::from(result))));
        }
        let (a, b) = (a.as_bigint(), b.as_bigint());
        let (a, b) = (&*a, &*b);
        let result = match op {
            Binary::Add => a + b,
            Binary::Sub => a - b,
            Binary::Mul => a * b,
            Binary::Div => {
                return ratio_to_float(a, b).map(|value| Some(Numeric::Float(Float::new(value))));
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
            Binary::Pow => return Ok(None),
            Binary::Min => a.min(b).clone(),
            Binary::Max => a.max(b).clone(),
        };
        return Ok(Some(Numeric::Int(Integer::new(result))));
    }
    if matches!(op, Binary::Min | Binary::Max) {
        let order = number_cmp(a, b)?;
        // Min/max propagate NaN and otherwise return an original operand.
        // Opposite floating zeros choose -0 for min and +0 for max.
        if order.is_none() {
            return Ok(Some(Numeric::Float(Float::new(f64::NAN))));
        }
        if let (Numeric::Float(a), Numeric::Float(b)) = (a, b)
            && a.get() == 0.0
            && b.get() == 0.0
        {
            let negative = if matches!(op, Binary::Min) {
                a.get().is_sign_negative() || b.get().is_sign_negative()
            } else {
                a.get().is_sign_negative() && b.get().is_sign_negative()
            };
            return Ok(Some(Numeric::Float(Float::new(if negative {
                -0.0
            } else {
                0.0
            }))));
        }
        return Ok(Some(
            if matches!(
                (op, order),
                (Binary::Min, Some(Ordering::Greater)) | (Binary::Max, Some(Ordering::Less))
            ) {
                b.clone()
            } else {
                a.clone()
            },
        ));
    }
    let a = as_float(a)?;
    let b = as_float(b)?;
    Ok(Some(Numeric::Float(float_binary(op, a, b))))
}

/// IEEE arithmetic shared by the general numeric call and prepared float
/// entries. Min/max retain their operand and signed-zero selection above.
#[inline]
pub(crate) fn float_binary(op: Binary, a: f64, b: f64) -> Float {
    let result = match op {
        Binary::Add => a + b,
        Binary::Sub => a - b,
        Binary::Mul => a * b,
        Binary::Div => a / b,
        Binary::DivTrunc => (a / b).trunc(),
        Binary::RemTrunc => trunc_remainder(a, b),
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
    Float::new(result)
}

/// `fmod(a, b)`, bit for bit. Integral operands below 2^63 in magnitude
/// with a nonzero divisor take an integer remainder: fmod is exact, so the
/// integer result is fmod's, and a zero result keeps the dividend's sign.
/// NaN, infinities, a zero divisor and fractions go to libm.
#[inline]
fn trunc_remainder(a: f64, b: f64) -> f64 {
    // Conversions saturate and NaN converts to 0, so an integral double in
    // [-2^63, 2^63) is one that converts back to itself; 2^63 and above
    // saturate to i64::MAX, which rounds back to 2^63.
    let (x, y) = (a as i64, b as i64);
    if x as f64 == a && y as f64 == b && x != i64::MAX && y != i64::MAX && y != 0 {
        // i64::MIN % -1 is 0, which wrapping_rem returns without overflow.
        match x.wrapping_rem(y) {
            0 => libm::copysign(0.0, a),
            r => r as f64,
        }
    } else {
        libm::fmod(a, b)
    }
}

fn floor_remainder(a: f64, b: f64) -> f64 {
    let remainder = trunc_remainder(a, b);
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
    new: &mut Nursery<'_>,
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
    let mut room = Room { new, peak: 0 };
    #[cfg(test)]
    crate::native_contract_laws::observe_power_state(&result, &base, &exponent);
    while !exponent.is_zero() {
        if exponent.bit(0) {
            #[cfg(test)]
            crate::native_contract_laws::observe_power_scratch(
                &result, &base, &result, &base, &exponent,
            );
            room.product(&result, &base, &result, &base, counter)?;
            result *= &base;
        }
        exponent >>= 1usize;
        if !exponent.is_zero() {
            #[cfg(test)]
            crate::native_contract_laws::observe_power_scratch(
                &result, &base, &base, &base, &exponent,
            );
            room.product(&result, &base, &base, &base, counter)?;
            base = &base * &base;
        }
    }
    Ok(result)
}

/// The memory an integer power holds at its largest: the result so far, the
/// base and the product being taken.
struct Room<'a, 'call> {
    new: &'a mut Nursery<'call>,
    /// The most bytes any product so far has needed, all of it reserved.
    peak: u64,
}

impl Room<'_, '_> {
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
            self.new.reserve(0, needed - self.peak)?;
            self.peak = needed;
        }
        Ok(())
    }
}

/// The integer operations whose operands and result fit in 64 bits, done
/// there. `None` is an operation the arbitrary-precision path does.
pub(crate) fn in_word(op: Binary, a: i64, b: i64) -> Option<i64> {
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

#[inline]
pub(crate) fn scalar_unary(op: Unary, value: &Numeric) -> Result<Numeric, NativeError> {
    if let Numeric::Int(value) = value {
        let word = value.to_i64();
        return Ok(match op {
            Unary::Neg => match word.and_then(i64::checked_neg) {
                Some(negated) => Numeric::Int(Integer::from(negated)),
                None => Numeric::Int(Integer::new(-value.as_bigint().into_owned())),
            },
            Unary::Abs => match word.and_then(i64::checked_abs) {
                Some(magnitude) => Numeric::Int(Integer::from(magnitude)),
                None => Numeric::Int(Integer::new(value.as_bigint().abs())),
            },
            Unary::Floor
            | Unary::Ceil
            | Unary::Trunc
            | Unary::RoundEven
            | Unary::RoundAway
            | Unary::RoundUp => Numeric::Int(value.clone()),
            Unary::Sign => Numeric::Int(Integer::from(if value.is_negative() {
                -1
            } else {
                i64::from(value.bits() > 0)
            })),
            Unary::IsFinite | Unary::IsInteger => Numeric::Bool(true),
            Unary::IsInfinite | Unary::IsNan => Numeric::Bool(false),
        });
    }
    let value = as_float(value)?;
    Ok(match op {
        Unary::Neg => Numeric::Float(Float::new(-value)),
        Unary::Abs => Numeric::Float(Float::new(value.abs())),
        Unary::Floor => Numeric::Float(Float::new(value.floor())),
        Unary::Ceil => Numeric::Float(Float::new(value.ceil())),
        Unary::Trunc => Numeric::Float(Float::new(value.trunc())),
        Unary::RoundEven => Numeric::Float(Float::new(value.round_ties_even())),
        Unary::RoundAway => Numeric::Float(Float::new(value.round())),
        Unary::RoundUp => {
            // Avoid adding .5 to a large integer float or a value immediately
            // below a half: compare the fraction without introducing a tie.
            let floor = value.floor();
            let result = if value - floor < 0.5 {
                floor
            } else {
                value.ceil()
            };
            Numeric::Float(Float::new(if result == 0.0 {
                libm::copysign(0.0, value)
            } else {
                result
            }))
        }
        Unary::Sign => Numeric::Float(Float::new(if value.is_nan() || value == 0.0 {
            value
        } else if value < 0.0 {
            -1.0
        } else {
            1.0
        })),
        Unary::IsFinite => Numeric::Bool(value.is_finite()),
        Unary::IsInfinite => Numeric::Bool(value.is_infinite()),
        Unary::IsNan => Numeric::Bool(value.is_nan()),
        Unary::IsInteger => Numeric::Bool(value.is_finite() && value.trunc() == value),
    })
}

pub(crate) fn unary(
    op: Unary,
    value: &Value,
    args: &Args<'_>,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    Ok(scalar_unary(op, &Numeric::read(value, args, new)?)?.store(new))
}

#[cfg(test)]
mod tests {
    use super::trunc_remainder;

    /// The integer remainder answers exactly as libm's fmod, bit for bit,
    /// on the edges of its admission: signed zeros, the exact-integer and
    /// i64 bounds, saturation, NaN, infinities and fractions.
    #[test]
    fn trunc_remainder_is_fmod_bit_for_bit() {
        let two53 = 9_007_199_254_740_992.0_f64;
        let two63 = 9_223_372_036_854_775_808.0_f64;
        let edges = [
            0.0,
            1.0,
            2.0,
            3.0,
            7.0,
            1.5,
            0.5,
            1e-300,
            f64::MIN_POSITIVE,
            two53 - 1.0,
            two53,
            two53 + 2.0,
            two63 - 1024.0,
            two63,
            two63 * 2.0,
            1e300,
            f64::MAX,
            f64::INFINITY,
            f64::NAN,
        ];
        let values: Vec<f64> = edges.iter().flat_map(|&x| [x, -x]).collect();
        for &a in &values {
            for &b in &values {
                let (got, want) = (trunc_remainder(a, b), libm::fmod(a, b));
                assert!(
                    got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
                    "{a:?} % {b:?}: {got:?}, fmod gives {want:?}"
                );
            }
        }
    }
}
