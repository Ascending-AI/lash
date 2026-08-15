//! JSON encoding and decoding for the JavaScript dialect.
//!
//! Split from the method-dispatch table because it is a different concern with
//! a different hazard: `serde_json` cannot hold a non-finite number, so an
//! out-of-range literal has to be rewritten out of the document before parsing
//! and restored afterwards. That rewrite-and-restore pair is only correct as a
//! unit, so it lives together, away from the per-method dispatch that merely
//! calls into it.

use super::super::{javascript_to_string, to_json_direct};
use super::javascript::{ecma_record_entries, js_stdlib_error};
use super::*;

pub(super) fn javascript_json_stringify(value: &Value) -> Result<String, RuntimeError> {
    match value {
        Value::Null | Value::Undefined => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) if !value.is_finite() => Ok("null".to_string()),
        Value::Number(_) => Ok(javascript_to_string(value)),
        Value::String(_) | Value::Image(_) | Value::Resource(_) => {
            serde_json::to_string(&to_json_direct(value))
                .map_err(|error| js_stdlib_error(format!("JSON.stringify: {error}")))
        }
        Value::Tuple(values) | Value::List(values) => {
            let values = values
                .iter()
                .map(javascript_json_stringify)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", values.join(",")))
        }
        Value::Record(record) => {
            let entries = ecma_record_entries(record)
                .into_iter()
                .filter(|(_, value)| !matches!(value, Value::Undefined))
                .map(|(key, value)| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("record keys are JSON strings"),
                        javascript_json_stringify(value)?
                    ))
                })
                .collect::<Result<Vec<_>, RuntimeError>>()?;
            Ok(format!("{{{}}}", entries.join(",")))
        }
        Value::Projected(_) | Value::Ref(_) => Err(js_stdlib_error(
            "JSON.stringify received an unexported value",
        )),
    }
}

enum OrderedJsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl<'de> serde::Deserialize<'de> for OrderedJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = OrderedJsonValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value as f64))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value as f64))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(OrderedJsonValue::String(value.to_string()))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Null)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(OrderedJsonValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((key, value)) = map.next_entry()? {
                    values.push((key, value));
                }
                Ok(OrderedJsonValue::Object(values))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

pub(super) fn parse_javascript_json(source: &str) -> Result<Value, RuntimeError> {
    match serde_json::from_str::<OrderedJsonValue>(source) {
        Ok(value) => Ok(ordered_json_to_value(value)),
        Err(error) => {
            // ECMA parses a JSON number as an IEEE double, so a magnitude past
            // the double range becomes an infinity — `JSON.parse` never fails
            // on range. serde_json's tokenizer rejects those instead, which
            // turned host data containing a large number into a failed cell.
            // Underflow already agrees (`1e-400` parses as `0`), so only the
            // overflowing tokens are rewritten, and only outside strings.
            let syntax_error = || js_stdlib_error(format!("JSON.parse: {error}"));
            let Some(planted) = rewrite_overflowing_json_numbers(source) else {
                return Err(syntax_error());
            };
            let value = serde_json::from_str::<OrderedJsonValue>(&planted.source)
                .map_err(|_| syntax_error())?;
            let mut value = ordered_json_to_value(value);
            let mut restored = 0;
            restore_clamped_json_infinities(&mut value, &planted.marker, &mut restored);
            // The marker is derived from the document, so guest data cannot
            // carry it without containing its own digest. Counting the
            // substitutions closes the class anyway: if the document somehow
            // held one, the counts diverge and the parse fails rather than
            // handing back reinterpreted guest data.
            if restored != planted.count {
                return Err(syntax_error());
            }
            Ok(value)
        }
    }
}

/// A rewritten document, plus what it takes to undo the rewrite exactly.
struct PlantedJsonOverflows {
    source: String,
    /// The object key standing in for an out-of-range number. Derived from the
    /// document, so a guest object can only collide by containing a digest of
    /// the very document it sits in.
    marker: String,
    /// How many markers were planted. The restore must consume exactly this
    /// many.
    count: usize,
}

/// Replace every out-of-range JSON number with a signed marker object.
///
/// Returns `None` when the source has no such number, so an ordinary syntax
/// error keeps its original diagnostic. Copying is by string slice: an earlier
/// version pushed raw bytes as `char`, which reinterpreted every UTF-8
/// continuation byte as Latin-1 and mojibaked every non-ASCII character in any
/// document that happened to contain one overflowing number.
fn rewrite_overflowing_json_numbers(source: &str) -> Option<PlantedJsonOverflows> {
    let marker = json_overflow_marker(source);
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    let mut count = 0;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }
        let starts_number = byte.is_ascii_digit()
            || (byte == b'-' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit));
        if !starts_number {
            index += 1;
            continue;
        }
        let start = index;
        if bytes[index] == b'-' {
            index += 1;
        }
        while index < bytes.len() {
            let current = bytes[index];
            if current.is_ascii_digit() || matches!(current, b'.' | b'e' | b'E') {
                index += 1;
                continue;
            }
            // A sign continues a number only straight after an exponent marker.
            if matches!(current, b'+' | b'-') && matches!(bytes[index - 1], b'e' | b'E') {
                index += 1;
                continue;
            }
            break;
        }
        let token = &source[start..index];
        if let Ok(number) = token.parse::<f64>()
            && number.is_infinite()
        {
            // Copy everything since the last plant as text, never as bytes.
            out.push_str(&source[copied..start]);
            let sign = if number.is_sign_negative() { -1 } else { 1 };
            out.push_str(&format!("{{\"{marker}\":{sign}}}"));
            copied = index;
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    out.push_str(&source[copied..]);
    Some(PlantedJsonOverflows {
        source: out,
        marker,
        count,
    })
}

/// A marker keyed to this document. Guest data can only collide with it by
/// containing a digest of itself, which no document can be written to do.
fn json_overflow_marker(source: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    format!("__lash_json_overflow_{:016x}__", hasher.finish())
}

/// Turn the planted markers back into the infinities ECMA produces, counting
/// each substitution so the caller can check none was missed or invented.
fn restore_clamped_json_infinities(value: &mut Value, marker: &str, restored: &mut usize) {
    match value {
        Value::Record(entries) => {
            if entries.len() == 1
                && let Some(Value::Number(sign)) = entries.get(marker)
            {
                *value = Value::Number(if *sign < 0.0 {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                });
                *restored += 1;
                return;
            }
            let mut replaced = entries.as_ref().clone();
            for entry in replaced.entries.iter_mut() {
                restore_clamped_json_infinities(&mut entry.value, marker, restored);
            }
            *entries = std::sync::Arc::new(replaced);
        }
        Value::List(items) => {
            let mut replaced = items.to_vec();
            for item in replaced.iter_mut() {
                restore_clamped_json_infinities(item, marker, restored);
            }
            *items = replaced.into();
        }
        _ => {}
    }
}

fn ordered_json_to_value(value: OrderedJsonValue) -> Value {
    match value {
        OrderedJsonValue::Null => Value::Null,
        OrderedJsonValue::Bool(value) => Value::Bool(value),
        OrderedJsonValue::Number(value) => Value::Number(value),
        OrderedJsonValue::String(value) => Value::String(value.into()),
        OrderedJsonValue::Array(values) => Value::List(
            values
                .into_iter()
                .map(ordered_json_to_value)
                .collect::<Vec<_>>()
                .into(),
        ),
        OrderedJsonValue::Object(entries) => Value::Record(std::sync::Arc::new(
            entries
                .into_iter()
                .map(|(key, value)| (key, ordered_json_to_value(value)))
                .collect(),
        )),
    }
}
