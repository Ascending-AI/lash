//! Token-preserving JSON (`K-EFF-005` to `K-EFF-007`).
use super::Selection;

use std::borrow::Cow;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{
    Args, Float, Integer, NativeCall, NativeError, NewObject, NumberPolicy, NumberToken, Nursery,
    Value,
};

use lash_kernel_value::Type;
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use super::{Function, arg, definition, materialized_text, raise, room, text_arg};

/// Whether `error` is a stated-type mismatch a union tries past, rather
/// than a cause that ends the call.
fn raised(error: &NativeError) -> bool {
    matches!(error, NativeError::Raised(_) | NativeError::Refused { .. })
}

/// Decodes one JSON number without passing integers through a float.
///
/// `Int` accepts any integral mathematical value, including fraction/exponent
/// spellings. `Float` rounds once to nearest binary64, ties to even, rejecting
/// infinity. `Number` and `Any` use `policy`. A union selects its first fitting
/// member. A wrong type, fractional integer or float overflow raises
/// `effect_result` (`K-EFF-005` to `K-EFF-007`). The digits an exponent adds
/// to an integer are reserved in `new` before they are written.
pub fn decode_number(
    token: &NumberToken,
    expected: &Type,
    policy: NumberPolicy,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    Ok(convert_number(token.as_str(), expected, policy, new)?.store(new))
}

/// A JSON number converted for its stated type, not yet stored.
enum Number {
    Int(Integer),
    Float(Float),
}

impl Number {
    /// The value naming the number: a big integer is built in `new`.
    fn store(self, new: &mut Nursery<'_>) -> Value {
        match self {
            Number::Int(integer) => new.integer(integer),
            Number::Float(float) => Value::Float(float),
        }
    }
}

