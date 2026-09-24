use super::super::{
    ErrorKind, ensure_javascript_string_size, javascript_string_size_error, javascript_to_string,
};
use super::*;

pub(super) fn js_stdlib_error(reason: impl Into<String>) -> RuntimeError {
    RuntimeError::ValidationFailed {
        reason: reason.into(),
    }
}

/// Calls whose dispatch arms are written against a fixed-length argument
/// vector: the normalizer truncates extras and pads omissions with
/// `Value::Undefined` before the match runs. Membership is dispatch policy,
/// not arity — a call whose ECMA semantics distinguish an omitted argument
/// from an explicit `undefined` (the `Date.UTC` defaults, the `split` limit)
/// must keep seeing the raw vector, and variadic rows can never be
/// fixed-length. How many slots each member pads to is looked up from the
/// signature row in [`crate::ecma_stdlib`], so the arity itself is stated
/// once, in the prose that advertises the call.
const FIXED_LENGTH_STATIC_METHODS: &[&str] = &[
    "Object.keys",
    "Object.values",
    "Object.entries",
    "Object.fromEntries",
    "Array.isArray",
    "Number.isFinite",
    "Number.isInteger",
    "Number.isNaN",
    "Number.isSafeInteger",
    "Number.parseFloat",
    "Math.abs",
    "Math.acos",
    "Math.acosh",
    "Math.asin",
    "Math.asinh",
    "Math.atan",
    "Math.atanh",
    "Math.cbrt",
    "Math.ceil",
    "Math.clz32",
    "Math.cos",
    "Math.cosh",
    "Math.exp",
    "Math.expm1",
    "Math.floor",
    "Math.fround",
    "Math.log",
    "Math.log1p",
    "Math.log10",
    "Math.log2",
    "Math.round",
    "Math.sin",
    "Math.sinh",
    "Math.sqrt",
    "Math.tan",
    "Math.tanh",
    "Math.trunc",
    "Math.sign",
    "Object.hasOwn",
    "Object.is",
    "Number.parseInt",
    "Math.atan2",
    "Math.imul",
    "Math.pow",
];

const FIXED_LENGTH_INSTANCE_METHODS: &[&str] = &[
    "at",
    "charAt",
    "charCodeAt",
    "codePointAt",
    "flat",
    "repeat",
    "join",
    "sort",
    "toExponential",
    "toFixed",
    "toPrecision",
    "toSorted",
    "endsWith",
    "includes",
    "indexOf",
    "lastIndexOf",
    "padEnd",
    "padStart",
    "replace",
    "replaceAll",
    "startsWith",
    "with",
    "fill",
    "slice",
    "substring",
    "reverse",
    "toReversed",
    "toLowerCase",
    "toUpperCase",
    "trim",
    "trimStart",
    "trimEnd",
    "toString",
    "valueOf",
    "pop",
    "shift",
];

// A fixed-length method whose signature row is missing or variadic would
// fall back to raw arguments and silently re-arm the shorter dispatcher
// patterns this ticket removed — check the whole policy at compile time.
const _: () = {
    let mut i = 0;
    while i < FIXED_LENGTH_STATIC_METHODS.len() {
        assert!(
            crate::ecma_stdlib::static_method_arity(FIXED_LENGTH_STATIC_METHODS[i]).is_some(),
            "fixed-length static method lacks a fixed-arity signature row"
        );
        i += 1;
    }
    let mut i = 0;
    while i < FIXED_LENGTH_INSTANCE_METHODS.len() {
        assert!(
            crate::ecma_stdlib::instance_method_arity(FIXED_LENGTH_INSTANCE_METHODS[i]).is_some(),
            "fixed-length instance method lacks a fixed-arity signature row"
        );
        i += 1;
    }
};

/// Each fixed-length call's padded arity, sorted by name: the policy list
/// joined with its signature rows once, so a dispatch finds its call by
/// binary search instead of scanning the policy list and then every
/// signature row on every call (FIG-3730).
fn fixed_length_arities(
    table: &'static std::sync::OnceLock<Vec<(&'static str, usize)>>,
    members: &[&'static str],
    arity: fn(&str) -> Option<usize>,
) -> &'static [(&'static str, usize)] {
    table.get_or_init(|| {
        let mut arities = members
            .iter()
            .filter_map(|&method| Some((method, arity(method)?)))
            .collect::<Vec<_>>();
        arities.sort_unstable_by_key(|&(method, _)| method);
        arities
    })
}

