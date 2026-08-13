//! String formatting and stringification: `apply_format` (the `f"..."`
//! template engine), `stringify_value*` (record/list/tuple/projected pretty-print),
//! `execute_compiled_format`, plus the slice-bound helpers used by the
//! `slice` builtin (which also fold strings).
//!
//! These were `pub(crate)` accessible to vm.rs / compiler.rs only; they
//! remain `pub(crate)` here.

use std::fmt;
use std::sync::Arc;

use compact_str::CompactString;

use super::instruction::{CompiledFormatOneArg, CompiledFormatPart, CompiledFormatTemplate};
use super::*;

pub(crate) async fn stringify_value_async(value: &Value) -> Result<String, RuntimeError> {
    let mut output = String::new();
    append_stringified_value_async(&mut output, value).await?;
    Ok(output)
}

pub(crate) fn stringify_value(value: &Value) -> Result<String, RuntimeError> {
    if value_contains_projected(value) {
        futures_executor::block_on(stringify_value_async(value))
    } else {
        stringify_value_direct(value)
    }
}

pub(crate) fn stringify_value_blocking(value: &Value) -> Result<String, RuntimeError> {
    stringify_value(value)
}

pub(crate) fn stringify_value_direct(value: &Value) -> Result<String, RuntimeError> {
    let mut output = String::new();
    append_stringified_value_direct(&mut output, value)?;
    Ok(output)
}

pub(crate) fn append_stringified_value_direct(
    output: &mut String,
    value: &Value,
) -> Result<(), RuntimeError> {
    match value {
        Value::String(value) => output.push_str(value),
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => {
            write_number(output, *value).expect("string writes should not fail")
        }
        Value::Projected(_) => unreachable!("projected values require async stringification"),
        Value::Tuple(values) => append_tuple_literal_direct(output, values)?,
        Value::Ref(id) => {
            return Err(RuntimeError::UnexportedHeapReference {
                id: id.get(),
                context: "string formatting".into(),
            });
        }
        Value::Image(_) | Value::Resource(_) | Value::List(_) | Value::Record(_) => {
            append_direct_json(output, value)
        }
    }
    Ok(())
}

pub(crate) fn append_stringified_value_async<'a>(
    output: &'a mut String,
    value: &'a Value,
) -> ProjectedFuture<'a, Result<(), RuntimeError>> {
    Box::pin(async move {
        match value {
            Value::String(value) => output.push_str(value),
            Value::Null => output.push_str("null"),
            Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => {
                write_number(output, *value).expect("string writes should not fail")
            }
            Value::Projected(value) => output.push_str(&value.render().await),
            Value::Tuple(values) => append_tuple_literal_async(output, values).await?,
            Value::Ref(id) => {
                return Err(RuntimeError::UnexportedHeapReference {
                    id: id.get(),
                    context: "string formatting".into(),
                });
            }
            Value::Image(_) | Value::Resource(_) | Value::List(_) | Value::Record(_) => {
                append_runtime_json_async(output, value).await;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn append_stringified_value(
    output: &mut String,
    value: &Value,
) -> Result<(), RuntimeError> {
    futures_executor::block_on(append_stringified_value_async(output, value))
}

pub(crate) fn append_tuple_literal_direct(
    output: &mut String,
    values: &ListValue,
) -> Result<(), RuntimeError> {
    output.push('(');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        append_tuple_item_literal_direct(output, value)?;
    }
    if values.len() == 1 {
        output.push(',');
    }
    output.push(')');
    Ok(())
}

fn append_tuple_item_literal_direct(
    output: &mut String,
    value: &Value,
) -> Result<(), RuntimeError> {
    match value {
        Value::String(value) => output.push_str(
            &serde_json::to_string(value).expect("string json serialization should succeed"),
        ),
        Value::Tuple(values) => append_tuple_literal_direct(output, values)?,
        other => append_stringified_value_direct(output, other)?,
    }
    Ok(())
}

pub(crate) fn append_tuple_literal_async<'a>(
    output: &'a mut String,
    values: &'a ListValue,
) -> ProjectedFuture<'a, Result<(), RuntimeError>> {
    Box::pin(async move {
        output.push('(');
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            append_tuple_item_literal_async(output, value).await?;
        }
        if values.len() == 1 {
            output.push(',');
        }
        output.push(')');
        Ok(())
    })
}

