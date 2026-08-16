use super::super::{
    ensure_javascript_string_size, javascript_string_size_error, javascript_to_string,
};
use super::*;

pub(super) fn js_stdlib_error(reason: impl Into<String>) -> RuntimeError {
    RuntimeError::ValidationFailed {
        reason: reason.into(),
    }
}

pub(super) fn normalized_static_arguments(method: &str, args: &[Value]) -> Vec<Value> {
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
        | "Math.acosh"
        | "Math.asin"
        | "Math.asinh"
        | "Math.atan"
        | "Math.atanh"
        | "Math.cbrt"
        | "Math.ceil"
        | "Math.clz32"
        | "Math.cos"
        | "Math.cosh"
        | "Math.exp"
        | "Math.expm1"
        | "Math.floor"
        | "Math.fround"
        | "Math.log"
        | "Math.log1p"
        | "Math.log10"
        | "Math.log2"
        | "Math.round"
        | "Math.sin"
        | "Math.sinh"
        | "Math.sqrt"
        | "Math.tan"
        | "Math.tanh"
        | "Math.trunc"
        | "Math.sign" => 1,
        "Object.hasOwn" | "Object.is" | "Number.parseInt" | "Math.atan2" | "Math.imul"
        | "Math.pow" => 2,
        _ => return args.to_vec(),
    };
    normalized_arguments(args, arity)
}

pub(super) fn normalized_instance_arguments(method: &str, args: &[Value]) -> Vec<Value> {
    let arity = match method {
        "at" | "charAt" | "charCodeAt" | "codePointAt" | "flat" | "repeat" | "join" | "sort"
        | "toExponential" | "toFixed" | "toPrecision" | "toSorted" => 1,
        "endsWith" | "includes" | "indexOf" | "lastIndexOf" | "padEnd" | "padStart" | "replace"
        | "replaceAll" | "startsWith" => 2,
        "with" => 2,
        "fill" => 3,
        "slice" | "substring" => 2,
        "reverse" | "toReversed" | "toLowerCase" | "toUpperCase" | "trim" | "trimStart"
        | "trimEnd" | "toString" | "valueOf" => 0,
        _ => return args.to_vec(),
    };
    normalized_arguments(args, arity)
}

pub(super) fn normalized_arguments(args: &[Value], arity: usize) -> Vec<Value> {
    let mut normalized = args[..args.len().min(arity)].to_vec();
    normalized.resize(arity, Value::Undefined);
    normalized
}