/// `decode_number`'s conversion. It builds nothing: `new` only reserves the
/// digits an exponent adds.
fn convert_number(
    token: &str,
    expected: &Type,
    policy: NumberPolicy,
    new: &mut Nursery<'_>,
) -> Result<Number, NativeError> {
    let selected = match expected {
        Type::Number | Type::Any => {
            if policy == NumberPolicy::BySpelling && !token.contains(['.', 'e', 'E']) {
                &Type::Int
            } else {
                &Type::Float
            }
        }
        Type::Union(members) => {
            for member in members {
                match convert_number(token, member, policy, new) {
                    Ok(number) => return Ok(number),
                    Err(error) if raised(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            return Err(mismatch());
        }
        other => other,
    };
    match selected {
        Type::Int => {
            if let Ok(word) = token.parse::<i64>() {
                return Ok(Number::Int(Integer::from(word)));
            }
            let integer = exact_integer(token, new)?;
            Ok(Number::Int(Integer::new(integer)))
        }
        Type::Float => token
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| Number::Float(Float::new(f)))
            .ok_or_else(mismatch),
        _ => Err(mismatch()),
    }
}

fn mismatch() -> NativeError {
    raise("effect_result", "JSON value does not fit the stated type")
}

fn exact_integer(token: &str, new: &mut Nursery<'_>) -> Result<BigInt, NativeError> {
    let (mantissa, exponent) = token.split_once(['e', 'E']).unwrap_or((token, "0"));
    let exponent = exponent.parse::<BigInt>().map_err(|_| mismatch())?;
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = format!("{whole}{fraction}");
    if digits.bytes().all(|b| b == b'0') {
        return Ok(BigInt::from(0));
    }
    let shift = exponent - BigInt::from(fraction.len());
    if shift < BigInt::from(0) {
        let remove = (-shift)
            .to_usize()
            .filter(|remove| *remove <= digits.len())
            .ok_or_else(mismatch)?;
        let keep = digits.len() - remove;
        if !digits[keep..].bytes().all(|b| b == b'0') {
            return Err(mismatch());
        }
        digits.truncate(keep);
    } else {
        // An exponent's magnitude, not the token's length, sets how many
        // digits the integer has: the zeros and the integer they spell are
        // reserved first.
        let add = shift.to_usize().ok_or_else(crate::exhausted)?;
        new.reserve(0, room(add).saturating_mul(2))?;
        digits.try_reserve(add).map_err(|_| crate::exhausted())?;
        digits.extend(std::iter::repeat_n('0', add));
    }
    let value = digits.parse::<BigInt>().map_err(|_| mismatch())?;
    Ok(if negative { -value } else { value })
}

#[derive(Debug)]
enum Json<'text> {
    Null,
    Bool(bool),
    Number(&'text str),
    Text(Cow<'text, str>),
    Array(Vec<Json<'text>>),
    Object(Vec<(Cow<'text, str>, Json<'text>)>),
}

mod reader;
use reader::{read, syntax};

/// A JSON value checked against its stated type: its numbers converted and
/// its containers' kinds chosen, its text borrowed from the parsed tree.
/// Checking builds no guest object; [`build`] does, from these decisions.
enum Checked<'j> {
    Null,
    Bool(bool),
    Text(&'j str),
    Number(Number),
    List(Vec<Checked<'j>>),
    Tuple(Vec<Checked<'j>>),
    Record(Vec<(&'j str, Checked<'j>)>),
    Map(Vec<(&'j str, Checked<'j>)>),
}

fn check<'j>(
    json: &'j Json<'_>,
    ty: &Type,
    policy: NumberPolicy,
    new: &mut Nursery<'_>,
) -> Result<Checked<'j>, NativeError> {
    if let Type::Union(members) = ty {
        for member in members {
            match check(json, member, policy, new) {
                Ok(checked) => return Ok(checked),
                Err(error) if raised(&error) => {}
                Err(error) => return Err(error),
            }
        }
        return Err(mismatch());
    }
    let list = |items: &'j [Json<'_>], element: &Type, new: &mut Nursery<'_>| {
        items
            .iter()
            .map(|item| check(item, element, policy, new))
            .collect::<Result<_, _>>()
    };
    Ok(match (json, ty) {
        (Json::Number(token), _) => Checked::Number(convert_number(token, ty, policy, new)?),
        (Json::Null, Type::Null | Type::Any) => Checked::Null,
        (Json::Bool(value), Type::Bool | Type::Any) => Checked::Bool(*value),
        (Json::Text(text), Type::Text | Type::Any) => Checked::Text(text),
        (Json::Text(text), Type::Enum(members))
            if members.iter().any(|member| member == text.as_ref()) =>
        {
            Checked::Text(text)
        }
        (Json::Array(items), Type::Tuple(types)) if items.len() == types.len() => Checked::Tuple(
            items
                .iter()
                .zip(types)
                .map(|(item, ty)| check(item, ty, policy, new))
                .collect::<Result<_, _>>()?,
        ),
        (Json::Array(items), Type::List(element)) => Checked::List(list(items, element, new)?),
        (Json::Array(items), Type::Any) => Checked::List(list(items, &Type::Any, new)?),
        (Json::Object(fields), Type::Any) => Checked::Record(
            fields
                .iter()
                .map(|(name, value)| Ok((name.as_ref(), check(value, &Type::Any, policy, new)?)))
                .collect::<Result<_, NativeError>>()?,
        ),
        (Json::Object(fields), Type::Record(record)) => {
            for field in &record.fields {
                if !field.optional && !fields.iter().any(|(name, _)| *name == field.name) {
                    return Err(mismatch());
                }
            }
            Checked::Record(
                fields
                    .iter()
                    .filter(|(name, _)| {
                        !matches!(record.rest, lash_kernel_value::RecordRest::Drop)
                            || record
                                .fields
                                .iter()
                                .any(|field| field.name == name.as_ref())
                    })
                    .map(|(name, value)| {
                        let ty = record
                            .fields
                            .iter()
                            .find(|field| field.name == *name)
                            .map(|field| &field.ty)
                            .or(record.rest.kept_type())
                            .ok_or_else(mismatch)?;
                        Ok((name.as_ref(), check(value, ty, policy, new)?))
                    })
                    .collect::<Result<_, NativeError>>()?,
            )
        }
        (Json::Object(fields), Type::Map(map)) => Checked::Map(
            fields
                .iter()
                .map(|(name, value)| {
                    if !key_fits(name, &map.key) {
                        return Err(mismatch());
                    }
                    Ok((name.as_ref(), check(value, &map.value, policy, new)?))
                })
                .collect::<Result<_, NativeError>>()?,
        ),
        _ => return Err(mismatch()),
    })
}

/// Whether an object key, which is always text, fits a map's key type.
fn key_fits(name: &str, ty: &Type) -> bool {
    match ty {
        Type::Text | Type::Any => true,
        Type::Enum(members) => members.iter().any(|member| member == name),
        Type::Union(members) => members.iter().any(|member| key_fits(name, member)),
        _ => false,
    }
}

/// The fresh graph a checked value names, built by its decisions.
fn build(checked: Checked<'_>, new: &mut Nursery<'_>) -> Value {
    let object = match checked {
        Checked::Null => return Value::Null,
        Checked::Bool(value) => return Value::Bool(value),
        Checked::Text(text) => return new.text(text),
        Checked::Number(number) => return number.store(new),
        Checked::List(items) => NewObject::List(build_all(items, new)),
        Checked::Tuple(items) => NewObject::Tuple(build_all(items, new)),
        Checked::Record(fields) => NewObject::Record(build_fields(fields, new)),
        Checked::Map(fields) => NewObject::Map(build_fields(fields, new)),
    };
    new.allocate(object)
}

fn build_all(items: Vec<Checked<'_>>, new: &mut Nursery<'_>) -> Vec<Value> {
    items.into_iter().map(|item| build(item, new)).collect()
}

fn build_fields(fields: Vec<(&str, Checked<'_>)>, new: &mut Nursery<'_>) -> Vec<(Value, Value)> {
    fields
        .into_iter()
        .map(|(name, item)| (new.text(name), build(item, new)))
        .collect()
}

/// Parses strict JSON into a fresh kernel graph, decoded by the stated type.
///
/// Numbers retain their spelling until `decode_number`; arrays under `Any`
/// become lists and objects become records. Duplicate keys take their last
/// value in the position of their first occurrence. Invalid JSON, including
/// lone surrogates, raises `json_syntax`; nesting past 64 raises `json_depth`.
/// Stated-type mismatches raise `effect_result`. Validation precedes allocation.
pub fn parse_json(
    text: &str,
    expected: &Type,
    policy: NumberPolicy,
    depth_limit: usize,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    let json = read(text, depth_limit, new)?;
    let checked = check(&json, expected, policy, new)?;
    Ok(build(checked, new))
}

/// The text `stringify` writes. A shared object is written once for every
/// place that holds it, so the text can outgrow the value: each piece is
/// reserved, as text and as the buffer it is built in, before it is appended.
struct Out<'a, 'call> {
    text: String,
    args: &'a Args<'call>,
    new: &'a mut Nursery<'call>,
    layout: Option<DecimalLayout>,
    verbatim_kinds: u8,
    verbatim_field: Option<String>,
    encoding: Option<Encoding>,
}

impl Out<'_, '_> {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.new.reserve(0, room(piece.len()).saturating_mul(2))?;
        self.text.push_str(piece);
        Ok(())
    }

    fn separator(&mut self, item: bool) -> Result<(), NativeError> {
        let piece = self
            .encoding
            .as_ref()
            .map_or(if item { "," } else { ":" }, |policy| {
                if item {
                    policy.item_separator.as_str()
                } else {
                    policy.key_separator.as_str()
                }
            });
        self.new.reserve(0, room(piece.len()).saturating_mul(2))?;
        self.text.push_str(piece);
        Ok(())
    }

    fn quote(&mut self, text: &str) -> Result<(), NativeError> {
        if self.encoding.as_ref().is_some_and(|policy| policy.ascii) {
            self.push("\"")?;
            let mut start = 0;
            for (index, character) in text.char_indices() {
                let escape = match character {
                    '"' => "\\\"",
                    '\\' => "\\\\",
                    '\n' => "\\n",
                    '\r' => "\\r",
                    '\t' => "\\t",
                    '\u{8}' => "\\b",
                    '\u{c}' => "\\f",
                    ' '..='~' => continue,
                    _ => "",
                };
                self.push(&text[start..index])?;
                if escape.is_empty() {
                    for unit in character.encode_utf16(&mut [0; 2]) {
                        let mut escaped = *b"\\u0000";
                        for digit in 0..4 {
                            escaped[5 - digit] =
                                b"0123456789abcdef"[usize::from((*unit >> (digit * 4)) & 15)];
                        }
                        self.push(std::str::from_utf8(&escaped).map_err(|_| syntax())?)?;
                    }
                } else {
                    self.push(escape)?;
                }
                start = index + character.len_utf8();
            }
            self.push(&text[start..])?;
            return self.push("\"");
        }
        self.push("\"")?;
        let mut start = 0;
        for (index, byte) in text.bytes().enumerate() {
            let escape = match byte {
                b'"' => "\\\"",
                b'\\' => "\\\\",
                b'\n' => "\\n",
                b'\r' => "\\r",
                b'\t' => "\\t",
                8 => "\\b",
                12 => "\\f",
                0..=31 => "",
                _ => continue,
            };
            self.push(&text[start..index])?;
            if escape.is_empty() {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let bytes = [
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[usize::from(byte / 16)],
                    HEX[usize::from(byte % 16)],
                ];
                // Every byte is ASCII.
                self.push(std::str::from_utf8(&bytes).map_err(|_| syntax())?)?;
            } else {
                self.push(escape)?;
            }
            start = index + 1;
        }
        self.push(&text[start..])?;
        self.push("\"")
    }

    fn number(&mut self, value: &Value, selected: bool) -> Result<(), NativeError> {
        if let Some(layout) = self
            .layout
            .filter(|_| selected && (self.encoding.is_none() || matches!(value, Value::Float(_))))
        {
            match value {
                Value::Float(number) => {
                    let value = number.get();
                    // Below 2^53 an integral binary64 has exact integer
                    // digits. Spell these directly instead of invoking the
                    // shortest floating-point converter for every row index.
                    if value.trunc() == value && value.abs() < 9007199254740992.0 {
                        return layout.small_integer(
                            value.abs() as u64,
                            value.is_sign_negative(),
                            self,
                        );
                    }
                    let mut scratch = [0; 17];
                    let (negative, digits, exponent) = number
                        .decimal_parts(&mut scratch)
                        .ok_or_else(|| raise("json_number", "JSON refuses NaN and infinity"))?;
                    return layout.write(negative, digits, i64::from(exponent), self);
                }
                Value::Int(value) => {
                    return layout.small_integer(value.unsigned_abs(), *value < 0, self);
                }
                Value::BigInt(_) => {
                    let Some(number) = self.args.integer(value)? else {
                        return Err(NativeError::Malformed("an integer names another body"));
                    };
                    // Reserve decimal scratch before converting arbitrary-precision
                    // integers; three digits per byte bounds their decimal extent.
                    self.new.reserve(
                        0,
                        number
                            .bits()
                            .div_ceil(8)
                            .saturating_mul(3)
                            .saturating_add(1),
                    )?;
                    let text = integer_digits(&number, self.new)?;
                    let digits = text.strip_prefix('-').unwrap_or(&text);
                    let trimmed = digits.trim_end_matches('0');
                    let (digits, exponent) = if trimmed.is_empty() {
                        ("0", 0)
                    } else {
                        (
                            trimmed,
                            i64::try_from(digits.len() - trimmed.len())
                                .map_err(|_| crate::exhausted())?,
                        )
                    };
                    return layout.write(text.starts_with('-'), digits, exponent, self);
                }
                _ => unreachable!("the JSON writer selects only numbers"),
            }
        }
        match value {
            Value::Int(number) => self.push(&number.to_string()),
            Value::BigInt(_) => {
                let Some(number) = self.args.integer(value)? else {
                    return Err(NativeError::Malformed("an integer names another body"));
                };
                let text = integer_digits(&number, self.new)?;
                self.push(&text)
            }
            Value::Float(number) => self.push(&number.to_string()),
            _ => unreachable!("the JSON writer selects only numbers"),
        }
    }

    fn raw_numbers(&self, kind: &str, inherited: bool) -> bool {
        inherited && self.verbatim_kinds & kind_bit(kind) == 0
    }
}

fn text_copy<'call>(
    args: &Args<'call>,
    text: &'call Value,
) -> Result<Cow<'call, str>, NativeError> {
    match args.text(text)? {
        Some(text) => text.whole(),
        None => Err(raise("type_error", "expected text")),
    }
}