fn append_tuple_item_literal_async<'a>(
    output: &'a mut String,
    value: &'a Value,
) -> ProjectedFuture<'a, Result<(), RuntimeError>> {
    Box::pin(async move {
        match value {
            Value::String(value) => output.push_str(
                &serde_json::to_string(value).expect("string json serialization should succeed"),
            ),
            Value::Tuple(values) => append_tuple_literal_async(output, values).await?,
            other => append_stringified_value_async(output, other).await?,
        }
        Ok(())
    })
}

pub(crate) fn write_number(output: &mut impl fmt::Write, value: f64) -> fmt::Result {
    if value.is_finite() && value.fract() == 0.0 {
        let as_i64 = value as i64 as f64;
        if as_i64 == value {
            return write_i64(output, value as i64);
        }
        let as_u64 = value as u64 as f64;
        if as_u64 == value {
            return write_u64(output, value as u64);
        }
    }
    write!(output, "{value}")
}

fn write_i64(output: &mut impl fmt::Write, value: i64) -> fmt::Result {
    if value < 0 {
        output.write_char('-')?;
        return write_u64(output, value.unsigned_abs());
    }
    write_u64(output, value as u64)
}

fn write_u64(output: &mut impl fmt::Write, mut value: u64) -> fmt::Result {
    let mut buffer = [0u8; 20];
    let mut index = buffer.len();
    loop {
        index -= 1;
        buffer[index] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let text = std::str::from_utf8(&buffer[index..]).expect("digits are valid utf-8");
    output.write_str(text)
}

pub(crate) async fn apply_format_async(
    template: &str,
    args: &[Value],
) -> Result<String, RuntimeError> {
    let mut output = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut index = 0;
    let mut last_literal = 0;
    let mut uses_sequential = false;
    let mut uses_indexed = false;
    let mut next_sequential = 0usize;
    let mut used_indexed_bits = 0u64;
    let mut used_indexed_args: Option<Vec<bool>> = None;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => {
                output.push_str(&template[last_literal..index]);
                output.push('{');
                index += 2;
                last_literal = index;
                continue;
            }
            b'{' => {
                output.push_str(&template[last_literal..index]);
                let mut cursor = index + 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }

                let (slot, slot_text) = if cursor >= bytes.len() {
                    return Err(RuntimeError::Format(FormatError::UnmatchedOpenBrace));
                } else if bytes[cursor] != b'}' {
                    return Err(RuntimeError::Format(FormatError::InvalidPlaceholder));
                } else if cursor == index + 1 {
                    if uses_indexed {
                        return Err(RuntimeError::Format(FormatError::MixedPlaceholderKinds));
                    }
                    uses_sequential = true;
                    let slot = next_sequential;
                    next_sequential += 1;
                    (slot, None)
                } else {
                    if uses_sequential {
                        return Err(RuntimeError::Format(FormatError::MixedPlaceholderKinds));
                    }
                    uses_indexed = true;
                    let digits = &template[index + 1..cursor];
                    let slot = digits.parse::<usize>().map_err(|_| {
                        RuntimeError::Format(FormatError::InvalidSlot {
                            slot: digits.to_string(),
                        })
                    })?;
                    if slot < args.len() {
                        if args.len() <= u64::BITS as usize {
                            used_indexed_bits |= 1u64 << slot;
                        } else {
                            let used_args =
                                used_indexed_args.get_or_insert_with(|| vec![false; args.len()]);
                            used_args[slot] = true;
                        }
                    }
                    (slot, Some(digits))
                };

                let value = args.get(slot).ok_or_else(|| {
                    RuntimeError::Format(FormatError::SlotOutOfRange {
                        slot: slot_text.unwrap_or("{}").to_string(),
                    })
                })?;
                append_stringified_value_async(&mut output, value).await?;
                index = cursor + 1;
                last_literal = index;
                continue;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => {
                output.push_str(&template[last_literal..index]);
                output.push('}');
                index += 2;
                last_literal = index;
                continue;
            }
            b'}' => {
                return Err(RuntimeError::Format(FormatError::UnmatchedCloseBrace));
            }
            _ => {}
        }
        index += 1;
    }
    output.push_str(&template[last_literal..]);
    let unused_index = if uses_sequential {
        (next_sequential < args.len()).then_some(next_sequential)
    } else if uses_indexed {
        if args.len() <= u64::BITS as usize {
            (0..args.len()).find(|slot| used_indexed_bits & (1u64 << slot) == 0)
        } else {
            used_indexed_args
                .as_ref()
                .and_then(|used_args| used_args.iter().position(|used| !*used))
        }
    } else {
        (!args.is_empty()).then_some(0)
    };
    if let Some(unused_index) = unused_index {
        return Err(RuntimeError::Format(FormatError::UnusedArgument {
            index: unused_index,
        }));
    }
    Ok(output)
}

