//! Binary operations, comparison, numeric coercion, range/iterator helpers,
//! `is_truthy` / `materialize_value` / `value_type_name`, and builtin
//! dispatch (`intrinsic` and the per-intrinsic direct paths).

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;

pub(crate) use super::fuel::{deep_proportional_units, proportional_units, sorting_work};
use super::instruction::Name;
use super::*;

pub(crate) fn expect_arg_count(
    name: &str,
    values: &[Value],
    expected: usize,
) -> Result<(), RuntimeError> {
    if values.len() == expected {
        Ok(())
    } else {
        Err(RuntimeError::InvalidArgumentCount {
            name: name.to_string(),
            expected: expected.to_string(),
            actual: values.len(),
        })
    }
}

pub(crate) fn execute_intrinsic(
    builtin: IntrinsicOp,
    names: &[Name],
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    match builtin {
        IntrinsicOp::Len => {
            expect_arg_count("len", values, 1)?;
            let result = execute_len_builtin(&values[0])?;
            // Counting a string's characters walks it once; every other
            // collection's length is already in hand.
            if matches!(values[0], Value::String(_))
                && let Value::Number(units) = result
            {
                charge_collection_work(instructions_executed, units as usize);
            }
            Ok(result)
        }
        IntrinsicOp::Empty => {
            expect_arg_count("empty", values, 1)?;
            match &values[0] {
                Value::String(value) => Ok(Value::Bool(value.is_empty())),
                Value::Tuple(values) => Ok(Value::Bool(values.is_empty())),
                Value::List(values) => Ok(Value::Bool(values.is_empty())),
                Value::Record(record) => Ok(Value::Bool(record.is_empty())),
                Value::Projected(value) => value
                    .empty()?
                    .map(Value::Bool)
                    .ok_or(RuntimeError::EmptyUnsupported),
                Value::Null => Ok(Value::Bool(true)),
                _ => Err(RuntimeError::EmptyUnsupported),
            }
        }
        IntrinsicOp::Keys => {
            expect_arg_count("keys", values, 1)?;
            match &values[0] {
                Value::Record(record) => {
                    // Enumerating writes one key per member.
                    charge_collection_work(instructions_executed, record.len());
                    Ok(Value::List(
                        record
                            .keys()
                            .map(|key| Value::String(key.into()))
                            .collect::<Vec<_>>()
                            .into(),
                    ))
                }
                Value::Projected(value) => Ok(Value::List(
                    value
                        .keys()?
                        .into_iter()
                        .map(|key| Value::String(key.into()))
                        .collect::<Vec<_>>()
                        .into(),
                )),
                Value::Null => Ok(Value::List(Vec::new().into())),
                _ => Err(RuntimeError::KeysUnsupported),
            }
        }
        IntrinsicOp::Values => {
            expect_arg_count("values", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.values()?
            {
                return Ok(value);
            }
            let value = materialize_value(values[0].clone())?;
            match &value {
                Value::Record(record) => {
                    // Enumerating writes one value per member.
                    charge_collection_work(instructions_executed, record.len());
                    Ok(Value::List(
                        record.values().cloned().collect::<Vec<_>>().into(),
                    ))
                }
                Value::Null => Ok(Value::List(Vec::new().into())),
                _ => Err(RuntimeError::ValuesUnsupported),
            }
        }
        IntrinsicOp::Contains => {
            expect_arg_count("contains", values, 2)?;
            // A text or sequence scan reads the haystack once; a record's
            // key lookup reads the needle.
            charge_collection_work(
                instructions_executed,
                match &values[0] {
                    Value::Record(_) => proportional_units(&values[1]),
                    haystack => proportional_units(haystack),
                },
            );
            execute_contains_builtin(&values[0], &values[1])
        }
        IntrinsicOp::Find(_) => {
            // The scan reads the haystack's text once.
            charge_collection_work(
                instructions_executed,
                values.first().map_or(0, proportional_units),
            );
            execute_find_builtin(values)
        }
        IntrinsicOp::GrepText => {
            // The line scan reads the text once.
            charge_collection_work(
                instructions_executed,
                values.first().map_or(0, proportional_units),
            );
            execute_grep_text_builtin(values)
        }
        IntrinsicOp::StartsWith => {
            expect_arg_count("starts_with", values, 2)?;
            let prefix = materialize_value(values[1].clone())?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.starts_with(prefix.clone())?
            {
                return Ok(value);
            }
            let value = materialize_value(values[0].clone())?;
            let value = coerce_string(&value)?;
            let prefix = coerce_string(&prefix)?;
            // The comparison reads at most the needle's bytes.
            charge_collection_work(instructions_executed, value.len().min(prefix.len()));
            Ok(Value::Bool(value.starts_with(prefix.as_ref())))
        }
        IntrinsicOp::EndsWith => {
            expect_arg_count("ends_with", values, 2)?;
            let suffix = materialize_value(values[1].clone())?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.ends_with(suffix.clone())?
            {
                return Ok(value);
            }
            let value = materialize_value(values[0].clone())?;
            let value = coerce_string(&value)?;
            let suffix = coerce_string(&suffix)?;
            // The comparison reads at most the needle's bytes.
            charge_collection_work(instructions_executed, value.len().min(suffix.len()));
            Ok(Value::Bool(value.ends_with(suffix.as_ref())))
        }
        IntrinsicOp::Split => {
            expect_arg_count("split", values, 2)?;
            let needle = materialize_value(values[1].clone())?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.split(needle.clone())?
            {
                return Ok(value);
            }
            let value = materialize_value(values[0].clone())?;
            let value = coerce_string(&value)?;
            let needle = coerce_string(&needle)?;
            // Splitting reads the text once and writes each part once.
            charge_collection_work(instructions_executed, value.len());
            Ok(Value::List(
                value
                    .split(needle.as_ref())
                    .map(|part| Value::String(part.into()))
                    .collect::<Vec<_>>()
                    .into(),
            ))
        }
        IntrinsicOp::JavaScriptSplit
        | IntrinsicOp::JavaScriptJoin
        | IntrinsicOp::JavaScriptStdlib(_)
        | IntrinsicOp::JavaScriptHeapNew(_)
        | IntrinsicOp::JavaScriptHeapInstanceOf
        | IntrinsicOp::JavaScriptHeapDeleteMember
        | IntrinsicOp::JavaScriptRegExp(_)
        | IntrinsicOp::JavaScriptGlobalDelete
        | IntrinsicOp::JavaScriptGlobalGet
        | IntrinsicOp::JavaScriptGlobalHas
        | IntrinsicOp::JavaScriptGlobalSet
        | IntrinsicOp::JavaScriptUriCodec(_)
        | IntrinsicOp::BindingCellNew
        | IntrinsicOp::BindingCellGet
        | IntrinsicOp::BindingCellSet => Err(RuntimeError::ContextDependentIntrinsicMisdispatch {
            context: "TypeScript container intrinsic".into(),
        }),
        IntrinsicOp::Join => {
            expect_arg_count("join", values, 2)?;
            let result = execute_join_builtin(&values[0], &values[1])?;
            // Joining writes every byte of the text once.
            if let Value::String(joined) = &result {
                charge_collection_work(instructions_executed, joined.len());
            }
            Ok(result)
        }
        IntrinsicOp::Trim => {
            expect_arg_count("trim", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.trim()?
            {
                return Ok(value);
            }
            let value = materialize_value(values[0].clone())?;
            let text = coerce_string(&value)?;
            // Trimming reads the whole text once.
            charge_collection_work(instructions_executed, text.len());
            Ok(Value::String(text.trim().into()))
        }
        IntrinsicOp::Slice => {
            expect_arg_count("slice", values, 3)?;
            let start = as_slice_bound(&values[1])?;
            let end = as_slice_bound(&values[2])?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.slice(start, end)?
            {
                return Ok(value);
            }
            let target = materialize_value(values[0].clone())?;
            match &target {
                Value::String(value) => {
                    // Slicing reads the text's characters once.
                    charge_collection_work(instructions_executed, value.len());
                    Ok(Value::String(slice_string(value, start, end).into()))
                }
                Value::Tuple(items) => {
                    let Some((start, end)) = clamp_slice_bounds(start, end, items.len()) else {
                        return Ok(Value::Tuple(Vec::new().into()));
                    };
                    // The slice writes each of its elements once.
                    charge_collection_work(instructions_executed, end - start);
                    Ok(Value::Tuple(items[start..end].to_vec().into()))
                }
                Value::List(items) => {
                    let Some((start, end)) = clamp_slice_bounds(start, end, items.len()) else {
                        return Ok(Value::List(Vec::new().into()));
                    };
                    charge_collection_work(instructions_executed, end - start);
                    Ok(Value::List(items[start..end].to_vec().into()))
                }
                _ => Err(RuntimeError::SliceUnsupported),
            }
        }
        IntrinsicOp::ToString => {
            expect_arg_count("to_string", values, 1)?;
            let rendered = stringify_value(&values[0])?;
            // Stringifying writes each output byte once.
            charge_collection_work(instructions_executed, rendered.len());
            Ok(Value::String(rendered.into()))
        }
        IntrinsicOp::ToInt => {
            expect_arg_count("to_int", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.to_number()?
            {
                return Ok(Value::Number(as_number(&value)?.trunc()));
            }
            let value = materialize_value(values[0].clone())?;
            // Parsing a number reads the whole text.
            charge_collection_work(instructions_executed, proportional_units(&value));
            Ok(Value::Number(as_number(&value)?.trunc()))
        }
        IntrinsicOp::ToFloat => {
            expect_arg_count("to_float", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.to_number()?
            {
                return Ok(Value::Number(as_number(&value)?));
            }
            let value = materialize_value(values[0].clone())?;
            // Parsing a number reads the whole text.
            charge_collection_work(instructions_executed, proportional_units(&value));
            Ok(Value::Number(as_number(&value)?))
        }
        IntrinsicOp::JsonParse => {
            expect_arg_count("json_parse", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.json_parse()?
            {
                return Ok(value);
            }
            let value = materialize_value(values[0].clone())?;
            let text = coerce_string(&value)?;
            // Parsing reads every byte of the text once.
            charge_collection_work(instructions_executed, text.len());
            let parsed: serde_json::Value =
                serde_json::from_str(&text).map_err(|err| RuntimeError::InvalidJson {
                    detail: err.to_string(),
                })?;
            Ok(from_json(parsed))
        }
        IntrinsicOp::Format(_) => {
            if values.is_empty() {
                return Err(RuntimeError::FormatTemplateMissing);
            }
            let template = match &values[0] {
                Value::String(value) => value.as_str(),
                other => {
                    return Err(RuntimeError::FormatTemplateInvalid {
                        actual: value_type_name(other).to_string(),
                    });
                }
            };
            let rendered = apply_format(template, &values[1..])?;
            // Rendering reads the template and writes each output byte once.
            charge_collection_work(
                instructions_executed,
                template.len().saturating_add(rendered.len()),
            );
            Ok(Value::String(rendered.into()))
        }
        IntrinsicOp::Validate => {
            expect_arg_count("validate", values, 2)?;
            let value = materialize_value(values[0].clone())?;
            let schema = materialize_value(values[1].clone())?;
            // A validation walks the schema and the value's members once.
            charge_collection_work(
                instructions_executed,
                deep_proportional_units(&value).saturating_add(deep_proportional_units(&schema)),
            );
            execute_validate_builtin(value, &schema)
        }
        IntrinsicOp::Range(_) => {
            let result = execute_range_builtin(values)?;
            // Building the range writes each element once.
            charge_collection_work(instructions_executed, proportional_units(&result));
            Ok(result)
        }
        IntrinsicOp::CeilDiv => execute_integer_div_builtin("ceil_div", values, f64::ceil),
        IntrinsicOp::FloorDiv => execute_integer_div_builtin("floor_div", values, f64::floor),
        IntrinsicOp::Push => {
            expect_arg_count("push", values, 2)?;
            let result = execute_push_builtin(values[0].clone(), values[1].clone())?;
            // Copying the list writes each element once.
            charge_collection_work(instructions_executed, proportional_units(&result));
            Ok(result)
        }
        IntrinsicOp::Sort => execute_sort_builtin(values, instructions_executed),
        IntrinsicOp::SortBy => execute_sort_by_builtin(values, instructions_executed),
        IntrinsicOp::Sum => execute_sum_builtin(values, instructions_executed),
        IntrinsicOp::Min => {
            execute_extreme_builtin("min", values, Ordering::Less, instructions_executed)
        }
        IntrinsicOp::Max => {
            execute_extreme_builtin("max", values, Ordering::Greater, instructions_executed)
        }
        IntrinsicOp::Replace => execute_replace_builtin(values, instructions_executed),
        IntrinsicOp::Lower => {
            execute_case_builtin("lower", values, str::to_lowercase, instructions_executed)
        }
        IntrinsicOp::Upper => {
            execute_case_builtin("upper", values, str::to_uppercase, instructions_executed)
        }
        IntrinsicOp::Unique => execute_unique_builtin(values, instructions_executed),
        IntrinsicOp::Reverse => execute_reverse_builtin(values, instructions_executed),
        IntrinsicOp::ValidateCompiled(_)
        | IntrinsicOp::PushAssign(_)
        | IntrinsicOp::FormatCompiled(_)
        | IntrinsicOp::FormatCompiledSlotNumber { .. } => {
            unreachable!("compiled-only intrinsic reached generic executor")
        }
        IntrinsicOp::InvalidArity { name, argc } => {
            Err(invalid_arity_error(names[name].text.as_ref(), argc))
        }
        IntrinsicOp::Unknown { name, .. } => Err(RuntimeError::UnknownBuiltin {
            name: names[name].text.to_string(),
        }),
    }
}

/// Adds `amount` units of proportional intrinsic work to the instruction
/// counter a builtin or dispatch is accumulating into — the same
/// saturating-add `Vm::charge_intrinsic_work` performs, in a form a pure
/// helper or a call site holding a heap borrow can use.
pub(crate) fn charge_collection_work(instructions_executed: &mut u64, amount: usize) {
    *instructions_executed = instructions_executed.saturating_add(amount as u64);
}

fn shaping_list(
    builtin: &'static str,
    value: &Value,
    instructions_executed: &mut u64,
) -> Result<Vec<Value>, RuntimeError> {
    let value = materialize_value(value.clone())?;
    match value {
        Value::Tuple(items) | Value::List(items) => {
            charge_collection_work(instructions_executed, items.len());
            Ok(items.into_vec())
        }
        other => Err(RuntimeError::ShapingListRequired {
            builtin: builtin.into(),
            actual: value_type_name(&other).to_string(),
        }),
    }
}

fn compare_shaping_values(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::Number(left), Value::Number(right)) => Some(left.total_cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

fn validate_comparable_items(builtin: &'static str, items: &[Value]) -> Result<(), RuntimeError> {
    let Some(first) = items.first() else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        if compare_shaping_values(first, item).is_none() {
            return Err(RuntimeError::ShapingComparableRequired {
                builtin: builtin.into(),
                index,
                reference: value_type_name(first).to_string(),
                actual: value_type_name(item).to_string(),
            });
        }
    }
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "validate_comparable_items() rejected incomparable shapes one line above, so the comparison cannot fail, per the message"
)]
fn execute_sort_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("sort", values, 1)?;
    let mut items = shaping_list("sort", &values[0], instructions_executed)?;
    validate_comparable_items("sort", &items)?;
    charge_collection_work(instructions_executed, sorting_work(items.len()));
    items.sort_by(|left, right| {
        compare_shaping_values(left, right).expect("comparability was validated")
    });
    Ok(Value::List(items.into()))
}

fn field_path_value<'value>(value: &'value Value, path: &str) -> Option<&'value Value> {
    if path.is_empty() {
        return None;
    }
    let mut current = value;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        let Value::Record(record) = current else {
            return None;
        };
        current = record.get(segment)?;
    }
    Some(current)
}

