//! JSON reads its text in place, without an input copy.
use super::{Json, raise, room};
use lash_kernel_doc::{NativeError, NumberToken, Nursery, Retry};
use std::borrow::Cow;

struct Parser<'text, 'read, 'call> {
    input: &'text [u8],
    cursor: usize,
    new: &'read mut Nursery<'call>,
    depth_limit: usize,
    /// The room the first refused reservation asked for, and what the
    /// reservations after it would have added.
    refused: Option<(u64, u64)>,
}
impl<'text> Parser<'text, '_, '_> {
    fn byte(&mut self) -> Result<Option<u8>, NativeError> {
        Ok(self.input.get(self.cursor).copied())
    }
    /// A memory refusal stops building the tree while syntax validation
    /// continues, counting the room the rest would take. Infrastructure
    /// causes end the read unchanged.
    fn keep(&mut self, node: &Json<'_>, name: usize) -> Result<bool, NativeError> {
        let bytes = match node {
            Json::Text(text) => text.len(),
            Json::Number(token) => token.as_str().len(),
            _ => 0,
        };
        let bytes = room(bytes.saturating_add(name));
        if let Some((_, owed)) = &mut self.refused {
            *owed = owed.saturating_add(16).saturating_add(bytes);
            return Ok(false);
        }
        match self.new.reserve(1, bytes) {
            Ok(()) => Ok(true),
            Err(NativeError::Retry(Retry::Room { wanted })) => {
                self.refused = Some((wanted.0, 0));
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    fn whitespace(&mut self) -> Result<(), NativeError> {
        while self
            .byte()?
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.cursor += 1;
        }
        Ok(())
    }
    fn take(&mut self, byte: u8) -> Result<bool, NativeError> {
        self.whitespace()?;
        if self.byte()? == Some(byte) {
            self.cursor += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn string(&mut self) -> Result<Cow<'text, str>, NativeError> {
        self.whitespace()?;
        if !self.take(b'"')? {
            return Err(syntax());
        }
        let start = self.cursor - 1;
        let mut escaped = false;
        let mut plain = true;
        while let Some(byte) = self.byte()? {
            self.cursor += 1;
            if byte == b'"' && !escaped {
                if plain {
                    let text = std::str::from_utf8(&self.input[start + 1..self.cursor - 1])
                        .map_err(|_| syntax())?;
                    return Ok(Cow::Borrowed(text));
                }
                return serde_json::from_slice::<String>(&self.input[start..self.cursor])
                    .map(Cow::Owned)
                    .map_err(|_| syntax());
            }
            plain &= byte >= 0x20 && byte != b'\\';
            escaped = byte == b'\\' && !escaped;
        }
        Err(syntax())
    }
    fn value(&mut self, depth: usize) -> Result<Json<'text>, NativeError> {
        self.whitespace()?;
        if depth > self.depth_limit {
            return Err(raise(
                "json_depth",
                format!(
                    "JSON exceeds the configured {}-level kernel nesting limit",
                    self.depth_limit
                ),
            ));
        }
        match self.byte()? {
            Some(b'"') => self.string().map(Json::Text),
            Some(b'[') => {
                self.cursor += 1;
                let mut items = Vec::new();
                if self.take(b']')? {
                    return Ok(Json::Array(items));
                }
                loop {
                    let item = self.value(depth + 1)?;
                    if self.keep(&item, 0)? {
                        items.push(item);
                    }
                    if self.take(b']')? {
                        return Ok(Json::Array(items));
                    }
                    if !self.take(b',')? {
                        return Err(syntax());
                    }
                }
            }
            Some(b'{') => {
                self.cursor += 1;
                let mut fields: Vec<(Cow<'text, str>, Json<'text>)> = Vec::new();
                if self.take(b'}')? {
                    return Ok(Json::Object(fields));
                }
                loop {
                    let name = self.string()?;
                    if !self.take(b':')? {
                        return Err(syntax());
                    }
                    let value = self.value(depth + 1)?;
                    if let Some((_, old)) = fields.iter_mut().find(|(key, _)| *key == name) {
                        *old = value;
                    } else if self.keep(&value, name.len())? {
                        fields.push((name, value));
                    }
                    if self.take(b'}')? {
                        return Ok(Json::Object(fields));
                    }
                    if !self.take(b',')? {
                        return Err(syntax());
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.cursor;
                while let Some(byte) = self.byte()? {
                    if !matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                        break;
                    }
                    self.cursor += 1;
                }
                let token =
                    std::str::from_utf8(&self.input[start..self.cursor]).map_err(|_| syntax())?;
                NumberToken::new(token)
                    .map(Json::Number)
                    .map_err(|_| syntax())
            }
            Some(first) => {
                let (word, value): (&[u8], Json<'text>) = match first {
                    b'n' => (b"null", Json::Null),
                    b't' => (b"true", Json::Bool(true)),
                    b'f' => (b"false", Json::Bool(false)),
                    _ => return Err(syntax()),
                };
                for byte in word {
                    if self.byte()? != Some(*byte) {
                        return Err(syntax());
                    }
                    self.cursor += 1;
                }
                Ok(value)
            }
            None => Err(syntax()),
        }
    }
}
pub(super) fn syntax() -> NativeError {
    raise("json_syntax", "invalid JSON, including unpaired surrogates")
}
/// Reads strict JSON nesting at most `depth_limit` levels, reserving
/// each node it keeps in `new`.
pub(super) fn read<'text>(
    text: &'text str,
    depth_limit: usize,
    new: &mut Nursery<'_>,
) -> Result<Json<'text>, NativeError> {
    let mut parser = Parser {
        input: text.as_bytes(),
        cursor: 0,
        new,
        depth_limit: depth_limit.min(64),
        refused: None,
    };
    let json = parser.value(0)?;
    parser.whitespace()?;
    if parser.cursor != parser.input.len() {
        return Err(syntax());
    }
    parser.keep(&json, 0)?;
    if let Some((wanted, owed)) = parser.refused {
        return Err(Retry::Room {
            wanted: lash_kernel_doc::heap::LogicalBytes(wanted.saturating_add(owed)),
        }
        .into());
    }
    Ok(json)
}
