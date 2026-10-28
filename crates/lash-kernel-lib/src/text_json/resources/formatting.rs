//! Exact output extents for neutral number formatting.
use super::*;
use crate::text_json::{arg, count_arg, integer_value, raise, text_arg};
use num_traits::ToPrimitive;

pub(super) fn evaluate(args: Args<'_>, op: &str) -> Result<NativeResourceContract, NativeError> {
    let bytes = match op {
        "decimal_parts" => {
            let Value::Float(number) = arg(args.values(), 0)? else {
                return Err(raise("type_error", "expected float"));
            };
            let mut digits = [0; 17];
            let (kind, text, exponent) = match number.decimal_parts(&mut digits) {
                Some((_, text, exponent)) => ("finite", text, exponent),
                None if number.get().is_nan() => ("nan", "", 0),
                None => ("infinity", "", 0),
            };
            let exponent =
                NativeResourceContract::returned(args, &Value::Int(i64::from(exponent)))?
                    .output
                    .bytes;
            let bytes =
                4 * LogicalHeldV1::REFERENCE + kind.len() as u64 + text.len() as u64 + exponent;
            let mut contract = NativeResourceContract::value(
                NativeResourceExit::Returned,
                bytes,
                NativeResourceOrigin::Immutable,
            );
            scratch(&mut contract, 17)?;
            return Ok(contract);
        }
        "fixed" => {
            let precision = count_arg(&args, 1)?;
            match arg(args.values(), 0)? {
                value @ (Value::Int(_) | Value::BigInt(_)) => {
                    let integer = args
                        .integer(value)?
                        .ok_or_else(|| raise("type_error", "expected number"))?;
                    let whole = integer.to_string().len();
                    if precision == 0 {
                        whole
                    } else {
                        whole
                            .checked_add(1)
                            .and_then(|n| n.checked_add(precision))
                            .ok_or_else(crate::exhausted)?
                    }
                }
                Value::Float(number) => {
                    if !number.get().is_finite() {
                        return Err(raise(
                            "number_range",
                            "precision formatting requires a finite number",
                        ));
                    }
                    format!("{:.*}", precision.min(1400), number.get())
                        .len()
                        .checked_add(precision.saturating_sub(1400))
                        .ok_or_else(crate::exhausted)?
                }
                _ => return Err(raise("type_error", "expected number")),
            }
        }
        "scientific" => {
            let Value::Float(number) = arg(args.values(), 0)? else {
                return Err(raise("type_error", "expected float"));
            };
            if !number.get().is_finite() {
                return Err(raise(
                    "number_range",
                    "precision formatting requires a finite number",
                ));
            }
            {
                let precision = count_arg(&args, 1)?;
                // Every binary64 has an exact decimal expansion within 1400
                // scientific fractional places. Larger precisions append zeros.
                format!("{:.*e}", precision.min(1400), number.get())
                    .len()
                    .checked_add(precision.saturating_sub(1400))
                    .ok_or_else(crate::exhausted)?
            }
        }
        "radix" => {
            let radix = integer_value(&args, 1)?
                .to_u32()
                .filter(|n| (2..=36).contains(n))
                .ok_or_else(|| raise("number_range", "radix must be between 2 and 36"))?;
            let integer = args
                .integer(arg(args.values(), 0)?)?
                .ok_or_else(|| raise("type_error", "expected an integer"))?;
            integer.as_bigint().to_str_radix(radix).len()
        }
        "pad" => {
            let side = text_arg(&args, 3)?;
            if !matches!(side.as_ref(), "start" | "end") {
                return Err(raise("type_error", "padding side must be start or end"));
            }
            return super::transforms::text_op(
                args,
                if side == "start" {
                    "pad_start"
                } else {
                    "pad_end"
                },
            );
        }
        _ => {
            return Err(NativeError::Malformed(
                "unknown formatting resource operation",
            ));
        }
    };
    let mut contract = text_result(bytes as u64)?;
    // Final text construction may overlap its decimal or padding work buffer.
    scratch(
        &mut contract,
        (bytes as u64).checked_mul(2).ok_or_else(overflow)?,
    )?;
    Ok(contract)
}
