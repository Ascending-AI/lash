//! ECMA-262 primitive coercion and operator semantics used by the TypeScript dialect.

use std::sync::Arc;

use crate::ast::{JavaScriptBinaryOp, JavaScriptUnaryOp};
use num_bigint::BigUint;
use num_traits::ToPrimitive;

use super::{Value, is_truthy};

pub(crate) fn eval_javascript_unary(value: Value, op: JavaScriptUnaryOp) -> Value {
    match op {
        JavaScriptUnaryOp::Plus => Value::Number(javascript_to_number(&value)),
        JavaScriptUnaryOp::Negate => Value::Number(-javascript_to_number(&value)),
        JavaScriptUnaryOp::Not => Value::Bool(!is_truthy(&value)),
        JavaScriptUnaryOp::TypeOf => Value::String(
            match value {
                Value::Undefined => "undefined",
                Value::Null => "object",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Ref(_) => "function",
                Value::Image(_)
                | Value::Resource(_)
                | Value::Tuple(_)
                | Value::List(_)
                | Value::Record(_)
                | Value::Projected(_) => "object",
            }
            .into(),
        ),
    }
}

pub(crate) fn eval_javascript_binary(left: Value, op: JavaScriptBinaryOp, right: Value) -> Value {
    use JavaScriptBinaryOp as Op;
    match op {
        Op::StrictEqual | Op::StrictNotEqual => {
            let equal = javascript_strict_equal(&left, &right);
            Value::Bool(if op == Op::StrictEqual { equal } else { !equal })
        }
        Op::LooseEqual | Op::LooseNotEqual => {
            let equal = javascript_loose_equal(&left, &right);
            Value::Bool(if op == Op::LooseEqual { equal } else { !equal })
        }
        Op::Add => {
            let left = javascript_to_primitive_string_or_number(&left);
            let right = javascript_to_primitive_string_or_number(&right);
            match (&left, &right) {
                (Value::String(_), _) | (_, Value::String(_)) => Value::String(
                    format!(
                        "{}{}",
                        javascript_to_string(&left),
                        javascript_to_string(&right)
                    )
                    .into(),
                ),
                _ => Value::Number(javascript_to_number(&left) + javascript_to_number(&right)),
            }
        }
        Op::Subtract => Value::Number(javascript_to_number(&left) - javascript_to_number(&right)),
        Op::Multiply => Value::Number(javascript_to_number(&left) * javascript_to_number(&right)),
        Op::Divide => Value::Number(javascript_to_number(&left) / javascript_to_number(&right)),
        Op::Remainder => Value::Number(javascript_to_number(&left) % javascript_to_number(&right)),
        Op::Less | Op::LessEqual | Op::Greater | Op::GreaterEqual => {
            let left = javascript_to_primitive_string_or_number(&left);
            let right = javascript_to_primitive_string_or_number(&right);
            let result = match (&left, &right) {
                (Value::String(left), Value::String(right)) => match op {
                    Op::Less => compare_utf16(left, right).is_lt(),
                    Op::LessEqual => !compare_utf16(left, right).is_gt(),
                    Op::Greater => compare_utf16(left, right).is_gt(),
                    Op::GreaterEqual => !compare_utf16(left, right).is_lt(),
                    _ => unreachable!(),
                },
                _ => {
                    let left = javascript_to_number(&left);
                    let right = javascript_to_number(&right);
                    match op {
                        Op::Less => left < right,
                        Op::LessEqual => left <= right,
                        Op::Greater => left > right,
                        Op::GreaterEqual => left >= right,
                        _ => unreachable!(),
                    }
                }
            };
            Value::Bool(result)
        }
    }
}

