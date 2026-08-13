//! ECMA-262 primitive coercion and operator semantics used by the TypeScript dialect.

use std::sync::Arc;

use crate::ast::{JavaScriptBinaryOp, JavaScriptUnaryOp};

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
                    Op::Less => left < right,
                    Op::LessEqual => left <= right,
                    Op::Greater => left > right,
                    Op::GreaterEqual => left >= right,
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
        (Value::Bool(_), _) => javascript_to_number(left) == javascript_to_number(right),
        (_, Value::Bool(_)) => javascript_to_number(left) == javascript_to_number(right),
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
    let unsigned = value.strip_prefix('+').unwrap_or(value);
    if let Some(digits) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        return u64::from_str_radix(digits, 16).map_or(f64::NAN, |value| value as f64);
    }
    if let Some(digits) = unsigned
        .strip_prefix("0b")
        .or_else(|| unsigned.strip_prefix("0B"))
    {
        return u64::from_str_radix(digits, 2).map_or(f64::NAN, |value| value as f64);
    }
    if let Some(digits) = unsigned
        .strip_prefix("0o")
        .or_else(|| unsigned.strip_prefix("0O"))
    {
        return u64::from_str_radix(digits, 8).map_or(f64::NAN, |value| value as f64);
    }
    value.parse().unwrap_or(f64::NAN)
}

fn javascript_to_string(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) if value.is_nan() => "NaN".to_string(),
        Value::Number(value) if *value == f64::INFINITY => "Infinity".to_string(),
        Value::Number(value) if *value == f64::NEG_INFINITY => "-Infinity".to_string(),
        Value::Number(value) if *value == 0.0 => "0".to_string(),
        Value::Number(value) => {
            let mut output = String::new();
            super::write_number(&mut output, *value).expect("string writes cannot fail");
            output
        }
        Value::String(value) => value.to_string(),
        value => match javascript_to_primitive_string_or_number(value) {
            Value::String(value) => value.to_string(),
            primitive => javascript_to_string(&primitive),
        },
    }
}
