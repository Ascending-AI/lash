//! The canonical form identities are taken over (`K-ID-003`).
//!
//! The canonical form of a document or a definition is its JSON encoding
//! with every object's members sorted by name (as UTF-8 bytes), no white
//! space, and strings written with the shortest escapes: `\"`, `\\` and
//! `\u00XX` for a control character below U+0020, everything else as
//! itself. The encoding holds no fractional number: integers and floats are
//! strings.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A value that could not be written as JSON. No type of this crate fails.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot encode as JSON: {message}")]
pub struct EncodeError {
    pub message: String,
}

pub(crate) fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    #[cfg_attr(not(feature = "synthetic-next"), expect(unused_mut))]
    let mut value = serde_json::to_value(value).map_err(|error| EncodeError {
        message: error.to_string(),
    })?;
    #[cfg(feature = "synthetic-next")]
    crate::version::synthetic::spell(&mut value);
    let mut out = String::new();
    write_value(&value, &mut out);
    Ok(out.into_bytes())
}

pub(crate) fn digest<T: Serialize>(domain: &str, value: &T) -> Result<[u8; 32], EncodeError> {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical_bytes(value)?);
    Ok(hasher.finalize().into())
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut members: Vec<(&String, &Value)> = members.iter().collect();
            members.sort_by_key(|(name, _)| name.as_bytes());
            out.push('{');
            for (index, (name, member)) in members.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(name, out);
                out.push(':');
                write_value(member, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The deepest nesting of arrays and objects in a JSON text, counted without
/// recursion so that an over-deep text is refused before it is decoded.
pub(crate) fn json_depth(text: &str) -> usize {
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}
