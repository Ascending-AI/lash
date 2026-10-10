//! Token-preserving JSON (`K-EFF-005` to `K-EFF-007`).

use std::collections::BTreeSet;
use std::ops::ControlFlow;

use lash_kernel_doc::{
    Element, Float, Integer, MAX_NESTING_DEPTH, NativeCall, NativeError, NativeHeap, NumberPolicy,
    NumberToken, Object, ObjectId, Type, Value,
};
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use super::{Function, arg, definition, raise, room, text_arg};

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
            exact_integer(token.as_str(), heap).map(|integer| Value::Int(Integer::new(integer)))
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
        if depth > MAX_NESTING_DEPTH {
            return Err(raise("json_depth", "JSON exceeds the kernel nesting limit"));
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
        Json::Text(text) => Ok(Value::text(text.as_str())),
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
                Type::Tuple(_) => Ok(Value::Tuple(values.into())),
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
                    Ok((name.clone(), decode(item, ty, policy, heap)?))
                })
                .collect();
            let values = values?;
            if matches!(ty, Type::Map(_)) {
                Ok(Value::Map(
                    heap.allocate(Object::Map(
                        values
                            .into_iter()
                            .map(|(key, value)| (Value::text(key), value))
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
    parts: Vec<Value>,
    verbatim_kinds: u8,
    verbatim_field: String,
}

impl Out<'_> {
    fn push(&mut self, piece: &str) -> Result<(), NativeError> {
        self.heap.reserve(0, room(piece.len()).saturating_mul(2))?;
        self.text.push_str(piece);
        Ok(())
    }

    fn quote(&mut self, text: &str) -> Result<(), NativeError> {
        self.push(&serde_json::to_string(text).map_err(|_| syntax())?)
    }

    fn flush(&mut self) -> Result<(), NativeError> {
        if !self.text.is_empty() {
            self.heap.reserve(1, 0)?;
            self.parts.push(Value::text(std::mem::take(&mut self.text)));
        }
        Ok(())
    }

    fn number(
        &mut self,
        value: &Value,
        raw: bool,
        spelling: impl FnOnce() -> String,
    ) -> Result<(), NativeError> {
        if raw {
            self.flush()?;
            self.heap.reserve(1, 0)?;
            self.parts.push(value.clone());
            Ok(())
        } else {
            self.push(&spelling())
        }
    }

    fn raw_numbers(&self, kind: &str, inherited: bool) -> bool {
        inherited && self.verbatim_kinds & kind_bit(kind) == 0
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
    active: &mut BTreeSet<ObjectId>,
    depth: usize,
    raw_numbers: bool,
    out: &mut Out<'_>,
) -> Result<(), NativeError> {
    if depth > MAX_NESTING_DEPTH {
        return Err(raise("json_depth", "JSON exceeds the kernel nesting limit"));
    }
    match value {
        Value::Null => out.push("null")?,
        Value::Bool(value) => out.push(if *value { "true" } else { "false" })?,
        Value::Int(integer) => out.number(value, raw_numbers, || integer.to_string())?,
        Value::Float(float) if float.get().is_finite() => {
            out.number(value, raw_numbers, || float.to_string())?;
        }
        Value::Text(text) => out.quote(text)?,
        Value::Tuple(items) => {
            let raw_numbers = out.raw_numbers("tuple", raw_numbers);
            write_array(items, active, depth, raw_numbers, out)?;
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
                    ) => fields.push((key.to_string(), value.clone())),
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
            });
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
                    out.raw_numbers("record", raw_numbers)
                        && !fields.iter().any(|(name, value)| {
                            *name == out.verbatim_field && matches!(value, Value::Text(_))
                        })
                };
                out.push("{")?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        out.push(",")?;
                    }
                    out.quote(name)?;
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
    active: &mut BTreeSet<ObjectId>,
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
        parts: Vec::new(),
        verbatim_kinds: 0,
        verbatim_field: String::new(),
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
        definition(
            "json.render_parts",
            &[
                ("value", Type::Any),
                ("verbatim_kinds", Type::Set(Box::new(Type::Text))),
                ("verbatim_field", Type::Text),
            ],
            Type::List(Box::new(Type::Union(vec![Type::Text, Type::Number]))),
            &["json_number", "json_key", "json_depth"],
            render_parts,
        ),
    ]
}

fn parse(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let json = read(text_arg(call.args, 0)?, call.heap)?;
    let numbers = match text_arg(call.args, 1)? {
        "int" => Type::Int,
        "float" => Type::Float,
        "number" => Type::Number,
        _ => return Err(raise("type_error", "expected int, float or number")),
    };
    let policy = match text_arg(call.args, 2)? {
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
                        key.clone(),
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
    stringify_json(arg(call.args, 0)?, call.heap).map(Value::text)
}

fn render_parts(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let Some(Value::Set(kinds)) = call.args.get(1) else {
        return Err(raise("type_error", "JSON verbatim kinds must be a set"));
    };
    let mut verbatim_kinds = 0;
    let mut invalid = false;
    call.heap.visit(*kinds, &mut |element| {
        if let Element::Item(Value::Text(name)) = element {
            verbatim_kinds |= kind_bit(name);
            ControlFlow::Continue(())
        } else {
            invalid = true;
            ControlFlow::Break(())
        }
    });
    if invalid {
        return Err(raise("type_error", "JSON verbatim kinds must be text"));
    }
    let verbatim_field = text_arg(call.args, 2)?.to_owned();
    let mut out = Out {
        text: String::new(),
        heap: call.heap,
        parts: Vec::new(),
        verbatim_kinds,
        verbatim_field,
    };
    write(arg(call.args, 0)?, &mut BTreeSet::new(), 0, true, &mut out)?;
    out.flush()?;
    let parts = std::mem::take(&mut out.parts);
    Ok(Value::List(out.heap.allocate(Object::List(parts))?))
}
