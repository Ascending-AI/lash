use super::super::{
    ensure_javascript_string_size, javascript_string_size_error,
    javascript_to_primitive_string_or_number, javascript_to_string, to_json_direct,
};
use super::*;

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn read_dialect_field(
        &mut self,
        target: Value,
        field: &Name,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.reference_semantics {
                return read_javascript_heap_field(&self.heap, id, field);
            }
            let target = self.heap.export_for_instruction(&Value::Ref(id))?;
            return read_field_direct(target, field);
        }
        if self.reference_semantics {
            read_javascript_field_direct(target, field)
        } else {
            read_field_direct(target, field)
        }
    }

    pub(super) fn read_dialect_index(
        &mut self,
        target: Value,
        index: Value,
    ) -> Result<Value, RuntimeError> {
        if let Value::Ref(id) = target {
            if self.reference_semantics {
                return read_javascript_heap_index(&self.heap, id, &index);
            }
            let target = self.heap.export_for_instruction(&Value::Ref(id))?;
            return read_index_direct(target, index);
        }
        if self.reference_semantics {
            read_javascript_index_direct(target, index)
        } else {
            read_index_direct(target, index)
        }
    }

    pub(super) fn execute_javascript_unary(
        &mut self,
        op: JavaScriptUnaryOp,
    ) -> Result<(), RuntimeError> {
        let mut value = self.pop_stack()?;
        if op == JavaScriptUnaryOp::TypeOf
            && let Value::Ref(id) = value
        {
            let kind = self.heap.get(id)?.kind_name();
            self.stack.push(Value::String(
                if kind == "function" {
                    "function"
                } else {
                    "object"
                }
                .into(),
            ));
        } else {
            if op != JavaScriptUnaryOp::TypeOf && matches!(value, Value::Ref(_)) {
                value = self.heap.export_for_instruction(&value)?;
            }
            self.stack.push(eval_javascript_unary(value, op));
        }
        Ok(())
    }

    pub(super) fn execute_javascript_binary(
        &mut self,
        op: JavaScriptBinaryOp,
    ) -> Result<(), RuntimeError> {
        let mut right = self.pop_stack()?;
        let mut left = self.pop_stack()?;
        if !matches!(
            op,
            JavaScriptBinaryOp::StrictEqual | JavaScriptBinaryOp::StrictNotEqual
        ) {
            if matches!(left, Value::Ref(_)) {
                left = self.heap.export_for_instruction(&left)?;
            }
            if matches!(right, Value::Ref(_)) {
                right = self.heap.export_for_instruction(&right)?;
            }
        }
        if op == JavaScriptBinaryOp::Add {
            let left_primitive = javascript_to_primitive_string_or_number(&left);
            let right_primitive = javascript_to_primitive_string_or_number(&right);
            if matches!(left_primitive, Value::String(_))
                || matches!(right_primitive, Value::String(_))
            {
                let left = javascript_to_string(&left_primitive);
                let right = javascript_to_string(&right_primitive);
                let bytes = left
                    .len()
                    .checked_add(right.len())
                    .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
                ensure_javascript_string_size(bytes)?;
                self.stack
                    .push(Value::String(format!("{left}{right}").into()));
                return Ok(());
            }
        }
        self.stack.push(eval_javascript_binary(left, op, right));
        Ok(())
    }

    pub(super) fn execute_javascript_split(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        let values = javascript_split(&value, &separator)?;
        self.stack.push(self.heap.allocate_list(values)?);
        Ok(())
    }

    pub(super) fn execute_javascript_join(&mut self) -> Result<(), RuntimeError> {
        let separator = self.pop_stack()?;
        let value = self.pop_stack()?;
        let separator = self.heap.export_for_instruction(&separator)?;
        let value = self.heap.export_for_instruction(&value)?;
        self.stack
            .push(Value::String(javascript_join(&value, &separator)?.into()));
        Ok(())
    }

    pub(super) fn execute_javascript_stdlib(&mut self, argc: usize) -> Result<(), RuntimeError> {
        let mut values = Vec::with_capacity(argc);
        for _ in 0..argc {
            values.push(self.pop_stack()?);
        }
        values.reverse();
        for value in &mut values {
            if matches!(value, Value::Ref(_)) {
                *value = self.heap.export_for_instruction(value)?;
            }
        }
        let result = javascript_stdlib(&values)?;
        if let Value::String(value) = &result {
            ensure_javascript_string_size(value.len())?;
        }
        self.stack.push(result);
        Ok(())
    }
}

