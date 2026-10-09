use super::super::{javascript_to_number, javascript_to_string, javascript_to_uint32};
use super::javascript_json::{javascript_json_stringify, parse_javascript_json};
use super::javascript_number::*;
use super::javascript_stdlib::*;
use super::*;

#[expect(
    clippy::expect_used,
    reason = "code points were validated as valid above before from_u32, per the message"
)]
pub(super) fn javascript_static_stdlib(
    method: &str,
    args: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::{javascript_strict_equal, javascript_to_number};
    let args = normalized_static_arguments(method, args);
    // Every static call reads its arguments once: a ToNumber reads a text
    // argument's whole body, an `Object.*` enumeration walks the receiver,
    // a parse reads the whole source.
    charge_collection_work(
        instructions_executed,
        args.iter().fold(0usize, |total, value| {
            total.saturating_add(deep_proportional_units(value))
        }),
    );
    match (method, args.as_slice()) {
        // ToObject of the receiver throws before anything is read.
        (
            "Object.keys" | "Object.values" | "Object.entries" | "Object.hasOwn",
            [Value::Null | Value::Undefined, ..],
        ) => Err(RuntimeError::type_error(
            "Cannot convert undefined or null to object",
        )),
        ("Object.keys", [Value::Record(record)]) => Ok(Value::List(
            ecma_record_entries(record)
                .into_iter()
                .map(|(key, _)| Value::String(key.into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.values", [Value::Record(record)]) => Ok(Value::List(
            ecma_record_entries(record)
                .into_iter()
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.entries", [Value::Record(record)]) => Ok(Value::List(
            ecma_record_entries(record)
                .into_iter()
                .map(|(key, value)| {
                    Value::List(vec![Value::String(key.into()), value.clone()].into())
                })
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.keys", [Value::List(values) | Value::Tuple(values)]) => Ok(Value::List(
            (0..values.len())
                .map(|index| Value::String(index.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.values", [Value::List(values) | Value::Tuple(values)]) => {
            Ok(Value::List(values.to_vec().into()))
        }
        ("Object.entries", [Value::List(values) | Value::Tuple(values)]) => Ok(Value::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    Value::List(vec![Value::String(index.to_string().into()), value.clone()].into())
                })
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.keys", [Value::String(value)]) => Ok(Value::List(
            value
                .encode_utf16()
                .enumerate()
                .map(|(index, _)| Value::String(index.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Object.values", [Value::String(value)]) => Ok(Value::List(
            value
                .encode_utf16()
                .map(|unit| utf16_value(vec![unit]))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        )),
        ("Object.entries", [Value::String(value)]) => Ok(Value::List(
            value
                .encode_utf16()
                .enumerate()
                .map(|(index, unit)| {
                    Ok(Value::List(
                        vec![
                            Value::String(index.to_string().into()),
                            utf16_value(vec![unit])?,
                        ]
                        .into(),
                    ))
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?
                .into(),
        )),
        (
            "Object.keys" | "Object.values" | "Object.entries",
            [Value::Bool(_) | Value::Number(_)],
        ) => Ok(Value::List(Vec::new().into())),
        ("Object.fromEntries", [Value::List(entries) | Value::Tuple(entries)]) => {
            let mut record = record_with_capacity(entries.len());
            // Each entry is an object read at "0" and "1"; a missing one
            // reads `undefined`, and a primitive entry is not an object.
            for entry in entries.iter() {
                let (key, value) = match entry {
                    Value::List(pair) | Value::Tuple(pair) => (pair.first(), pair.get(1)),
                    Value::Record(pair) => (pair.get("0"), pair.get("1")),
                    _ => {
                        return Err(RuntimeError::type_error(format!(
                            "Iterator value {} is not an entry object",
                            javascript_to_string(entry)
                        )));
                    }
                };
                record.insert(
                    javascript_to_string(key.unwrap_or(&Value::Undefined)),
                    value.cloned().unwrap_or(Value::Undefined),
                );
            }
            Ok(Value::Record(std::sync::Arc::new(record)))
        }
        ("Object.assign", [target, sources @ ..]) => {
            let target = match target {
                Value::Record(target) => target,
                // ToObject(target) throws on `null` and `undefined`.
                Value::Null | Value::Undefined => {
                    return Err(RuntimeError::type_error(
                        "Cannot convert undefined or null to object",
                    ));
                }
                // ToObject of any other primitive is a wrapper object, which
                // this value model does not have.
                Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                    return Err(js_stdlib_error(
                        "TS_METHOD_UNSUPPORTED: Object.assign on a primitive target would return a wrapper object, which this value model does not have; assign onto a plain object",
                    ));
                }
                _ => {
                    return Err(js_stdlib_error(
                        "TS_METHOD_UNSUPPORTED: Object.assign onto an array or a built-in object is unavailable; assign onto a plain object",
                    ));
                }
            };
            let mut output = target.as_ref().clone();
            for source in sources {
                if matches!(source, Value::Null | Value::Undefined) {
                    continue;
                }
                let Value::Record(source) = source else {
                    continue;
                };
                for (key, value) in ecma_record_entries(source) {
                    output.insert(key.to_string(), value.clone());
                }
            }
            Ok(Value::Record(std::sync::Arc::new(output)))
        }
        ("Object.hasOwn", [Value::Record(record), key]) => Ok(Value::Bool(
            record.get(&javascript_to_string(key)).is_some(),
        )),
        ("Object.hasOwn", [Value::List(values) | Value::Tuple(values), key]) => {
            let key = javascript_to_string(key);
            Ok(Value::Bool(
                key == "length"
                    || array_index_property(&key).is_some_and(|index| index < values.len() as u32),
            ))
        }
        ("Object.hasOwn", [Value::String(value), key]) => {
            let key = javascript_to_string(key);
            Ok(Value::Bool(
                key == "length"
                    || array_index_property(&key)
                        .is_some_and(|index| index < value.encode_utf16().count() as u32),
            ))
        }
        ("Object.hasOwn", [Value::Bool(_) | Value::Number(_), _]) => Ok(Value::Bool(false)),
        ("Object.is", [left, right]) => Ok(Value::Bool(match (left, right) {
            (Value::Number(left), Value::Number(right)) => {
                (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
            }
            _ => javascript_strict_equal(left, right),
        })),
        ("Array.isArray", [value]) => Ok(Value::Bool(matches!(
            value,
            Value::List(_) | Value::Tuple(_)
        ))),
        (
            "Lash.ArrayFromIterable",
            [value @ (Value::Null | Value::Undefined | Value::Bool(_) | Value::Number(_))],
        ) => Err(crate::runtime::not_iterable_error(value)),
        ("Lash.ArrayFromIterable", [Value::List(values) | Value::Tuple(values)]) => {
            Ok(Value::List(values.to_vec().into()))
        }
        ("Lash.ArrayFromIterable", [Value::String(value)]) => Ok(Value::List(
            value
                .chars()
                .map(|character| Value::String(character.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Array.from", [Value::List(values) | Value::Tuple(values)]) => {
            Ok(Value::List(values.to_vec().into()))
        }
        ("Array.from", [Value::String(value)]) => Ok(Value::List(
            value
                .chars()
                .map(|character| Value::String(character.to_string().into()))
                .collect::<Vec<_>>()
                .into(),
        )),
        ("Array.of", values) => Ok(Value::List(values.to_vec().into())),
        ("String.fromCharCode", values) => utf16_value(
            values
                .iter()
                .map(|value| to_uint16(javascript_to_number(value)))
                .collect(),
        ),
        ("String.fromCodePoint", values) => {
            let mut output = String::new();
            for value in values {
                let point = javascript_to_number(value);
                if !point.is_finite()
                    || point.fract() != 0.0
                    || !(0.0..=0x10ffff as f64).contains(&point)
                {
                    return Err(RuntimeError::range_error(format!(
                        "Invalid code point {}",
                        javascript_to_string(value)
                    )));
                }
                // A surrogate code point is a valid argument: ECMA returns a
                // string holding that lone code unit. The value model cannot
                // hold one, which is the registered lone-surrogate refusal,
                // not an invalid argument.
                if (0xd800 as f64..=0xdfff as f64).contains(&point) {
                    return Err(js_stdlib_error(
                        "TS_LONE_SURROGATE_UNSUPPORTED: String.fromCodePoint would create an unrepresentable lone surrogate",
                    ));
                }
                output.push(char::from_u32(point as u32).expect("validated code point"));
            }
            Ok(Value::String(output.into()))
        }
        ("Number.isFinite", [Value::Number(value)]) => Ok(Value::Bool(value.is_finite())),
        ("Number.isFinite", [_]) => Ok(Value::Bool(false)),
        ("Number.isInteger", [Value::Number(value)]) => {
            Ok(Value::Bool(value.is_finite() && value.fract() == 0.0))
        }
        ("Number.isInteger", [_]) => Ok(Value::Bool(false)),
        ("Number.isNaN", [Value::Number(value)]) => Ok(Value::Bool(value.is_nan())),
        ("Number.isNaN", [_]) => Ok(Value::Bool(false)),
        ("Number.isSafeInteger", [Value::Number(value)]) => Ok(Value::Bool(
            value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0,
        )),
        ("Number.isSafeInteger", [_]) => Ok(Value::Bool(false)),
        ("Number.parseFloat", [value]) => Ok(Value::Number(parse_float_prefix(
            &javascript_to_string(value),
        ))),
        ("Number.parseInt", [value]) => Ok(Value::Number(parse_int_prefix(
            &javascript_to_string(value),
            None,
        ))),
        ("Number.parseInt", [value, radix]) => Ok(Value::Number(parse_int_prefix(
            &javascript_to_string(value),
            Some(javascript_to_number(radix)),
        ))),
        ("JSON.parse", [Value::String(value)]) => parse_javascript_json(value),
        ("JSON.stringify", [Value::Undefined]) => Ok(Value::Undefined),
        ("JSON.stringify", [value]) => {
            javascript_json_stringify(value).map(|value| Value::String(value.into()))
        }
        ("Math.abs", [value]) => Ok(Value::Number(javascript_to_number(value).abs())),
        ("Math.acos", [value]) => Ok(Value::Number(javascript_to_number(value).acos())),
        ("Math.asin", [value]) => Ok(Value::Number(javascript_to_number(value).asin())),
        ("Math.acosh", [value]) => Ok(Value::Number(javascript_to_number(value).acosh())),
        ("Math.asinh", [value]) => Ok(Value::Number(javascript_to_number(value).asinh())),
        ("Math.atan", [value]) => Ok(Value::Number(javascript_to_number(value).atan())),
        ("Math.atan2", [y, x]) => Ok(Value::Number(
            javascript_to_number(y).atan2(javascript_to_number(x)),
        )),
        ("Math.atanh", [value]) => Ok(Value::Number(javascript_to_number(value).atanh())),
        ("Math.cbrt", [value]) => Ok(Value::Number(javascript_to_number(value).cbrt())),
        ("Math.ceil", [value]) => Ok(Value::Number(javascript_to_number(value).ceil())),
        ("Math.clz32", [value]) => Ok(Value::Number(
            javascript_to_uint32(javascript_to_number(value)).leading_zeros() as f64,
        )),
        ("Math.cos", [value]) => Ok(Value::Number(javascript_to_number(value).cos())),
        ("Math.cosh", [value]) => Ok(Value::Number(javascript_to_number(value).cosh())),
        ("Math.exp", [value]) => Ok(Value::Number(javascript_to_number(value).exp())),
        ("Math.expm1", [value]) => Ok(Value::Number(javascript_to_number(value).exp_m1())),
        ("Math.floor", [value]) => Ok(Value::Number(javascript_to_number(value).floor())),
        ("Math.fround", [value]) => Ok(Value::Number(javascript_to_number(value) as f32 as f64)),
        ("Math.hypot", values) => Ok(Value::Number(javascript_hypot(values))),
        ("Math.imul", [left, right]) => Ok(Value::Number(
            (javascript_to_uint32(javascript_to_number(left))
                .wrapping_mul(javascript_to_uint32(javascript_to_number(right)))
                as i32) as f64,
        )),
        ("Math.log", [value]) => Ok(Value::Number(javascript_to_number(value).ln())),
        ("Math.log1p", [value]) => Ok(Value::Number(javascript_to_number(value).ln_1p())),
        ("Math.log10", [value]) => Ok(Value::Number(javascript_to_number(value).log10())),
        ("Math.log2", [value]) => Ok(Value::Number(javascript_to_number(value).log2())),
        ("Math.round", [value]) => Ok(Value::Number(javascript_round(javascript_to_number(value)))),
        ("Math.trunc", [value]) => Ok(Value::Number(javascript_to_number(value).trunc())),
        ("Math.max", values) => Ok(Value::Number(javascript_extreme(values, true))),
        ("Math.min", values) => Ok(Value::Number(javascript_extreme(values, false))),
        ("Math.pow", [base, exponent]) => Ok(Value::Number(javascript_pow(
            javascript_to_number(base),
            javascript_to_number(exponent),
        ))),
        ("Math.sqrt", [value]) => Ok(Value::Number(javascript_to_number(value).sqrt())),
        ("Math.sin", [value]) => Ok(Value::Number(javascript_to_number(value).sin())),
        ("Math.sinh", [value]) => Ok(Value::Number(javascript_to_number(value).sinh())),
        ("Math.tan", [value]) => Ok(Value::Number(javascript_to_number(value).tan())),
        ("Math.tanh", [value]) => Ok(Value::Number(javascript_to_number(value).tanh())),
        ("Math.sign", [value]) => {
            let value = javascript_to_number(value);
            Ok(Value::Number(if value.is_nan() || value == 0.0 {
                value
            } else {
                value.signum()
            }))
        }
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: unsupported call `{method}` with {} argument(s)",
            args.len()
        ))),
    }
}

fn javascript_hypot(values: &[Value]) -> f64 {
    let values = values.iter().map(javascript_to_number).collect::<Vec<_>>();
    if values.iter().any(|value| value.is_infinite()) {
        return f64::INFINITY;
    }
    if values.iter().any(|value| value.is_nan()) {
        return f64::NAN;
    }
    let scale = values
        .iter()
        .fold(0.0_f64, |scale, value| scale.max(value.abs()));
    if scale == 0.0 {
        return 0.0;
    }
    scale
        * values
            .iter()
            .map(|value| (value / scale).powi(2))
            .sum::<f64>()
            .sqrt()
}