/// `text` whole, as the heap copied it out: a paged text holds its bytes
/// ([`Nursery::read_whole`]).
fn text_held<'call>(
    args: &Args<'call>,
    new: &mut Nursery<'_>,
    text: &'call Value,
) -> Result<Cow<'call, str>, NativeError> {
    match args.text(text)? {
        Some(text) => {
            new.read_whole(text.len())?;
            text.whole()
        }
        None => Err(raise("type_error", "expected text")),
    }
}

/// `number`'s digits, as the heap copied a paged integer out to spell it.
fn integer_digits(number: &Integer, new: &mut Nursery<'_>) -> Result<String, NativeError> {
    new.read_whole(number.bits().div_ceil(8))?;
    Ok(number.to_string())
}

fn inline_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Int(value) => Some(*value),
        _ => None,
    }
}

fn kind_bit(kind: &str) -> u8 {
    match kind {
        "list" => 1,
        "tuple" => 2,
        "record" => 4,
        "map" => 8,
        _ => 0,
    }
}

fn write(
    value: &Value,
    active: &mut Vec<ArenaHandle>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_, '_>,
) -> Result<(), NativeError> {
    let limit = out.args.limits().document_depth().min(64);
    if depth > limit {
        return Err(raise(
            "json_depth",
            format!("JSON exceeds the configured {limit}-level kernel nesting limit"),
        ));
    }
    let args = out.args;
    match value {
        Value::Null => out.push("null")?,
        Value::Bool(value) => out.push(if *value { "true" } else { "false" })?,
        Value::Int(_) | Value::BigInt(_) => out.number(value, raw_numbers)?,
        Value::Float(float) if float.get().is_finite() => {
            out.number(value, raw_numbers)?;
        }
        Value::Text(_) | Value::LongText(_) => {
            let Some(text) = args.text(value)? else {
                return Err(NativeError::Malformed("a text names another body"));
            };
            out.new.read_whole(text.len())?;
            out.quote(&text.whole()?)?;
        }
        Value::Tuple(_) => {
            let raw_numbers = out.raw_numbers("tuple", raw_numbers);
            let items = super::sequence_values(args, value)?;
            write_array(&items, active, depth, raw_numbers, out)?;
        }
        Value::List(id) | Value::Map(id) | Value::Record(id) => {
            if active.contains(id) {
                return Err(raise("cycle", "cyclic value cannot be JSON"));
            }
            active.push(*id);
            if matches!(value, Value::List(_)) {
                let raw_numbers = out.raw_numbers("list", raw_numbers);
                let items = super::sequence_values(args, value)?;
                write_array(&items, active, depth, raw_numbers, out)?;
            } else {
                let Some(table) = args.table(value)? else {
                    return Err(NativeError::Malformed("a table names another body"));
                };
                let mut fields = Vec::with_capacity(table.len() as usize);
                for chunk in table.chunks() {
                    for pair in chunk?.as_chunks::<2>().0.iter() {
                        if !matches!(pair[0], Value::Text(_) | Value::LongText(_)) {
                            return Err(raise("json_key", "JSON map keys must be text"));
                        }
                        if matches!(value, Value::Record(_))
                            && let Some(policy) = &out.encoding
                        {
                            let key = text_held(args, out.new, &pair[0])?;
                            if policy.excluded.iter().any(|field| *key == **field) {
                                return Err(raise("not_data", "record has an excluded field"));
                            }
                        }
                        fields.push((&pair[0], &pair[1]));
                    }
                }
                let raw_numbers = if matches!(value, Value::Map(_)) {
                    out.raw_numbers("map", raw_numbers)
                } else {
                    let mut verbatim = false;
                    if let Some(field) = &out.verbatim_field {
                        for (name, value) in &fields {
                            if matches!(value, Value::Text(_) | Value::LongText(_))
                                && *text_copy(args, name)? == **field
                            {
                                verbatim = true;
                                break;
                            }
                        }
                    }
                    out.raw_numbers("record", raw_numbers) && !verbatim
                };
                out.push("{")?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        out.separator(true)?;
                    }
                    let name = text_held(args, out.new, name)?;
                    out.quote(&name)?;
                    out.separator(false)?;
                    write(value, active, depth + 1, raw_numbers, out)?;
                }
                out.push("}")?;
            }
            active.pop();
        }
        Value::Float(_) => return Err(raise("json_number", "JSON refuses NaN and infinity")),
        _ => return Err(raise("not_data", "value kind has no JSON representation")),
    }
    Ok(())
}