#[cfg(test)]
pub(crate) fn apply_format(template: &str, args: &[Value]) -> Result<String, RuntimeError> {
    futures_executor::block_on(apply_format_async(template, args))
}

pub(crate) fn compile_format_template(template: &str, argc: usize) -> CompiledFormatTemplate {
    match parse_format_template(template, argc) {
        Ok(parts) => {
            let one_arg = compiled_format_one_arg(&parts, argc);
            CompiledFormatTemplate {
                parts: parts.into_boxed_slice(),
                argc,
                min_capacity: template.len(),
                error: None,
                one_arg,
            }
        }
        Err(message) => CompiledFormatTemplate {
            parts: Box::new([]),
            argc,
            min_capacity: template.len(),
            error: Some(message),
            one_arg: None,
        },
    }
}

fn compiled_format_one_arg(
    parts: &[CompiledFormatPart],
    argc: usize,
) -> Option<CompiledFormatOneArg> {
    if argc != 1 {
        return None;
    }
    match parts {
        [CompiledFormatPart::Arg(0)] => Some(CompiledFormatOneArg {
            prefix: None,
            suffix: None,
        }),
        [
            CompiledFormatPart::Literal(prefix),
            CompiledFormatPart::Arg(0),
        ] => Some(CompiledFormatOneArg {
            prefix: Some(prefix.clone()),
            suffix: None,
        }),
        [
            CompiledFormatPart::Arg(0),
            CompiledFormatPart::Literal(suffix),
        ] => Some(CompiledFormatOneArg {
            prefix: None,
            suffix: Some(suffix.clone()),
        }),
        [
            CompiledFormatPart::Literal(prefix),
            CompiledFormatPart::Arg(0),
            CompiledFormatPart::Literal(suffix),
        ] => Some(CompiledFormatOneArg {
            prefix: Some(prefix.clone()),
            suffix: Some(suffix.clone()),
        }),
        _ => None,
    }
}