fn javascript_stdlib(values: &[Value]) -> Result<Value, RuntimeError> {
    let Some(Value::String(method)) = values.first() else {
        return Err(js_stdlib_error("missing method discriminator"));
    };
    let args = &values[1..];
    if method.contains('.') {
        return javascript_static_stdlib(method, args);
    }
    let Some((target, args)) = args.split_first() else {
        return Err(js_stdlib_error("missing receiver"));
    };
    match target {
        Value::String(value) => javascript_string_method(method, value, args),
        Value::List(items) | Value::Tuple(items) => {
            javascript_array_method(method, items.as_ref(), args)
        }
        Value::Null | Value::Undefined if matches!(method.as_str(), "toString" | "valueOf") => {
            Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: cannot call `{method}` on null or undefined"
            )))
        }
        _ if method == "toString" && args.is_empty() => {
            Ok(Value::String(javascript_to_string(target).into()))
        }
        _ if method == "valueOf" && args.is_empty() => Ok(target.clone()),
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: method `{method}` is unavailable on this value"
        ))),
    }
}

fn javascript_static_stdlib(method: &str, args: &[Value]) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::{javascript_strict_equal, javascript_to_number};
    let args = normalized_static_arguments(method, args);
    match (method, args.as_slice()) {
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
            for entry in entries.iter() {
                let (Value::List(pair) | Value::Tuple(pair)) = entry else {
                    return Err(js_stdlib_error("Object.fromEntries entry is not iterable"));
                };
                if pair.len() < 2 {
                    return Err(js_stdlib_error(
                        "Object.fromEntries entry has fewer than two values",
                    ));
                }
                record.insert(javascript_to_string(&pair[0]), pair[1].clone());
            }
            Ok(Value::Record(std::sync::Arc::new(record)))
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
                    || (0xd800 as f64..=0xdfff as f64).contains(&point)
                {
                    return Err(js_stdlib_error(
                        "String.fromCodePoint received an invalid code point",
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
        ("Math.cbrt", [value]) => Ok(Value::Number(javascript_to_number(value).cbrt())),
        ("Math.ceil", [value]) => Ok(Value::Number(javascript_to_number(value).ceil())),
        ("Math.cos", [value]) => Ok(Value::Number(javascript_to_number(value).cos())),
        ("Math.exp", [value]) => Ok(Value::Number(javascript_to_number(value).exp())),
        ("Math.floor", [value]) => Ok(Value::Number(javascript_to_number(value).floor())),
        ("Math.log", [value]) => Ok(Value::Number(javascript_to_number(value).ln())),
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
        ("Math.tan", [value]) => Ok(Value::Number(javascript_to_number(value).tan())),
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

fn javascript_string_method(
    method: &str,
    value: &str,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_to_number;
    let args = normalized_instance_arguments(method, args);
    let units = value.encode_utf16().collect::<Vec<_>>();
    match (method, args.as_slice()) {
        ("at", [index]) => {
            let index = relative_index(javascript_to_number(index), units.len());
            index.map_or(Ok(Value::Undefined), |index| {
                utf16_value(vec![units[index]])
            })
        }
        ("charAt", []) => units
            .first()
            .copied()
            .map_or(Ok(Value::String("".into())), |unit| utf16_value(vec![unit])),
        ("charAt", [index]) => relative_nonnegative_index(javascript_to_number(index), units.len())
            .map_or(Ok(Value::String("".into())), |index| {
                utf16_value(vec![units[index]])
            }),
        ("charCodeAt", []) => Ok(Value::Number(
            units.first().map_or(f64::NAN, |value| *value as f64),
        )),
        ("charCodeAt", [index]) => Ok(Value::Number(
            relative_nonnegative_index(javascript_to_number(index), units.len())
                .map_or(f64::NAN, |index| units[index] as f64),
        )),
        ("codePointAt", []) => code_point_at(&units, 0),
        ("codePointAt", [index]) => {
            relative_nonnegative_index(javascript_to_number(index), units.len())
                .map_or(Ok(Value::Undefined), |index| code_point_at(&units, index))
        }
        ("concat", values) => {
            let values = values.iter().map(javascript_to_string).collect::<Vec<_>>();
            let bytes = values
                .iter()
                .try_fold(value.len(), |total, item| total.checked_add(item.len()));
            ensure_javascript_string_size(
                bytes.ok_or_else(|| javascript_string_size_error(usize::MAX))?,
            )?;
            let mut output = value.to_string();
            for item in values {
                output.push_str(&item);
            }
            Ok(Value::String(output.into()))
        }
        ("startsWith", [needle]) => string_starts_with(&units, needle, 0),
        ("startsWith", [needle, position]) => string_starts_with(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("endsWith", [needle]) | ("endsWith", [needle, Value::Undefined]) => {
            string_ends_with(&units, needle, units.len())
        }
        ("endsWith", [needle, position]) => string_ends_with(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("includes", [needle]) => string_includes(&units, needle, 0),
        ("includes", [needle, position]) => string_includes(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("indexOf", [needle]) => string_index_of(&units, needle, 0),
        ("indexOf", [needle, position]) => string_index_of(
            &units,
            needle,
            clamp_nonnegative_index(javascript_to_number(position), units.len()),
        ),
        ("lastIndexOf", [needle]) => string_last_index_of(&units, needle, units.len()),
        ("lastIndexOf", [needle, Value::Undefined]) => {
            string_last_index_of(&units, needle, units.len())
        }
        ("lastIndexOf", [needle, position]) => {
            let position = javascript_to_number(position);
            string_last_index_of(
                &units,
                needle,
                if position.is_nan() {
                    units.len()
                } else {
                    clamp_nonnegative_index(position, units.len())
                },
            )
        }
        ("padStart", [length]) | ("padStart", [length, Value::Undefined]) => {
            pad_string(value, javascript_to_number(length), " ", true)
        }
        ("padStart", [length, fill]) => pad_string(
            value,
            javascript_to_number(length),
            &javascript_to_string(fill),
            true,
        ),
        ("padEnd", [length]) | ("padEnd", [length, Value::Undefined]) => {
            pad_string(value, javascript_to_number(length), " ", false)
        }
        ("padEnd", [length, fill]) => pad_string(
            value,
            javascript_to_number(length),
            &javascript_to_string(fill),
            false,
        ),
        ("repeat", [count]) => {
            let count = javascript_to_number(count);
            let count = if count.is_nan() { 0.0 } else { count };
            if !count.is_finite() || count < 0.0 {
                return Err(js_stdlib_error("String.repeat count is out of range"));
            }
            let count = count.trunc() as usize;
            let output_bytes = value
                .len()
                .checked_mul(count)
                .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
            ensure_javascript_string_size(output_bytes)?;
            Ok(Value::String(value.repeat(count).into()))
        }
        ("replace", [needle, replacement]) => replace_string(
            value,
            &javascript_to_string(needle),
            &javascript_to_string(replacement),
        )
        .map(|value| Value::String(value.into())),
        ("replaceAll", [needle, replacement]) => Ok(Value::String(
            value
                .replace(
                    &javascript_to_string(needle),
                    &javascript_to_string(replacement),
                )
                .into(),
        )),
        ("slice", bounds) => slice_utf16(&units, bounds, true),
        ("substring", bounds) => substring_utf16(&units, bounds),
        ("split", []) => Ok(Value::List(vec![Value::String(value.into())].into())),
        ("split", [separator]) => Ok(Value::List(
            javascript_split(&Value::String(value.into()), separator)?.into(),
        )),
        ("split", [separator, limit]) => {
            let mut values = javascript_split(&Value::String(value.into()), separator)?;
            let limit = javascript_to_number(limit);
            let limit = if limit.is_nan() || limit <= 0.0 {
                0
            } else {
                (limit.trunc() as usize).min(u32::MAX as usize)
            };
            values.truncate(limit);
            Ok(Value::List(values.into()))
        }
        ("toLowerCase", []) => Ok(Value::String(value.to_lowercase().into())),
        ("toUpperCase", []) => Ok(Value::String(value.to_uppercase().into())),
        ("trim", []) => Ok(Value::String(
            value
                .trim_matches(super::super::javascript::is_ecma_string_whitespace)
                .into(),
        )),
        ("trimStart", []) => Ok(Value::String(
            value
                .trim_start_matches(super::super::javascript::is_ecma_string_whitespace)
                .into(),
        )),
        ("trimEnd", []) => Ok(Value::String(
            value
                .trim_end_matches(super::super::javascript::is_ecma_string_whitespace)
                .into(),
        )),
        ("toString", []) | ("valueOf", []) => Ok(Value::String(value.into())),
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: String.{method}"
        ))),
    }
}

fn javascript_array_method(
    method: &str,
    items: &[Value],
    args: &[Value],
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_to_number;
    let argument_count = args.len();
    let args = normalized_instance_arguments(method, args);
    match (method, args.as_slice()) {
        ("at", [index]) => Ok(relative_index(javascript_to_number(index), items.len())
            .map_or(Value::Undefined, |index| items[index].clone())),
        ("concat", values) => {
            let mut output = items.to_vec();
            for value in values {
                match value {
                    Value::List(values) | Value::Tuple(values) => {
                        output.extend(values.iter().cloned())
                    }
                    value => output.push(value.clone()),
                }
            }
            Ok(Value::List(output.into()))
        }
        ("includes", [needle]) => array_includes(items, needle, 0),
        ("includes", [needle, from]) => array_includes(
            items,
            needle,
            clamp_relative_index(javascript_to_number(from), items.len()),
        ),
        ("indexOf", [needle]) => array_index_of(items, needle, 0),
        ("indexOf", [needle, from]) => array_index_of(
            items,
            needle,
            clamp_relative_index(javascript_to_number(from), items.len()),
        ),
        ("lastIndexOf", [needle]) => array_last_index_of(items, needle, items.len()),
        ("lastIndexOf", [needle, Value::Undefined]) if argument_count < 2 => {
            array_last_index_of(items, needle, items.len())
        }
        ("lastIndexOf", [needle, from]) => {
            last_index_exclusive(javascript_to_number(from), items.len())
                .map_or(Ok(Value::Number(-1.0)), |end| {
                    array_last_index_of(items, needle, end)
                })
        }
        ("join", []) => Ok(Value::String(
            javascript_join(&Value::List(items.to_vec().into()), &Value::Undefined)?.into(),
        )),
        ("join", [separator]) => Ok(Value::String(
            javascript_join(&Value::List(items.to_vec().into()), separator)?.into(),
        )),
        ("slice", bounds) => {
            let start = bounds.first().map_or(0.0, javascript_to_number);
            let end = bounds
                .get(1)
                .map_or(items.len() as f64, javascript_to_number);
            let start = clamp_relative_index(start, items.len());
            let end = clamp_relative_index(end, items.len()).max(start);
            Ok(Value::List(items[start..end].to_vec().into()))
        }
        ("toString", []) => Ok(Value::String(
            javascript_join(&Value::List(items.to_vec().into()), &Value::Undefined)?.into(),
        )),
        _ => Err(js_stdlib_error(format!(
            "TS_METHOD_UNSUPPORTED: Array.{method}"
        ))),
    }
}

fn js_stdlib_error(reason: impl Into<String>) -> RuntimeError {
    RuntimeError::ValidationFailed {
        reason: reason.into(),
    }
}

fn normalized_static_arguments(method: &str, args: &[Value]) -> Vec<Value> {
    let arity = match method {
        "Object.keys"
        | "Object.values"
        | "Object.entries"
        | "Object.fromEntries"
        | "Array.isArray"
        | "Number.isFinite"
        | "Number.isInteger"
        | "Number.isNaN"
        | "Number.isSafeInteger"
        | "Number.parseFloat"
        | "Math.abs"
        | "Math.acos"
        | "Math.asin"
        | "Math.cbrt"
        | "Math.ceil"
        | "Math.cos"
        | "Math.exp"
        | "Math.floor"
        | "Math.log"
        | "Math.log10"
        | "Math.log2"
        | "Math.round"
        | "Math.sin"
        | "Math.sqrt"
        | "Math.tan"
        | "Math.trunc"
        | "Math.sign" => 1,
        "Object.hasOwn" | "Object.is" | "Number.parseInt" | "Math.pow" => 2,
        _ => return args.to_vec(),
    };
    normalized_arguments(args, arity)
}

fn normalized_instance_arguments(method: &str, args: &[Value]) -> Vec<Value> {
    let arity = match method {
        "at" | "charAt" | "charCodeAt" | "codePointAt" | "repeat" | "join" => 1,
        "endsWith" | "includes" | "indexOf" | "lastIndexOf" | "padEnd" | "padStart" | "replace"
        | "replaceAll" | "startsWith" => 2,
        "slice" | "substring" => 2,
        "toLowerCase" | "toUpperCase" | "trim" | "trimStart" | "trimEnd" | "toString"
        | "valueOf" => 0,
        _ => return args.to_vec(),
    };
    normalized_arguments(args, arity)
}

fn normalized_arguments(args: &[Value], arity: usize) -> Vec<Value> {
    let mut normalized = args[..args.len().min(arity)].to_vec();
    normalized.resize(arity, Value::Undefined);
    normalized
}

fn ecma_record_entries(record: &Record) -> Vec<(&str, &Value)> {
    let mut indices = Vec::new();
    let mut names = Vec::new();
    for (key, value) in record.iter() {
        match array_index_property(key) {
            Some(index) => indices.push((index, key, value)),
            None => names.push((key, value)),
        }
    }
    indices.sort_unstable_by_key(|(index, _, _)| *index);
    indices
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(names)
        .collect()
}

fn array_index_property(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 1 && key.starts_with('0') {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

fn replace_string(value: &str, needle: &str, replacement: &str) -> Result<String, RuntimeError> {
    let Some(start) = value.find(needle) else {
        ensure_javascript_string_size(value.len())?;
        return Ok(value.to_string());
    };
    let end = start + needle.len();
    let prefix = &value[..start];
    let suffix = &value[end..];
    let mut output_bytes = prefix
        .len()
        .checked_add(suffix.len())
        .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
    let mut chars = replacement.chars().peekable();
    while let Some(character) = chars.next() {
        let additional = if character != '$' {
            character.len_utf8()
        } else {
            match chars.peek().copied() {
                Some('$') => {
                    chars.next();
                    1
                }
                Some('&') => {
                    chars.next();
                    needle.len()
                }
                Some('`') => {
                    chars.next();
                    prefix.len()
                }
                Some('\'') => {
                    chars.next();
                    suffix.len()
                }
                _ => 1,
            }
        };
        output_bytes = output_bytes
            .checked_add(additional)
            .ok_or_else(|| javascript_string_size_error(usize::MAX))?;
        ensure_javascript_string_size(output_bytes)?;
    }

    let mut output = String::with_capacity(output_bytes);
    output.push_str(prefix);
    let mut chars = replacement.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            output.push(character);
            continue;
        }
        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                output.push('$');
            }
            Some('&') => {
                chars.next();
                output.push_str(needle);
            }
            Some('`') => {
                chars.next();
                output.push_str(prefix);
            }
            Some('\'') => {
                chars.next();
                output.push_str(suffix);
            }
            _ => output.push('$'),
        }
    }
    output.push_str(suffix);
    Ok(output)
}

fn javascript_json_stringify(value: &Value) -> Result<String, RuntimeError> {
    match value {
        Value::Null | Value::Undefined => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) if !value.is_finite() => Ok("null".to_string()),
        Value::Number(_) => Ok(javascript_to_string(value)),
        Value::String(_) | Value::Image(_) | Value::Resource(_) => {
            serde_json::to_string(&to_json_direct(value))
                .map_err(|error| js_stdlib_error(format!("JSON.stringify: {error}")))
        }
        Value::Tuple(values) | Value::List(values) => {
            let values = values
                .iter()
                .map(javascript_json_stringify)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", values.join(",")))
        }
        Value::Record(record) => {
            let entries = ecma_record_entries(record)
                .into_iter()
                .filter(|(_, value)| !matches!(value, Value::Undefined))
                .map(|(key, value)| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("record keys are JSON strings"),
                        javascript_json_stringify(value)?
                    ))
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?;
            Ok(format!("{{{}}}", entries.join(",")))
        }
        Value::Projected(_) | Value::Ref(_) => Err(js_stdlib_error(
            "JSON.stringify received an unexported value",
        )),
    }
}