fn fixed_length_arity(arities: &[(&'static str, usize)], method: &str) -> Option<usize> {
    arities
        .binary_search_by_key(&method, |&(name, _)| name)
        .ok()
        .map(|index| arities[index].1)
}

pub(super) fn normalized_static_arguments(method: &str, args: &[Value]) -> Vec<Value> {
    static ARITIES: std::sync::OnceLock<Vec<(&'static str, usize)>> = std::sync::OnceLock::new();
    let arities = fixed_length_arities(
        &ARITIES,
        FIXED_LENGTH_STATIC_METHODS,
        crate::ecma_stdlib::static_method_arity,
    );
    match fixed_length_arity(arities, method) {
        Some(arity) => normalized_arguments(args, arity),
        None => args.to_vec(),
    }
}

pub(super) fn normalized_instance_arguments(method: &str, args: &[Value]) -> Vec<Value> {
    static ARITIES: std::sync::OnceLock<Vec<(&'static str, usize)>> = std::sync::OnceLock::new();
    let arities = fixed_length_arities(
        &ARITIES,
        FIXED_LENGTH_INSTANCE_METHODS,
        crate::ecma_stdlib::instance_method_arity,
    );
    match fixed_length_arity(arities, method) {
        Some(arity) => normalized_arguments(args, arity),
        None => args.to_vec(),
    }
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
    if !value.is_finite() || value == 0.0 || value.trunc() == value {
        return value;
    }
    if (-0.5..0.5).contains(&value) {
        return if value.is_sign_negative() { -0.0 } else { 0.0 };
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

impl<H: ExecutionHost> Vm<'_, H> {
    /// The ECMA-262 guards a stdlib call answers before anything is
    /// exported: IsCallable, which a lowering asks for by name and which
    /// passes the callable through with its identity, and IsRegExp on a
    /// string search. Whether it answered the call.
    pub(super) fn execute_ecma_guard(&mut self, values: &[Value]) -> Result<bool, RuntimeError> {
        match values {
            [Value::String(method), value] if method.as_str() == "Lash.RequireCallable" => {
                let callable = matches!(value, Value::Ref(id)
                    if self.heap.get(*id)?.is_function());
                if !callable {
                    return Err(RuntimeError::type_error(format!(
                        "{} is not a function",
                        non_callable_text(value)
                    )));
                }
                self.stack.push(value.clone());
                Ok(true)
            }
            // A string search that takes a substring refuses a RegExp outright
            // rather than reading it as text.
            [
                Value::String(method),
                Value::String(_),
                Value::Ref(search),
                ..,
            ] if matches!(method.as_str(), "startsWith" | "endsWith" | "includes")
                && matches!(self.heap.get(*search)?, HeapObject::RegExp(_)) =>
            {
                Err(RuntimeError::type_error(format!(
                    "First argument to String.prototype.{method} must not be a regular expression"
                )))
            }
            _ => Ok(false),
        }
    }

    /// `String.raw(template, ...substitutions)`, ECMA-262 22.1.2.4: the cooked
    /// template is never read; the raw segments come from `template.raw`,
    /// counted by its `length` after `ToLength`, and a substitution joins the
    /// output only when a later raw segment follows it.
    pub(super) fn javascript_string_raw(&mut self, args: &[Value]) -> Result<Value, RuntimeError> {
        let template = args.first().cloned().unwrap_or(Value::Undefined);
        if matches!(template, Value::Null | Value::Undefined) {
            return Err(self.javascript_type_error("Cannot convert undefined or null to object"));
        }
        let raw = self.read_dialect_index(template, Value::String("raw".into()))?;
        if matches!(raw, Value::Null | Value::Undefined) {
            return Err(self.javascript_type_error("Cannot convert undefined or null to object"));
        }
        // `ToObject(raw)` before the length read: a sequence or string raw is
        // an object with an indexed length, which the value model answers
        // without materialising a wrapper.
        let length = match &raw {
            Value::List(items) | Value::Tuple(items) => items.len() as f64,
            Value::String(value) => value.encode_utf16().count() as f64,
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(items) | HeapObject::Tuple(items) => items.len() as f64,
                HeapObject::RegExpMatch(result) => result.items.len() as f64,
                _ => {
                    let value =
                        self.read_dialect_index(raw.clone(), Value::String("length".into()))?;
                    self.heap.javascript_to_number(&value)?
                }
            },
            _ => {
                let value = self.read_dialect_index(raw.clone(), Value::String("length".into()))?;
                self.heap.javascript_to_number(&value)?
            }
        };
        // ToLength: ToIntegerOrInfinity clamped to [0, 2^53 - 1].
        let segments = if length.is_nan() || length <= 0.0 {
            0
        } else {
            (length.trunc() as u64).min(9_007_199_254_740_991)
        };
        let substitutions = args.get(1..).unwrap_or(&[]);
        let mut output = String::new();
        for index in 0..segments {
            let key = Value::String(index.to_string().into());
            let literal = self.read_dialect_index(raw.clone(), key)?;
            output.push_str(&self.heap.javascript_to_string(&literal)?);
            ensure_javascript_string_size(output.len())?;
            if index + 1 < segments && (index as usize) < substitutions.len() {
                output.push_str(
                    &self
                        .heap
                        .javascript_to_string(&substitutions[index as usize])?,
                );
                ensure_javascript_string_size(output.len())?;
            }
        }
        Ok(Value::String(output.into()))
    }

    /// A guest-visible `TypeError`, thrown the way `assert.throws` expects:
    /// as an uncaught exception carrying an error object, not a VM fault.
    fn javascript_type_error(&mut self, message: &str) -> RuntimeError {
        match self
            .heap
            .allocate_error(ErrorKind::TypeError, Some(message.to_string()), None, None)
        {
            Ok(value) => RuntimeError::UncaughtException { value },
            Err(error) => error,
        }
    }

    /// `IsCallable`, ECMA-262 7.2.3: in the value model a heap closure or a
    /// built-in method read as a value is callable.
    fn javascript_is_callable(&self, value: &Value) -> Result<bool, RuntimeError> {
        Ok(match value {
            Value::Ref(id) => self.heap.get(*id)?.is_function(),
            _ => false,
        })
    }

    /// GetSetRecord, ECMA-262 24.2.1.2: a `Set` or `Map` argument answers
    /// `size`, `has` and `keys` natively; every other value is validated in
    /// ECMA order — numeric `size`, callable `has`, callable `keys` — so an
    /// invalid argument throws the TypeError the tests expect, while a
    /// *valid* set-like object stays a refusal, because its `has`/`keys` are
    /// guest closures a synchronous builtin cannot invoke.
    fn javascript_set_like(&mut self, other: &Value) -> Result<(SetLike, f64), RuntimeError> {
        if let Value::Ref(id) = other {
            match self.heap.get(*id)? {
                HeapObject::Set(set) => {
                    return Ok((SetLike::Set(*id), set.values.len() as f64));
                }
                HeapObject::Map(map) => {
                    return Ok((SetLike::Map(*id), map.entries.len() as f64));
                }
                _ => {}
            }
        }
        if matches!(other, Value::Null | Value::Undefined) {
            return Err(self.javascript_type_error(&format!(
                "Cannot read properties of {} (reading 'size')",
                if matches!(other, Value::Null) {
                    "null"
                } else {
                    "undefined"
                },
            )));
        }
        let size = self.read_dialect_index(other.clone(), Value::String("size".into()))?;
        let size = self.heap.javascript_to_number(&size)?;
        if size.is_nan() {
            return Err(self.javascript_type_error("size property is not a number"));
        }
        let has = self.read_dialect_index(other.clone(), Value::String("has".into()))?;
        if !self.javascript_is_callable(&has)? {
            return Err(self.javascript_type_error("has property is not callable"));
        }
        let keys = self.read_dialect_index(other.clone(), Value::String("keys".into()))?;
        if !self.javascript_is_callable(&keys)? {
            return Err(self.javascript_type_error("keys property is not callable"));
        }
        // ToIntegerOrInfinity: the declared size only chooses which side of the
        // comparison methods iterates.
        let size = if size.is_finite() { size.trunc() } else { size };
        Ok((SetLike::Guest, size))
    }

    fn javascript_set_like_has(
        &self,
        like: &SetLike,
        method: &str,
        value: &Value,
    ) -> Result<bool, RuntimeError> {
        match like {
            SetLike::Set(id) => self.heap.set_has(*id, value),
            SetLike::Map(id) => self.heap.map_has(*id, value),
            SetLike::Guest => Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: Set.{method} would invoke the argument's has/keys callbacks; pass a Set or a Map"
            ))),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "a SetLike::Set/Map is only constructed after the heap kind check, per each message"
    )]
    fn javascript_set_like_keys(
        &self,
        like: &SetLike,
        method: &str,
    ) -> Result<Vec<Value>, RuntimeError> {
        match like {
            SetLike::Set(id) => Ok(self.heap.set_values(*id)?.expect("Set kind was checked")),
            SetLike::Map(id) => Ok(self
                .heap
                .map_entries(*id)?
                .expect("Map kind was checked")
                .into_iter()
                .map(|(key, _)| key)
                .collect()),
            SetLike::Guest => Err(js_stdlib_error(format!(
                "TS_METHOD_UNSUPPORTED: Set.{method} would invoke the argument's has/keys callbacks; pass a Set or a Map"
            ))),
        }
    }

    /// The seven `Set.prototype` combinational methods. The spec iterates
    /// `this` and calls the argument's `has` when `this.size <= other.size`,
    /// and iterates the argument's `keys()` otherwise — which fixes the result
    /// order for `intersection` — so both directions are implemented against
    /// the same set-like record.
    #[expect(
        clippy::expect_used,
        reason = "the receiver kind was checked by the dispatch arm, per the message"
    )]
    pub(super) fn execute_javascript_set_method(
        &mut self,
        method: &str,
        receiver: HeapId,
        other: &Value,
    ) -> Result<Value, RuntimeError> {
        let left = self
            .heap
            .set_values(receiver)?
            .expect("Set receiver was checked");
        let (like, size) = self.javascript_set_like(other)?;
        let contains = |values: &[Value], value: &Value| {
            values
                .iter()
                .any(|candidate| same_value_zero(candidate, value))
        };
        Ok(match method {
            "union" => {
                let mut output = left.clone();
                for value in self.javascript_set_like_keys(&like, method)? {
                    if !contains(&output, &value) {
                        output.push(value);
                    }
                }
                self.heap.allocate_set(output)?
            }
            "intersection" => {
                let mut output = Vec::new();
                if left.len() as f64 <= size {
                    for value in &left {
                        if self.javascript_set_like_has(&like, method, value)? {
                            output.push(value.clone());
                        }
                    }
                } else {
                    for value in self.javascript_set_like_keys(&like, method)? {
                        if contains(&left, &value) && !contains(&output, &value) {
                            output.push(value);
                        }
                    }
                }
                self.heap.allocate_set(output)?
            }
            "difference" => {
                let mut output;
                if left.len() as f64 <= size {
                    output = Vec::new();
                    for value in &left {
                        if !self.javascript_set_like_has(&like, method, value)? {
                            output.push(value.clone());
                        }
                    }
                } else {
                    output = left.clone();
                    for value in self.javascript_set_like_keys(&like, method)? {
                        output.retain(|candidate| !same_value_zero(candidate, &value));
                    }
                }
                self.heap.allocate_set(output)?
            }
            "symmetricDifference" => {
                let mut output = left.clone();
                for value in self.javascript_set_like_keys(&like, method)? {
                    if contains(&left, &value) {
                        output.retain(|candidate| !same_value_zero(candidate, &value));
                    } else if !contains(&output, &value) {
                        output.push(value);
                    }
                }
                self.heap.allocate_set(output)?
            }
            "isSubsetOf" => {
                let mut all = true;
                for value in &left {
                    if !self.javascript_set_like_has(&like, method, value)? {
                        all = false;
                        break;
                    }
                }
                Value::Bool(all)
            }
            "isSupersetOf" => {
                let mut all = true;
                for value in self.javascript_set_like_keys(&like, method)? {
                    if !contains(&left, &value) {
                        all = false;
                        break;
                    }
                }
                Value::Bool(all)
            }
            "isDisjointFrom" => {
                let mut all = true;
                if left.len() as f64 <= size {
                    for value in &left {
                        if self.javascript_set_like_has(&like, method, value)? {
                            all = false;
                            break;
                        }
                    }
                } else {
                    for value in self.javascript_set_like_keys(&like, method)? {
                        if contains(&left, &value) {
                            all = false;
                            break;
                        }
                    }
                }
                Value::Bool(all)
            }
            _ => unreachable!(),
        })
    }
}

