//! Token-preserving JSON (`K-EFF-005` to `K-EFF-007`).

use std::collections::BTreeSet;
use std::ops::ControlFlow;

use lash_kernel_doc::heap::{HeapFault, IntRef, Payload, TextRef};
use lash_kernel_doc::{
    Element, EntityId, Float, Integer, NativeCall, NativeError, NativeHeap, NumberPolicy,
    NumberToken, Object, Type, Value,
};
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use super::{Function, arg, definition, materialized_text, raise, room};

/// Decodes one JSON number without passing integers through a float.
///
/// `Int` accepts any integral mathematical value, including fraction/exponent
/// spellings. `Float` rounds once to nearest binary64, ties to even, rejecting
/// infinity. `Number` and `Any` use `policy`. A union selects its first fitting
/// member. A wrong type, fractional integer or float overflow raises
/// `effect_result` (`K-EFF-005` to `K-EFF-007`). The digits an exponent adds
/// to an integer are reserved in `heap` before they are written; a refusal
/// ends with `NativeError::Memory`.
pub fn decode_number(
    token: &NumberToken,
    expected: &Type,
    policy: NumberPolicy,
    heap: &mut dyn NativeHeap,
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
                match decode_number(token, member, policy, heap) {
                    Ok(value) => return Ok(value),
                    Err(NativeError::Raised(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            return Err(mismatch());
        }
        other => other,
    };
    match selected {
        Type::Int => {
            let integer = exact_integer(token.as_str(), heap)?;
            Ok(Value::Int(heap.int(Integer::new(integer))?))
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

fn exact_integer(token: &str, heap: &mut dyn NativeHeap) -> Result<BigInt, NativeError> {
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
        let add = shift.to_usize().ok_or(NativeError::Memory)?;
        heap.reserve(0, room(add).saturating_mul(2))?;
        digits.try_reserve(add).map_err(|_| NativeError::Memory)?;
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

/// Reads JSON text into a tree that can be many times the size of the
/// text (`[0,0,...]` holds a node for every two bytes). Each node is
/// reserved in `heap`, as one value and the bytes of its text, before it
/// joins the tree. After a refusal the parser stops building and reads on,
/// so a syntax or depth error later in the text is still the error.
struct Parser<'a> {
    text: &'a str,
    cursor: usize,
    heap: &'a mut dyn NativeHeap,
    refused: bool,
}

impl<'a> Parser<'a> {
    /// Whether `node`, with `name` bytes of field name, may join the tree.
    fn keep(&mut self, node: &Json, name: usize) -> bool {
        let bytes = match node {
            Json::Text(text) => text.len(),
            Json::Number(token) => token.as_str().len(),
            _ => 0,
        };
        self.refused = self.refused
            || self
                .heap
                .reserve(1, room(bytes.saturating_add(name)))
                .is_err();
        !self.refused
    }

    fn whitespace(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.cursor)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.cursor += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.text.as_bytes().get(self.cursor) == Some(&byte) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Result<String, NativeError> {
        self.whitespace();
        let start = self.cursor;
        if !self.take(b'"') {
            return Err(syntax());
        }
        let mut escaped = false;
        while let Some(byte) = self.text.as_bytes().get(self.cursor) {
            self.cursor += 1;
            if *byte == b'"' && !escaped {
                return serde_json::from_str(&self.text[start..self.cursor]).map_err(|_| syntax());
            }
            escaped = *byte == b'\\' && !escaped;
        }
        Err(syntax())
    }

    fn value(&mut self, depth: usize) -> Result<Json, NativeError> {
        self.whitespace();
        if depth > self.heap.data_depth_limit().min(64) {
            return Err(raise(
                "json_depth",
                &format!(
                    "JSON exceeds the configured {}-level kernel nesting limit",
                    self.heap.data_depth_limit().min(64)
                ),
            ));
        }
        match self.text.as_bytes().get(self.cursor) {
            Some(b'"') => self.string().map(Json::Text),
            Some(b'[') => {
                self.cursor += 1;
                let mut items = Vec::new();
                if self.take(b']') {
                    return Ok(Json::Array(items));
                }
                loop {
                    let item = self.value(depth + 1)?;
                    if self.keep(&item, 0) {
                        items.push(item);
                    }
                    if self.take(b']') {
                        return Ok(Json::Array(items));
                    }
                    if !self.take(b',') {
                        return Err(syntax());
                    }
                }
            }
            Some(b'{') => {
                self.cursor += 1;
                let mut fields: Vec<(String, Json)> = Vec::new();
                if self.take(b'}') {
                    return Ok(Json::Object(fields));
                }
                loop {
                    let name = self.string()?;
                    if !self.take(b':') {
                        return Err(syntax());
                    }
                    let value = self.value(depth + 1)?;
                    if let Some((_, old)) = fields.iter_mut().find(|(key, _)| *key == name) {
                        *old = value;
                    } else if self.keep(&value, name.len()) {
                        fields.push((name, value));
                    }
                    if self.take(b'}') {
                        return Ok(Json::Object(fields));
                    }
                    if !self.take(b',') {
                        return Err(syntax());
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.cursor;
                while self
                    .text
                    .as_bytes()
                    .get(self.cursor)
                    .is_some_and(|b| matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
                {
                    self.cursor += 1;
                }
                NumberToken::new(&self.text[start..self.cursor])
                    .map(Json::Number)
                    .map_err(|_| syntax())
            }
            _ => {
                for (word, value) in [
                    ("null", Json::Null),
                    ("true", Json::Bool(true)),
                    ("false", Json::Bool(false)),
                ] {
                    if self.text[self.cursor..].starts_with(word) {
                        self.cursor += word.len();
                        return Ok(value);
                    }
                }
                Err(syntax())
            }
        }
    }
}

fn syntax() -> NativeError {
    raise("json_syntax", "invalid JSON, including unpaired surrogates")
}

fn read(text: &str, heap: &mut dyn NativeHeap) -> Result<Json, NativeError> {
    let mut parser = Parser {
        text,
        cursor: 0,
        heap,
        refused: false,
    };
    let json = parser.value(0)?;
    parser.whitespace();
    if parser.cursor != text.len() {
        return Err(syntax());
    }
    if !parser.keep(&json, 0) {
        return Err(NativeError::Memory);
    }
    Ok(json)
}

fn fits(
    json: &Json,
    ty: &Type,
    policy: NumberPolicy,
    heap: &mut dyn NativeHeap,
) -> Result<(), NativeError> {
    if let Type::Union(members) = ty {
        for member in members {
            match fits(json, member, policy, heap) {
                Ok(()) => return Ok(()),
                Err(NativeError::Raised(_)) => {}
                Err(error) => return Err(error),
            }
        }
        return Err(mismatch());
    }
    match (json, ty) {
        (Json::Number(token), _) => {
            decode_number(token, ty, policy, heap)?;
        }
        (Json::Null, Type::Null | Type::Any)
        | (Json::Bool(_), Type::Bool | Type::Any)
        | (Json::Text(_), Type::Text | Type::Any) => {}
        (Json::Text(text), Type::Enum(members)) if members.contains(text) => {}
        (Json::Array(items), Type::Tuple(types)) if items.len() == types.len() => {
            for (item, ty) in items.iter().zip(types) {
                fits(item, ty, policy, heap)?;
            }
        }
        (Json::Array(items), Type::List(element)) => {
            for item in items {
                fits(item, element, policy, heap)?;
            }
        }
        (Json::Array(items), Type::Any) => {
            for item in items {
                fits(item, &Type::Any, policy, heap)?;
            }
        }
        (Json::Object(fields), Type::Any) => {
            for (_, value) in fields {
                fits(value, &Type::Any, policy, heap)?;
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
                fits(value, ty, policy, heap)?;
            }
        }
        (Json::Object(fields), Type::Map(map)) => {
            for (name, value) in fields {
                fits(&Json::Text(name.clone()), &map.key, policy, heap)?;
                fits(value, &map.value, policy, heap)?;
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
    heap: &mut dyn NativeHeap,
) -> Result<Value, NativeError> {
    if let Type::Union(members) = ty {
        for member in members {
            match fits(json, member, policy, heap) {
                Ok(()) => return decode(json, member, policy, heap),
                Err(NativeError::Raised(_)) => {}
                Err(error) => return Err(error),
            }
        }
        return Err(mismatch());
    }
    match json {
        Json::Null => Ok(Value::Null),
        Json::Bool(value) => Ok(Value::Bool(*value)),
        Json::Text(text) => Ok(Value::Text(heap.text(text)?)),
        Json::Number(token) => decode_number(token, ty, policy, heap),
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
                    decode(item, ty, policy, heap)
                })
                .collect();
            let values = values?;
            match ty {
                Type::Tuple(_) => Ok(Value::Tuple(heap.tuple(values)?)),
                _ => Ok(Value::List(heap.allocate(Object::List(values))?)),
            }
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
                    Ok((heap.text(name)?, decode(item, ty, policy, heap)?))
                })
                .collect();
            let values = values?;
            if matches!(ty, Type::Map(_)) {
                Ok(Value::Map(
                    heap.allocate(Object::Map(
                        values
                            .into_iter()
                            .map(|(key, value)| (Value::Text(key), value))
                            .collect(),
                    ))?,
                ))
            } else {
                Ok(Value::Record(heap.allocate(Object::Record(values))?))
            }
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
    heap: &mut dyn NativeHeap,
) -> Result<Value, NativeError> {
    let json = read(text, heap)?;
    fits(&json, expected, policy, heap)?;
    decode(&json, expected, policy, heap)
}

/// The text `stringify` writes. A shared object is written once for every
/// place that holds it, so the text can outgrow the value: each piece is
/// reserved, as text and as the buffer it is built in, before it is appended.
struct Out<'a> {
    text: String,
    heap: &'a mut dyn NativeHeap,
    layout: Option<DecimalLayout>,
    verbatim_kinds: u8,
    verbatim_field: Option<String>,
}

impl Out<'_> {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.heap.reserve(0, room(piece.len()).saturating_mul(2))?;
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
                Value::Int(number) => {
                    if let IntRef::Inline(integer) = number
                        && let Some(value) = integer.to_i64()
                    {
                        return layout.small_integer(value.unsigned_abs(), value < 0, self);
                    }
                    // Reserve decimal scratch before converting arbitrary-precision
                    // integers; three digits per byte bounds their decimal extent.
                    self.heap.reserve(
                        0,
                        (match number {
                            IntRef::Inline(value) => value.bits(),
                            IntRef::Paged { summary, .. } => summary.bits,
                        })
                        .div_ceil(8)
                        .saturating_mul(3)
                        .saturating_add(1),
                    )?;
                    let text = integer_text(number, self.heap)?;
                    let digits = text.strip_prefix('-').unwrap_or(&text);
                    let trimmed = digits.trim_end_matches('0');
                    let (digits, exponent) = if trimmed.is_empty() {
                        ("0", 0)
                    } else {
                        (
                            trimmed,
                            i64::try_from(digits.len() - trimmed.len())
                                .map_err(|_| NativeError::Memory)?,
                        )
                    };
                    return layout.write(text.starts_with('-'), digits, exponent, self);
                }
                _ => unreachable!("the JSON writer selects only numbers"),
            }
        }
        match value {
            Value::Int(number) => {
                let text = integer_text(number, self.heap)?;
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

fn text_copy(
    text: &TextRef,
    heap: &mut dyn NativeHeap,
) -> Result<std::sync::Arc<str>, NativeError> {
    match &text.body {
        Payload::Resident(value) => Ok(value.clone()),
        Payload::Paged(_) => heap.materialize_text(text),
    }
}

fn inline_i64(int: &IntRef) -> Option<i64> {
    match int {
        IntRef::Inline(value) => value.to_i64(),
        IntRef::Paged { .. } => None,
    }
}

fn integer_text(int: &IntRef, heap: &mut dyn NativeHeap) -> Result<String, NativeError> {
    match int {
        IntRef::Inline(value) => Ok(value.to_string()),
        IntRef::Paged { .. } => Ok(heap.materialize_int(int)?.to_string()),
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
    active: &mut BTreeSet<EntityId>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_>,
) -> Result<(), NativeError> {
    if depth > out.heap.data_depth_limit().min(64) {
        return Err(raise(
            "json_depth",
            &format!(
                "JSON exceeds the configured {}-level kernel nesting limit",
                out.heap.data_depth_limit().min(64)
            ),
        ));
    }
    match value {
        Value::Null => out.push("null")?,
        Value::Bool(value) => out.push(if *value { "true" } else { "false" })?,
        Value::Int(_) => out.number(value, raw_numbers)?,
        Value::Float(float) if float.get().is_finite() => {
            out.number(value, raw_numbers)?;
        }
        Value::Text(text) => {
            let text = text_copy(text, out.heap)?;
            out.quote(&text)?;
        }
        Value::Tuple(items) => {
            let raw_numbers = out.raw_numbers("tuple", raw_numbers);
            out.push("[")?;
            for index in 0..items.summary.len {
                if index != 0 {
                    out.push(",")?;
                }
                let value = out.heap.tuple_get(items, index)?.ok_or_else(|| {
                    HeapFault::Malformed("tuple descriptor extent has no member".into())
                })?;
                write(&value, active, depth + 1, raw_numbers, out)?;
            }
            out.push("]")?;
        }
        Value::List(id) | Value::Map(id) | Value::Record(id) => {
            if !active.insert(*id) {
                return Err(raise("cycle", "cyclic value cannot be JSON"));
            }
            let mut items = Vec::new();
            let mut fields = Vec::new();
            let mut error = None;
            out.heap.visit(*id, &mut |element| {
                match (value, element) {
                    (Value::List(_), Element::Item(value)) => items.push(value.clone()),
                    (Value::Record(_), Element::Field { name, value }) => {
                        fields.push((name.to_owned(), value.clone()))
                    }
                    (
                        Value::Map(_),
                        Element::Entry {
                            key: Value::Text(key),
                            value,
                        },
                    ) => fields.push((key.clone(), value.clone())),
                    (Value::Map(_), Element::Entry { .. }) => {
                        error = Some(raise("json_key", "JSON map keys must be text"));
                        return ControlFlow::Break(());
                    }
                    _ => {
                        error = Some(raise("not_data", "invalid heap object for JSON"));
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            })?;
            if let Some(error) = error {
                return Err(error);
            }
            if matches!(value, Value::List(_)) {
                let raw_numbers = out.raw_numbers("list", raw_numbers);
                write_array(&items, active, depth, raw_numbers, out)?;
            } else {
                let raw_numbers = if matches!(value, Value::Map(_)) {
                    out.raw_numbers("map", raw_numbers)
                } else {
                    let mut verbatim = false;
                    if let Some(field) = &out.verbatim_field {
                        for (name, value) in &fields {
                            if matches!(value, Value::Text(_))
                                && out.heap.equal(
                                    &Value::Text(name.clone()),
                                    &Value::text(field.as_str()),
                                )?
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
                    let name = text_copy(name, out.heap)?;
                    out.quote(&name)?;
                    out.push(":")?;
                    write(value, active, depth + 1, raw_numbers, out)?;
                }
                out.push("}")?;
            }
            active.remove(id);
        }
        Value::Float(_) => return Err(raise("json_number", "JSON refuses NaN and infinity")),
        _ => return Err(raise("not_data", "value kind has no JSON representation")),
    }
    Ok(())
}

fn write_array(
    items: &[Value],
    active: &mut BTreeSet<EntityId>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_>,
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
/// The text is reserved in `heap` as it grows; a refusal ends with
/// `NativeError::Memory`.
pub fn stringify_json(value: &Value, heap: &mut dyn NativeHeap) -> Result<String, NativeError> {
    let mut out = Out {
        text: String::new(),
        heap,
        layout: None,
        verbatim_kinds: 0,
        verbatim_field: None,
    };
    write(value, &mut BTreeSet::new(), 0, false, &mut out)?;
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

fn parse(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let json = read(&materialized_text(call.args, 0, call.heap)?, call.heap)?;
    let numbers = match materialized_text(call.args, 1, call.heap)?.as_ref() {
        "int" => Type::Int,
        "float" => Type::Float,
        "number" => Type::Number,
        _ => return Err(raise("type_error", "expected int, float or number")),
    };
    let policy = match materialized_text(call.args, 2, call.heap)?.as_ref() {
        "by_spelling" => NumberPolicy::BySpelling,
        "float" => NumberPolicy::Float,
        _ => return Err(raise("type_error", "expected a number policy")),
    };
    decode_with_numbers(&json, &numbers, policy, call.heap)
}

fn decode_with_numbers(
    json: &Json,
    numbers: &Type,
    policy: NumberPolicy,
    heap: &mut dyn NativeHeap,
) -> Result<Value, NativeError> {
    match json {
        Json::Number(token) => decode_number(token, numbers, policy, heap),
        Json::Array(items) => {
            let values = items
                .iter()
                .map(|item| decode_with_numbers(item, numbers, policy, heap))
                .collect::<Result<_, _>>()?;
            Ok(Value::List(heap.allocate(Object::List(values))?))
        }
        Json::Object(fields) => {
            let values = fields
                .iter()
                .map(|(key, item)| {
                    Ok((
                        heap.text(key)?,
                        decode_with_numbers(item, numbers, policy, heap)?,
                    ))
                })
                .collect::<Result<_, NativeError>>()?;
            Ok(Value::Record(heap.allocate(Object::Record(values))?))
        }
        _ => decode(json, &Type::Any, policy, heap),
    }
}

fn stringify(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let text = stringify_json(arg(call.args, 0)?, call.heap)?;
    Ok(Value::Text(call.heap.text(&text)?))
}

fn render(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let Some(Value::Set(kinds)) = call.args.get(1) else {
        return Err(raise("type_error", "JSON verbatim kinds must be a set"));
    };
    let mut verbatim_kinds = 0;
    let mut invalid = false;
    let mut names = Vec::new();
    call.heap.visit(*kinds, &mut |element| {
        if let Element::Item(Value::Text(name)) = element {
            names.push(name.clone());
            ControlFlow::Continue(())
        } else {
            invalid = true;
            ControlFlow::Break(())
        }
    })?;
    for name in names {
        verbatim_kinds |= kind_bit(&text_copy(&name, call.heap)?);
    }
    if invalid {
        return Err(raise("type_error", "JSON verbatim kinds must be text"));
    }
    let verbatim_field = match arg(call.args, 2)? {
        Value::Absent => None,
        Value::Text(field) => Some(text_copy(field, call.heap)?.to_string()),
        _ => {
            return Err(raise(
                "type_error",
                "JSON verbatim field must be text or absent",
            ));
        }
    };
    let layout = DecimalLayout::read(arg(call.args, 3)?, call.heap)?;
    let mut out = Out {
        text: String::new(),
        heap: call.heap,
        layout: Some(layout),
        verbatim_kinds,
        verbatim_field,
    };
    write(arg(call.args, 0)?, &mut BTreeSet::new(), 0, true, &mut out)?;
    Ok(Value::Text(out.heap.text(&out.text)?))
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
        out: &mut Out<'_>,
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

    fn read(value: &Value, heap: &dyn NativeHeap) -> Result<Self, NativeError> {
        let Value::Tuple(items) = value else {
            return Err(raise("type_error", "expected decimal layout tuple"));
        };
        let values = (0..items.summary.len.min(5))
            .map(|index| heap.tuple_get(items, index))
            .collect::<Result<Vec<_>, _>>()?;
        let [
            Some(Value::Int(from)),
            Some(Value::Int(until)),
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
        out: &mut Out<'_>,
    ) -> Result<(), NativeError> {
        if negative && (digits != "0" || self.negative_zero) {
            out.push("-")?;
        }
        let point = i64::try_from(digits.len())
            .map_err(|_| NativeError::Memory)?
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
            let point = usize::try_from(point).map_err(|_| NativeError::Memory)?;
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