enum OrderedJsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl<'de> serde::Deserialize<'de> for OrderedJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedJsonValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value as f64))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value as f64))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(OrderedJsonValue::String(value.to_string()))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Null)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(OrderedJsonValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((key, value)) = map.next_entry()? {
                    values.push((key, value));
                }
                Ok(OrderedJsonValue::Object(values))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn parse_javascript_json(value: &str) -> Result<Value, RuntimeError> {
    let value = serde_json::from_str::<OrderedJsonValue>(value)
        .map_err(|error| js_stdlib_error(format!("JSON.parse: {error}")))?;
    Ok(ordered_json_to_value(value))
}

fn ordered_json_to_value(value: OrderedJsonValue) -> Value {
    match value {
        OrderedJsonValue::Null => Value::Null,
        OrderedJsonValue::Bool(value) => Value::Bool(value),
        OrderedJsonValue::Number(value) => Value::Number(value),
        OrderedJsonValue::String(value) => Value::String(value.into()),
        OrderedJsonValue::Array(values) => Value::List(
            values
                .into_iter()
                .map(ordered_json_to_value)
                .collect::<Vec<_>>()
                .into(),
        ),
        OrderedJsonValue::Object(entries) => Value::Record(std::sync::Arc::new(
            entries
                .into_iter()
                .map(|(key, value)| (key, ordered_json_to_value(value)))
                .collect(),
        )),
    }
}

fn relative_index(value: f64, len: usize) -> Option<usize> {
    let value = if value.is_nan() {
        0
    } else {
        value.trunc() as isize
    };
    let index = if value < 0 {
        len as isize + value
    } else {
        value
    };
    (index >= 0 && index < len as isize).then_some(index as usize)
}

fn relative_nonnegative_index(value: f64, len: usize) -> Option<usize> {
    let value = if value.is_nan() {
        0
    } else {
        value.trunc() as isize
    };
    (value >= 0 && value < len as isize).then_some(value as usize)
}

fn clamp_relative_index(value: f64, len: usize) -> usize {
    if value.is_nan() {
        return 0;
    }
    if value <= -(len as f64) {
        0
    } else if value < 0.0 {
        (len as f64 + value.trunc()) as usize
    } else {
        value.trunc().min(len as f64) as usize
    }
}

fn clamp_nonnegative_index(value: f64, len: usize) -> usize {
    if value.is_nan() || value <= 0.0 {
        0
    } else {
        (value.trunc() as usize).min(len)
    }
}

fn string_starts_with(
    units: &[u16],
    needle: &Value,
    position: usize,
) -> Result<Value, RuntimeError> {
    let needle = javascript_to_string(needle)
        .encode_utf16()
        .collect::<Vec<_>>();
    Ok(Value::Bool(
        units.get(position..position.saturating_add(needle.len())) == Some(needle.as_slice()),
    ))
}

fn string_ends_with(units: &[u16], needle: &Value, end: usize) -> Result<Value, RuntimeError> {
    let needle = javascript_to_string(needle)
        .encode_utf16()
        .collect::<Vec<_>>();
    let start = end.saturating_sub(needle.len());
    Ok(Value::Bool(
        needle.len() <= end && units.get(start..end) == Some(needle.as_slice()),
    ))
}

fn string_includes(units: &[u16], needle: &Value, position: usize) -> Result<Value, RuntimeError> {
    let needle = javascript_to_string(needle)
        .encode_utf16()
        .collect::<Vec<_>>();
    Ok(Value::Bool(
        needle.is_empty()
            || units
                .get(position..)
                .is_some_and(|tail| tail.windows(needle.len()).any(|window| window == needle)),
    ))
}

fn string_index_of(units: &[u16], needle: &Value, position: usize) -> Result<Value, RuntimeError> {
    let needle = javascript_to_string(needle)
        .encode_utf16()
        .collect::<Vec<_>>();
    let index = if needle.is_empty() {
        Some(position.min(units.len()))
    } else {
        units
            .get(position..)
            .and_then(|tail| {
                tail.windows(needle.len())
                    .position(|window| window == needle)
            })
            .map(|index| position + index)
    };
    Ok(Value::Number(index.map_or(-1.0, |index| index as f64)))
}

fn string_last_index_of(
    units: &[u16],
    needle: &Value,
    position: usize,
) -> Result<Value, RuntimeError> {
    let needle = javascript_to_string(needle)
        .encode_utf16()
        .collect::<Vec<_>>();
    let position = position.min(units.len());
    let index = if needle.is_empty() {
        Some(position)
    } else {
        let last_start = position.min(units.len().saturating_sub(needle.len()));
        (0..=last_start)
            .rev()
            .find(|start| units.get(*start..start + needle.len()) == Some(needle.as_slice()))
    };
    Ok(Value::Number(index.map_or(-1.0, |index| index as f64)))
}

fn array_includes(items: &[Value], needle: &Value, start: usize) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_strict_equal;
    Ok(Value::Bool(items.get(start..).is_some_and(|tail| {
        tail.iter().any(|item| {
            javascript_strict_equal(item, needle)
                || matches!((item, needle), (Value::Number(left), Value::Number(right)) if left.is_nan() && right.is_nan())
        })
    })))
}