fn write_array(
    items: &[Value],
    active: &mut Vec<ArenaHandle>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_, '_>,
) -> Result<(), NativeError> {
    out.push("[")?;
    for (index, value) in items.iter().enumerate() {
        if index != 0 {
            out.separator(true)?;
        }
        write(value, active, depth + 1, raw_numbers, out)?;
    }
    out.push("]")
}

/// Writes compact JSON in insertion order, preserving all integer digits.
///
/// Tuples and lists are arrays; records and text-keyed maps are objects. Shared
/// objects are copied each time; cycles raise `cycle`. Absent, bytes, timestamps,
/// sets, errors, functions, closures, task handles, host handles and refs raise
/// `not_data`; no value is silently omitted or coerced. NaN/infinity raise
/// `json_number`, non-text map keys `json_key`, excessive nesting `json_depth`.
/// The text is reserved in `new` as it grows.
pub fn stringify_json<'call>(
    value: &Value,
    args: &Args<'call>,
    new: &mut Nursery<'call>,
) -> Result<String, NativeError> {
    let mut out = Out {
        text: String::new(),
        args,
        new,
        layout: None,
        verbatim_kinds: 0,
        verbatim_field: None,
        encoding: None,
    };
    write(value, &mut Vec::new(), 0, false, &mut out)?;
    Ok(out.text)
}