#[expect(
    clippy::expect_used,
    reason = "validate_comparable_items() rejected incomparable keys two lines above, per the message"
)]
fn execute_sort_by_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("sort_by", values, 2)?;
    let items = shaping_list("sort_by", &values[0], instructions_executed)?;
    let path_value = materialize_value(values[1].clone())?;
    let Value::String(path) = path_value else {
        return Err(RuntimeError::ShapingTextRequired {
            builtin: "sort_by".into(),
            argument: "field path".into(),
            actual: value_type_name(&path_value).to_string(),
        });
    };
    if path.is_empty() {
        return Err(RuntimeError::SortByEmptyPath);
    }
    let mut keyed = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if !matches!(item, Value::Record(_)) {
            return Err(RuntimeError::SortByRecordRequired {
                index,
                actual: value_type_name(&item).to_string(),
            });
        }
        let key = field_path_value(&item, path.as_str())
            .cloned()
            .ok_or_else(|| RuntimeError::SortByMissingPath {
                path: path.to_string(),
                index,
            })?;
        keyed.push((key, item));
    }
    let keys = keyed.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
    validate_comparable_items("sort_by", &keys)?;
    charge_collection_work(instructions_executed, sorting_work(keyed.len()));
    keyed.sort_by(|(left, _), (right, _)| {
        compare_shaping_values(left, right).expect("comparability was validated")
    });
    Ok(Value::List(
        keyed
            .into_iter()
            .map(|(_, item)| item)
            .collect::<Vec<_>>()
            .into(),
    ))
}