pub(crate) fn parse_format_template(
    template: &str,
    argc: usize,
) -> Result<Vec<CompiledFormatPart>, FormatError> {
    let mut parts = Vec::new();
    let bytes = template.as_bytes();
    let mut index = 0;
    let mut last_literal = 0;
    let mut uses_sequential = false;
    let mut uses_indexed = false;
    let mut next_sequential = 0usize;
    let mut used_indexed_bits = 0u64;
    let mut used_indexed_args: Option<Vec<bool>> = None;

    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => {
                push_format_literal(&mut parts, &template[last_literal..index]);
                push_format_literal(&mut parts, "{");
                index += 2;
                last_literal = index;
                continue;
            }
            b'{' => {
                push_format_literal(&mut parts, &template[last_literal..index]);
                let mut cursor = index + 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }

                let (slot, slot_text) = if cursor >= bytes.len() {
                    return Err(FormatError::UnmatchedOpenBrace);
                } else if bytes[cursor] != b'}' {
                    return Err(FormatError::InvalidPlaceholder);
                } else if cursor == index + 1 {
                    if uses_indexed {
                        return Err(FormatError::MixedPlaceholderKinds);
                    }
                    uses_sequential = true;
                    let slot = next_sequential;
                    next_sequential += 1;
                    (slot, None)
                } else {
                    if uses_sequential {
                        return Err(FormatError::MixedPlaceholderKinds);
                    }
                    uses_indexed = true;
                    let digits = &template[index + 1..cursor];
                    let slot = digits
                        .parse::<usize>()
                        .map_err(|_| FormatError::InvalidSlot {
                            slot: digits.to_string(),
                        })?;
                    if slot < argc {
                        if argc <= u64::BITS as usize {
                            used_indexed_bits |= 1u64 << slot;
                        } else {
                            let used_args =
                                used_indexed_args.get_or_insert_with(|| vec![false; argc]);
                            used_args[slot] = true;
                        }
                    }
                    (slot, Some(digits))
                };

                if slot >= argc {
                    return Err(FormatError::SlotOutOfRange {
                        slot: slot_text.unwrap_or("{}").to_string(),
                    });
                }
                parts.push(CompiledFormatPart::Arg(slot));
                index = cursor + 1;
                last_literal = index;
                continue;
            }
            b'}' if bytes.get(index + 1) == Some(&b'}') => {
                push_format_literal(&mut parts, &template[last_literal..index]);
                push_format_literal(&mut parts, "}");
                index += 2;
                last_literal = index;
                continue;
            }
            b'}' => {
                return Err(FormatError::UnmatchedCloseBrace);
            }
            _ => {}
        }
        index += 1;
    }

    push_format_literal(&mut parts, &template[last_literal..]);
    let unused_index = if uses_sequential {
        (next_sequential < argc).then_some(next_sequential)
    } else if uses_indexed {
        if argc <= u64::BITS as usize {
            (0..argc).find(|slot| used_indexed_bits & (1u64 << slot) == 0)
        } else {
            used_indexed_args
                .as_ref()
                .and_then(|used_args| used_args.iter().position(|used| !*used))
        }
    } else {
        (argc > 0).then_some(0)
    };
    if let Some(unused_index) = unused_index {
        return Err(FormatError::UnusedArgument {
            index: unused_index,
        });
    }
    Ok(parts)
}

pub(crate) fn push_format_literal(parts: &mut Vec<CompiledFormatPart>, literal: &str) {
    if !literal.is_empty() {
        parts.push(CompiledFormatPart::Literal(Arc::<str>::from(literal)));
    }
}

pub(crate) async fn execute_compiled_format(
    template: &CompiledFormatTemplate,
    args: &[Value],
) -> Result<String, RuntimeError> {
    if let Some(error) = &template.error {
        return Err(RuntimeError::Format(error.clone()));
    }
    if let Some(shape) = &template.one_arg {
        let value = args.first().ok_or_else(|| {
            RuntimeError::Format(FormatError::SlotOutOfRange {
                slot: "0".to_string(),
            })
        })?;
        let mut output = String::with_capacity(template.min_capacity);
        push_compiled_one_arg_prefix(&mut output, shape);
        append_stringified_value_async(&mut output, value).await?;
        push_compiled_one_arg_suffix(&mut output, shape);
        return Ok(output);
    }

    let mut output = String::with_capacity(template.min_capacity);
    for part in template.parts.iter() {
        match part {
            CompiledFormatPart::Literal(literal) => output.push_str(literal),
            CompiledFormatPart::Arg(slot) => {
                let value = args.get(*slot).ok_or_else(|| {
                    RuntimeError::Format(FormatError::SlotOutOfRange {
                        slot: slot.to_string(),
                    })
                })?;
                append_stringified_value_async(&mut output, value).await?;
            }
        }
    }
    Ok(output)
}