pub(super) fn functions(selection: &Selection<'_>) -> Vec<Function> {
    vec![
        definition(
            selection,
            "json.parse",
            &[
                ("text", Type::Text),
                (
                    "numbers",
                    Type::Enum(vec!["int".into(), "float".into(), "number".into()]),
                ),
                (
                    "policy",
                    Type::Enum(vec!["by_spelling".into(), "float".into()]),
                ),
            ],
            Type::Any,
            &["json_syntax", "json_depth"],
            parse,
        ),
        definition(
            selection,
            "json.parse_binary64",
            &[("text", Type::Text)],
            Type::Any,
            &["json_syntax", "json_depth"],
            parse_binary64,
        ),
        definition(
            selection,
            "json.stringify",
            &[("value", Type::Any)],
            Type::Text,
            &["json_number", "json_key", "json_depth"],
            stringify,
        ),
        definition(
            selection,
            "json.decode",
            &[
                ("text", Type::Text),
                (
                    "numbers",
                    Type::Enum(vec!["int".into(), "float".into(), "number".into()]),
                ),
                (
                    "policy",
                    Type::Enum(vec!["by_spelling".into(), "float".into()]),
                ),
                ("objects", Type::Enum(vec!["record".into(), "map".into()])),
            ],
            Type::Any,
            &["json_syntax", "json_depth"],
            decode,
        ),
        render_definition(selection),
        encode_definition(selection),
    ]
}