fn execute_sum_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("sum", values, 1)?;
    let items = shaping_list("sum", &values[0], instructions_executed)?;
    let mut total = 0.0;
    for (index, item) in items.iter().enumerate() {
        let Value::Number(number) = item else {
            return Err(RuntimeError::ShapingNumberRequired {
                builtin: "sum".into(),
                index,
                actual: value_type_name(item).to_string(),
            });
        };
        total += number;
    }
    Ok(Value::Number(total))
}

fn execute_extreme_builtin(
    builtin: &'static str,
    values: &[Value],
    wanted: Ordering,
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count(builtin, values, 1)?;
    let items = shaping_list(builtin, &values[0], instructions_executed)?;
    validate_comparable_items(builtin, &items)?;
    let Some(mut extreme) = items.first().cloned() else {
        return Err(RuntimeError::ShapingEmptyList {
            builtin: builtin.into(),
        });
    };
    for item in &items[1..] {
        if compare_shaping_values(item, &extreme) == Some(wanted) {
            extreme = item.clone();
        }
    }
    Ok(extreme)
}

fn shaping_text(
    builtin: &'static str,
    argument: &'static str,
    value: &Value,
    instructions_executed: &mut u64,
) -> Result<String, RuntimeError> {
    let value = materialize_value(value.clone())?;
    match value {
        Value::String(value) => {
            charge_collection_work(instructions_executed, value.chars().count());
            Ok(value.to_string())
        }
        other => Err(RuntimeError::ShapingTextRequired {
            builtin: builtin.into(),
            argument: argument.into(),
            actual: value_type_name(&other).to_string(),
        }),
    }
}

