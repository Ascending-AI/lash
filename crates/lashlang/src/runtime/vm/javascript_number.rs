//! `Number.prototype` rendering and the `parseInt`/`parseFloat` prefix
//! parsers: exact-decimal `toExponential`, non-decimal `toString` radixes,
//! `toFixed`/`toPrecision` rounding, and the ToInt32 these calls route
//! through.

use super::super::{javascript_to_number, javascript_to_string};
use super::javascript_stdlib::js_stdlib_error;
use super::*;
use num_traits::Float;

/// `toExponential(f)` with ECMA rounding: the absolute value's exact decimal
/// expansion is rounded to `f + 1` significant digits, halves up.
pub(super) fn exact_exponential(value: f64, fraction: usize) -> String {
    if value == 0.0 {
        return if fraction == 0 {
            "0e+0".to_string()
        } else {
            format!("0.{}e+0", "0".repeat(fraction))
        };
    }
    // `value = mantissa * 2^exponent` exactly.
    let (mantissa, exponent, sign) = value.integer_decode();
    let (digits, shift) = if exponent >= 0 {
        (
            num_bigint::BigUint::from(mantissa) << exponent as usize,
            0i64,
        )
    } else {
        let halvings = (-exponent) as u32;
        (
            num_bigint::BigUint::from(mantissa) * num_bigint::BigUint::from(5u64).pow(halvings),
            -i64::from(halvings),
        )
    };
    // `|value| = digits * 10^shift`; `digits` is the full exact expansion.
    let digits = digits.to_string();
    let scientific_exponent = digits.len() as i64 + shift - 1;
    let kept = fraction + 1;
    let mut rounded: Vec<u8> = digits
        .bytes()
        .take(kept)
        .chain(std::iter::repeat(b'0'))
        .take(kept)
        .collect();
    let mut overflowed = false;
    if digits
        .as_bytes()
        .get(kept)
        .copied()
        .is_some_and(|digit| digit >= b'5')
    {
        // Round half up: an exact tie takes the larger mantissa, and anything
        // past the first dropped digit can only widen the gap upward.
        let mut position = kept;
        loop {
            position -= 1;
            if rounded[position] == b'9' {
                rounded[position] = b'0';
                if position == 0 {
                    rounded.insert(0, b'1');
                    rounded.truncate(kept);
                    overflowed = true;
                    break;
                }
            } else {
                rounded[position] += 1;
                break;
            }
        }
    }
    let mut mantissa_text = String::with_capacity(kept + 1);
    mantissa_text.push(rounded[0] as char);
    if fraction > 0 {
        mantissa_text.push('.');
        mantissa_text.extend(rounded[1..].iter().map(|digit| *digit as char));
    }
    // A carry out of the leading digit (`9.99` rounding to `10`) lifts the
    // scientific exponent one place.
    let exponent_text = scientific_exponent + i64::from(overflowed);
    format!(
        "{}{mantissa_text}e{}{exponent_text}",
        if sign < 0 { "-" } else { "" },
        if exponent_text >= 0 { "+" } else { "" },
    )
}

pub(super) fn parse_float_prefix(value: &str) -> f64 {
    let value = value.trim_start_matches(super::super::javascript::is_ecma_string_whitespace);
    let bytes = value.as_bytes();
    let mut cursor = 0usize;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        cursor = 1;
    }
    let negative = bytes.first() == Some(&b'-');
    if value[cursor..].starts_with("Infinity") {
        return if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let integer_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    let integer_digits = cursor - integer_start;
    let mut fraction_digits = 0usize;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        fraction_digits = cursor - fraction_start;
        if integer_digits == 0 && fraction_digits == 0 {
            cursor -= 1 + fraction_digits;
        }
    }
    if integer_digits + fraction_digits == 0 {
        return f64::NAN;
    }
    let mantissa_end = cursor;
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        let mut probe = cursor + 1;
        if matches!(bytes.get(probe), Some(b'+' | b'-')) {
            probe += 1;
        }
        let exponent_start = probe;
        while bytes.get(probe).is_some_and(u8::is_ascii_digit) {
            probe += 1;
        }
        if probe > exponent_start {
            cursor = probe;
        }
    }
    // `5.e3` matched the grammar with a bare fraction point; Rust's parser
    // wants a digit after it, so supply the implied zero.
    let mut literal = String::with_capacity(cursor + 1);
    literal.push_str(&value[..mantissa_end]);
    if literal.ends_with('.') {
        literal.push('0');
    }
    literal.push_str(&value[mantissa_end..cursor]);
    literal.parse::<f64>().unwrap_or(f64::NAN)
}