fn parse(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    parse_objects(call, false)
}

// Binary64 language JSON permits overflow to infinity. Typed kernel JSON keeps
// its finite Float contract; only this explicit codec selects IEEE overflow.
fn parse_binary64(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = text_arg(&call.args, 0)?;
    let json = read(&text, call.args.limits().document_depth(), call.new)?;
    decode_with_numbers(
        &json,
        &Type::Float,
        NumberPolicy::Float,
        false,
        true,
        call.new,
    )
}

fn decode(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let maps = match materialized_text(&call.args, call.new, 3)?.as_ref() {
        "map" => true,
        "record" => false,
        _ => return Err(raise("type_error", "expected record or map objects")),
    };
    parse_objects(call, maps)
}

fn parse_objects(call: NativeCall<'_, '_>, maps: bool) -> Result<Value, NativeError> {
    let text = text_arg(&call.args, 0)?;
    let json = read(&text, call.args.limits().document_depth(), call.new)?;
    let numbers = match materialized_text(&call.args, call.new, 1)?.as_ref() {
        "int" => Type::Int,
        "float" => Type::Float,
        "number" => Type::Number,
        _ => return Err(raise("type_error", "expected int, float or number")),
    };
    let policy = match materialized_text(&call.args, call.new, 2)?.as_ref() {
        "by_spelling" => NumberPolicy::BySpelling,
        "float" => NumberPolicy::Float,
        _ => return Err(raise("type_error", "expected a number policy")),
    };
    decode_with_numbers(&json, &numbers, policy, maps, false, call.new)
}

fn decode_with_numbers(
    json: &Json<'_>,
    numbers: &Type,
    policy: NumberPolicy,
    maps: bool,
    ieee: bool,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    match json {
        Json::Number(token) if ieee => {
            new.read_whole(token.len() as u64)?;
            let value = token.parse::<f64>().map_err(|_| mismatch())?;
            Ok(Value::Float(Float::new(value)))
        }
        Json::Number(token) => Ok(convert_number(token, numbers, policy, new)?.store(new)),
        Json::Array(items) => {
            let values = items
                .iter()
                .map(|item| decode_with_numbers(item, numbers, policy, maps, ieee, new))
                .collect::<Result<_, _>>()?;
            Ok(new.allocate(NewObject::List(values)))
        }
        Json::Object(fields) => {
            let values = fields
                .iter()
                .map(|(key, item)| {
                    let key = new.text(key);
                    Ok((
                        key,
                        decode_with_numbers(item, numbers, policy, maps, ieee, new)?,
                    ))
                })
                .collect::<Result<_, NativeError>>()?;
            Ok(new.allocate(if maps {
                NewObject::Map(values)
            } else {
                NewObject::Record(values)
            }))
        }
        Json::Null => Ok(Value::Null),
        Json::Bool(value) => Ok(Value::Bool(*value)),
        Json::Text(text) => Ok(new.text(text)),
    }
}

fn stringify(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let text = stringify_json(arg(call.args.values(), 0)?, &call.args, call.new)?;
    Ok(call.new.text(&text))
}