fn execute_replace_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("replace", values, 3)?;
    let text = shaping_text("replace", "text", &values[0], instructions_executed)?;
    let from = shaping_text("replace", "needle", &values[1], instructions_executed)?;
    let to = shaping_text("replace", "replacement", &values[2], instructions_executed)?;
    Ok(Value::String(text.replace(&from, &to).into()))
}

fn execute_case_builtin(
    builtin: &'static str,
    values: &[Value],
    transform: fn(&str) -> String,
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count(builtin, values, 1)?;
    let text = shaping_text(builtin, "value", &values[0], instructions_executed)?;
    Ok(Value::String(transform(&text).into()))
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum ScalarEqualityKey {
    Null,
    Bool(bool),
    Number(u64),
    String(String),
}

fn scalar_equality_key(value: &Value) -> Option<ScalarEqualityKey> {
    match value {
        Value::Null => Some(ScalarEqualityKey::Null),
        Value::Bool(value) => Some(ScalarEqualityKey::Bool(*value)),
        Value::Number(value) if !value.is_nan() => {
            Some(ScalarEqualityKey::Number(if *value == 0.0 {
                0
            } else {
                value.to_bits()
            }))
        }
        Value::String(value) => Some(ScalarEqualityKey::String(value.to_string())),
        _ => None,
    }
}

fn execute_unique_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("unique", values, 1)?;
    let items = shaping_list("unique", &values[0], instructions_executed)?;
    let count = items.len();
    let mut unique = Vec::with_capacity(count);
    let mut scalar_seen = BTreeSet::new();
    let mut scanned = 0usize;
    for item in items {
        let unseen = match scalar_equality_key(&item) {
            Some(key) => scalar_seen.insert(key),
            // Composite values retain Value's typed equality. This fallback is
            // intentionally quadratic for records/collections; scalar-heavy
            // inputs use the O(n log n) ordered-set path above.
            None => {
                let position = unique.iter().position(|kept| *kept == item);
                scanned = scanned.saturating_add(position.map_or(unique.len(), |found| found + 1));
                position.is_none()
            }
        };
        if unseen {
            unique.push(item);
        }
    }
    // The scalar probes cost an ordered-set lookup each, and every composite
    // item compared against the elements kept before it.
    charge_collection_work(
        instructions_executed,
        sorting_work(count).saturating_add(scanned),
    );
    Ok(Value::List(unique.into()))
}

