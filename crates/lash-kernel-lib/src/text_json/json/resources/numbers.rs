//! Mathematical number selection for the input model; no native reservation.
use super::super::{Number, mismatch};
use lash_kernel_doc::{Float, Integer, NativeError, NumberPolicy};
use lash_kernel_value::Type;
use num_bigint::BigInt;
use num_traits::ToPrimitive;

pub(super) fn convert_number(
    token: &str,
    expected: &Type,
    policy: NumberPolicy,
) -> Result<Number, NativeError> {
    let ty = match expected {
        Type::Number | Type::Any
            if policy == NumberPolicy::BySpelling && !token.contains(['.', 'e', 'E']) =>
        {
            &Type::Int
        }
        Type::Number | Type::Any => &Type::Float,
        other => other,
    };
    match ty {
        Type::Int => {
            if let Ok(word) = token.parse::<i64>() {
                return Ok(Number::Int(Integer::from(word)));
            }
            let (mantissa, exponent) = token.split_once(['e', 'E']).unwrap_or((token, "0"));
            let exponent = exponent.parse::<BigInt>().map_err(|_| mismatch())?;
            let negative = mantissa.starts_with('-');
            let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
            let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
            let mut digits = format!("{whole}{fraction}");
            if digits.bytes().all(|b| b == b'0') {
                return Ok(Number::Int(Integer::from(0)));
            }
            let shift = exponent - BigInt::from(fraction.len());
            if shift < BigInt::from(0) {
                let remove = (-shift)
                    .to_usize()
                    .filter(|n| *n <= digits.len())
                    .ok_or_else(mismatch)?;
                let keep = digits.len() - remove;
                if !digits[keep..].bytes().all(|b| b == b'0') {
                    return Err(mismatch());
                }
                digits.truncate(keep);
            } else {
                let add = shift.to_usize().ok_or_else(crate::exhausted)?;
                digits.try_reserve(add).map_err(|_| crate::exhausted())?;
                digits.extend(std::iter::repeat_n('0', add));
            }
            let n = digits.parse::<BigInt>().map_err(|_| mismatch())?;
            Ok(Number::Int(Integer::new(if negative { -n } else { n })))
        }
        Type::Float => token
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(|n| Number::Float(Float::new(n)))
            .ok_or_else(mismatch),
        _ => Err(mismatch()),
    }
}
