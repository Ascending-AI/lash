use super::Selection;
use lash_kernel_doc::{
    Formula, Integer, NativeCall, NativeError, NewObject, Nursery, Operand, PreparedNative,
    TextView, Value,
};

use num_traits::ToPrimitive;
use unicode_normalization::UnicodeNormalization;
use {lash_kernel_value::Name, lash_kernel_value::Type};

use super::{
    Function, arg, count_arg, definition, int, integer_value, materialized_text, ordering,
    prepared_definition, raise, reserve_list, sequence, sequence_type, text_arg, text_buffer,
    text_view,
};

pub(super) fn functions(selection: &Selection<'_>) -> Vec<Function> {
    let mut functions = Vec::new();
    for (name, native) in [
        (
            "text.len",
            len as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_len", utf16_len),
    ] {
        functions.push(definition(
            selection,
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
            get as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_get", Type::Int, utf16_get),
    ] {
        functions.push(definition(
            selection,
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
            slice as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_slice", utf16_slice),
    ] {
        functions.push(definition(
            selection,
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
            find as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.utf16_find", utf16_find),
    ] {
        functions.push(definition(
            selection,
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
    for (name, result, native, prepared) in [
        (
            "text.compare",
            Type::Int,
            compare as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
            None,
        ),
        ("text.utf16_compare", Type::Int, utf16_compare, None),
        (
            "text.concat",
            Type::Text,
            concat,
            Some(PreparedNative {
                pair: None,
                entry: short_concat,
                charged_entry: None,
                operation: "text.concat(text: Text, other: Text) -> Text",
            }),
        ),
        ("text.starts_with", Type::Bool, starts_with, None),
        ("text.ends_with", Type::Bool, ends_with, None),
    ] {
        functions.push(prepared_definition(
            selection,
            name,
            &[("text", Type::Text), ("other", Type::Text)],
            result,
            &[],
            native,
            prepared,
        ));
    }
    functions.push(definition(
        selection,
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
    functions.push(
        definition(
            selection,
            "text.join",
            &[
                ("items", sequence_type(Type::Text)),
                ("separator", Type::Text),
            ],
            Type::Text,
            &[],
            join,
        )
        .resident_join(),
    );
    let mut admission = definition(
        selection,
        "text.join_or_index",
        &[
            ("items", sequence_type(Type::Any)),
            ("separator", Type::Text),
        ],
        Type::Union(vec![Type::Text, Type::Int]),
        &[],
        join_or_index,
    );
    // Two borrowed list passes, buffer writing and final text copying. The list's
    // own size never follows a rejected member, even when it holds a cycle.
    admission.charge(|_| {
        Formula::Sum(vec![
            Formula::Constant(1),
            Formula::Product(vec![
                Formula::Constant(2),
                Formula::Size(Operand::Param(Name::new("items"))),
            ]),
            Formula::Size(Operand::Param(Name::new("separator"))),
            Formula::Product(vec![Formula::Constant(2), Formula::Size(Operand::Result)]),
        ])
    });
    functions.push(admission);
    functions.push(definition(
        selection,
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
        selection,
        "text.repeat",
        &[("text", Type::Text), ("count", Type::Int)],
        Type::Text,
        &[],
        repeat,
    ));
    for (name, native) in [
        (
            "text.pad_start",
            pad_start as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.pad_end", pad_end),
    ] {
        functions.push(definition(
            selection,
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
    let mut trim_definition = definition(
        selection,
        "text.trim_set",
        &[
            ("text", Type::Text),
            ("characters", Type::Text),
            ("leading", Type::Bool),
            ("trailing", Type::Bool),
        ],
        Type::Text,
        &[],
        trim_set,
    );
    // Membership searches the caller's set for each boundary scalar. Two
    // stopped scans together probe at most B + 1 scalars, including a middle
    // scalar twice. Charge decoding, set bytes searched and output copying.
    let size = |name| Formula::Size(Operand::Param(Name::new(name)));
    trim_definition.charge(|_| {
        Formula::Sum(vec![
            Formula::Constant(8),
            Formula::Product(vec![
                Formula::Sum(vec![Formula::Constant(1), size("text")]),
                Formula::Sum(vec![Formula::Constant(1), size("characters")]),
            ]),
            Formula::Size(Operand::Result),
        ])
    });
    functions.push(trim_definition);
    let (major, minor, patch) = char::UNICODE_VERSION;
    for (name, native) in [
        (
            "trim",
            trim as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("trim_start", trim_start),
        ("trim_end", trim_end),
        ("lower", lower),
        ("upper", upper),
    ] {
        functions.push(definition(
            selection,
            &format!("text.{name}_u{major}_{minor}_{patch}"),
            &[("text", Type::Text)],
            Type::Text,
            &[],
            native,
        ));
    }
    let (major, minor, patch) = unicode_normalization::UNICODE_VERSION;
    functions.push(definition(
        selection,
        &format!("text.normalize_u{major}_{minor}_{patch}"),
        &[("text", Type::Text), ("form", Type::Text)],
        Type::Text,
        &["normalization_form"],
        normalize,
    ));
    for (name, native) in [
        (
            "text.to_code_points",
            to_code_points as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.to_utf16_units", to_utf16_units),
    ] {
        functions.push(definition(
            selection,
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
            from_code_points as fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
        ),
        ("text.from_utf16_units", from_utf16_units),
    ] {
        functions.push(definition(
            selection,
            name,
            &[("items", sequence_type(Type::Int))],
            Type::Text,
            &["invalid_scalar", "invalid_utf16"],
            native,
        ));
    }
    functions
}

fn trim_set(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let characters = materialized_text(&call.args, call.new, 1)?;
    let Value::Bool(leading) = arg(call.args.values(), 2)? else {
        return Err(raise("type_error", "expected boolean"));
    };
    let Value::Bool(trailing) = arg(call.args.values(), 3)? else {
        return Err(raise("type_error", "expected boolean"));
    };
    let mut remaining = text.chars();
    if *leading {
        while let Some(character) = remaining.clone().next() {
            if !characters.contains(character) {
                break;
            }
            remaining.next();
        }
    }
    if *trailing {
        while let Some(character) = remaining.clone().next_back() {
            if !characters.contains(character) {
                break;
            }
            remaining.next_back();
        }
    }
    let trimmed = remaining.as_str();
    let mut output = text_buffer(call.new, trimmed.len())?;
    output.push_str(trimmed);
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(output.len() as u64);
    Ok(call.new.text(&output))
}

fn normalize(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let form = materialized_text(&call.args, call.new, 1)?;
    if !matches!(form.as_ref(), "NFC" | "NFD" | "NFKC" | "NFKD") {
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
        .ok_or_else(crate::exhausted)?;
    let bytes = scalars.checked_mul(4).ok_or_else(crate::exhausted)?;
    call.new
        .reserve(0, super::room(scalars).saturating_mul(16))?;
    #[cfg(test)]
    let _decomposed = crate::native_contract_laws::scratch(
        if form.starts_with("NFK") {
            text.nfkd().count() as u64
        } else {
            text.nfd().count() as u64
        } * 16,
    );
    let mut output = text_buffer(call.new, bytes)?;
    match form.as_ref() {
        "NFC" => output.extend(text.nfc()),
        "NFD" => output.extend(text.nfd()),
        "NFKC" => output.extend(text.nfkc()),
        "NFKD" => output.extend(text.nfkd()),
        _ => unreachable!("validated normalization form"),
    }
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(output.len() as u64);
    Ok(call.new.text(&output))
}

/// A distance from the selected end. A magnitude beyond machine size can
/// only be out of range, or clamp to that end's opposite boundary.
fn negative(index: &Integer) -> bool {
    index.is_negative()
}
fn distance(index: &Integer) -> usize {
    index
        .to_i128()
        .and_then(|i| usize::try_from(i.unsigned_abs()).ok())
        .unwrap_or(usize::MAX)
}

/// The byte offset of character `index` of `text`, or its length past
/// the last.
fn offset(text: &str, index: u64) -> usize {
    text.char_indices()
        .nth(index as usize)
        .map_or(text.len(), |(at, _)| at)
}

/// A text of `text[range]`, its bytes reserved first.
fn sliced(
    new: &mut Nursery<'_>,
    text: &str,
    range: std::ops::Range<usize>,
) -> Result<Value, NativeError> {
    new.hold(0, super::room(range.len()))?;
    let slice = text
        .get(range)
        .ok_or(NativeError::Malformed("a text slice cuts a scalar"))?;
    Ok(new.text(slice))
}

/// [`sliced`] over a text's view: the slice is read from the chunks its
/// range addresses alone, so a slice of a large text on its pages decodes
/// those chunks and no others (FIG-6193).
fn sliced_view(
    new: &mut Nursery<'_>,
    text: TextView<'_>,
    range: std::ops::Range<usize>,
) -> Result<Value, NativeError> {
    new.hold(0, super::room(range.len()))?;
    let cut = || NativeError::Malformed("a text slice cuts a scalar");
    if let Some(whole) = text.as_str() {
        return Ok(new.text(whole.get(range).ok_or_else(cut)?));
    }
    let mut slice = String::with_capacity(range.len());
    let mut offset = 0usize;
    for chunk in text.chunks() {
        if offset >= range.end {
            break;
        }
        let chunk = chunk?;
        let end = offset + chunk.len();
        if end > range.start {
            let from = range.start.saturating_sub(offset);
            let to = (range.end - offset).min(chunk.len());
            slice.push_str(chunk.get(from..to).ok_or_else(cut)?);
        }
        offset = end;
    }
    Ok(new.text(&slice))
}

/// Negative positions count from the end; slice positions are clamped.
pub(super) fn position(index: &Integer, len: usize) -> usize {
    let distance = distance(index);
    if negative(index) {
        len.saturating_sub(distance)
    } else {
        distance.min(len)
    }
}

fn len(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    Ok(int(text_view(&call.args, 0)?.summary().chars))
}
fn utf16_len(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    Ok(int(text_view(&call.args, 0)?.summary().utf16_units))
}

fn get(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = text_view(&call.args, 0)?;
    let index = &*integer_value(&call.args, 1)?;
    let distance = distance(index) as u64;
    let index = if negative(index) {
        text.summary().chars.checked_sub(distance)
    } else {
        Some(distance)
    }
    .filter(|i| *i < text.summary().chars)
    .ok_or_else(|| raise("index_out_of_range", "text index is out of range"))?;
    let whole = text.whole()?;
    let start = offset(&whole, index);
    let end = offset(&whole, index + 1);
    sliced(call.new, &whole, start..end)
}
fn utf16_get(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = text_view(&call.args, 0)?;
    let index = &*integer_value(&call.args, 1)?;
    let distance = distance(index) as u64;
    let target = if negative(index) {
        text.summary().utf16_units.checked_sub(distance)
    } else {
        Some(distance)
    }
    .filter(|i| *i < text.summary().utf16_units)
    .ok_or_else(|| raise("index_out_of_range", "text index is out of range"))?;
    let mut passed = 0;
    let mut found = None;
    'walk: for chunk in text.chunks() {
        for unit in chunk?.encode_utf16() {
            if passed == target {
                found = Some(unit);
                break 'walk;
            }
            passed += 1;
        }
    }
    found
        .map(int)
        .ok_or_else(|| raise("index_out_of_range", "text index is out of range"))
}

/// Advances monotonically to a boundary from one end, without first counting
/// the whole text. Exhaustion clamps to the opposite end. Two bounds from the same end share
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
}

fn slice(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = text_view(&call.args, 0)?;
    let first = position(
        &*integer_value(&call.args, 1)?,
        text.summary().chars as usize,
    ) as u64;
    let last = (position(
        &*integer_value(&call.args, 2)?,
        text.summary().chars as usize,
    ) as u64)
        .max(first);
    let [start, end] = slice_boundaries(text, [first, last], false)?;
    sliced_view(call.new, text, start as usize..end as usize)
}
fn utf16_slice(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = text_view(&call.args, 0)?;
    let total = text.summary().utf16_units;
    // A negative position counts back from the end, and both clamp to the
    // text: the same unit a walk from that end would stop at.
    let target = |index: &Integer| {
        let distance = distance(index) as u64;
        if negative(index) {
            total.saturating_sub(distance)
        } else {
            distance.min(total)
        }
    };
    let start = target(&*integer_value(&call.args, 1)?);
    let end = target(&*integer_value(&call.args, 2)?);
    let [from, to] = slice_boundaries(text, [start, end], true)?;
    if start > end {
        return Ok(call.new.text(""));
    }
    sliced_view(call.new, text, from as usize..to as usize)
}

/// The byte offsets of scalar or UTF-16 positions `targets`, each at most
/// the text's length, read from the front up to the farther one. Both slice
/// operations fault only that prefix's chunks (FIG-6193, FIG-6750). A UTF-16
/// position inside a surrogate pair is refused.
fn slice_boundaries(
    text: TextView<'_>,
    targets: [u64; 2],
    utf16: bool,
) -> Result<[u64; 2], NativeError> {
    let mut offsets = [text.summary().utf8_bytes; 2];
    let mut found = targets.map(|target| target == 0);
    for (offset, found) in offsets.iter_mut().zip(found) {
        if found {
            *offset = 0;
        }
    }
    let mut split = false;
    if !found.iter().all(|found| *found) {
        let (mut units, mut bytes) = (0_u64, 0_u64);
        'walk: for chunk in text.chunks() {
            for c in chunk?.chars() {
                units += if utf16 { c.len_utf16() as u64 } else { 1 };
                bytes += c.len_utf8() as u64;
                for at in 0..2 {
                    if !found[at] && units >= targets[at] {
                        found[at] = true;
                        offsets[at] = bytes;
                        split |= units > targets[at];
                    }
                }
                if found.iter().all(|found| *found) {
                    break 'walk;
                }
            }
        }
    }
    if split {
        return Err(raise(
            "text_boundary",
            "UTF-16 slice splits a surrogate pair",
        ));
    }
    Ok(offsets)
}

fn find(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    find_by(call, false)
}

fn utf16_find(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    find_by(call, true)
}

fn find_by(call: NativeCall<'_, '_>, units: bool) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let needle = materialized_text(&call.args, call.new, 1)?;
    let start = &*integer_value(&call.args, 2)?;
    let target = distance(start);
    let count = |text: &str| {
        if units {
            text.encode_utf16().count()
        } else {
            text.chars().count()
        }
    };
    let (byte, offset) = if negative(start) {
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
        .find(needle.as_ref())
        .map_or_else(|| int(-1), |byte| int(offset + count(&suffix[..byte]))))
}

fn compare(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    // UTF-8 lexicographic order agrees with scalar order.
    Ok(ordering(
        text_arg(&call.args, 0)?
            .as_bytes()
            .cmp(text_arg(&call.args, 1)?.as_bytes()),
    ))
}
fn utf16_compare(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let [text, other] = call.args.values() else {
        return Err(NativeError::Malformed(
            "UTF-16 comparison needs two operands",
        ));
    };
    Ok(ordering(crate::comparison::utf16_compare(
        text, other, &call.args,
    )?))
}
fn concat(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let (text, other) = (text_arg(&call.args, 0)?, text_arg(&call.args, 1)?);
    let len = text
        .len()
        .checked_add(other.len())
        .ok_or_else(crate::exhausted)?;
    call.new.hold(0, super::room(len))?;
    let mut out = String::new();
    out.try_reserve_exact(len).map_err(|_| crate::exhausted())?;
    out.push_str(&text);
    out.push_str(&other);
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(call.new.text(&out))
}
/// [`concat`] of two short texts whose concatenation is short: the same
/// text, which is inline, so the call builds and holds nothing of the
/// arena's; `None` for any other operands.
fn short_concat(args: &[Value]) -> Option<Value> {
    let [Value::Text(text), Value::Text(other)] = args else {
        return None;
    };
    text.concat(other).map(Value::Text)
}

fn starts_with(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let affix = materialized_text(&call.args, call.new, 1)?;
    Ok(Value::Bool(text.starts_with(affix.as_ref())))
}
fn ends_with(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let affix = materialized_text(&call.args, call.new, 1)?;
    Ok(Value::Bool(text.ends_with(affix.as_ref())))
}

/// The first `limit` pieces, the only ones split counts, reserves and
/// builds (`K-LTXT-005`); an omitted limit takes them all.
fn split(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let separator = materialized_text(&call.args, call.new, 1)?;
    let limit = match call.args.values().get(2) {
        None | Some(Value::Absent) => usize::MAX,
        Some(_) => count_arg(&call.args, 2)?,
    };
    let (count, bytes) = pieces(&text, &separator)
        .take(limit)
        .fold((0, 0), |(count, bytes), piece| {
            (count + 1, bytes + piece.len())
        });
    reserve_list(call.new, count, bytes)?;
    let items = pieces(&text, &separator)
        .take(limit)
        .map(|piece| call.new.text(piece))
        .collect();
    Ok(call.new.allocate(NewObject::List(items)))
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

fn join(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    match join_or_index(call)? {
        Value::Int(_) => Err(raise("type_error", "join requires text members")),
        result => Ok(result),
    }
}

fn join_or_index(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let list = arg(call.args.values(), 0)?;
    let (Value::List(_), Some(items)) = (list, call.args.sequence(list)?) else {
        return Err(raise("type_error", "expected list"));
    };
    let mut count = 0usize;
    let mut bytes = Some(0usize);
    for chunk in items.chunks() {
        for item in chunk? {
            let Some(text) = call.args.text(item)? else {
                return Ok(int(count));
            };
            count += 1;
            bytes = bytes.and_then(|bytes| bytes.checked_add(text.len() as usize));
        }
    }
    let separator = materialized_text(&call.args, call.new, 1)?;
    // Validate every member and the separator before checking size or
    // reserving the output, preserving the call's error order.
    let size = separator
        .len()
        .checked_mul(count.saturating_sub(1))
        .and_then(|separators| bytes.and_then(|bytes| separators.checked_add(bytes)))
        .ok_or_else(crate::exhausted)?;
    // ShortText already owns UTF-8 bytes admitted by its str constructor.
    // Copy them without decoding each part; validate the completed buffer once
    // to recover a str in safe code. Long-text chunks are already str.
    let mut out = text_buffer(call.new, size)?.into_bytes();
    let mut first = true;
    for chunk in items.chunks() {
        for item in chunk? {
            if !first {
                out.extend_from_slice(separator.as_bytes());
            }
            first = false;
            if let Some(text) = call.args.text(item)? {
                match text {
                    TextView::Short(short) => out.extend_from_slice(short.as_bytes()),
                    text => {
                        for piece in text.chunks() {
                            out.extend_from_slice(piece?.as_bytes());
                        }
                    }
                }
            }
        }
    }
    let out =
        String::from_utf8(out).map_err(|_| NativeError::Malformed("joined text is not UTF-8"))?;
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(call.new.text(&out))
}

fn replace(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let needle = materialized_text(&call.args, call.new, 1)?;
    let replacement = materialized_text(&call.args, call.new, 2)?;
    // The result's size follows the match count, so the matches are
    // counted before any of the result is built.
    let matches = text.matches(needle.as_ref()).count();
    let size = matches
        .checked_mul(replacement.len())
        .and_then(|replaced| replaced.checked_add(text.len() - matches * needle.len()))
        .ok_or_else(crate::exhausted)?;
    let mut out = text_buffer(call.new, size)?;
    let mut end = 0;
    for (start, found) in text.match_indices(needle.as_ref()) {
        out.push_str(&text[end..start]);
        out.push_str(&replacement);
        end = start + found.len();
    }
    out.push_str(&text[end..]);
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(call.new.text(&out))
}
fn trim(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    Ok(call.new.text(text.trim()))
}
fn trim_start(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    Ok(call.new.text(text.trim_start()))
}
fn trim_end(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    Ok(call.new.text(text.trim_end()))
}
fn lower(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let out = text.to_lowercase();
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(call.new.text(&out))
}
fn upper(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let out = text.to_uppercase();
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(call.new.text(&out))
}

fn repeat(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let count = count_arg(&call.args, 1)?;
    let size = text.len().checked_mul(count).ok_or_else(crate::exhausted)?;
    let mut out = text_buffer(call.new, size)?;
    if !text.is_empty() {
        for _ in 0..count {
            out.push_str(&text);
        }
    }
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(call.new.text(&out))
}

fn pad_start(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    pad(call, true)
}
fn pad_end(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    pad(call, false)
}

pub(super) fn padded(
    new: &mut Nursery<'_>,
    text: &str,
    width: usize,
    fill: &str,
    start: bool,
) -> Result<Value, NativeError> {
    let needed = width.saturating_sub(text.chars().count());
    if needed == 0 || fill.is_empty() {
        return Ok(new.text(text));
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
        .ok_or_else(crate::exhausted)?;
    let mut out = text_buffer(new, size)?;
    if !start {
        out.push_str(text);
    }
    out.extend(fill.chars().cycle().take(needed));
    if start {
        out.push_str(text);
    }
    #[cfg(test)]
    let _buffer = crate::native_contract_laws::scratch(out.len() as u64);
    Ok(new.text(&out))
}

fn pad(call: NativeCall<'_, '_>, start: bool) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    let width = count_arg(&call.args, 1)?;
    let fill = materialized_text(&call.args, call.new, 2)?;
    padded(call.new, &text, width, &fill, start)
}

fn to_code_points(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    // A code point's integer is no longer than its UTF-8 bytes.
    reserve_list(call.new, text.chars().count(), text.len())?;
    let items = text.chars().map(|c| int(u32::from(c))).collect();
    Ok(call.new.allocate(NewObject::List(items)))
}
fn to_utf16_units(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = materialized_text(&call.args, call.new, 0)?;
    reserve_list(call.new, text.encode_utf16().count(), text.len())?;
    let items = text.encode_utf16().map(int).collect();
    Ok(call.new.allocate(NewObject::List(items)))
}

fn from_code_points(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    #[cfg(test)]
    let mut _prefix = crate::native_contract_laws::scratch(0);
    let chars: Result<String, _> = sequence(&call.args, 0)?
        .iter()
        .map(|v| {
            match v {
                Value::Int(i) => u32::try_from(*i).ok(),
                Value::BigInt(_) => None,
                _ => return Err(raise("type_error", "expected integer code point")),
            }
            .and_then(char::from_u32)
            .ok_or_else(|| raise("invalid_scalar", "code point is not a Unicode scalar"))
            .inspect(|_c| {
                #[cfg(test)]
                _prefix.grow(_c.len_utf8() as u64);
            })
        })
        .collect();
    let chars = chars?;
    Ok(call.new.text(&chars))
}

fn from_utf16_units(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    #[cfg(test)]
    let mut _prefix = crate::native_contract_laws::scratch(0);
    let units: Result<Vec<_>, _> = sequence(&call.args, 0)?
        .iter()
        .map(|v| {
            match v {
                Value::Int(i) => u16::try_from(*i).ok(),
                Value::BigInt(_) => None,
                _ => return Err(raise("type_error", "expected integer UTF-16 unit")),
            }
            .ok_or_else(|| raise("invalid_utf16", "UTF-16 unit is outside 0..65535"))
            .inspect(|_unit| {
                #[cfg(test)]
                _prefix.grow(16 + u64::from(u16::BITS - _unit.leading_zeros()).div_ceil(8));
            })
        })
        .collect();
    let units = units?;
    String::from_utf16(&units)
        .map(|text| {
            #[cfg(test)]
            let _buffer = crate::native_contract_laws::scratch(text.len() as u64);
            call.new.text(&text)
        })
        .map_err(|_| raise("invalid_utf16", "UTF-16 contains an unpaired surrogate"))
}