/// A validated `GetSetRecord`: a heap `Set` or `Map`, or an object whose
/// `has`/`keys` are guest closures — which a synchronous builtin cannot call,
/// so the enum marks them lazy and refuses only when the algorithm reaches
/// them.
enum SetLike {
    Set(HeapId),
    Map(HeapId),
    Guest,
}

/// How V8 names a value that was called but has no `[[Call]]`: its type,
/// then its value when that is a primitive.
fn non_callable_text(value: &Value) -> String {
    match value {
        Value::Null => "object null".to_string(),
        Value::Undefined => "undefined".to_string(),
        Value::Bool(value) => format!("boolean {value}"),
        Value::Number(_) => format!("number {}", javascript_to_string(value)),
        Value::String(value) => format!("string \"{value}\""),
        _ => "object".to_string(),
    }
}

/// `Lash.*` intrinsics the lowerer emits for semantics no surface method
/// names: first-class built-in values, the `in` operator's full property
/// question, sparse array literals, and the `arguments` record.
impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn execute_lash_intrinsic(
        &mut self,
        values: &[Value],
    ) -> Result<bool, RuntimeError> {
        match values {
            [Value::String(method), name] if method.as_str() == "Lash.Builtin" => {
                let name = self.heap.javascript_to_string(name)?;
                let value = self.heap.builtin_value(&name)?;
                self.stack.push(value);
                Ok(true)
            }
            [Value::String(method), key, receiver] if method.as_str() == "Lash.HasProperty" => {
                // ECMA `in`: ToPropertyKey the left operand, then a
                // non-object right operand is a TypeError, then presence is
                // own keys plus the kind's built-in prototype surface.
                let key = self.heap.javascript_to_string(key)?;
                let has = match receiver {
                    Value::Null
                    | Value::Undefined
                    | Value::Number(_)
                    | Value::Bool(_)
                    | Value::String(_)
                    | Value::Image(_)
                    | Value::Resource(_) => {
                        let error = self.heap.allocate_error(
                            ErrorKind::TypeError,
                            Some(format!(
                                "Cannot use 'in' operator to search for '{key}' in {}",
                                crate::runtime::value_type_name(receiver)
                            )),
                            None,
                            None,
                        )?;
                        return Err(RuntimeError::UncaughtException { value: error });
                    }
                    _ => crate::runtime::access::javascript_value_has_property(
                        &self.heap, receiver, &key,
                    )?,
                };
                self.stack.push(Value::Bool(has));
                Ok(true)
            }
            [Value::String(method), elements, holes] if method.as_str() == "Lash.SparseArray" => {
                // Either operand may arrive heapified: the operand-import
                // pass turns an inline `List` into a `HeapObject::List` ref.
                let elements = match elements {
                    Value::List(elements) => elements.to_vec(),
                    Value::Ref(id) => match self.heap.get(*id)? {
                        HeapObject::List(elements) => elements.clone(),
                        _ => {
                            return Err(js_stdlib_error(
                                "Lash.SparseArray expects an element list and a hole-index list",
                            ));
                        }
                    },
                    _ => {
                        return Err(js_stdlib_error(
                            "Lash.SparseArray expects an element list and a hole-index list",
                        ));
                    }
                };
                let holes = match holes {
                    Value::List(holes) => holes.to_vec(),
                    Value::Ref(id) => match self.heap.get(*id)? {
                        HeapObject::List(holes) => holes.clone(),
                        _ => {
                            return Err(js_stdlib_error(
                                "Lash.SparseArray expects an element list and a hole-index list",
                            ));
                        }
                    },
                    _ => {
                        return Err(js_stdlib_error(
                            "Lash.SparseArray expects an element list and a hole-index list",
                        ));
                    }
                };
                let holes = holes
                    .iter()
                    .map(|hole| match hole {
                        Value::Number(index)
                            if index.fract() == 0.0
                                && *index >= 0.0
                                && *index <= usize::MAX as f64 =>
                        {
                            Ok(*index as usize)
                        }
                        _ => Err(js_stdlib_error(
                            "Lash.SparseArray hole indexes must be non-negative integers",
                        )),
                    })
                    .collect::<Result<std::collections::BTreeSet<usize>, _>>()?;
                self.heap.ensure_list_allocation_len(elements.len())?;
                let list = self.heap.allocate_list(elements.to_vec())?;
                if let Value::Ref(id) = list {
                    self.heap.mark_list_holes(id, holes);
                }
                self.stack.push(list);
                Ok(true)
            }
            [Value::String(method)] if method.as_str() == "Lash.Arguments" => {
                if let Some(existing) = self.slots.extras.get("lash:arguments") {
                    let existing = existing.clone();
                    self.stack.push(existing);
                    return Ok(true);
                }
                let extras = self.slots.extras.clone();
                let argv: Vec<Value> = match extras.get("lash:argv") {
                    Some(Value::List(argv)) => argv.to_vec(),
                    // The heapify pass imports an inline extras list, so by
                    // the time `arguments` is mentioned the argv is a heap
                    // `List` reference.
                    Some(Value::Ref(id)) => match self.heap.get(*id)? {
                        HeapObject::List(argv) => argv.clone(),
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                // The dialect is strict-mode, so `callee` and `caller` are
                // own poisoned names: present for `hasOwnProperty`, absent
                // from enumeration, and a `TypeError` on read or write.
                let mut arguments = Record::new();
                arguments.insert_str("callee", Value::Null);
                arguments.insert_str("caller", Value::Null);
                arguments.insert_str("length", Value::Number(argv.len() as f64));
                for (index, value) in argv.iter().enumerate() {
                    arguments.insert_str(&index.to_string(), value.clone());
                }
                let arguments = self.heap.allocate_record(arguments)?;
                if let Value::Ref(id) = arguments {
                    self.heap.mark_arguments_record(id);
                    self.slots
                        .extras
                        .insert_str("lash:arguments", Value::Ref(id));
                }
                self.stack.push(arguments);
                Ok(true)
            }
            [Value::String(method), source] if method.as_str() == "Lash.GroupBySource" => {
                // `Map.groupBy`/`Object.groupBy` take an iterable only — never
                // an array-like — so a record or primitive is the same
                // TypeError GetIterator raises.
                match self.group_by_elements(source)? {
                    Some(elements) => {
                        self.stack.push(Value::List(elements.into()));
                        Ok(true)
                    }
                    None => Err(crate::runtime::not_iterable_error(source)),
                }
            }
            [Value::String(method), milliseconds] if method.as_str() == "Lash.DateString" => {
                // A bare `Date()` call answers the current date-time string;
                // the lowerer feeds it the journaled clock so the UTC math
                // stays deterministic.
                let Value::Number(milliseconds) = milliseconds else {
                    return Err(js_stdlib_error(
                        "Lash.DateString expects the journaled clock milliseconds",
                    ));
                };
                self.stack.push(Value::String(
                    super::javascript_date::javascript_date_string(*milliseconds).into(),
                ));
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The elements `groupBy` would iterate: the dialect's iterable kinds —
    /// lists, tuples, strings, and the heap collections — or `None` when
    /// ECMA's GetIterator would throw.
    fn group_by_elements(&self, source: &Value) -> Result<Option<Vec<Value>>, RuntimeError> {
        Ok(match source {
            Value::List(elements) | Value::Tuple(elements) => Some(elements.to_vec()),
            Value::String(text) => Some(
                text.chars()
                    .map(|character| Value::String(character.to_string().into()))
                    .collect(),
            ),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(elements) | HeapObject::Tuple(elements) => Some(elements.clone()),
                HeapObject::Set(set) => Some(set.values.clone()),
                HeapObject::Map(map) => Some(
                    map.entries
                        .iter()
                        .map(|(key, value)| Value::List(vec![key.clone(), value.clone()].into()))
                        .collect(),
                ),
                HeapObject::UrlSearchParams(params) => Some(
                    params
                        .entries
                        .iter()
                        .map(|(key, value)| {
                            Value::List(
                                vec![
                                    Value::String(key.clone().into()),
                                    Value::String(value.clone().into()),
                                ]
                                .into(),
                            )
                        })
                        .collect(),
                ),
                HeapObject::RegExpMatch(result) => Some(result.items.clone()),
                _ => None,
            },
            _ => None,
        })
    }

    /// `Object.keys`/`values`/`entries`/`hasOwn`/`assign` on heap receivers:
    /// the materializing `javascript_stdlib` dispatch below can only see
    /// inline values, so heap objects answer here while they are still
    /// references. Errors own no enumerable data keys.
    pub(super) fn execute_heap_property_intrinsic(
        &mut self,
        values: &[Value],
    ) -> Result<bool, RuntimeError> {
        if let [Value::String(method), Value::Ref(receiver)] = values
            && matches!(self.heap.get(*receiver)?, HeapObject::Error(_))
        {
            let result = match method.as_str() {
                "Object.keys" | "Object.values" | "Object.entries" => {
                    Some(Value::List(Vec::new().into()))
                }
                "JSON.stringify" => Some(Value::String("{}".into())),
                _ => None,
            };
            if let Some(result) = result {
                self.stack.push(result);
                return Ok(true);
            }
        }
        if let [Value::String(method), Value::Ref(receiver)] = values
            && matches!(
                method.as_str(),
                "Object.keys" | "Object.values" | "Object.entries"
            )
        {
            if self.heap.is_builtin_object(*receiver) {
                let keys = self.heap.builtin_enumerable_keys(*receiver)?;
                let result = match method.as_str() {
                    "Object.keys" => keys
                        .into_iter()
                        .map(|key| Value::String(key.into()))
                        .collect(),
                    _ => {
                        let mut result = Vec::with_capacity(keys.len());
                        for key in keys {
                            let value = self.heap.builtin_read(*receiver, &key)?;
                            result.push(if method.as_str() == "Object.values" {
                                value
                            } else {
                                Value::List(vec![Value::String(key.into()), value].into())
                            });
                        }
                        result
                    }
                };
                self.stack.push(Value::List(result.into()));
                return Ok(true);
            }
            let result = match self.heap.get(*receiver)? {
                HeapObject::Record(record) => {
                    let mut entries = ecma_record_entries(record);
                    // The arguments record's `length`/`callee`/`caller` are
                    // own but non-enumerable.
                    if self.heap.is_arguments_record(*receiver) {
                        entries.retain(|(key, _)| !matches!(*key, "length" | "callee" | "caller"));
                    }
                    match method.as_str() {
                        "Object.keys" => entries
                            .into_iter()
                            .map(|(key, _)| Value::String(key.into()))
                            .collect(),
                        "Object.values" => entries
                            .into_iter()
                            .map(|(_, value)| value.clone())
                            .collect(),
                        "Object.entries" => entries
                            .into_iter()
                            .map(|(key, value)| {
                                Value::List(vec![Value::String(key.into()), value.clone()].into())
                            })
                            .collect(),
                        _ => unreachable!(),
                    }
                }
                HeapObject::List(items) => {
                    // A hole is not an own property: `Object.*` skip it, as
                    // Node does on a sparse array.
                    let present = |index: usize| !self.heap.is_list_hole(*receiver, index);
                    match method.as_str() {
                        "Object.keys" => (0..items.len())
                            .filter(|index| present(*index))
                            .map(|index| Value::String(index.to_string().into()))
                            .collect(),
                        "Object.values" => items
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| present(*index))
                            .map(|(_, value)| value.clone())
                            .collect(),
                        "Object.entries" => items
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| present(*index))
                            .map(|(index, value)| {
                                Value::List(
                                    vec![Value::String(index.to_string().into()), value.clone()]
                                        .into(),
                                )
                            })
                            .collect(),
                        _ => unreachable!(),
                    }
                }
                HeapObject::Tuple(items) => match method.as_str() {
                    "Object.keys" => (0..items.len())
                        .map(|index| Value::String(index.to_string().into()))
                        .collect(),
                    "Object.values" => items.to_vec(),
                    "Object.entries" => items
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            Value::List(
                                vec![Value::String(index.to_string().into()), value.clone()].into(),
                            )
                        })
                        .collect(),
                    _ => unreachable!(),
                },
                _ => Vec::new(),
            };
            self.stack.push(Value::List(result.into()));
            return Ok(true);
        }
        if let [Value::String(method), Value::Ref(receiver), key] = values
            && method.as_str() == "Object.hasOwn"
        {
            let key = self.heap.javascript_to_string(key)?;
            let has = crate::runtime::access::javascript_heap_has_own(&self.heap, *receiver, &key)?;
            self.stack.push(Value::Bool(has));
            return Ok(true);
        }
        if let [Value::String(method), Value::Ref(receiver), args @ ..] = values
            && method.as_str() == "Object.assign"
            && matches!(self.heap.get(*receiver)?, HeapObject::Record(_))
        {
            let HeapObject::Record(target) = self.heap.get(*receiver)? else {
                unreachable!("record receiver checked")
            };
            let mut output = target.as_ref().clone();
            for source in args {
                // This arm runs before the materializing dispatch below, because
                // a heap receiver has to stay a reference. That left a projected
                // *source* handle to match no source shape and be skipped
                // silently, so `Object.assign(target, projected)` copied
                // nothing. A projected handle is a host-side view of a value:
                // assign the record behind it. Nullish sources are still
                // skipped, projected or not.
                let source = materialize_value(source.clone())?;
                let entries = match &source {
                    Value::Ref(id) => match self.heap.get(*id)? {
                        HeapObject::Record(record) => Some(ecma_record_entries(record)),
                        _ => None,
                    },
                    Value::Record(record) => Some(ecma_record_entries(record)),
                    _ => None,
                };
                for (key, value) in entries.unwrap_or_default() {
                    output.insert(key.to_string(), value.clone());
                }
            }
            self.heap.replace_javascript_record(*receiver, output)?;
            self.stack.push(Value::Ref(*receiver));
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixed-length member's arity is derived from its signature row,
    /// so the assertions below pin the prose the number comes from — a row
    /// edited to a different arity moves the padding and kills the arms
    /// written against the old length, which is exactly the silent failure
    /// this census exists to catch at build time rather than review time.
    #[test]
    fn fixed_length_methods_resolve_their_signature_arity() {
        let cases: &[(&str, usize)] = &[
            ("at", 1),
            ("join", 1),
            ("flat", 1),
            ("endsWith", 2),
            ("padStart", 2),
            ("lastIndexOf", 2),
            ("slice", 2),
            ("with", 2),
            ("fill", 3),
            ("reverse", 0),
            ("toString", 0),
        ];
        for (method, arity) in cases {
            assert_eq!(
                crate::ecma_stdlib::instance_method_arity(method),
                Some(*arity),
                "{method}"
            );
        }
        let statics: &[(&str, usize)] = &[
            ("Object.keys", 1),
            ("Math.abs", 1),
            ("Number.parseInt", 2),
            ("Math.pow", 2),
        ];
        for (name, arity) in statics {
            assert_eq!(
                crate::ecma_stdlib::static_method_arity(name),
                Some(*arity),
                "{name}"
            );
        }
        // Presence-sensitive and variadic rows are never fixed-length.
        assert_eq!(crate::ecma_stdlib::static_method_arity("Date.UTC"), Some(7));
        assert_eq!(crate::ecma_stdlib::static_method_arity("Math.max"), None);
        assert_eq!(crate::ecma_stdlib::instance_method_arity("concat"), None);
        assert_eq!(
            crate::ecma_stdlib::instance_method_arity("hasOwnProperty"),
            Some(1)
        );
    }

    #[test]
    fn normalization_pads_and_truncates_to_the_derived_arity() {
        assert_eq!(normalized_instance_arguments("endsWith", &[]).len(), 2);
        assert_eq!(
            normalized_instance_arguments("endsWith", &[Value::Null, Value::Null, Value::Null])
                .len(),
            2
        );
        assert_eq!(
            normalized_instance_arguments("endsWith", &[Value::Null]),
            vec![Value::Null, Value::Undefined]
        );
        assert_eq!(normalized_instance_arguments("fill", &[]).len(), 3);
        assert_eq!(
            normalized_instance_arguments("reverse", &[Value::Null]).len(),
            0
        );
        assert_eq!(normalized_static_arguments("Math.pow", &[]).len(), 2);

        // Methods outside the fixed-length policy keep the raw vector: the
        // omitted/explicit-`undefined` distinction reaches the dispatcher.
        assert!(normalized_instance_arguments("split", &[]).is_empty());
        assert_eq!(
            normalized_static_arguments("Date.UTC", &[Value::Null]).len(),
            1
        );
    }

    /// A string pattern has no captures, so GetSubstitution runs with `m = 0`
    /// and every capture-shaped token — `$0`, `$00`, `$01`, `$n`, `$nn`,
    /// `$<name>` — is literal text; only `$$`, `$&`, `` $` `` and `$'` expand
    /// (FIG-3649).
    #[test]
    fn string_pattern_replacement_keeps_capture_tokens_literal() {
        let cases: &[(&str, &str)] = &[
            ("|$0|", "foo-|$0|-bar"),
            ("|$00|", "foo-|$00|-bar"),
            ("|$000|", "foo-|$000|-bar"),
            ("|$01|", "foo-|$01|-bar"),
            ("|$010|", "foo-|$010|-bar"),
            ("|$1|", "foo-|$1|-bar"),
            ("|$9|", "foo-|$9|-bar"),
            ("|$<n>|", "foo-|$<n>|-bar"),
            ("|$$|", "foo-|$|-bar"),
            ("|$&|", "foo-|x|-bar"),
            ("|$`|", "foo-|foo-|-bar"),
            ("|$'|", "foo-|-bar|-bar"),
        ];
        for (replacement, expected) in cases {
            assert_eq!(
                replace_string("foo-x-bar", "x", replacement).unwrap(),
                *expected,
                "{replacement}"
            );
        }
    }
}