pub(super) fn parse_int_prefix(value: &str, radix: Option<f64>) -> f64 {
    let value = value.trim_start_matches(super::super::javascript::is_ecma_string_whitespace);
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let value = value.strip_prefix('+').unwrap_or(value);
    let radix = radix.map_or(0, |radix| {
        i64::from(crate::runtime::javascript_to_int32(radix))
    });
    if radix != 0 && !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let (radix, value) = if radix == 0 {
        value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .map_or((10, value), |value| (16, value))
    } else if radix == 16 {
        (
            16,
            value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .unwrap_or(value),
        )
    } else {
        (radix as u32, value)
    };
    let digits = value
        .chars()
        .take_while(|character| character.is_digit(radix))
        .collect::<String>();
    if digits.is_empty() {
        return f64::NAN;
    }
    let number = num_bigint::BigUint::parse_bytes(digits.as_bytes(), radix)
        .and_then(|value| num_traits::ToPrimitive::to_f64(&value))
        .unwrap_or(f64::INFINITY);
    if negative { -number } else { number }
}

pub(super) fn javascript_number_method(
    method: &str,
    value: f64,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    // A digit/radix argument that is a plain object with its own `valueOf`/
    // `toString` arrives pre-coerced: the operand layer's hook replay already
    // ran it (FIG-3652).
    let digits = |default: i64, min: i64| -> Result<i64, RuntimeError> {
        let value = args
            .first()
            .map(javascript_to_number)
            .unwrap_or(default as f64);
        let value = if value.is_nan() {
            0
        } else {
            value.trunc() as i64
        };
        if !(min..=100).contains(&value) {
            let message = match method {
                "toFixed" => "toFixed() digits argument must be between 0 and 100",
                "toExponential" => "toExponential() argument must be between 0 and 100",
                "toPrecision" => "toPrecision() argument must be between 1 and 100",
                _ => "precision must be between 0 and 100",
            };
            return Err(RuntimeError::range_error(message));
        }
        Ok(value)
    };
    let rendered = match method {
        "toFixed" => {
            let digits = digits(0, 0)? as u8;
            ryu_js::Buffer::new()
                .format_to_fixed(value, digits)
                .to_string()
        }
        "toExponential" => {
            let fraction = if args.is_empty() || matches!(args, [Value::Undefined]) {
                None
            } else {
                Some(digits(0, 0)? as usize)
            };
            javascript_exponential(value, fraction)
        }
        "toPrecision" if args.is_empty() || matches!(args, [Value::Undefined]) => {
            javascript_to_string(&Value::Number(value))
        }
        "toPrecision" => javascript_precision(value, digits(1, 1)? as usize),
        "toString" => {
            let Some(radix_value) = args.first() else {
                return Ok(Value::String(
                    javascript_to_string(&Value::Number(value)).into(),
                ));
            };
            if matches!(radix_value, Value::Undefined) {
                return Ok(Value::String(
                    javascript_to_string(&Value::Number(value)).into(),
                ));
            }
            let radix = javascript_to_number(radix_value);
            let radix = radix.trunc();
            if !(2.0..=36.0).contains(&radix) {
                return Err(RuntimeError::range_error(
                    "toString() radix argument must be between 2 and 36",
                ));
            }
            if radix == 10.0 {
                javascript_to_string(&Value::Number(value))
            } else {
                javascript_radix_string(value, radix as u32)
            }
        }
        "valueOf" if args.is_empty() => return Ok(Value::Number(value)),
        _ => {
            return Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: Number.{method}"
            )));
        }
    };
    Ok(Value::String(rendered.into()))
}

