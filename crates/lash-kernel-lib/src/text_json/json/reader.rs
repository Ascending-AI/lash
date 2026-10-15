//! JSON reads paged text one bounded chunk at a time, without an input copy.
use super::{Json, raise, room};
use lash_kernel_doc::heap::{Chunk, TextRef};
use lash_kernel_doc::{NativeError, NativeHeap, NumberToken, Value};
use std::ops::ControlFlow;

enum Input<'a> {
    Borrowed(&'a [u8]),
    Paged {
        text: &'a TextRef,
        start: usize,
        chunk: Vec<u8>,
    },
}
impl Input<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Borrowed(bytes) => bytes.len(),
            Self::Paged { text, .. } => text.summary.utf8_bytes as usize,
        }
    }
    fn byte(&mut self, cursor: usize, heap: &dyn NativeHeap) -> Result<Option<u8>, NativeError> {
        if cursor >= self.len() {
            return Ok(None);
        }
        match self {
            Self::Borrowed(bytes) => Ok(bytes.get(cursor).copied()),
            Self::Paged { text, start, chunk } => {
                if cursor < *start || cursor >= *start + chunk.len() {
                    chunk.clear();
                    *start = cursor;
                    heap.payload_visit(
                        &Value::Text((*text).clone()),
                        cursor as u64..text.summary.utf8_bytes,
                        &mut |part| {
                            match part {
                                Chunk::Text(text) => chunk.extend_from_slice(text.as_bytes()),
                                Chunk::Bytes(_) => unreachable!("text payload"),
                            }
                            ControlFlow::Break(())
                        },
                    )?;
                }
                chunk
                    .get(cursor - *start)
                    .copied()
                    .map(Some)
                    .ok_or_else(|| {
                        lash_kernel_doc::heap::HeapFault::Malformed(
                            "text cursor has no payload chunk".into(),
                        )
                        .into()
                    })
            }
        }
    }
}
struct Parser<'a> {
    input: Input<'a>,
    cursor: usize,
    heap: &'a mut dyn NativeHeap,
    refused: bool,
}
impl Parser<'_> {
    fn byte(&mut self) -> Result<Option<u8>, NativeError> {
        self.input.byte(self.cursor, self.heap)
    }
    /// A memory refusal stops building the tree while syntax validation
    /// continues. Infrastructure causes end the read unchanged.
    fn keep(&mut self, node: &Json, name: usize) -> Result<bool, NativeError> {
        if self.refused {
            return Ok(false);
        }
        let bytes = match node {
            Json::Text(text) => text.len(),
            Json::Number(token) => token.as_str().len(),
            _ => 0,
        };
        match self.heap.reserve(1, room(bytes.saturating_add(name))) {
            Ok(()) => Ok(true),
            Err(NativeError::Memory) => {
                self.refused = true;
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
    fn string(&mut self) -> Result<String, NativeError> {
        self.whitespace()?;
        if !self.take(b'"')? {
            return Err(syntax());
        }
        let mut token = vec![b'"'];
        let mut escaped = false;
        while let Some(byte) = self.byte()? {
            self.cursor += 1;
            token.push(byte);
            if byte == b'"' && !escaped {
                return serde_json::from_slice(&token).map_err(|_| syntax());
            }
            escaped = byte == b'\\' && !escaped;
        }
        Err(syntax())
    }
    fn value(&mut self, depth: usize) -> Result<Json, NativeError> {
        self.whitespace()?;
        if depth > self.heap.data_depth_limit().min(64) {
            return Err(raise(
                "json_depth",
                &format!(
                    "JSON exceeds the configured {}-level kernel nesting limit",
                    self.heap.data_depth_limit().min(64)
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
                let mut fields: Vec<(String, Json)> = Vec::new();
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
                let mut token = Vec::new();
                while let Some(byte) = self.byte()? {
                    if !matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                        break;
                    }
                    token.push(byte);
                    self.cursor += 1;
                }
                let token = std::str::from_utf8(&token).map_err(|_| syntax())?;
                NumberToken::new(token)
                    .map(Json::Number)
                    .map_err(|_| syntax())
            }
            Some(first) => {
                let (word, value): (&[u8], Json) = match first {
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
fn read_input(input: Input<'_>, heap: &mut dyn NativeHeap) -> Result<Json, NativeError> {
    let mut parser = Parser {
        input,
        cursor: 0,
        heap,
        refused: false,
    };
    let json = parser.value(0)?;
    parser.whitespace()?;
    if parser.cursor != parser.input.len() {
        return Err(syntax());
    }
    if !parser.keep(&json, 0)? {
        return Err(NativeError::Memory);
    }
    Ok(json)
}
pub(super) fn read(text: &str, heap: &mut dyn NativeHeap) -> Result<Json, NativeError> {
    read_input(Input::Borrowed(text.as_bytes()), heap)
}
pub(super) fn read_descriptor(
    text: &TextRef,
    heap: &mut dyn NativeHeap,
) -> Result<Json, NativeError> {
    match &text.body {
        lash_kernel_doc::heap::Payload::Resident(text) => read(text, heap),
        _ => read_input(
            Input::Paged {
                text,
                start: 0,
                chunk: Vec::new(),
            },
            heap,
        ),
    }
}