fn array_index_of(items: &[Value], needle: &Value, start: usize) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_strict_equal;
    Ok(Value::Number(
        items
            .get(start..)
            .and_then(|tail| {
                tail.iter()
                    .position(|item| javascript_strict_equal(item, needle))
            })
            .map_or(-1.0, |index| (start + index) as f64),
    ))
}

fn array_last_index_of(items: &[Value], needle: &Value, end: usize) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_strict_equal;
    Ok(Value::Number(
        items[..end.min(items.len())]
            .iter()
            .rposition(|item| javascript_strict_equal(item, needle))
            .map_or(-1.0, |index| index as f64),
    ))
}

fn last_index_exclusive(value: f64, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let value = if value.is_nan() { 0.0 } else { value.trunc() };
    if value < -(len as f64) {
        None
    } else if value < 0.0 {
        Some((len as f64 + value) as usize + 1)
    } else {
        Some(value.min((len - 1) as f64) as usize + 1)
    }
}

fn to_uint16(value: f64) -> u16 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    value.trunc().rem_euclid(65_536.0) as u16
}

fn javascript_round(value: f64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    if (-0.5..0.0).contains(&value) {
        return -0.0;
    }
    (value + 0.5).floor()
}

fn javascript_pow(base: f64, exponent: f64) -> f64 {
    if base.abs() == 1.0 && exponent.is_infinite() {
        f64::NAN
    } else {
        base.powf(exponent)
    }
}

