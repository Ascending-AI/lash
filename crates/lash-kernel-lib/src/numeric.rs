//! Exact dyadic comparison and single-round conversion. No integer is first
//! rounded to a float while comparing (`K-VAL-021`, `K-NUM-004`).

use std::cmp::Ordering;

use lash_kernel_doc::{Args, Float, Integer, NativeError, Nursery, Value};

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{FromPrimitive, One, ToPrimitive, Zero};

use crate::raised;

/// Converts an integer to the nearest binary64, ties to even. A result
/// that rounds to infinity raises `number_range` (`K-NUM-003`).
pub fn integer_to_float(value: &Integer) -> Result<Float, NativeError> {
    ratio_to_float(&value.as_bigint(), &BigInt::one()).map(Float::new)
}

/// Converts a finite, integral binary64 exactly to an arbitrary integer.
/// Non-finite or fractional values raise `number_range`. Both zeros become 0.
pub fn float_to_integer(value: Float) -> Result<Integer, NativeError> {
    /// 2^63: every integral float in `[-2^63, 2^63)` is exactly a word.
    const WORD: f64 = 9_223_372_036_854_775_808.0;
    let value = value.get();
    if !value.is_finite() || value.trunc() != value {
        return Err(raised("number_range", "expected a finite integral float"));
    }
    if (-WORD..WORD).contains(&value) {
        return Ok(Integer::from(value as i64));
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

/// [`int_float_cmp`] of `integer`, a word compared in place (FIG-6820).
fn integer_float_cmp(integer: &Integer, float: f64) -> Option<Ordering> {
    match integer.word() {
        Some(word) => word_float_cmp(word, float),
        None => int_float_cmp(&integer.as_bigint(), float),
    }
}

/// [`int_float_cmp`] of a word, exact without an arbitrary integer: a
/// float in `[-2^63, 2^63)` is its integral part, a word, and its fraction.
fn word_float_cmp(word: i64, float: f64) -> Option<Ordering> {
    /// 2^63: every integral float in `[-2^63, 2^63)` is exactly a word.
    const WORD: f64 = 9_223_372_036_854_775_808.0;
    if float.is_nan() {
        return None;
    }
    if float >= WORD {
        return Some(Ordering::Less);
    }
    if float < -WORD {
        return Some(Ordering::Greater);
    }
    let whole = float.trunc();
    Some(
        word.cmp(&(whole as i64))
            .then_with(|| whole.partial_cmp(&float).unwrap_or(Ordering::Equal)),
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

#[inline]
pub(crate) fn float_cmp(a: Float, b: Float) -> Option<Ordering> {
    a.get().partial_cmp(&b.get())
}

pub(crate) fn scalar_cmp(a: &Numeric, b: &Numeric) -> Result<Option<Ordering>, NativeError> {
    match (a, b) {
        (Numeric::Int(a), Numeric::Int(b)) => Ok(Some(a.cmp(b))),
        (Numeric::Float(a), Numeric::Float(b)) => Ok(float_cmp(*a, *b)),
        (Numeric::Int(a), Numeric::Float(b)) => Ok(integer_float_cmp(a, b.get())),
        (Numeric::Float(a), Numeric::Int(b)) => {
            Ok(integer_float_cmp(b, a.get()).map(Ordering::reverse))
        }
        _ => Err(raised("type_error", "expected two numbers")),
    }
}

pub(crate) fn scalar_float(value: &Numeric) -> Result<f64, NativeError> {
    match value {
        Numeric::Int(value) => integer_to_float(value).map(Float::get),
        Numeric::Float(value) => Ok(value.get()),
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

/// Materialized operands of numeric algorithms. Arena reads remain explicit
/// at the call boundary; this domain contains no collection identities.
#[derive(Clone)]
pub(crate) enum Numeric {
    Int(Integer),
    Float(Float),
    Bool(bool),
}
impl Numeric {
    pub(crate) fn resident(value: &Value) -> Option<Self> {
        match value {
            Value::Int(i) => Some(Self::Int(Integer::from(*i))),
            Value::Float(f) => Some(Self::Float(*f)),
            Value::Bool(b) => Some(Self::Bool(*b)),
            _ => None,
        }
    }
    /// The number `value` names; a paged integer read whole holds its
    /// bytes ([`Nursery::read_whole`]).
    pub(crate) fn read(
        value: &Value,
        args: &Args<'_>,
        new: &mut Nursery<'_>,
    ) -> Result<Self, NativeError> {
        match value {
            Value::Float(f) => Ok(Self::Float(*f)),
            _ => match args.integer(value)? {
                Some(i) => {
                    new.read_whole(i.bits().div_ceil(8))?;
                    Ok(Self::Int(i.into_owned()))
                }
                None => Err(raised("type_error", "expected a number")),
            },
        }
    }
    pub(crate) fn store(self, new: &mut Nursery<'_>) -> Value {
        match self {
            Self::Int(i) => new.integer(i),
            Self::Float(f) => Value::Float(f),
            Self::Bool(b) => Value::Bool(b),
        }
    }
    pub(crate) fn inline(self) -> Option<Value> {
        Some(match self {
            Self::Int(i) => Value::Int(i.word()?),
            Self::Float(f) => Value::Float(f),
            Self::Bool(b) => Value::Bool(b),
        })
    }
}
pub(crate) fn number_cmp(
    a: &Value,
    b: &Value,
    args: &Args<'_>,
    new: &mut Nursery<'_>,
) -> Result<Option<Ordering>, NativeError> {
    scalar_cmp(&Numeric::read(a, args, new)?, &Numeric::read(b, args, new)?)
}

#[cfg(test)]
mod tests {
    use lash_kernel_doc::{Float, Integer};

    use num_bigint::BigInt;
    use num_traits::FromPrimitive;

    use super::float_to_integer;

    /// A word compares with a float as its arbitrary integer does
    /// ([`super::int_float_cmp`]) on both sides of 2^53 and 2^63, negated,
    /// at fractions, zeros, infinities and NaN (FIG-6820).
    #[test]
    fn word_float_comparison_is_exact_across_the_word_bounds() {
        let two53 = 9_007_199_254_740_992.0_f64;
        let two63 = 9_223_372_036_854_775_808.0_f64;
        let mut words = vec![
            0_i64,
            1,
            2,
            (1 << 53) - 1,
            1 << 53,
            (1 << 53) + 1,
            i64::MAX - 1,
            i64::MAX,
        ];
        words.extend(words.clone().into_iter().map(|word| -word));
        words.push(i64::MIN);
        let mut floats = vec![
            0.0,
            0.5,
            1.0,
            1.5,
            two53 - 1.0,
            two53,
            two53 + 2.0,
            f64::from_bits(two63.to_bits() - 1),
            two63,
            f64::from_bits(two63.to_bits() + 1),
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::INFINITY,
        ];
        floats.extend(floats.clone().into_iter().map(|value| -value));
        floats.push(f64::NAN);
        for &word in &words {
            for &float in &floats {
                assert_eq!(
                    super::word_float_cmp(word, float),
                    super::int_float_cmp(&BigInt::from(word), float),
                    "{word} against {float:e}"
                );
            }
        }
    }

    /// An integral float converts to the integer `BigInt::from_f64` makes
    /// of it on both sides of the word path's bounds, 2^53 and 2^63, and
    /// negated, and every non-finite or fractional float raises
    /// `number_range` (FIG-6820).
    #[test]
    fn float_to_integer_is_exact_across_the_word_bounds() {
        let two53 = 9_007_199_254_740_992.0_f64;
        let two63 = 9_223_372_036_854_775_808.0_f64;
        let mut integral = vec![
            0.0,
            -0.0,
            1.0,
            two53 - 1.0,
            two53,
            two53 + 2.0,
            two63 / 2.0,
            f64::from_bits(two63.to_bits() - 1),
            two63,
            f64::from_bits(two63.to_bits() + 1),
            two63 * 2.0,
            f64::MAX,
        ];
        integral.extend(integral.clone().into_iter().map(|value| -value));
        for value in integral {
            let expected = Integer::new(BigInt::from_f64(value).expect("an integral float"));
            let converted = float_to_integer(Float::new(value)).expect("an integral float");
            assert_eq!(converted, expected, "{value:e}");
            assert_eq!(converted.word(), expected.word(), "{value:e}");
        }
        assert_eq!(
            float_to_integer(Float::new(-two63))
                .ok()
                .and_then(|n| n.word()),
            Some(i64::MIN)
        );
        assert_eq!(
            float_to_integer(Float::new(two63))
                .ok()
                .and_then(|n| n.word()),
            None
        );
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.5,
            -1.5,
            two53 / 2.0 - 0.5,
        ] {
            let error =
                float_to_integer(Float::new(value)).expect_err("not a finite integral float");
            assert!(
                format!("{error:?}").contains("number_range"),
                "{value:e}: {error:?}"
            );
        }
    }
}