fn render(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let args = call.args;
    let kinds = arg(args.values(), 1)?;
    let (Value::Set(_), Some(kinds)) = (kinds, args.table(kinds)?) else {
        return Err(raise("type_error", "JSON verbatim kinds must be a set"));
    };
    let mut verbatim_kinds = 0;
    for chunk in kinds.chunks() {
        for pair in chunk?.as_chunks::<2>().0.iter() {
            let Some(name) = args.text(&pair[0])? else {
                return Err(raise("type_error", "JSON verbatim kinds must be text"));
            };
            call.new.read_whole(name.len())?;
            verbatim_kinds |= kind_bit(&name.whole()?);
        }
    }
    let verbatim_field = match arg(args.values(), 2)? {
        Value::Absent => None,
        field @ (Value::Text(_) | Value::LongText(_)) => {
            Some(text_held(&args, call.new, field)?.into_owned())
        }
        _ => {
            return Err(raise(
                "type_error",
                "JSON verbatim field must be text or absent",
            ));
        }
    };
    let layout = DecimalLayout::read(arg(args.values(), 3)?, &args)?;
    let mut out = Out {
        text: String::new(),
        args: &args,
        new: call.new,
        layout: Some(layout),
        verbatim_kinds,
        verbatim_field,
        encoding: None,
    };
    write(arg(args.values(), 0)?, &mut Vec::new(), 0, true, &mut out)?;
    let text = out.text;
    Ok(call.new.text(&text))
}

/// Shortest decimal layout is caller policy, not a language in the codec.
#[derive(Clone, Copy)]
struct DecimalLayout {
    fixed_from: i64,
    fixed_until: i64,
    exponent_plus: bool,
    negative_zero: bool,
}