fn execute_reverse_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("reverse", values, 1)?;
    let mut items = shaping_list("reverse", &values[0], instructions_executed)?;
    items.reverse();
    Ok(Value::List(items.into()))
}

fn invalid_arity_error(name: &str, argc: usize) -> RuntimeError {
    // A handful of variadic builtins have bespoke prose; the rest derive their
    // message directly from the single arity registry.
    match name {
        "find" => RuntimeError::InvalidArgumentCount {
            name: "find".to_string(),
            expected: "2 or 3".to_string(),
            actual: argc,
        },
        "range" => RuntimeError::InvalidArgumentCount {
            name: "range".to_string(),
            expected: "1, 2, or 3".to_string(),
            actual: argc,
        },
        "format" => RuntimeError::FormatTemplateMissing,
        _ => {
            let expected = match crate::builtins::lookup(name).map(|builtin| builtin.arity) {
                Some(crate::builtins::Arity::Exact(n)) => n,
                _ => 0,
            };
            RuntimeError::InvalidArgumentCount {
                name: name.to_string(),
                expected: expected.to_string(),
                actual: argc,
            }
        }
    }
}

pub(crate) fn execute_len_builtin(value: &Value) -> Result<Value, RuntimeError> {
    if let Value::Projected(value) = value {
        return Ok(Value::Number(value.len()? as f64));
    }
    execute_len_direct(value)
}

pub(crate) fn execute_len_direct(value: &Value) -> Result<Value, RuntimeError> {
    value_len(value)
        .map(|len| Value::Number(len as f64))
        .ok_or(RuntimeError::LenUnsupported)
}

pub(crate) fn execute_contains_builtin(
    haystack: &Value,
    needle: &Value,
) -> Result<Value, RuntimeError> {
    let needle = materialize_value(needle.clone())?;
    if !matches!(haystack, Value::Projected(_)) {
        return execute_contains_direct(haystack, &needle).map(Value::Bool);
    }
    match haystack {
        Value::Projected(value) => Ok(Value::Bool(value.contains(&needle)?)),
        Value::Null => Ok(Value::Bool(false)),
        _ => Err(RuntimeError::ContainsUnsupported),
    }
}

pub(crate) fn execute_contains_direct(
    haystack: &Value,
    needle: &Value,
) -> Result<bool, RuntimeError> {
    match (haystack, needle) {
        (Value::String(haystack), needle) => Ok(haystack.contains(coerce_string(needle)?.as_ref())),
        (Value::Tuple(items), needle) => Ok(items.contains(needle)),
        (Value::List(items), needle) => Ok(items.contains(needle)),
        (Value::Record(record), needle) => {
            Ok(record.get(coerce_string(needle)?.as_ref()).is_some())
        }
        (Value::Null, _) => Ok(false),
        _ => Err(RuntimeError::ContainsUnsupported),
    }
}

pub(crate) fn execute_find_builtin(values: &[Value]) -> Result<Value, RuntimeError> {
    if !(values.len() == 2 || values.len() == 3) {
        return Err(RuntimeError::InvalidArgumentCount {
            name: "find".to_string(),
            expected: "2 or 3".to_string(),
            actual: values.len(),
        });
    }

    let needle = materialize_value(values[1].clone())?;
    let start = match values.get(2) {
        Some(value) => {
            let value = materialize_value(value.clone())?;
            as_non_negative_char_index("find", "start", &value)?
        }
        None => 0,
    };

    if let Value::Projected(value) = &values[0]
        && let Some(value) = value.find(needle.clone(), start)?
    {
        return Ok(value);
    }
    let haystack = materialize_value(values[0].clone())?;
    execute_find_direct(&haystack, &needle, start)
}

pub(crate) fn execute_grep_text_builtin(values: &[Value]) -> Result<Value, RuntimeError> {
    expect_arg_count("grep_text", values, 2)?;
    let needle = materialize_value(values[1].clone())?;
    if let Value::Projected(value) = &values[0]
        && let Some(value) = value.grep_text(needle.clone())?
    {
        return Ok(value);
    }
    let text = materialize_value(values[0].clone())?;
    execute_grep_text_direct(&text, &needle)
}

pub(crate) fn execute_find_direct(
    text: &Value,
    needle: &Value,
    start: usize,
) -> Result<Value, RuntimeError> {
    let text = coerce_string(text)?;
    let needle = coerce_string(needle)?;
    Ok(match find_text(text.as_ref(), needle.as_ref(), start) {
        Some(index) => Value::Number(index as f64),
        None => Value::Null,
    })
}

pub(crate) fn execute_grep_text_direct(
    text: &Value,
    needle: &Value,
) -> Result<Value, RuntimeError> {
    let text = coerce_string(text)?;
    let needle = coerce_string(needle)?;
    grep_text_strings(text.as_ref(), needle.as_ref())
}

fn grep_text_strings(text: &str, needle: &str) -> Result<Value, RuntimeError> {
    if needle.is_empty() {
        return Err(RuntimeError::EmptyGrepNeedle);
    }

    let needle_len = needle.chars().count();
    let needle_value = Value::String(needle.into());
    let mut matches = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let Some(start) = find_text(line, needle, 0) else {
            continue;
        };
        let mut record = record_with_capacity(5);
        record.insert_str("line", Value::Number((line_index + 1) as f64));
        record.insert_str("text", Value::String(line.into()));
        record.insert_str("match", needle_value.clone());
        record.insert_str("start", Value::Number(start as f64));
        record.insert_str("end", Value::Number((start + needle_len) as f64));
        matches.push(Value::Record(Arc::new(record)));
    }

    Ok(Value::List(matches.into()))
}