pub(crate) fn execute_compiled_format_direct(
    template: &CompiledFormatTemplate,
    args: &[Value],
) -> Result<String, RuntimeError> {
    if let Some(error) = &template.error {
        return Err(RuntimeError::Format(error.clone()));
    }
    if let Some(shape) = &template.one_arg {
        let value = args.first().ok_or_else(|| {
            RuntimeError::Format(FormatError::SlotOutOfRange {
                slot: "0".to_string(),
            })
        })?;
        let mut output = String::with_capacity(template.min_capacity);
        push_compiled_one_arg_prefix(&mut output, shape);
        append_stringified_value_direct(&mut output, value)?;
        push_compiled_one_arg_suffix(&mut output, shape);
        return Ok(output);
    }

    let mut output = String::with_capacity(template.min_capacity);
    for part in template.parts.iter() {
        match part {
            CompiledFormatPart::Literal(literal) => output.push_str(literal),
            CompiledFormatPart::Arg(slot) => {
                let value = args.get(*slot).ok_or_else(|| {
                    RuntimeError::Format(FormatError::SlotOutOfRange {
                        slot: slot.to_string(),
                    })
                })?;
                append_stringified_value_direct(&mut output, value)?;
            }
        }
    }
    Ok(output)
}

pub(crate) fn execute_compiled_format_one_number_compact_direct(
    template: &CompiledFormatTemplate,
    value: f64,
) -> Result<CompactString, RuntimeError> {
    if let Some(error) = &template.error {
        return Err(RuntimeError::Format(error.clone()));
    }
    if let Some(shape) = &template.one_arg {
        let mut output = CompactString::with_capacity(template.min_capacity);
        push_compiled_one_arg_prefix(&mut output, shape);
        write_number(&mut output, value).expect("string writes should not fail");
        push_compiled_one_arg_suffix(&mut output, shape);
        return Ok(output);
    }

    let mut output = CompactString::with_capacity(template.min_capacity);
    for part in template.parts.iter() {
        match part {
            CompiledFormatPart::Literal(literal) => output.push_str(literal),
            CompiledFormatPart::Arg(0) => {
                write_number(&mut output, value).expect("string writes should not fail");
            }
            CompiledFormatPart::Arg(slot) => {
                return Err(RuntimeError::Format(FormatError::SlotOutOfRange {
                    slot: slot.to_string(),
                }));
            }
        }
    }
    Ok(output)
}

fn push_compiled_one_arg_prefix(output: &mut impl fmt::Write, shape: &CompiledFormatOneArg) {
    if let Some(prefix) = &shape.prefix {
        output
            .write_str(prefix)
            .expect("string writes should not fail");
    }
}

fn push_compiled_one_arg_suffix(output: &mut impl fmt::Write, shape: &CompiledFormatOneArg) {
    if let Some(suffix) = &shape.suffix {
        output
            .write_str(suffix)
            .expect("string writes should not fail");
    }
}

pub(crate) fn clamp_slice_bounds(
    start: Option<isize>,
    end: Option<isize>,
    len: usize,
) -> Option<(usize, usize)> {
    let len = len as isize;
    let start = normalize_slice_bound(start.unwrap_or(0), len);
    let end = normalize_slice_bound(end.unwrap_or(len), len);
    (start < end).then_some((start, end))
}

pub(crate) fn normalize_slice_bound(bound: isize, len: isize) -> usize {
    let normalized = if bound < 0 { len + bound } else { bound };
    normalized.clamp(0, len) as usize
}

pub(crate) fn slice_string(value: &str, start: Option<isize>, end: Option<isize>) -> String {
    let char_count = value.chars().count();
    let Some((start, end)) = clamp_slice_bounds(start, end, char_count) else {
        return String::new();
    };

    let mut indices = value
        .char_indices()
        .map(|(idx, _)| idx)
        .chain(std::iter::once(value.len()));
    let Some(byte_start) = indices.nth(start) else {
        return String::new();
    };
    let Some(byte_end) = indices.nth(end - start - 1) else {
        return value[byte_start..].to_string();
    };
    value[byte_start..byte_end].to_string()
}