impl DecimalLayout {
    fn small_integer(
        self,
        mut value: u64,
        negative: bool,
        out: &mut Out<'_, '_>,
    ) -> Result<(), NativeError> {
        let mut scratch = [0u8; 20];
        let mut start = scratch.len();
        loop {
            start -= 1;
            scratch[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let mut end = scratch.len();
        while end > start + 1 && scratch[end - 1] == b'0' {
            end -= 1;
        }
        let digits = std::str::from_utf8(&scratch[start..end]).map_err(|_| syntax())?;
        self.write(negative, digits, (scratch.len() - end) as i64, out)
    }

    fn read(value: &Value, args: &Args<'_>) -> Result<Self, NativeError> {
        let (Value::Tuple(_), Some(items)) = (value, args.sequence(value)?) else {
            return Err(raise("type_error", "expected decimal layout tuple"));
        };
        let values = (0..items.len().min(5))
            .map(|index| items.get(index))
            .collect::<Result<Vec<_>, _>>()?;
        let [
            Some(from @ (Value::Int(_) | Value::BigInt(_))),
            Some(until @ (Value::Int(_) | Value::BigInt(_))),
            Some(Value::Bool(exponent_plus)),
            Some(Value::Bool(negative_zero)),
        ] = values.as_slice()
        else {
            return Err(raise(
                "type_error",
                "expected decimal layout bounds and flags",
            ));
        };
        let from = inline_i64(from).filter(|value| (-324..=309).contains(value));
        let until = inline_i64(until).filter(|value| (-324..=309).contains(value));
        let (Some(fixed_from), Some(fixed_until)) = (from, until) else {
            return Err(raise(
                "number_range",
                "decimal layout bounds must be in -324..309",
            ));
        };
        if fixed_from > fixed_until {
            return Err(raise("number_range", "decimal layout bounds are reversed"));
        }
        Ok(Self {
            fixed_from,
            fixed_until,
            exponent_plus: *exponent_plus,
            negative_zero: *negative_zero,
        })
    }

    fn write(
        self,
        negative: bool,
        digits: &str,
        exponent: i64,
        out: &mut Out<'_, '_>,
    ) -> Result<(), NativeError> {
        if negative && (digits != "0" || self.negative_zero) {
            out.push("-")?;
        }
        let point = i64::try_from(digits.len())
            .map_err(|_| crate::exhausted())?
            .saturating_add(exponent);
        if point < self.fixed_from || point > self.fixed_until {
            out.push(&digits[..1])?;
            if digits.len() > 1 {
                out.push(".")?;
                out.push(&digits[1..])?;
            }
            out.push("e")?;
            let exponent = point - 1;
            if exponent >= 0 && self.exponent_plus {
                out.push("+")?;
            }
            if exponent < 0 {
                out.push("-")?;
            }
            let mut scratch = [0u8; 20];
            let mut rest = exponent.unsigned_abs();
            let mut start = scratch.len();
            loop {
                start -= 1;
                scratch[start] = b'0' + (rest % 10) as u8;
                rest /= 10;
                if rest == 0 {
                    break;
                }
            }
            let width = out
                .encoding
                .as_ref()
                .map_or(0, |policy| policy.exponent_digits);
            for _ in scratch.len() - start..width {
                out.push("0")?;
            }
            out.push(std::str::from_utf8(&scratch[start..]).map_err(|_| syntax())?)
        } else if point <= 0 {
            out.push("0.")?;
            for _ in 0..-point {
                out.push("0")?;
            }
            out.push(digits)
        } else {
            let point = usize::try_from(point).map_err(|_| crate::exhausted())?;
            if point >= digits.len() {
                out.push(digits)?;
                for _ in digits.len()..point {
                    out.push("0")?;
                }
                if out
                    .encoding
                    .as_ref()
                    .is_some_and(|policy| policy.fractional)
                {
                    out.push(".0")?;
                }
                Ok(())
            } else {
                out.push(&digits[..point])?;
                out.push(".")?;
                out.push(&digits[point..])
            }
        }
    }
}

fn render_definition(selection: &Selection<'_>) -> Function {
    definition(
        selection,
        "json.render",
        &[
            ("value", Type::Any),
            ("verbatim_kinds", Type::Set(Box::new(Type::Text))),
            (
                "verbatim_field",
                Type::Union(vec![Type::Text, Type::Absent]),
            ),
            (
                "layout",
                Type::Tuple(vec![Type::Int, Type::Int, Type::Bool, Type::Bool]),
            ),
        ],
        Type::Text,
        &["json_number", "json_key", "json_depth", "number_range"],
        render,
    )
}

/// Lexical JSON choices, supplied by the caller independently of its value model.
struct Encoding {
    ascii: bool,
    item_separator: String,
    key_separator: String,
    exponent_digits: usize,
    fractional: bool,
    excluded: Vec<String>,
}

fn encode_definition(selection: &Selection<'_>) -> Function {
    definition(
        selection,
        "json.encode",
        &[
            ("value", Type::Any),
            (
                "layout",
                Type::Tuple(vec![Type::Int, Type::Int, Type::Bool, Type::Bool]),
            ),
            (
                "style",
                Type::Tuple(vec![
                    Type::Bool,
                    Type::Text,
                    Type::Text,
                    Type::Int,
                    Type::Bool,
                    Type::Tuple(vec![Type::Text, Type::Text, Type::Text]),
                ]),
            ),
        ],
        Type::Text,
        &[
            "json_number",
            "json_key",
            "json_depth",
            "number_range",
            "not_data",
            "cycle",
        ],
        encode,
    )
}

fn encode(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let args = call.args;
    let layout = DecimalLayout::read(arg(args.values(), 1)?, &args)?;
    let style = arg(args.values(), 2)?;
    let (Value::Tuple(_), Some(style)) = (style, args.sequence(style)?) else {
        return Err(raise("type_error", "expected JSON style tuple"));
    };
    if style.len() != 6 {
        return Err(raise("type_error", "expected six JSON style fields"));
    }
    let values = (0..6)
        .map(|index| style.get(index))
        .collect::<Result<Vec<_>, _>>()?;
    let [
        Some(Value::Bool(ascii)),
        Some(item),
        Some(key),
        Some(Value::Int(width)),
        Some(Value::Bool(fractional)),
        Some(excluded),
    ] = values.as_slice()
    else {
        return Err(raise("type_error", "invalid JSON style fields"));
    };
    if !(0..=20).contains(width) {
        return Err(raise("number_range", "exponent width must be in 0..20"));
    }
    let (Value::Tuple(_), Some(excluded)) = (excluded, args.sequence(excluded)?) else {
        return Err(raise("type_error", "expected excluded record fields"));
    };
    if excluded.len() != 3 {
        return Err(raise("type_error", "expected three excluded record fields"));
    }
    let excluded = (0..excluded.len())
        .map(|index| {
            let field = excluded
                .get(index)?
                .ok_or_else(|| raise("type_error", "missing excluded field"))?;
            Ok(text_held(&args, call.new, field)?.into_owned())
        })
        .collect::<Result<Vec<_>, NativeError>>()?;
    let encoding = Encoding {
        ascii: *ascii,
        item_separator: text_held(&args, call.new, item)?.into_owned(),
        key_separator: text_held(&args, call.new, key)?.into_owned(),
        exponent_digits: *width as usize,
        fractional: *fractional,
        excluded,
    };
    let mut out = Out {
        text: String::new(),
        args: &args,
        new: call.new,
        layout: Some(layout),
        verbatim_kinds: 0,
        verbatim_field: None,
        encoding: Some(encoding),
    };
    write(arg(args.values(), 0)?, &mut Vec::new(), 0, true, &mut out)?;
    let text = out.text;
    Ok(call.new.text(&text))
}