fn find_text(text: &str, needle: &str, start: usize) -> Option<usize> {
    let start_byte = if start == 0 {
        0
    } else {
        byte_index_for_char(text, start)?
    };
    if needle.is_empty() {
        return Some(start);
    }
    let tail = &text[start_byte..];
    let match_byte = tail.find(needle)?;
    Some(start + tail[..match_byte].chars().count())
}

fn byte_index_for_char(text: &str, target: usize) -> Option<usize> {
    let mut char_count = 0;
    for (byte_index, _) in text.char_indices() {
        if char_count == target {
            return Some(byte_index);
        }
        char_count += 1;
    }
    if char_count == target {
        Some(text.len())
    } else {
        None
    }
}

pub(crate) fn value_len(value: &Value) -> Option<usize> {
    match value {
        Value::String(value) => Some(value.chars().count()),
        Value::Tuple(values) => Some(values.len()),
        Value::List(values) => Some(values.len()),
        Value::Record(record) => Some(record.len()),
        Value::Null => Some(0),
        _ => None,
    }
}

pub(crate) fn iterable_values(value: Value) -> Result<ListValue, RuntimeError> {
    match value {
        Value::List(values) => Ok(values),
        Value::Tuple(values) => Ok(values),
        Value::Projected(value) => match value.materialize()? {
            Value::List(values) => Ok(values),
            Value::Tuple(values) => Ok(values),
            _ => Err(RuntimeError::NonListIteration),
        },
        _ => Err(RuntimeError::NonListIteration),
    }
}

pub(crate) fn execute_join_builtin(items: &Value, sep: &Value) -> Result<Value, RuntimeError> {
    let sep = materialize_value(sep.clone())?;
    if let Value::Projected(value) = items
        && let Some(value) = value.join(sep.clone())?
    {
        return Ok(value);
    }
    let items = materialize_value(items.clone())?;
    let items = match &items {
        Value::List(items) | Value::Tuple(items) => items,
        _ => {
            return Err(RuntimeError::JoinUnsupported);
        }
    };
    let sep = coerce_string(&sep)?;
    let mut joined = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            joined.push_str(sep.as_ref());
        }
        let item = materialize_value(item.clone())?;
        joined.push_str(coerce_string(&item)?.as_ref());
    }
    Ok(Value::String(joined.into()))
}

pub(crate) fn execute_range_builtin(values: &[Value]) -> Result<Value, RuntimeError> {
    let (start, end, step) = range_bounds_projected(values)?;
    build_range(start, end, step)
}

pub(crate) fn range_bounds_projected(values: &[Value]) -> Result<(i64, i64, i64), RuntimeError> {
    let mut materialized = Vec::with_capacity(values.len());
    for value in values {
        let value = match value {
            Value::Projected(projected) => match projected.range_bound()? {
                Some(value) => value,
                None => projected.materialize()?,
            },
            other => other.clone(),
        };
        materialized.push(value);
    }
    range_bounds(&materialized)
}

pub(crate) fn range_bounds(values: &[Value]) -> Result<(i64, i64, i64), RuntimeError> {
    let (start, end, step) = match values {
        [end] => (0, as_range_bound(end)?, 1),
        [start, end] => (as_range_bound(start)?, as_range_bound(end)?, 1),
        [start, end, step] => (
            as_range_bound(start)?,
            as_range_bound(end)?,
            as_range_bound(step)?,
        ),
        _ => {
            return Err(RuntimeError::InvalidArgumentCount {
                name: "range".to_string(),
                expected: "1, 2, or 3".to_string(),
                actual: values.len(),
            });
        }
    };
    if step == 0 {
        return Err(RuntimeError::ZeroRangeStep);
    }
    validate_range_len(start, end, step)?;
    Ok((start, end, step))
}

pub(crate) fn execute_push_builtin(list: Value, item: Value) -> Result<Value, RuntimeError> {
    let item = materialize_value(item)?;
    if let Value::Projected(value) = &list
        && let Some(value) = value.push(item.clone())?
    {
        return Ok(value);
    }
    let list = materialize_value(list)?;
    let Value::List(items) = list else {
        return Err(RuntimeError::PushUnsupported);
    };
    let mut values = items.into_vec();
    if values.len() == values.capacity() {
        values.reserve(1);
    }
    values.push(item);
    Ok(Value::List(values.into()))
}

