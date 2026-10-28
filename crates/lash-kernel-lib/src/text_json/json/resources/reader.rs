//! Token-preserving input model for logical JSON descriptions.
use super::super::reader::syntax;
use super::super::{Json, raise};
use lash_kernel_doc::{NativeError, NumberToken};
use std::borrow::Cow;

struct Parser<'text> {
    input: &'text [u8],
    cursor: usize,
    depth_limit: usize,
}
impl<'text> Parser<'text> {
    fn byte(&mut self) -> Result<Option<u8>, NativeError> {
        Ok(self.input.get(self.cursor).copied())
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
                    items.push(item);
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
                    } else {
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
                if !NumberToken::is_valid(token) {
                    return Err(syntax());
                }
                Ok(Json::Number(token))
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
pub(super) fn read<'text>(
    text: &'text str,
    depth_limit: usize,
) -> Result<Json<'text>, NativeError> {
    let mut parser = Parser {
        input: text.as_bytes(),
        cursor: 0,
        depth_limit: depth_limit.min(64),
    };
    let json = parser.value(0)?;
    parser.whitespace()?;
    if parser.cursor != parser.input.len() {
        return Err(syntax());
    }
    Ok(json)
}