fn javascript_strict_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Ref(left), Value::Ref(right)) => left == right,
        (Value::Image(left), Value::Image(right)) => std::ptr::eq(left.as_ref(), right.as_ref()),
        (Value::Resource(left), Value::Resource(right)) => left == right,
        (Value::Tuple(left), Value::Tuple(right)) | (Value::List(left), Value::List(right)) => {
            std::ptr::eq(left.as_ref(), right.as_ref())
        }
        (Value::Record(left), Value::Record(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn javascript_loose_equal(left: &Value, right: &Value) -> bool {
    if javascript_strict_equal(left, right) {
        return true;
    }
    match (left, right) {
        (Value::Null | Value::Undefined, Value::Null | Value::Undefined) => true,
        (Value::Number(left), Value::String(_)) => *left == javascript_to_number(right),
        (Value::String(_), Value::Number(right)) => javascript_to_number(left) == *right,
        (Value::Bool(_), _) => {
            javascript_loose_equal(&Value::Number(javascript_to_number(left)), right)
        }
        (_, Value::Bool(_)) => {
            javascript_loose_equal(left, &Value::Number(javascript_to_number(right)))
        }
        (Value::String(_) | Value::Number(_), right) if javascript_is_object(right) => {
            javascript_loose_equal(left, &javascript_to_primitive_string_or_number(right))
        }
        (left, Value::String(_) | Value::Number(_)) if javascript_is_object(left) => {
            javascript_loose_equal(&javascript_to_primitive_string_or_number(left), right)
        }
        _ => false,
    }
}

fn javascript_is_object(value: &Value) -> bool {
    matches!(
        value,
        Value::Image(_)
            | Value::Resource(_)
            | Value::Tuple(_)
            | Value::List(_)
            | Value::Record(_)
            | Value::Ref(_)
            | Value::Projected(_)
    )
}

fn javascript_to_primitive_string_or_number(value: &Value) -> Value {
    match value {
        Value::Tuple(items) | Value::List(items) => Value::String(
            items
                .iter()
                .map(|item| match item {
                    Value::Null | Value::Undefined => String::new(),
                    other => javascript_to_string(other),
                })
                .collect::<Vec<_>>()
                .join(",")
                .into(),
        ),
        Value::Record(_) | Value::Image(_) | Value::Resource(_) => {
            Value::String("[object Object]".into())
        }
        other => other.clone(),
    }
}

fn javascript_to_number(value: &Value) -> f64 {
    match value {
        Value::Undefined => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(value) => u8::from(*value).into(),
        Value::Number(value) => *value,
        Value::String(value) => javascript_string_to_number(value),
        value => javascript_to_number(&javascript_to_primitive_string_or_number(value)),
    }
}

fn javascript_string_to_number(value: &str) -> f64 {
    let value = value.trim();
    if value.is_empty() {
        return 0.0;
    }
    if value == "Infinity" || value == "+Infinity" {
        return f64::INFINITY;
    }
    if value == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return parse_radix_integer(digits, 16);
    }
    if let Some(digits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        return parse_radix_integer(digits, 2);
    }
    if let Some(digits) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        return parse_radix_integer(digits, 8);
    }
    if !is_string_decimal_literal(value) {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}

pub(crate) fn javascript_to_string(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) if value.is_nan() => "NaN".to_string(),
        Value::Number(value) if *value == f64::INFINITY => "Infinity".to_string(),
        Value::Number(value) if *value == f64::NEG_INFINITY => "-Infinity".to_string(),
        Value::Number(value) if *value == 0.0 => "0".to_string(),
        Value::Number(value) => javascript_number_to_string(*value),
        Value::String(value) => value.to_string(),
        value => match javascript_to_primitive_string_or_number(value) {
            Value::String(value) => value.to_string(),
            primitive => javascript_to_string(&primitive),
        },
    }
}

pub(crate) fn javascript_array_index(index: &Value) -> Option<usize> {
    let key = javascript_to_string(index);
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index < u32::MAX).then_some(index as usize)
}

pub(crate) fn javascript_split(
    value: &Value,
    separator: &Value,
) -> Result<Vec<Value>, super::RuntimeError> {
    let value = javascript_to_string(value);
    if matches!(separator, Value::Undefined) {
        return Ok(vec![Value::String(value.into())]);
    }
    let separator = javascript_to_string(separator);
    if separator.is_empty() {
        return value
            .chars()
            .map(|ch| {
                if ch.len_utf16() != 1 {
                    return Err(super::RuntimeError::ValidationFailed {
                        reason: "TS_LONE_SURROGATE_UNSUPPORTED: split('') would create unrepresentable lone surrogates".to_string(),
                    });
                }
                Ok(Value::String(ch.to_string().into()))
            })
            .collect();
    }
    Ok(value
        .split(&separator)
        .map(|part| Value::String(part.into()))
        .collect())
}

pub(crate) fn javascript_join(
    value: &Value,
    separator: &Value,
) -> Result<String, super::RuntimeError> {
    let values = match value {
        Value::Tuple(values) | Value::List(values) => values.as_ref(),
        _ => return Err(super::RuntimeError::JoinUnsupported),
    };
    let separator = if matches!(separator, Value::Undefined) {
        ",".to_string()
    } else {
        javascript_to_string(separator)
    };
    Ok(values
        .iter()
        .map(|value| match value {
            Value::Null | Value::Undefined => String::new(),
            value => javascript_to_string(value),
        })
        .collect::<Vec<_>>()
        .join(&separator))
}

fn parse_radix_integer(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() || !digits.chars().all(|ch| ch.to_digit(radix).is_some()) {
        return f64::NAN;
    }
    BigUint::parse_bytes(digits.as_bytes(), radix)
        .and_then(|value| value.to_f64())
        .unwrap_or(f64::INFINITY)
}

fn is_string_decimal_literal(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
        digits += 1;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

fn compare_utf16(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn javascript_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "Infinity".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }

    let negative = value.is_sign_negative();
    let mut buffer = ryu::Buffer::new();
    let rendered = buffer.format_finite(value.abs());
    let (mantissa, explicit_exponent) = rendered
        .split_once(['e', 'E'])
        .map_or((rendered, 0), |(mantissa, exponent)| {
            (mantissa, exponent.parse::<i32>().expect("ryu exponent"))
        });
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let mut digits = mantissa
        .bytes()
        .filter(|byte| *byte != b'.')
        .map(char::from)
        .collect::<String>();
    while digits.len() > 1 && digits.ends_with('0') && mantissa.contains('.') {
        digits.pop();
    }
    let n = decimal + explicit_exponent;
    let k = digits.len() as i32;
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if k <= n && n <= 21 {
        output.push_str(&digits);
        output.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        let split = n as usize;
        output.push_str(&digits[..split]);
        output.push('.');
        output.push_str(&digits[split..]);
    } else if -6 < n && n <= 0 {
        output.push_str("0.");
        output.extend(std::iter::repeat_n('0', (-n) as usize));
        output.push_str(&digits);
    } else {
        output.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        let exponent = n - 1;
        output.push('e');
        if exponent >= 0 {
            output.push('+');
        }
        output.push_str(&exponent.to_string());
    }
    output
}