pub(crate) fn as_range_bound(value: &Value) -> Result<i64, RuntimeError> {
    let Value::Number(number) = value else {
        return Err(RuntimeError::InvalidRangeBoundType {
            actual: value_type_name(value).to_string(),
        });
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || *number < i64::MIN as f64
        || *number > i64::MAX as f64
    {
        return Err(RuntimeError::InvalidRangeBound);
    }
    Ok(*number as i64)
}

pub(crate) fn build_range(start: i64, end: i64, step: i64) -> Result<Value, RuntimeError> {
    if range_len(start, end, step)? == 0 {
        return Ok(Value::List(Vec::new().into()));
    }
    let mut items = Vec::new();
    let mut value = start;
    if step > 0 {
        while value < end {
            items.push(Value::Number(value as f64));
            value = value.saturating_add(step);
        }
    } else {
        while value > end {
            items.push(Value::Number(value as f64));
            value = value.saturating_add(step);
        }
    }
    Ok(Value::List(items.into()))
}

pub(crate) fn validate_range_len(start: i64, end: i64, step: i64) -> Result<(), RuntimeError> {
    const MAX_RANGE_ITEMS: i128 = 1_000_000;
    if range_len(start, end, step)? > MAX_RANGE_ITEMS {
        return Err(RuntimeError::RangeTooLarge {
            limit: MAX_RANGE_ITEMS,
        });
    }
    Ok(())
}

fn range_len(start: i64, end: i64, step: i64) -> Result<i128, RuntimeError> {
    if step == 0 {
        return Err(RuntimeError::ZeroRangeStep);
    }
    if (step > 0 && start >= end) || (step < 0 && start <= end) {
        return Ok(0);
    }
    let distance = if step > 0 {
        end as i128 - start as i128
    } else {
        start as i128 - end as i128
    };
    let step = (step as i128).abs();
    Ok((distance + step - 1) / step)
}

pub(crate) fn execute_integer_div_builtin(
    name: &'static str,
    values: &[Value],
    round: impl FnOnce(f64) -> f64,
) -> Result<Value, RuntimeError> {
    expect_arg_count(name, values, 2)?;
    let dividend = materialize_value(values[0].clone())?;
    let divisor = materialize_value(values[1].clone())?;
    let dividend = as_integer_div_arg(name, "dividend", &dividend)?;
    let divisor = as_integer_div_arg(name, "divisor", &divisor)?;
    if divisor == 0.0 {
        return Err(RuntimeError::IntegerDivisionByZero {
            builtin: name.into(),
        });
    }
    Ok(Value::Number(round(dividend / divisor)))
}

fn as_integer_div_arg(
    builtin: &'static str,
    arg_name: &'static str,
    value: &Value,
) -> Result<f64, RuntimeError> {
    let Value::Number(number) = value else {
        return Err(RuntimeError::InvalidIntegerDivisionArgumentType {
            builtin: builtin.into(),
            argument: arg_name.into(),
            actual: value_type_name(value).to_string(),
        });
    };
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(RuntimeError::InvalidIntegerDivisionArgument {
            builtin: builtin.into(),
            argument: arg_name.into(),
        });
    }
    Ok(*number)
}

pub(crate) fn as_number(value: &Value) -> Result<f64, RuntimeError> {
    match value {
        Value::Number(value) => Ok(*value),
        Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::Null => Ok(0.0),
        Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(0.0);
            }
            value
                .parse::<f64>()
                .map_err(|_| RuntimeError::ExpectedNumber)
        }
        _ => Err(RuntimeError::ExpectedNumberType {
            actual: value_type_name(value).to_string(),
        }),
    }
}

pub(crate) fn coerce_string(value: &Value) -> Result<Cow<'_, str>, RuntimeError> {
    match value {
        Value::String(value) => Ok(Cow::Borrowed(value)),
        Value::Null => Ok(Cow::Borrowed("null")),
        Value::Undefined => Ok(Cow::Borrowed("undefined")),
        Value::Bool(value) => Ok(Cow::Owned(value.to_string())),
        Value::Number(value) => Ok(Cow::Owned(value.to_string())),
        Value::Image(_)
        | Value::Resource(_)
        | Value::Tuple(_)
        | Value::List(_)
        | Value::Record(_)
        | Value::Ref(_)
        | Value::Projected(_) => Err(RuntimeError::ExpectedText {
            actual: value_type_name(value).to_string(),
        }),
    }
}

pub(crate) fn as_offset(value: &Value) -> Result<isize, RuntimeError> {
    let number = as_number(value)?;
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(RuntimeError::InvalidIndex);
    }
    Ok(number as isize)
}

pub(crate) fn as_slice_bound(value: &Value) -> Result<Option<isize>, RuntimeError> {
    let value = match value {
        Value::Projected(projected) => match projected.slice_bound()? {
            Some(value) => value,
            None => projected.materialize()?,
        },
        other => other.clone(),
    };
    match &value {
        Value::Null => Ok(None),
        other => as_offset(other).map(Some),
    }
}

fn as_non_negative_char_index(
    builtin: &'static str,
    arg_name: &'static str,
    value: &Value,
) -> Result<usize, RuntimeError> {
    let number = as_number(value)?;
    if !number.is_finite() || number.fract() != 0.0 || number < 0.0 || number > usize::MAX as f64 {
        return Err(RuntimeError::InvalidCharacterIndex {
            builtin: builtin.into(),
            argument: arg_name.into(),
        });
    }
    Ok(number as usize)
}