fn javascript_extreme(values: &[Value], maximum: bool) -> f64 {
    use crate::runtime::javascript::javascript_to_number;
    let mut result = if maximum {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    };
    for value in values {
        let value = javascript_to_number(value);
        if value.is_nan() {
            return f64::NAN;
        }
        if (maximum
            && (value > result || value == 0.0 && result == 0.0 && value.is_sign_positive()))
            || (!maximum
                && (value < result || value == 0.0 && result == 0.0 && value.is_sign_negative()))
        {
            result = value;
        }
    }
    result
}

fn utf16_value(units: Vec<u16>) -> Result<Value, RuntimeError> {
    String::from_utf16(&units)
        .map(|value| Value::String(value.into()))
        .map_err(|_| js_stdlib_error("TS_LONE_SURROGATE_UNSUPPORTED: result is not representable"))
}

fn code_point_at(units: &[u16], index: usize) -> Result<Value, RuntimeError> {
    let Some(first) = units.get(index).copied() else {
        return Ok(Value::Undefined);
    };
    let point = if (0xd800..=0xdbff).contains(&first)
        && let Some(second @ 0xdc00..=0xdfff) = units.get(index + 1).copied()
    {
        0x10000 + (((first as u32 - 0xd800) << 10) | (second as u32 - 0xdc00))
    } else {
        first as u32
    };
    Ok(Value::Number(point as f64))
}

