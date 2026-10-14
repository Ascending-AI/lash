use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeSet;
use std::fmt;

struct Unique;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        decoder.deserialize_any(Self)
    }
}
impl<'de> Visitor<'de> for Unique {
    type Value = Self;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON with unique object fields")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self, M::Error> {
        let mut seen = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(serde::de::Error::custom("duplicate metadata field"));
            }
            map.next_value::<Self>()?;
        }
        Ok(Self)
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self, S::Error> {
        while seq.next_element::<Self>()?.is_some() {}
        Ok(Self)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(Self)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(Self)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(Self)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(Self)
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(Self)
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
        Ok(Self)
    }
}
/// Reject duplicate object fields anywhere in generated-source JSON metadata.
///
/// Run this before serde decoding, which would erase duplicate fields. The
/// caller must bound JSON nesting before this recursive check and owns diagnostic
/// conversion; the check also rejects trailing input.
pub fn check_metadata_uniqueness(text: &str) -> Result<(), serde_json::Error> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    Unique::deserialize(&mut decoder)?;
    decoder.end()
}

/// Convert only unquoted scalar tokens between native metadata and JSON.
/// Strings, keys, escapes and whitespace retain their exact bytes. The caller
/// bounds the input before conversion and validates the result as metadata.
pub fn metadata_json(text: &str, format: crate::SourceFormat) -> String {
    literal_tokens(text, format, true)
}

pub(crate) fn literal_tokens(text: &str, format: crate::SourceFormat, to_json: bool) -> String {
    if format == crate::SourceFormat::TypeScriptV1 {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if quoted {
            if escaped {
                escaped = false;
            } else if bytes[i] == b'\\' {
                escaped = true;
            } else if bytes[i] == b'"' {
                quoted = false;
            }
            i += 1;
        } else if bytes[i] == b'"' {
            quoted = true;
            i += 1;
        } else if bytes[i].is_ascii_alphabetic() {
            let word_start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word = &text[word_start..i];
            let replacement = match (to_json, word) {
                (true, "True") => "true",
                (true, "False") => "false",
                (true, "None") => "null",
                (false, "true") => "True",
                (false, "false") => "False",
                (false, "null") => "None",
                _ => word,
            };
            output.push_str(&text[start..word_start]);
            output.push_str(replacement);
            start = i;
        } else {
            i += 1;
        }
    }
    output.push_str(&text[start..]);
    output
}
