//! Binary operations, comparison, numeric coercion, range/iterator helpers,
//! `is_truthy` / `materialize_projected` / `value_type_name`, and builtin
//! dispatch (`intrinsic` and the per-intrinsic direct paths).

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::ast::BinaryOp;

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

pub(crate) async fn execute_intrinsic(
    builtin: IntrinsicOp,
    names: &[Name],
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    match builtin {
        IntrinsicOp::Len => {
            expect_arg_count("len", values, 1)?;
            execute_len_builtin(&values[0]).await
        }
        IntrinsicOp::Empty => {
            expect_arg_count("empty", values, 1)?;
            match &values[0] {
                Value::String(value) => Ok(Value::Bool(value.is_empty())),
                Value::Tuple(values) => Ok(Value::Bool(values.is_empty())),
                Value::List(values) => Ok(Value::Bool(values.is_empty())),
                Value::Record(record) => Ok(Value::Bool(record.is_empty())),
                Value::Projected(value) => value
                    .empty()
                    .await
                    .map(Value::Bool)
                    .ok_or(RuntimeError::EmptyUnsupported),
                Value::Null => Ok(Value::Bool(true)),
                _ => Err(RuntimeError::EmptyUnsupported),
            }
        }
        IntrinsicOp::Keys => {
            expect_arg_count("keys", values, 1)?;
            match &values[0] {
                Value::Record(record) => Ok(Value::List(
                    record
                        .keys()
                        .map(|key| Value::String(key.into()))
                        .collect::<Vec<_>>()
                        .into(),
                )),
                Value::Projected(value) => Ok(Value::List(
                    value
                        .keys()
                        .await
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
                && let Some(value) = value.values().await
            {
                return Ok(value);
            }
            let value = materialize_projected_async(values[0].clone()).await;
            match &value {
                Value::Record(record) => Ok(Value::List(
                    record.values().cloned().collect::<Vec<_>>().into(),
                )),
                Value::Null => Ok(Value::List(Vec::new().into())),
                _ => Err(RuntimeError::ValuesUnsupported),
            }
        }
        IntrinsicOp::Contains => {
            expect_arg_count("contains", values, 2)?;
            execute_contains_builtin(&values[0], &values[1]).await
        }
        IntrinsicOp::Find(_) => execute_find_builtin(values).await,
        IntrinsicOp::GrepText => execute_grep_text_builtin(values).await,
        IntrinsicOp::StartsWith => {
            expect_arg_count("starts_with", values, 2)?;
            let prefix = materialize_projected_async(values[1].clone()).await;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.starts_with(prefix.clone()).await
            {
                return Ok(value);
            }
            let value = materialize_projected_async(values[0].clone()).await;
            let value = coerce_string(&value)?;
            let prefix = coerce_string(&prefix)?;
            Ok(Value::Bool(value.starts_with(prefix.as_ref())))
        }
        IntrinsicOp::EndsWith => {
            expect_arg_count("ends_with", values, 2)?;
            let suffix = materialize_projected_async(values[1].clone()).await;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.ends_with(suffix.clone()).await
            {
                return Ok(value);
            }
            let value = materialize_projected_async(values[0].clone()).await;
            let value = coerce_string(&value)?;
            let suffix = coerce_string(&suffix)?;
            Ok(Value::Bool(value.ends_with(suffix.as_ref())))
        }
        IntrinsicOp::Split => {
            expect_arg_count("split", values, 2)?;
            let needle = materialize_projected_async(values[1].clone()).await;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.split(needle.clone()).await
            {
                return Ok(value);
            }
            let value = materialize_projected_async(values[0].clone()).await;
            let value = coerce_string(&value)?;
            let needle = coerce_string(&needle)?;
            Ok(Value::List(
                value
                    .split(needle.as_ref())
                    .map(|part| Value::String(part.into()))
                    .collect::<Vec<_>>()
                    .into(),
            ))
        }
        IntrinsicOp::Join => {
            expect_arg_count("join", values, 2)?;
            execute_join_builtin_async(&values[0], &values[1]).await
        }
        IntrinsicOp::Trim => {
            expect_arg_count("trim", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.trim().await
            {
                return Ok(value);
            }
            let value = materialize_projected_async(values[0].clone()).await;
            Ok(Value::String(coerce_string(&value)?.trim().into()))
        }
        IntrinsicOp::Slice => {
            expect_arg_count("slice", values, 3)?;
            let start = as_slice_bound_async(&values[1]).await?;
            let end = as_slice_bound_async(&values[2]).await?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.slice(start, end).await
            {
                return Ok(value);
            }
            let target = materialize_projected_async(values[0].clone()).await;
            match &target {
                Value::String(value) => Ok(Value::String(slice_string(value, start, end).into())),
                Value::Tuple(items) => {
                    let Some((start, end)) = clamp_slice_bounds(start, end, items.len()) else {
                        return Ok(Value::Tuple(Vec::new().into()));
                    };
                    Ok(Value::Tuple(items[start..end].to_vec().into()))
                }
                Value::List(items) => {
                    let Some((start, end)) = clamp_slice_bounds(start, end, items.len()) else {
                        return Ok(Value::List(Vec::new().into()));
                    };
                    Ok(Value::List(items[start..end].to_vec().into()))
                }
                _ => Err(RuntimeError::SliceUnsupported),
            }
        }
        IntrinsicOp::ToString => {
            expect_arg_count("to_string", values, 1)?;
            let value = if value_contains_projected(&values[0]) {
                stringify_value_async(&values[0]).await?
            } else {
                stringify_value_direct(&values[0])?
            };
            Ok(Value::String(value.into()))
        }
        IntrinsicOp::ToInt => {
            expect_arg_count("to_int", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.to_number().await
            {
                return Ok(Value::Number(as_number(&value)?.trunc()));
            }
            let value = materialize_projected_async(values[0].clone()).await;
            Ok(Value::Number(as_number(&value)?.trunc()))
        }
        IntrinsicOp::ToFloat => {
            expect_arg_count("to_float", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.to_number().await
            {
                return Ok(Value::Number(as_number(&value)?));
            }
            let value = materialize_projected_async(values[0].clone()).await;
            Ok(Value::Number(as_number(&value)?))
        }
        IntrinsicOp::JsonParse => {
            expect_arg_count("json_parse", values, 1)?;
            if let Value::Projected(value) = &values[0]
                && let Some(value) = value.json_parse().await
            {
                return Ok(value);
            }
            let value = materialize_projected_async(values[0].clone()).await;
            let parsed: serde_json::Value =
                serde_json::from_str(&coerce_string(&value)?).map_err(|err| {
                    RuntimeError::InvalidJson {
                        detail: err.to_string(),
                    }
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
            Ok(Value::String(
                apply_format_async(template, &values[1..]).await?.into(),
            ))
        }
        IntrinsicOp::Validate => {
            expect_arg_count("validate", values, 2)?;
            execute_validate_builtin(
                materialize_projected_async(values[0].clone()).await,
                &materialize_projected_async(values[1].clone()).await,
            )
        }
        IntrinsicOp::Range(_) => execute_range_builtin_async(values).await,
        IntrinsicOp::CeilDiv => {
            execute_integer_div_builtin_async("ceil_div", values, f64::ceil).await
        }
        IntrinsicOp::FloorDiv => {
            execute_integer_div_builtin_async("floor_div", values, f64::floor).await
        }
        IntrinsicOp::Push => {
            expect_arg_count("push", values, 2)?;
            execute_push_builtin_async(values[0].clone(), values[1].clone()).await
        }
        IntrinsicOp::Sort => execute_sort_builtin(values, instructions_executed).await,
        IntrinsicOp::SortBy => execute_sort_by_builtin(values, instructions_executed).await,
        IntrinsicOp::Sum => execute_sum_builtin(values, instructions_executed).await,
        IntrinsicOp::Min => {
            execute_extreme_builtin("min", values, Ordering::Less, instructions_executed).await
        }
        IntrinsicOp::Max => {
            execute_extreme_builtin("max", values, Ordering::Greater, instructions_executed).await
        }
        IntrinsicOp::Replace => execute_replace_builtin(values, instructions_executed).await,
        IntrinsicOp::Lower => {
            execute_case_builtin("lower", values, str::to_lowercase, instructions_executed).await
        }
        IntrinsicOp::Upper => {
            execute_case_builtin("upper", values, str::to_uppercase, instructions_executed).await
        }
        IntrinsicOp::Unique => execute_unique_builtin(values, instructions_executed).await,
        IntrinsicOp::Reverse => execute_reverse_builtin(values, instructions_executed).await,
        IntrinsicOp::ValidateCompiled(_)
        | IntrinsicOp::PushAssign(_)
        | IntrinsicOp::FormatCompiled(_)
        | IntrinsicOp::FormatCompiledSlotNumber { .. }
        | IntrinsicOp::FormatCompiledSlotNumberBinary { .. } => {
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

fn charge_collection_work(instructions_executed: &mut u64, amount: usize) {
    *instructions_executed = instructions_executed.saturating_add(amount as u64);
}

fn sorting_work(items: usize) -> usize {
    if items < 2 {
        return 0;
    }
    items.saturating_mul(usize::BITS.saturating_sub((items - 1).leading_zeros()) as usize)
}

async fn shaping_list(
    builtin: &'static str,
    value: &Value,
    instructions_executed: &mut u64,
) -> Result<Vec<Value>, RuntimeError> {
    let value = materialize_projected_async(value.clone()).await;
    match value {
        Value::Tuple(items) | Value::List(items) => {
            charge_collection_work(instructions_executed, items.len());
            Ok(items.into_vec())
        }
        other => Err(RuntimeError::ShapingListRequired {
            builtin,
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
                builtin,
                index,
                reference: value_type_name(first).to_string(),
                actual: value_type_name(item).to_string(),
            });
        }
    }
    Ok(())
}

async fn execute_sort_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("sort", values, 1)?;
    let mut items = shaping_list("sort", &values[0], instructions_executed).await?;
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

async fn execute_sort_by_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("sort_by", values, 2)?;
    let items = shaping_list("sort_by", &values[0], instructions_executed).await?;
    let path_value = materialize_projected_async(values[1].clone()).await;
    let Value::String(path) = path_value else {
        return Err(RuntimeError::ShapingTextRequired {
            builtin: "sort_by",
            argument: "field path",
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

async fn execute_sum_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("sum", values, 1)?;
    let items = shaping_list("sum", &values[0], instructions_executed).await?;
    let mut total = 0.0;
    for (index, item) in items.iter().enumerate() {
        let Value::Number(number) = item else {
            return Err(RuntimeError::ShapingNumberRequired {
                builtin: "sum",
                index,
                actual: value_type_name(item).to_string(),
            });
        };
        total += number;
    }
    Ok(Value::Number(total))
}

async fn execute_extreme_builtin(
    builtin: &'static str,
    values: &[Value],
    wanted: Ordering,
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count(builtin, values, 1)?;
    let items = shaping_list(builtin, &values[0], instructions_executed).await?;
    validate_comparable_items(builtin, &items)?;
    let Some(mut extreme) = items.first().cloned() else {
        return Err(RuntimeError::ShapingEmptyList { builtin });
    };
    for item in &items[1..] {
        if compare_shaping_values(item, &extreme) == Some(wanted) {
            extreme = item.clone();
        }
    }
    Ok(extreme)
}

async fn shaping_text(
    builtin: &'static str,
    argument: &'static str,
    value: &Value,
    instructions_executed: &mut u64,
) -> Result<String, RuntimeError> {
    let value = materialize_projected_async(value.clone()).await;
    match value {
        Value::String(value) => {
            charge_collection_work(instructions_executed, value.chars().count());
            Ok(value.to_string())
        }
        other => Err(RuntimeError::ShapingTextRequired {
            builtin,
            argument,
            actual: value_type_name(&other).to_string(),
        }),
    }
}

async fn execute_replace_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("replace", values, 3)?;
    let text = shaping_text("replace", "text", &values[0], instructions_executed).await?;
    let from = shaping_text("replace", "needle", &values[1], instructions_executed).await?;
    let to = shaping_text("replace", "replacement", &values[2], instructions_executed).await?;
    Ok(Value::String(text.replace(&from, &to).into()))
}

async fn execute_case_builtin(
    builtin: &'static str,
    values: &[Value],
    transform: fn(&str) -> String,
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count(builtin, values, 1)?;
    let text = shaping_text(builtin, "value", &values[0], instructions_executed).await?;
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

async fn execute_unique_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("unique", values, 1)?;
    let items = shaping_list("unique", &values[0], instructions_executed).await?;
    let mut unique = Vec::with_capacity(items.len());
    let mut scalar_seen = BTreeSet::new();
    for item in items {
        let unseen = match scalar_equality_key(&item) {
            Some(key) => scalar_seen.insert(key),
            // Composite values retain Value's typed equality. This fallback is
            // intentionally quadratic for records/collections; scalar-heavy
            // inputs use the O(n log n) ordered-set path above.
            None => !unique.contains(&item),
        };
        if unseen {
            unique.push(item);
        }
    }
    Ok(Value::List(unique.into()))
}

async fn execute_reverse_builtin(
    values: &[Value],
    instructions_executed: &mut u64,
) -> Result<Value, RuntimeError> {
    expect_arg_count("reverse", values, 1)?;
    let mut items = shaping_list("reverse", &values[0], instructions_executed).await?;
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

pub(crate) async fn execute_len_builtin(value: &Value) -> Result<Value, RuntimeError> {
    if let Value::Projected(value) = value {
        return Ok(Value::Number(value.len().await as f64));
    }
    execute_len_direct(value)
}

pub(crate) fn execute_len_direct(value: &Value) -> Result<Value, RuntimeError> {
    value_len(value)
        .map(|len| Value::Number(len as f64))
        .ok_or(RuntimeError::LenUnsupported)
}

pub(crate) async fn execute_contains_builtin(
    haystack: &Value,
    needle: &Value,
) -> Result<Value, RuntimeError> {
    let needle = materialize_projected_async(needle.clone()).await;
    if !matches!(haystack, Value::Projected(_)) {
        return execute_contains_direct(haystack, &needle).map(Value::Bool);
    }
    match haystack {
        Value::Projected(value) => Ok(Value::Bool(value.contains(&needle).await?)),
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

pub(crate) fn execute_membership_direct(
    haystack: &Value,
    needle: &Value,
) -> Result<bool, RuntimeError> {
    match (haystack, needle) {
        (Value::String(haystack), Value::String(needle)) => Ok(haystack.contains(needle.as_str())),
        (Value::Tuple(items), needle) => Ok(items.contains(needle)),
        (Value::List(items), needle) => Ok(items.contains(needle)),
        (Value::Record(record), Value::String(needle)) => Ok(record.get(needle.as_str()).is_some()),
        (Value::Null, _) => Ok(false),
        _ => Err(RuntimeError::InUnsupported),
    }
}

pub(crate) async fn execute_find_builtin(values: &[Value]) -> Result<Value, RuntimeError> {
    if !(values.len() == 2 || values.len() == 3) {
        return Err(RuntimeError::InvalidArgumentCount {
            name: "find".to_string(),
            expected: "2 or 3".to_string(),
            actual: values.len(),
        });
    }

    let needle = materialize_projected_async(values[1].clone()).await;
    let start = match values.get(2) {
        Some(value) => {
            let value = materialize_projected_async(value.clone()).await;
            as_non_negative_char_index("find", "start", &value)?
        }
        None => 0,
    };

    if let Value::Projected(value) = &values[0]
        && let Some(value) = value.find(needle.clone(), start).await
    {
        return Ok(value);
    }
    let haystack = materialize_projected_async(values[0].clone()).await;
    execute_find_direct(&haystack, &needle, start)
}

pub(crate) async fn execute_grep_text_builtin(values: &[Value]) -> Result<Value, RuntimeError> {
    expect_arg_count("grep_text", values, 2)?;
    let needle = materialize_projected_async(values[1].clone()).await;
    if let Value::Projected(value) = &values[0]
        && let Some(value) = value.grep_text(needle.clone()).await
    {
        return Ok(value);
    }
    let text = materialize_projected_async(values[0].clone()).await;
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

pub(crate) async fn iterable_values(value: Value) -> Result<ListValue, RuntimeError> {
    match value {
        Value::List(values) => Ok(values),
        Value::Tuple(values) => Ok(values),
        Value::Projected(value) => match value.materialize_async().await {
            Value::List(values) => Ok(values),
            Value::Tuple(values) => Ok(values),
            _ => Err(RuntimeError::NonListIteration),
        },
        _ => Err(RuntimeError::NonListIteration),
    }
}

pub(crate) async fn execute_join_builtin_async(
    items: &Value,
    sep: &Value,
) -> Result<Value, RuntimeError> {
    let sep = materialize_projected_async(sep.clone()).await;
    if let Value::Projected(value) = items
        && let Some(value) = value.join(sep.clone()).await
    {
        return Ok(value);
    }
    let items = materialize_projected_async(items.clone()).await;
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
        let item = materialize_projected_async(item.clone()).await;
        joined.push_str(coerce_string(&item)?.as_ref());
    }
    Ok(Value::String(joined.into()))
}

#[cfg(test)]
pub(crate) fn execute_join_builtin(items: &Value, sep: &Value) -> Result<Value, RuntimeError> {
    futures_executor::block_on(execute_join_builtin_async(items, sep))
}

pub(crate) fn execute_range_builtin(values: &[Value]) -> Result<Value, RuntimeError> {
    let (start, end, step) = range_bounds(values)?;
    build_range(start, end, step)
}

pub(crate) async fn execute_range_builtin_async(values: &[Value]) -> Result<Value, RuntimeError> {
    let (start, end, step) = range_bounds_async(values).await?;
    build_range(start, end, step)
}

pub(crate) async fn range_bounds_async(values: &[Value]) -> Result<(i64, i64, i64), RuntimeError> {
    let mut materialized = Vec::with_capacity(values.len());
    for value in values {
        let value = match value {
            Value::Projected(projected) => match projected.range_bound().await {
                Some(value) => value,
                None => projected.materialize_async().await,
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

pub(crate) async fn execute_push_builtin_async(
    list: Value,
    item: Value,
) -> Result<Value, RuntimeError> {
    let item = materialize_projected_async(item).await;
    if let Value::Projected(value) = &list
        && let Some(value) = value.push(item.clone()).await
    {
        return Ok(value);
    }
    let list = materialize_projected_async(list).await;
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

#[cfg(test)]
pub(crate) fn execute_push_builtin(list: &Value, item: Value) -> Result<Value, RuntimeError> {
    futures_executor::block_on(execute_push_builtin_async(list.clone(), item))
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
    let dividend = as_integer_div_arg(name, "dividend", &values[0])?;
    let divisor = as_integer_div_arg(name, "divisor", &values[1])?;
    if divisor == 0.0 {
        return Err(RuntimeError::IntegerDivisionByZero { builtin: name });
    }
    Ok(Value::Number(round(dividend / divisor)))
}

async fn execute_integer_div_builtin_async(
    name: &'static str,
    values: &[Value],
    round: impl FnOnce(f64) -> f64,
) -> Result<Value, RuntimeError> {
    expect_arg_count(name, values, 2)?;
    let dividend = materialize_projected_async(values[0].clone()).await;
    let divisor = materialize_projected_async(values[1].clone()).await;
    execute_integer_div_builtin(name, &[dividend, divisor], round)
}

fn as_integer_div_arg(
    builtin: &'static str,
    arg_name: &'static str,
    value: &Value,
) -> Result<f64, RuntimeError> {
    let Value::Number(number) = value else {
        return Err(RuntimeError::InvalidIntegerDivisionArgumentType {
            builtin,
            argument: arg_name,
            actual: value_type_name(value).to_string(),
        });
    };
    if !number.is_finite() || number.fract() != 0.0 {
        return Err(RuntimeError::InvalidIntegerDivisionArgument {
            builtin,
            argument: arg_name,
        });
    }
    Ok(*number)
}

pub(crate) fn eval_binary_values(
    left: Value,
    op: BinaryOp,
    right: Value,
) -> Result<Value, RuntimeError> {
    match op {
        BinaryOp::Add => add_values(left, right),
        BinaryOp::Subtract => numeric_binary_values(left, right, |a, b| a - b),
        BinaryOp::Multiply => numeric_binary_values(left, right, |a, b| a * b),
        BinaryOp::Divide => numeric_binary_values(left, right, |a, b| a / b),
        BinaryOp::Modulo => numeric_binary_values(left, right, |a, b| a % b),
        BinaryOp::Equal => Ok(Value::Bool(left == right)),
        BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
        BinaryOp::Less => compare_ordered(left, right, |a, b| a < b, |a, b| a < b),
        BinaryOp::LessEqual => compare_ordered(left, right, |a, b| a <= b, |a, b| a <= b),
        BinaryOp::Greater => compare_ordered(left, right, |a, b| a > b, |a, b| a > b),
        BinaryOp::GreaterEqual => compare_ordered(left, right, |a, b| a >= b, |a, b| a >= b),
        BinaryOp::In => execute_membership_direct(&right, &left).map(Value::Bool),
        BinaryOp::And | BinaryOp::Or => unreachable!("logical ops are compiled with jumps"),
    }
}

pub(crate) fn eval_number_binary_values(left: f64, op: BinaryOp, right: f64) -> Value {
    match op {
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo => Value::Number(eval_number_numeric_binary_value(left, op, right)),
        BinaryOp::Equal => Value::Bool(left == right),
        BinaryOp::NotEqual => Value::Bool(left != right),
        BinaryOp::Less => Value::Bool(left < right),
        BinaryOp::LessEqual => Value::Bool(left <= right),
        BinaryOp::Greater => Value::Bool(left > right),
        BinaryOp::GreaterEqual => Value::Bool(left >= right),
        BinaryOp::In => unreachable!("membership is not numeric"),
        BinaryOp::And | BinaryOp::Or => unreachable!("logical ops are compiled with jumps"),
    }
}

pub(crate) fn eval_number_numeric_binary_value(left: f64, op: BinaryOp, right: f64) -> f64 {
    match op {
        BinaryOp::Add => left + right,
        BinaryOp::Subtract => left - right,
        BinaryOp::Multiply => left * right,
        BinaryOp::Divide => left / right,
        BinaryOp::Modulo => left % right,
        _ => unreachable!("non-numeric op in fused numeric branch"),
    }
}

pub(crate) fn eval_number_compare_values(left: f64, op: BinaryOp, right: f64) -> bool {
    match op {
        BinaryOp::Equal => left == right,
        BinaryOp::NotEqual => left != right,
        BinaryOp::Less => left < right,
        BinaryOp::LessEqual => left <= right,
        BinaryOp::Greater => left > right,
        BinaryOp::GreaterEqual => left >= right,
        BinaryOp::In => unreachable!("membership is not a numeric comparison"),
        _ => unreachable!("non-comparison op in fused slot branch"),
    }
}

pub(crate) fn is_comparison_binary_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Equal
            | BinaryOp::NotEqual
            | BinaryOp::Less
            | BinaryOp::LessEqual
            | BinaryOp::Greater
            | BinaryOp::GreaterEqual
    )
}

pub(crate) fn is_numeric_binary_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add
            | BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Modulo
    )
}

pub(crate) fn eval_compare_values(
    left: Value,
    op: BinaryOp,
    right: Value,
) -> Result<bool, RuntimeError> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            Ok(eval_number_compare_values(left, op, right))
        }
        (left, right) => match op {
            BinaryOp::Equal => Ok(left == right),
            BinaryOp::NotEqual => Ok(left != right),
            BinaryOp::Less => {
                compare_ordered(left, right, |a, b| a < b, |a, b| a < b).map(expect_bool_value)
            }
            BinaryOp::LessEqual => {
                compare_ordered(left, right, |a, b| a <= b, |a, b| a <= b).map(expect_bool_value)
            }
            BinaryOp::Greater => {
                compare_ordered(left, right, |a, b| a > b, |a, b| a > b).map(expect_bool_value)
            }
            BinaryOp::GreaterEqual => {
                compare_ordered(left, right, |a, b| a >= b, |a, b| a >= b).map(expect_bool_value)
            }
            BinaryOp::In => unreachable!("membership is not a fused comparison"),
            _ => unreachable!("non-comparison op in fused branch"),
        },
    }
}

pub(crate) fn expect_bool_value(value: Value) -> bool {
    match value {
        Value::Bool(value) => value,
        _ => unreachable!("comparison produced non-bool value"),
    }
}

pub(crate) async fn materialize_projected_async(value: Value) -> Value {
    match value {
        Value::Projected(projected) => projected.materialize_async().await,
        other => other,
    }
}

pub(crate) fn numeric_binary_values(
    left: Value,
    right: Value,
    op: impl FnOnce(f64, f64) -> f64,
) -> Result<Value, RuntimeError> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => Ok(Value::Number(op(left, right))),
        (left, right) => Ok(Value::Number(op(as_number(&left)?, as_number(&right)?))),
    }
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
        Value::Bool(value) => Ok(Cow::Owned(value.to_string())),
        Value::Number(value) => Ok(Cow::Owned(value.to_string())),
        Value::Image(_)
        | Value::Resource(_)
        | Value::Tuple(_)
        | Value::List(_)
        | Value::Record(_)
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
    match value {
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
            builtin,
            argument: arg_name,
        });
    }
    Ok(number as usize)
}

pub(crate) async fn as_slice_bound_async(value: &Value) -> Result<Option<isize>, RuntimeError> {
    let value = match value {
        Value::Projected(projected) => match projected.slice_bound().await {
            Some(value) => value,
            None => projected.materialize_async().await,
        },
        other => other.clone(),
    };
    as_slice_bound(&value)
}

pub(crate) fn compare_numbers(
    left: Value,
    right: Value,
    cmp: impl FnOnce(f64, f64) -> bool,
) -> Result<Value, RuntimeError> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => Ok(Value::Bool(cmp(left, right))),
        (left, right) => Ok(Value::Bool(cmp(as_number(&left)?, as_number(&right)?))),
    }
}

pub(crate) fn compare_ordered(
    left: Value,
    right: Value,
    number_cmp: impl FnOnce(f64, f64) -> bool,
    string_cmp: impl FnOnce(&str, &str) -> bool,
) -> Result<Value, RuntimeError> {
    match (&left, &right) {
        (Value::String(left), Value::String(right)) => Ok(Value::Bool(string_cmp(left, right))),
        _ => compare_numbers(left, right, number_cmp),
    }
}

pub(crate) fn add_values(left: Value, right: Value) -> Result<Value, RuntimeError> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
        (Value::String(a), Value::String(b)) => Ok(Value::String(a + &b)),
        (Value::String(mut a), other) => {
            a.push_str(&stringify_value_blocking(&other)?);
            Ok(Value::String(a))
        }
        (other, Value::String(b)) => {
            let mut text = stringify_value_blocking(&other)?;
            text.push_str(&b);
            Ok(Value::String(text.into()))
        }
        (Value::List(a), Value::List(b)) => {
            let mut values = Vec::with_capacity(a.len() + b.len());
            values.extend(a.iter().cloned());
            values.extend(b.iter().cloned());
            Ok(Value::List(values.into()))
        }
        (Value::Tuple(a), Value::Tuple(b)) => {
            let mut values = Vec::with_capacity(a.len() + b.len());
            values.extend(a.iter().cloned());
            values.extend(b.iter().cloned());
            Ok(Value::Tuple(values.into()))
        }
        (Value::List(_), Value::Tuple(_)) | (Value::Tuple(_), Value::List(_)) => {
            Err(RuntimeError::IncompatibleSequenceConcatenation)
        }
        (left, right) => Ok(Value::Number(as_number(&left)? + as_number(&right)?)),
    }
}

pub(crate) fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::String(value) => !value.is_empty(),
        Value::Image(_) | Value::Resource(_) | Value::List(_) | Value::Record(_) => true,
        Value::Tuple(values) => !values.is_empty(),
        Value::Projected(value) => futures_executor::block_on(value.truthy()),
    }
}

pub(crate) async fn is_truthy_async(value: &Value) -> bool {
    match value {
        Value::Projected(value) => value.truthy().await,
        other => is_truthy(other),
    }
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

pub(crate) fn error_value(message: String) -> Value {
    let result_names = result_wrapper_names();
    let mut record = record_with_capacity(2);
    record.insert_symbolized(
        result_names.ok.symbol,
        result_names.ok.text.clone(),
        Value::Bool(false),
    );
    record.insert_symbolized(
        result_names.error.symbol,
        result_names.error.text.clone(),
        Value::String(message.into()),
    );
    Value::Record(Arc::new(record))
}

pub(crate) fn value_type_name(value: &Value) -> &str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Image(_) => "image",
        Value::Resource(_) => "resource",
        Value::Tuple(_) => "tuple",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Projected(value) => value.value_type_name(),
    }
}

pub(crate) fn value_contains_projected(value: &Value) -> bool {
    match value {
        Value::Projected(_) => true,
        Value::Tuple(values) => values.iter().any(value_contains_projected),
        Value::List(values) => values.iter().any(value_contains_projected),
        Value::Record(record) => record.values().any(value_contains_projected),
        Value::Null
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Image(_)
        | Value::Resource(_) => false,
    }
}

pub(crate) fn materialize_value(value: Value) -> Value {
    match value {
        Value::Projected(projected) => projected.materialize(),
        other => other,
    }
}