pub(super) fn ecma_record_entries(record: &Record) -> Vec<(&str, &Value)> {
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

pub(super) fn array_index_property(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 1 && key.starts_with('0') {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

/// `replaceAll`: every occurrence, with the same `$`-token expansion `replace`
/// applies to the one occurrence it touches. Each match expands against its own
/// prefix and suffix, so `` $` `` and `$'` mean what they mean at that match.
pub(super) fn replace_all_string(
    value: &str,
    needle: &str,
    replacement: &str,
) -> Result<String, RuntimeError> {
    if needle.is_empty() {
        // An empty search matches at every position *and* once past the last
        // character, so `"abc".replaceAll("", "-")` is `-a-b-c-` and
        // `"".replaceAll("", "-")` is `-`. Only `replace` — the single-match
        // path — stops after the first, which is why this cannot delegate to
        // it. Each match is empty, so the tokens around it see the whole string
        // split at that position.
        let mut output = String::new();
        let mut index = 0;
        loop {
            expand_replacement_tokens(
                &mut output,
                replacement,
                needle,
                &value[..index],
                &value[index..],
            )?;
            let Some(matched) = value[index..].chars().next() else {
                return Ok(output);
            };
            if matched.len_utf16() != 1 {
                // ECMA matches between the two code units of a surrogate pair,
                // so node's answer here contains lone surrogates. Expanding per
                // Unicode scalar instead would quietly give a different string;
                // `split('')` refuses this same shape for this same reason.
                return Err(js_stdlib_error(
                    "TS_LONE_SURROGATE_UNSUPPORTED: replaceAll('') would create unrepresentable lone surrogates",
                ));
            }
            ensure_javascript_string_size(output.len() + matched.len_utf8())?;
            output.push(matched);
            index += matched.len_utf8();
        }
    }
    let mut output = String::new();
    let mut searched = 0;
    while let Some(offset) = value[searched..].find(needle) {
        let start = searched + offset;
        let end = start + needle.len();
        ensure_javascript_string_size(output.len() + (start - searched))?;
        output.push_str(&value[searched..start]);
        // `$\`` and `$'` mean the text either side of *this* match in the whole
        // string, not in the slice being scanned.
        expand_replacement_tokens(
            &mut output,
            replacement,
            needle,
            &value[..start],
            &value[end..],
        )?;
        searched = end;
    }
    ensure_javascript_string_size(output.len() + (value.len() - searched))?;
    output.push_str(&value[searched..]);
    Ok(output)
}

/// Append `replacement` to `output`, expanding the `$`-tokens ECMA defines for
/// a match of `needle` sitting between `prefix` and `suffix`.
pub(super) fn expand_replacement_tokens(
    output: &mut String,
    replacement: &str,
    needle: &str,
    prefix: &str,
    suffix: &str,
) -> Result<(), RuntimeError> {
    let mut chars = replacement.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            output.push(character);
        } else {
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
        ensure_javascript_string_size(output.len())?;
    }
    Ok(())
}

pub(super) fn replace_string(
    value: &str,
    needle: &str,
    replacement: &str,
) -> Result<String, RuntimeError> {
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

pub(super) fn relative_index(value: f64, len: usize) -> Option<usize> {
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

pub(super) fn relative_nonnegative_index(value: f64, len: usize) -> Option<usize> {
    let value = if value.is_nan() {
        0
    } else {
        value.trunc() as isize
    };
    (value >= 0 && value < len as isize).then_some(value as usize)
}

pub(super) fn clamp_relative_index(value: f64, len: usize) -> usize {
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

pub(super) fn clamp_nonnegative_index(value: f64, len: usize) -> usize {
    if value.is_nan() || value <= 0.0 {
        0
    } else {
        (value.trunc() as usize).min(len)
    }
}

pub(super) fn string_starts_with(
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

pub(super) fn string_ends_with(
    units: &[u16],
    needle: &Value,
    end: usize,
) -> Result<Value, RuntimeError> {
    let needle = javascript_to_string(needle)
        .encode_utf16()
        .collect::<Vec<_>>();
    let start = end.saturating_sub(needle.len());
    Ok(Value::Bool(
        needle.len() <= end && units.get(start..end) == Some(needle.as_slice()),
    ))
}

pub(super) fn string_includes(
    units: &[u16],
    needle: &Value,
    position: usize,
) -> Result<Value, RuntimeError> {
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

pub(super) fn string_index_of(
    units: &[u16],
    needle: &Value,
    position: usize,
) -> Result<Value, RuntimeError> {
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

pub(super) fn string_last_index_of(
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

pub(super) fn array_includes(
    items: &[Value],
    needle: &Value,
    start: usize,
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_strict_equal;
    Ok(Value::Bool(items.get(start..).is_some_and(|tail| {
        tail.iter().any(|item| {
            javascript_strict_equal(item, needle)
                || matches!((item, needle), (Value::Number(left), Value::Number(right)) if left.is_nan() && right.is_nan())
        })
    })))
}

pub(super) fn array_index_of(
    items: &[Value],
    needle: &Value,
    start: usize,
) -> Result<Value, RuntimeError> {
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

pub(super) fn array_last_index_of(
    items: &[Value],
    needle: &Value,
    end: usize,
) -> Result<Value, RuntimeError> {
    use crate::runtime::javascript::javascript_strict_equal;
    Ok(Value::Number(
        items[..end.min(items.len())]
            .iter()
            .rposition(|item| javascript_strict_equal(item, needle))
            .map_or(-1.0, |index| index as f64),
    ))
}

pub(super) fn last_index_exclusive(value: f64, len: usize) -> Option<usize> {
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

pub(super) fn to_uint16(value: f64) -> u16 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    value.trunc().rem_euclid(65_536.0) as u16
}

pub(super) fn javascript_round(value: f64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    if (-0.5..0.0).contains(&value) {
        return -0.0;
    }
    (value + 0.5).floor()
}

pub(super) fn javascript_pow(base: f64, exponent: f64) -> f64 {
    if base.abs() == 1.0 && exponent.is_infinite() {
        f64::NAN
    } else {
        base.powf(exponent)
    }
}

pub(super) fn javascript_extreme(values: &[Value], maximum: bool) -> f64 {
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

pub(super) fn utf16_value(units: Vec<u16>) -> Result<Value, RuntimeError> {
    String::from_utf16(&units)
        .map(|value| Value::String(value.into()))
        .map_err(|_| js_stdlib_error("TS_LONE_SURROGATE_UNSUPPORTED: result is not representable"))
}

pub(super) fn code_point_at(units: &[u16], index: usize) -> Result<Value, RuntimeError> {
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

pub(super) fn slice_utf16(
    units: &[u16],
    bounds: &[Value],
    relative: bool,
) -> Result<Value, RuntimeError> {
    let to_number = crate::runtime::javascript::javascript_to_number;
    let start_value = match bounds.first() {
        None | Some(Value::Undefined) => 0.0,
        Some(value) => to_number(value),
    };
    // Absent and explicitly `undefined` are the same thing here: end of input.
    let end_value = match bounds.get(1) {
        None | Some(Value::Undefined) => units.len() as f64,
        Some(value) => to_number(value),
    };
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

pub(super) fn substring_utf16(units: &[u16], bounds: &[Value]) -> Result<Value, RuntimeError> {
    let to_number = crate::runtime::javascript::javascript_to_number;
    // Absent and explicitly `undefined` both mean end-of-input; coercing the
    // padded `Undefined` gives NaN, which clamps to zero and silently swaps the
    // bounds below.
    let mut start = match bounds.first() {
        None | Some(Value::Undefined) => 0.0,
        Some(value) => to_number(value),
    }
    .max(0.0) as usize;
    let mut end = match bounds.get(1) {
        None | Some(Value::Undefined) => units.len() as f64,
        Some(value) => to_number(value),
    }
    .max(0.0) as usize;
    start = start.min(units.len());
    end = end.min(units.len());
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    utf16_value(units[start..end].to_vec())
}

pub(super) fn pad_string(
    value: &str,
    length: f64,
    fill: &str,
    start: bool,
) -> Result<Value, RuntimeError> {
    let current = value.encode_utf16().count();
    let length = if length.is_nan() {
        0
    } else {
        length.max(0.0).trunc() as usize
    };
    if length <= current || fill.is_empty() {
        return Ok(Value::String(value.into()));
    }
    // Size before allocating, as `repeat` and `concat` do. `'a'.padStart(1e15)`
    // is a memory-limit rejection, never a host allocation abort.
    let added = length - current;
    ensure_javascript_string_size(
        added
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(|| javascript_string_size_error(usize::MAX))?,
    )?;
    let fill_units = fill.encode_utf16().collect::<Vec<_>>();
    let padding = (0..added)
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

pub(super) fn parse_float_prefix(value: &str) -> f64 {
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

pub(super) fn parse_int_prefix(value: &str, radix: Option<f64>) -> f64 {
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

pub(super) fn to_int32(value: f64) -> i64 {
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