fn slice_utf16(units: &[u16], bounds: &[Value], relative: bool) -> Result<Value, RuntimeError> {
    let to_number = crate::runtime::javascript::javascript_to_number;
    let start_value = bounds.first().map_or(0.0, to_number);
    let end_value = bounds.get(1).map_or(units.len() as f64, to_number);
    let start = if relative {
        clamp_relative_index(start_value, units.len())
    } else {
        start_value.max(0.0) as usize
    };
    let end = if relative {
        clamp_relative_index(end_value, units.len())
    } else {
        (end_value.max(0.0) as usize).min(units.len())
    };
    utf16_value(units[start..end.max(start)].to_vec())
}

fn substring_utf16(units: &[u16], bounds: &[Value]) -> Result<Value, RuntimeError> {
    let to_number = crate::runtime::javascript::javascript_to_number;
    let mut start = bounds.first().map_or(0.0, to_number).max(0.0) as usize;
    let mut end = bounds.get(1).map_or(units.len() as f64, to_number).max(0.0) as usize;
    start = start.min(units.len());
    end = end.min(units.len());
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    utf16_value(units[start..end].to_vec())
}

fn pad_string(value: &str, length: f64, fill: &str, start: bool) -> Result<Value, RuntimeError> {
    let current = value.encode_utf16().count();
    let length = if length.is_nan() {
        0
    } else {
        length.max(0.0).trunc() as usize
    };
    if length <= current || fill.is_empty() {
        return Ok(Value::String(value.into()));
    }
    let fill_units = fill.encode_utf16().collect::<Vec<_>>();
    let padding = (0..length - current)
        .map(|index| fill_units[index % fill_units.len()])
        .collect::<Vec<_>>();
    let padding = match utf16_value(padding)? {
        Value::String(value) => value,
        _ => unreachable!(),
    };
    Ok(Value::String(
        if start {
            format!("{padding}{value}")
        } else {
            format!("{value}{padding}")
        }
        .into(),
    ))
}

fn parse_float_prefix(value: &str) -> f64 {
    let value = value.trim_start_matches(super::super::javascript::is_ecma_string_whitespace);
    for end in (1..=value.len()).rev() {
        if let Some(prefix) = value.get(..end)
            && let Ok(number) = prefix.parse::<f64>()
        {
            return number;
        }
    }
    f64::NAN
}

fn parse_int_prefix(value: &str, radix: Option<f64>) -> f64 {
    let value = value.trim_start_matches(super::super::javascript::is_ecma_string_whitespace);
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let value = value.strip_prefix('+').unwrap_or(value);
    let radix = radix.map_or(0, to_int32);
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

fn to_int32(value: f64) -> i64 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    let value = value.trunc().rem_euclid(4_294_967_296.0) as i64;
    if value >= 2_147_483_648 {
        value - 4_294_967_296
    } else {
        value
    }
}