pub(crate) fn is_truthy(value: &Value) -> Result<bool, RuntimeError> {
    Ok(match value {
        Value::Null | Value::Undefined => false,
        Value::Bool(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::String(value) => !value.is_empty(),
        Value::Image(_) | Value::Resource(_) | Value::List(_) | Value::Record(_) => true,
        Value::Tuple(values) => !values.is_empty(),
        Value::Projected(value) => value.truthy()?,
        Value::Ref(_) => {
            debug_assert_exported_value("truthiness");
            true
        }
    })
}

pub(crate) fn success(value: Value) -> Value {
    let result_names = result_wrapper_names();
    let mut record = record_with_capacity(2);
    record.insert_symbolized(
        result_names.ok.symbol,
        result_names.ok.text.clone(),
        Value::Bool(true),
    );
    record.insert_symbolized(
        result_names.value.symbol,
        result_names.value.text.clone(),
        value,
    );
    Value::Record(Arc::new(record))
}

pub(crate) fn execution_host_error_value(error: ExecutionHostError, operation: &str) -> Value {
    let result_names = result_wrapper_names();
    let mut details = record_with_capacity(2);
    details.insert("kind".to_string(), Value::String("effect".into()));
    details.insert(
        "operation".to_string(),
        Value::String(operation.to_string().into()),
    );
    let mut cause = tool_failure_fields(&error).unwrap_or_else(|| {
        let mut cause = record_with_capacity(1);
        cause.insert(
            "code".to_string(),
            Value::String("ResourceOperationFailed".into()),
        );
        cause
    });
    cause.insert("details".to_string(), Value::Record(Arc::new(details)));

    let mut record = record_with_capacity(3);
    record.insert_symbolized(
        result_names.ok.symbol,
        result_names.ok.text.clone(),
        Value::Bool(false),
    );
    record.insert_symbolized(
        result_names.error.symbol,
        result_names.error.text.clone(),
        Value::String(error.message().into()),
    );
    record.insert("cause".to_string(), Value::Record(Arc::new(cause)));
    Value::Record(Arc::new(record))
}

pub(crate) fn tool_failure_fields(error: &ExecutionHostError) -> Option<Record> {
    let class = match error.tool_failure_class()? {
        lash_sansio::ToolFailureClass::InvalidRequest => "invalid_request",
        lash_sansio::ToolFailureClass::Io => "io",
        lash_sansio::ToolFailureClass::Unavailable => "unavailable",
        lash_sansio::ToolFailureClass::PermissionDenied => "permission_denied",
        lash_sansio::ToolFailureClass::Timeout => "timeout",
        lash_sansio::ToolFailureClass::Execution => "execution",
        lash_sansio::ToolFailureClass::External => "external",
        lash_sansio::ToolFailureClass::ResourceLimit => "resource_limit",
        lash_sansio::ToolFailureClass::Internal => "internal",
    };
    let source = match error.tool_failure_source()? {
        lash_sansio::ToolFailureSource::Runtime => "runtime",
        lash_sansio::ToolFailureSource::Tool => "tool",
        lash_sansio::ToolFailureSource::Plugin => "plugin",
        lash_sansio::ToolFailureSource::Policy => "policy",
        lash_sansio::ToolFailureSource::Cancellation => "cancellation",
        lash_sansio::ToolFailureSource::UnknownLegacy => "unknown_legacy",
    };
    let retry = match error.tool_failure_retry()? {
        lash_sansio::ToolRetryStatus::Never => {
            let mut retry = record_with_capacity(1);
            retry.insert("type".to_string(), Value::String("never".into()));
            retry
        }
        lash_sansio::ToolRetryStatus::Safe { after_ms } => {
            let mut retry = record_with_capacity(2);
            retry.insert("type".to_string(), Value::String("safe".into()));
            if let Some(after_ms) = after_ms {
                retry.insert("after_ms".to_string(), Value::Number(*after_ms as f64));
            }
            retry
        }
        lash_sansio::ToolRetryStatus::Exhausted { attempts } => {
            let mut retry = record_with_capacity(2);
            retry.insert("type".to_string(), Value::String("exhausted".into()));
            retry.insert("attempts".to_string(), Value::Number((*attempts).into()));
            retry
        }
        lash_sansio::ToolRetryStatus::UnknownLegacy => {
            let mut retry = record_with_capacity(1);
            retry.insert("type".to_string(), Value::String("unknown_legacy".into()));
            retry
        }
    };

    let mut fields = record_with_capacity(4);
    fields.insert(
        "code".to_string(),
        Value::String(error.tool_failure_code()?.into()),
    );
    fields.insert("class".to_string(), Value::String(class.into()));
    fields.insert("source".to_string(), Value::String(source.into()));
    fields.insert("retry".to_string(), Value::Record(Arc::new(retry)));
    Some(fields)
}

/// Fails loudly in debug builds when a heap reference reaches a boundary that
/// cannot report an error, and lets release builds carry on with the caller's
/// defined fallback.
///
/// A reference here is always a VM bug — the instruction heap plan is supposed
/// to have exported it — but a durable session losing its host process over a
/// display or serialization path is a far worse failure than a wrong rendering,
/// so these paths never panic in release.
pub(crate) fn debug_assert_exported_value(context: &str) {
    debug_assert!(
        false,
        "heap references must be exported before {context}; the instruction heap plan is missing this opcode"
    );
    let _ = context;
}

pub(crate) fn value_type_name(value: &Value) -> &str {
    match value {
        Value::Null => "null",
        Value::Undefined => "undefined",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Image(_) => "image",
        Value::Resource(_) => "resource",
        Value::Tuple(_) => "tuple",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Projected(value) => value.value_type_name(),
        Value::Ref(_) => "heap_ref",
    }
}

pub(crate) fn value_contains_projected(value: &Value) -> bool {
    match value {
        Value::Projected(_) => true,
        Value::Tuple(values) => values.iter().any(value_contains_projected),
        Value::List(values) => values.iter().any(value_contains_projected),
        Value::Record(record) => record.values().any(value_contains_projected),
        Value::Null
        | Value::Undefined
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Image(_)
        | Value::Resource(_)
        | Value::Ref(_) => false,
    }
}

pub(crate) fn materialize_value(value: Value) -> Result<Value, RuntimeError> {
    match value {
        Value::Projected(projected) => projected.materialize(),
        other => Ok(other),
    }
}
