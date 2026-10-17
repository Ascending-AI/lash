//! Token-preserving JSON (`K-EFF-005` to `K-EFF-007`).

use std::borrow::Cow;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{
    Args, Float, Integer, NativeCall, NativeError, NewObject, NumberPolicy, NumberToken, Nursery,
    Type, Value,
};
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
    let selected = match expected {
        Type::Number | Type::Any => {
            if policy == NumberPolicy::BySpelling && token.is_integer_spelling() {
                &Type::Int
            } else {
                &Type::Float
            }
        }
        Type::Union(members) => {
            for member in members {
                match decode_number(token, member, policy, new) {
                    Ok(value) => return Ok(value),
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
            let integer = exact_integer(token.as_str(), new)?;
            Ok(new.integer(Integer::new(integer)))
        }
        Type::Float => token
            .as_str()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| Value::Float(Float::new(f)))
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
enum Json {
    Null,
    Bool(bool),
    Number(NumberToken),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

mod reader;
use reader::{read, syntax};

fn fits(
    json: &Json,
    ty: &Type,
    policy: NumberPolicy,
    new: &mut Nursery<'_>,
) -> Result<(), NativeError> {
    if let Type::Union(members) = ty {
        for member in members {
            match fits(json, member, policy, new) {
                Ok(()) => return Ok(()),
                Err(error) if raised(&error) => {}
                Err(error) => return Err(error),
            }
        }
        return Err(mismatch());
    }
    match (json, ty) {
        (Json::Number(token), _) => {
            decode_number(token, ty, policy, new)?;
        }
        (Json::Null, Type::Null | Type::Any)
        | (Json::Bool(_), Type::Bool | Type::Any)
        | (Json::Text(_), Type::Text | Type::Any) => {}
        (Json::Text(text), Type::Enum(members)) if members.contains(text) => {}
        (Json::Array(items), Type::Tuple(types)) if items.len() == types.len() => {
            for (item, ty) in items.iter().zip(types) {
                fits(item, ty, policy, new)?;
            }
        }
        (Json::Array(items), Type::List(element)) => {
            for item in items {
                fits(item, element, policy, new)?;
            }
        }
        (Json::Array(items), Type::Any) => {
            for item in items {
                fits(item, &Type::Any, policy, new)?;
            }
        }
        (Json::Object(fields), Type::Any) => {
            for (_, value) in fields {
                fits(value, &Type::Any, policy, new)?;
            }
        }
        (Json::Object(fields), Type::Record(record)) => {
            for field in &record.fields {
                if !field.optional && !fields.iter().any(|(name, _)| *name == field.name) {
                    return Err(mismatch());
                }
            }
            for (name, value) in fields {
                let ty = record
                    .fields
                    .iter()
                    .find(|field| field.name == *name)
                    .map(|field| &field.ty)
                    .or(record.rest.as_deref())
                    .ok_or_else(mismatch)?;
                fits(value, ty, policy, new)?;
            }
        }
        (Json::Object(fields), Type::Map(map)) => {
            for (name, value) in fields {
                fits(&Json::Text(name.clone()), &map.key, policy, new)?;
                fits(value, &map.value, policy, new)?;
            }
        }
        _ => return Err(mismatch()),
    }
    Ok(())
}

fn decode(
    json: &Json,
    ty: &Type,
    policy: NumberPolicy,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    if let Type::Union(members) = ty {
        for member in members {
            match fits(json, member, policy, new) {
                Ok(()) => return decode(json, member, policy, new),
                Err(error) if raised(&error) => {}
                Err(error) => return Err(error),
            }
        }
        return Err(mismatch());
    }
    match json {
        Json::Null => Ok(Value::Null),
        Json::Bool(value) => Ok(Value::Bool(*value)),
        Json::Text(text) => Ok(new.text(text)),
        Json::Number(token) => decode_number(token, ty, policy, new),
        Json::Array(items) => {
            let values: Result<Vec<_>, _> = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let ty = match ty {
                        Type::List(element) => element.as_ref(),
                        Type::Tuple(types) => &types[index],
                        _ => &Type::Any,
                    };
                    decode(item, ty, policy, new)
                })
                .collect();
            let values = values?;
            Ok(new.allocate(match ty {
                Type::Tuple(_) => NewObject::Tuple(values),
                _ => NewObject::List(values),
            }))
        }
        Json::Object(fields) => {
            let values: Result<Vec<_>, NativeError> = fields
                .iter()
                .map(|(name, item)| {
                    let ty = match ty {
                        Type::Record(record) => record
                            .fields
                            .iter()
                            .find(|field| field.name == *name)
                            .map(|field| &field.ty)
                            .or(record.rest.as_deref())
                            .ok_or_else(mismatch)?,
                        Type::Map(map) => &map.value,
                        _ => &Type::Any,
                    };
                    let key = new.text(name);
                    Ok((key, decode(item, ty, policy, new)?))
                })
                .collect();
            let values = values?;
            Ok(new.allocate(if matches!(ty, Type::Map(_)) {
                NewObject::Map(values)
            } else {
                NewObject::Record(values)
            }))
        }
    }
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
    fits(&json, expected, policy, new)?;
    decode(&json, expected, policy, new)
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
}

impl Out<'_, '_> {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.new.reserve(0, room(piece.len()).saturating_mul(2))?;
        self.text.push_str(piece);
        Ok(())
    }

    fn quote(&mut self, text: &str) -> Result<(), NativeError> {
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
        if let Some(layout) = self.layout.filter(|_| selected) {
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
                        out.push(",")?;
                    }
                    let name = text_held(args, out.new, name)?;
                    out.quote(&name)?;
                    out.push(":")?;
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
            out.push(",")?;
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
    };
    write(value, &mut Vec::new(), 0, false, &mut out)?;
    Ok(out.text)
}

pub(super) fn functions() -> Vec<Function> {
    vec![
        definition(
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
            "json.stringify",
            &[("value", Type::Any)],
            Type::Text,
            &["json_number", "json_key", "json_depth"],
            stringify,
        ),
        render_definition(),
    ]
}

fn parse(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
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
    decode_with_numbers(&json, &numbers, policy, call.new)
}

fn decode_with_numbers(
    json: &Json,
    numbers: &Type,
    policy: NumberPolicy,
    new: &mut Nursery<'_>,
) -> Result<Value, NativeError> {
    match json {
        Json::Number(token) => decode_number(token, numbers, policy, new),
        Json::Array(items) => {
            let values = items
                .iter()
                .map(|item| decode_with_numbers(item, numbers, policy, new))
                .collect::<Result<_, _>>()?;
            Ok(new.allocate(NewObject::List(values)))
        }
        Json::Object(fields) => {
            let values = fields
                .iter()
                .map(|(key, item)| {
                    let key = new.text(key);
                    Ok((key, decode_with_numbers(item, numbers, policy, new)?))
                })
                .collect::<Result<_, NativeError>>()?;
            Ok(new.allocate(NewObject::Record(values)))
        }
        _ => decode(json, &Type::Any, policy, new),
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
                Ok(())
            } else {
                out.push(&digits[..point])?;
                out.push(".")?;
                out.push(&digits[point..])
            }
        }
    }
}

fn render_definition() -> Function {
    definition(
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