/// `Number.prototype.toString(radix)` for a radix other than ten: the
/// integer part by repeated division and the fraction by repeated
/// multiplication, the ECMA algorithm. `NaN` and the infinities keep their
/// radix-independent spellings.
pub(super) fn javascript_radix_string(value: f64, radix: u32) -> String {
    if !value.is_finite() {
        return javascript_to_string(&Value::Number(value));
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let negative = value.is_sign_negative() && value != 0.0;
    let value = value.abs();
    let mut integer = value.trunc();
    let mut fraction = value.fract();
    let mut output = Vec::new();
    if integer == 0.0 {
        output.push(DIGITS[0]);
    }
    // `trunc()` of a magnitude under 2^128 fits u128 exactly.
    while integer >= 1.0 {
        let whole = integer as u128;
        let base = u128::from(radix);
        output.push(DIGITS[(whole % base) as usize]);
        integer = (whole / base) as f64;
    }
    if negative {
        output.push(b'-');
    }
    output.reverse();
    if fraction > 0.0 {
        output.push(b'.');
        let mut digits = 0;
        while fraction > 0.0 && digits < 1100 {
            fraction *= f64::from(radix);
            let digit = fraction.trunc();
            output.push(DIGITS[digit as usize]);
            fraction -= digit;
            digits += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

pub(super) fn javascript_exponential(value: f64, fraction: Option<usize>) -> String {
    if !value.is_finite() {
        return javascript_to_string(&Value::Number(value));
    }
    let value = if value == 0.0 { 0.0 } else { value };
    match fraction {
        // ECMA rounds the exact decimal value to `fraction + 1` significant
        // digits and takes the larger mantissa on an exact tie (`25` with zero
        // fraction digits is `3e+1`, not `2e+1`). Rust's own formatter rounds
        // half-to-even, so the digits come from the exact binary expansion
        // instead.
        Some(fraction) => exact_exponential(value, fraction),
        None => {
            let shortest = javascript_to_string(&Value::Number(value));
            let parsed = shortest.parse::<f64>().unwrap_or(value);
            normalize_exponent(format!("{parsed:e}"), fraction)
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "the raw string is Rust's own exponent formatting output, split into mantissa and digits, per both messages"
)]
pub(super) fn normalize_exponent(raw: String, fraction: Option<usize>) -> String {
    let (mantissa, exponent) = raw.split_once('e').expect("Rust exponent formatting");
    let mut mantissa = mantissa.to_string();
    if fraction.is_none() {
        while mantissa.contains('.') && mantissa.ends_with('0') {
            mantissa.pop();
        }
        if mantissa.ends_with('.') {
            mantissa.pop();
        }
    }
    let exponent = exponent.parse::<i32>().expect("Rust exponent digits");
    format!(
        "{mantissa}e{}{exponent}",
        if exponent >= 0 { "+" } else { "" }
    )
}

pub(super) fn javascript_precision(value: f64, precision: usize) -> String {
    if !value.is_finite() {
        return javascript_to_string(&Value::Number(value));
    }
    let absolute = value.abs();
    let exponent = if absolute == 0.0 {
        0
    } else {
        absolute.log10().floor() as i32
    };
    if exponent >= precision as i32 || exponent < -6 {
        javascript_exponential(value, Some(precision - 1))
    } else {
        let fraction = (precision as i32 - exponent - 1).max(0) as u8;
        ryu_js::Buffer::new()
            .format_to_fixed(value, fraction)
            .to_string()
    }
}
