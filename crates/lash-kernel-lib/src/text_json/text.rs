use std::ops::ControlFlow;

use lash_kernel_doc::{Element, Integer, NativeCall, NativeError, NativeHeap, Object, Type, Value};
use num_traits::ToPrimitive;
use unicode_normalization::UnicodeNormalization;

use super::{
    Function, arg, count_arg, definition, int, integer_value, ordering, raise, reserve_list,
    sequence, sequence_type, text_arg, text_buffer,
};

pub(super) fn functions() -> Vec<Function> {
    let mut functions = Vec::new();
    for (name, native) in [
        (
            "text.len",
            len as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_len", utf16_len),
    ] {
        functions.push(definition(
            name,
            &[("text", Type::Text)],
            Type::Int,
            &[],
            native,
        ));
    }
    for (name, result, native) in [
        (
            "text.get",
            Type::Text,
            get as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_get", Type::Int, utf16_get),
    ] {
        functions.push(definition(
            name,
            &[("text", Type::Text), ("index", Type::Int)],
            result,
            &[],
            native,
        ));
    }
    for (name, native) in [
        (
            "text.slice",
            slice as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_slice", utf16_slice),
    ] {
        functions.push(definition(
            name,
            &[
                ("text", Type::Text),
                ("start", Type::Int),
                ("end", Type::Int),
            ],
            Type::Text,
            &[],
            native,
        ));
    }
    for (name, native) in [
        (
            "text.find",
            find as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_find", utf16_find),
    ] {
        functions.push(definition(
            name,
            &[
                ("text", Type::Text),
                ("needle", Type::Text),
                ("start", Type::Int),
            ],
            Type::Int,
            &[],
            native,
        ));
    }
    for (name, result, native) in [
        (
            "text.compare",
            Type::Int,
            compare as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_compare", Type::Int, utf16_compare),
        ("text.concat", Type::Text, concat),
        ("text.starts_with", Type::Bool, starts_with),
        ("text.ends_with", Type::Bool, ends_with),
    ] {
        functions.push(definition(
            name,
            &[("text", Type::Text), ("other", Type::Text)],
            result,
            &[],
            native,
        ));
    }
    functions.push(definition(
        "text.split",
        &[
            ("text", Type::Text),
            ("separator", Type::Text),
            ("limit?", Type::Int),
        ],
        sequence_type(Type::Text),
        &[],
        split,
    ));
    functions.push(definition(
        "text.join",
        &[
            ("items", sequence_type(Type::Text)),
            ("separator", Type::Text),
        ],
        Type::Text,
        &[],
        join,
    ));
    functions.push(definition(
        "text.replace",
        &[
            ("text", Type::Text),
            ("needle", Type::Text),
            ("replacement", Type::Text),
        ],
        Type::Text,
        &[],
        replace,
    ));
    functions.push(definition(
        "text.repeat",
        &[("text", Type::Text), ("count", Type::Int)],
        Type::Text,
        &[],
        repeat,
    ));
    for (name, native) in [
        (
            "text.pad_start",
            pad_start as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.pad_end", pad_end),
    ] {
        functions.push(definition(
            name,
            &[
                ("text", Type::Text),
                ("width", Type::Int),
                ("fill", Type::Text),
            ],
            Type::Text,
            &[],
            native,
        ));
    }
    let (major, minor, patch) = char::UNICODE_VERSION;
    for (name, native) in [
        (
            "trim",
            trim as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("trim_start", trim_start),
        ("trim_end", trim_end),
        ("lower", lower),
        ("upper", upper),
    ] {
        functions.push(definition(
            &format!("text.{name}_u{major}_{minor}_{patch}"),
            &[("text", Type::Text)],
            Type::Text,
            &[],
            native,
        ));
    }
    let (major, minor, patch) = unicode_normalization::UNICODE_VERSION;
    functions.push(definition(
        &format!("text.normalize_u{major}_{minor}_{patch}"),
        &[("text", Type::Text), ("form", Type::Text)],
        Type::Text,
        &["normalization_form"],
        normalize,
    ));
    for (name, native) in [
        (
            "text.to_code_points",
            to_code_points as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.to_utf16_units", to_utf16_units),
    ] {
        functions.push(definition(
            name,
            &[("text", Type::Text)],
            sequence_type(Type::Int),
            &[],
            native,
        ));
    }
    for (name, native) in [
        (
            "text.from_code_points",
            from_code_points as fn(NativeCall<'_>) -> Result<Value, NativeError>,
        ),
        ("text.from_utf16_units", from_utf16_units),
    ] {
        functions.push(definition(
            name,
            &[("items", sequence_type(Type::Int))],
            Type::Text,
            &["invalid_scalar", "invalid_utf16"],
            native,
        ));
    }
    functions
}

fn normalize(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let form = text_arg(call.args, 1)?;
    if !matches!(form, "NFC" | "NFD" | "NFKC" | "NFKD") {
        return Err(raise(
            "normalization_form",
            "expected NFC, NFD, NFKC or NFKD",
        ));
    }
    // Unicode 17's largest recursive decomposition has 18 scalars. Reserve
    // the reorder buffer and output before the iterator allocates either.
    let scalars = text
        .chars()
        .count()
        .checked_mul(18)
        .ok_or(NativeError::Memory)?;
    let bytes = scalars.checked_mul(4).ok_or(NativeError::Memory)?;
    call.heap
        .reserve(0, super::room(scalars).saturating_mul(16))?;
    let mut output = text_buffer(call.heap, bytes)?;
    match form {
        "NFC" => output.extend(text.nfc()),
        "NFD" => output.extend(text.nfd()),
        "NFKC" => output.extend(text.nfkc()),
        "NFKD" => output.extend(text.nfkd()),
        _ => unreachable!("validated normalization form"),
    }
    Ok(Value::text(output))
}

/// A distance from the selected end. A magnitude beyond machine size can
/// only be out of range, or clamp to that end's opposite boundary.
fn distance(index: &Integer) -> usize {
    index
        .to_i128()
        .and_then(|i| usize::try_from(i.unsigned_abs()).ok())
        .unwrap_or(usize::MAX)
}

/// Negative positions count from the end; slice positions are clamped.
pub(super) fn position(index: &Integer, len: usize) -> usize {
    let distance = distance(index);
    if index.is_negative() {
        len.saturating_sub(distance)
    } else {
        distance.min(len)
    }
}

fn len(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(int(text_arg(call.args, 0)?.chars().count()))
}
fn utf16_len(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(int(text_arg(call.args, 0)?.encode_utf16().count()))
}

fn get(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let index = integer_value(call.args, 1)?;
    let mut chars = text.chars();
    let found = if index.is_negative() {
        chars.nth_back(distance(index) - 1)
    } else {
        chars.nth(distance(index))
    };
    found
        .map(|c| Value::text(c.to_string()))
        .ok_or_else(|| raise("index_out_of_range", "text index is out of range"))
}

fn utf16_get(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let index = integer_value(call.args, 1)?;
    let found = if index.is_negative() {
        let mut remaining = distance(index) - 1;
        text.chars().rev().find_map(|c| {
            let width = c.len_utf16();
            if remaining < width {
                let mut units = [0; 2];
                Some(c.encode_utf16(&mut units)[width - remaining - 1])
            } else {
                remaining -= width;
                None
            }
        })
    } else {
        text.encode_utf16().nth(distance(index))
    };
    found
        .map(int)
        .ok_or_else(|| raise("index_out_of_range", "text index is out of range"))
}

/// Advances monotonically to a boundary from one end, without first counting
/// the whole text. `walked > target` means the target splits a UTF-16 pair.
/// Exhaustion clamps to the opposite end. Two bounds from the same end share
/// this cursor; mixed bounds each traverse only the end they address.
struct Boundaries<I> {
    chars: I,
    walked: usize,
    byte: usize,
    units: bool,
    backwards: bool,
}

impl<I: Iterator<Item = (usize, char)>> Boundaries<I> {
    fn at(&mut self, target: usize) -> usize {
        while self.walked < target {
            let Some((byte, c)) = self.chars.next() else {
                break;
            };
            self.walked += if self.units { c.len_utf16() } else { 1 };
            self.byte = if self.backwards {
                byte
            } else {
                byte + c.len_utf8()
            };
        }
        self.byte
    }

    fn slice_at(&mut self, target: usize) -> Result<usize, NativeError> {
        let byte = self.at(target);
        if self.walked > target {
            Err(raise(
                "text_boundary",
                "UTF-16 slice splits a surrogate pair",
            ))
        } else {
            Ok(byte)
        }
    }
}

fn slice(call: NativeCall<'_>) -> Result<Value, NativeError> {
    slice_by(call, false)
}
fn utf16_slice(call: NativeCall<'_>) -> Result<Value, NativeError> {
    slice_by(call, true)
}

fn slice_by(call: NativeCall<'_>, units: bool) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let start = integer_value(call.args, 1)?;
    let end = integer_value(call.args, 2)?;
    let mut front = Boundaries {
        chars: text.char_indices(),
        walked: 0,
        byte: 0,
        units,
        backwards: false,
    };
    let mut back = Boundaries {
        chars: text.char_indices().rev(),
        walked: 0,
        byte: text.len(),
        units,
        backwards: true,
    };
    let (a, b) = (distance(start), distance(end));
    let (start, end) = match (start.is_negative(), end.is_negative()) {
        (false, false) if a <= b => (front.slice_at(a)?, front.slice_at(b)?),
        (false, false) => {
            let end = front.slice_at(b)?;
            (front.slice_at(a)?, end)
        }
        (true, true) if a <= b => (back.slice_at(a)?, back.slice_at(b)?),
        (true, true) => {
            let end = back.slice_at(b)?;
            (back.slice_at(a)?, end)
        }
        (false, true) => (front.slice_at(a)?, back.slice_at(b)?),
        (true, false) => (back.slice_at(a)?, front.slice_at(b)?),
    };
    Ok(Value::text(if start > end {
        ""
    } else {
        &text[start..end]
    }))
}

fn find(call: NativeCall<'_>) -> Result<Value, NativeError> {
    find_by(call, false)
}

fn utf16_find(call: NativeCall<'_>) -> Result<Value, NativeError> {
    find_by(call, true)
}

fn find_by(call: NativeCall<'_>, units: bool) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let needle = text_arg(call.args, 1)?;
    let start = integer_value(call.args, 2)?;
    let target = distance(start);
    let count = |text: &str| {
        if units {
            text.encode_utf16().count()
        } else {
            text.chars().count()
        }
    };
    let (byte, offset) = if start.is_negative() {
        let mut back = Boundaries {
            chars: text.char_indices().rev(),
            walked: 0,
            byte: text.len(),
            units,
            backwards: true,
        };
        let mut byte = back.at(target);
        let split = back.walked > target;
        // A valid text needle starts at a scalar boundary. An empty needle
        // can still be found between the units of a surrogate pair.
        let offset = count(&text[..byte]) + usize::from(split);
        if split && !needle.is_empty() {
            byte += text[byte..].chars().next().map_or(0, char::len_utf8);
        }
        (byte, offset + usize::from(split && !needle.is_empty()))
    } else {
        let mut front = Boundaries {
            chars: text.char_indices(),
            walked: 0,
            byte: 0,
            units,
            backwards: false,
        };
        let byte = front.at(target);
        let offset = if front.walked > target && needle.is_empty() {
            target
        } else {
            front.walked
        };
        (byte, offset)
    };
    if needle.is_empty() {
        return Ok(int(offset));
    }
    let suffix = &text[byte..];
    Ok(suffix
        .find(needle)
        .map_or_else(|| int(-1), |byte| int(offset + count(&suffix[..byte]))))
}

fn compare(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(ordering(
        text_arg(call.args, 0)?
            .chars()
            .cmp(text_arg(call.args, 1)?.chars()),
    ))
}
fn utf16_compare(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(ordering(
        text_arg(call.args, 0)?
            .encode_utf16()
            .cmp(text_arg(call.args, 1)?.encode_utf16()),
    ))
}
fn concat(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::text(format!(
        "{}{}",
        text_arg(call.args, 0)?,
        text_arg(call.args, 1)?
    )))
}
fn starts_with(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::Bool(
        text_arg(call.args, 0)?.starts_with(text_arg(call.args, 1)?),
    ))
}
fn ends_with(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::Bool(
        text_arg(call.args, 0)?.ends_with(text_arg(call.args, 1)?),
    ))
}

/// The first `limit` pieces, the only ones split counts, reserves and
/// builds (`K-LTXT-005`); an omitted limit takes them all.
fn split(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let separator = text_arg(call.args, 1)?;
    let limit = match call.args.get(2) {
        None | Some(Value::Absent) => usize::MAX,
        Some(_) => count_arg(call.args, 2)?,
    };
    let (count, bytes) = pieces(text, separator)
        .take(limit)
        .fold((0, 0), |(count, bytes), piece| {
            (count + 1, bytes + piece.len())
        });
    reserve_list(call.heap, count, bytes)?;
    let items = pieces(text, separator)
        .take(limit)
        .map(|piece| Value::text(piece.to_owned()))
        .collect();
    Ok(Value::List(call.heap.allocate(Object::List(items))?))
}

/// The text between the separator's occurrences or, for an empty
/// separator, each scalar value.
fn pieces<'a>(text: &'a str, separator: &'a str) -> impl Iterator<Item = &'a str> {
    let scalars = separator.is_empty().then(|| {
        text.char_indices()
            .map(|(start, scalar)| &text[start..start + scalar.len_utf8()])
    });
    let cuts = (!separator.is_empty()).then(|| text.split(separator));
    scalars
        .into_iter()
        .flatten()
        .chain(cuts.into_iter().flatten())
}

fn join(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let Value::List(id) = arg(call.args, 0)? else {
        return Err(raise("type_error", "expected list"));
    };
    let len = call.heap.len(*id);
    if len != 0 && call.heap.list_get(*id, 0).is_none() {
        return Err(raise("type_error", "invalid list"));
    }
    let mut count = 0usize;
    let mut bytes = Some(0usize);
    let mut error = None;
    call.heap.visit(*id, &mut |element| {
        let Element::Item(Value::Text(text)) = element else {
            error = Some(raise("type_error", "join requires text members"));
            return ControlFlow::Break(());
        };
        count += 1;
        bytes = bytes.and_then(|bytes| bytes.checked_add(text.len()));
        ControlFlow::Continue(())
    });
    if let Some(error) = error {
        return Err(error);
    }
    if count != len {
        return Err(raise("type_error", "invalid list"));
    }
    let separator = text_arg(call.args, 1)?;
    // Validate every member and the separator before checking size or
    // reserving the output, preserving the call's error order.
    let size = separator
        .len()
        .checked_mul(count.saturating_sub(1))
        .and_then(|separators| bytes.and_then(|bytes| separators.checked_add(bytes)))
        .ok_or(NativeError::Memory)?;
    let mut out = text_buffer(call.heap, size)?;
    let mut first = true;
    call.heap.visit(*id, &mut |element| {
        if let Element::Item(Value::Text(text)) = element {
            if !first {
                out.push_str(separator);
            }
            first = false;
            out.push_str(text);
        }
        ControlFlow::Continue(())
    });
    Ok(Value::text(out))
}

fn replace(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let needle = text_arg(call.args, 1)?;
    let replacement = text_arg(call.args, 2)?;
    // The result's size follows the match count, so the matches are
    // counted before any of the result is built.
    let matches = text.matches(needle).count();
    let size = matches
        .checked_mul(replacement.len())
        .and_then(|replaced| replaced.checked_add(text.len() - matches * needle.len()))
        .ok_or(NativeError::Memory)?;
    let mut out = text_buffer(call.heap, size)?;
    let mut end = 0;
    for (start, found) in text.match_indices(needle) {
        out.push_str(&text[end..start]);
        out.push_str(replacement);
        end = start + found.len();
    }
    out.push_str(&text[end..]);
    Ok(Value::text(out))
}
fn trim(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::text(text_arg(call.args, 0)?.trim()))
}
fn trim_start(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::text(text_arg(call.args, 0)?.trim_start()))
}
fn trim_end(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::text(text_arg(call.args, 0)?.trim_end()))
}
fn lower(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::text(text_arg(call.args, 0)?.to_lowercase()))
}
fn upper(call: NativeCall<'_>) -> Result<Value, NativeError> {
    Ok(Value::text(text_arg(call.args, 0)?.to_uppercase()))
}

fn repeat(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    let count = count_arg(call.args, 1)?;
    let size = text.len().checked_mul(count).ok_or(NativeError::Memory)?;
    let mut out = text_buffer(call.heap, size)?;
    if !text.is_empty() {
        for _ in 0..count {
            out.push_str(text);
        }
    }
    Ok(Value::text(out))
}

fn pad_start(call: NativeCall<'_>) -> Result<Value, NativeError> {
    pad(call, true)
}
fn pad_end(call: NativeCall<'_>) -> Result<Value, NativeError> {
    pad(call, false)
}

pub(super) fn padded(
    heap: &mut dyn NativeHeap,
    text: &str,
    width: usize,
    fill: &str,
    start: bool,
) -> Result<Value, NativeError> {
    let needed = width.saturating_sub(text.chars().count());
    if needed == 0 || fill.is_empty() {
        return Ok(Value::text(text));
    }
    // The fill repeats whole, then its first characters make up the rest.
    let fill_chars = fill.chars().count();
    let rest: usize = fill
        .chars()
        .take(needed % fill_chars)
        .map(char::len_utf8)
        .sum();
    let size = (needed / fill_chars)
        .checked_mul(fill.len())
        .and_then(|padding| padding.checked_add(rest + text.len()))
        .ok_or(NativeError::Memory)?;
    let mut out = text_buffer(heap, size)?;
    if !start {
        out.push_str(text);
    }
    out.extend(fill.chars().cycle().take(needed));
    if start {
        out.push_str(text);
    }
    Ok(Value::text(out))
}

fn pad(call: NativeCall<'_>, start: bool) -> Result<Value, NativeError> {
    padded(
        call.heap,
        text_arg(call.args, 0)?,
        count_arg(call.args, 1)?,
        text_arg(call.args, 2)?,
        start,
    )
}

fn to_code_points(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    // A code point's integer is no longer than its UTF-8 bytes.
    reserve_list(call.heap, text.chars().count(), text.len())?;
    let items = text.chars().map(|c| int(u32::from(c))).collect();
    Ok(Value::List(call.heap.allocate(Object::List(items))?))
}
fn to_utf16_units(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = text_arg(call.args, 0)?;
    reserve_list(call.heap, text.encode_utf16().count(), text.len())?;
    let items = text.encode_utf16().map(int).collect();
    Ok(Value::List(call.heap.allocate(Object::List(items))?))
}

fn from_code_points(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let chars: Result<String, _> = sequence(&call, 0)?
        .iter()
        .map(|v| {
            let Value::Int(i) = v else {
                return Err(raise("type_error", "expected integer code point"));
            };
            i.to_u32()
                .and_then(char::from_u32)
                .ok_or_else(|| raise("invalid_scalar", "code point is not a Unicode scalar"))
        })
        .collect();
    Ok(Value::text(chars?))
}

fn from_utf16_units(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let units: Result<Vec<_>, _> = sequence(&call, 0)?
        .iter()
        .map(|v| {
            let Value::Int(i) = v else {
                return Err(raise("type_error", "expected integer UTF-16 unit"));
            };
            i.to_u16()
                .ok_or_else(|| raise("invalid_utf16", "UTF-16 unit is outside 0..65535"))
        })
        .collect();
    String::from_utf16(&units?)
        .map(Value::text)
        .map_err(|_| raise("invalid_utf16", "UTF-16 contains an unpaired surrogate"))
}
