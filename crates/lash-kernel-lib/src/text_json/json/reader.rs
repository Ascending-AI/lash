//! JSON reads its text in place, without an input copy. One validating scan
//! writes the document as a flat tape, which a decode then walks once.
use super::{raise, room};
use lash_kernel_doc::{NativeError, NumberToken, Nursery, Retry};
use std::borrow::Cow;

/// One value of a read document. The tape holds the values in document
/// order, each container before its members.
enum Node<'text> {
    Null,
    Bool(bool),
    Number(&'text str),
    Text(Cow<'text, str>),
    /// A list of `len` items. The first item follows it. Each later item
    /// follows the end of the one before. Its members end before `end`.
    Array {
        len: usize,
        end: usize,
    },
    /// An object whose distinct fields are `fields[first..first + len]`. Its
    /// members, including values a later duplicate replaced, end before
    /// `end`.
    Object {
        first: usize,
        len: usize,
        end: usize,
    },
}

/// A document read and validated whole: no tape exists for a text with a
/// syntax error anywhere, or one whose reservations were refused.
pub(super) struct Tape<'text> {
    nodes: Vec<Node<'text>>,
    /// Each object's distinct names, in the order they first appear, each
    /// with the tape index of the last value given for it.
    fields: Vec<(Cow<'text, str>, usize)>,
    #[cfg(test)]
    _held: crate::native_contract_laws::ScratchLease,
}

impl Tape<'_> {
    /// The document's value.
    pub(super) fn root(&self) -> At<'_> {
        At {
            tape: self,
            index: 0,
        }
    }
}

/// One value of a tape.
#[derive(Clone, Copy)]
pub(super) struct At<'t> {
    tape: &'t Tape<'t>,
    index: usize,
}

/// What a tape value is, its members borrowed from the tape.
pub(super) enum View<'t> {
    Null,
    Bool(bool),
    Number(&'t str),
    Text(&'t str),
    Array(Items<'t>),
    Object(Fields<'t>),
}

impl<'t> At<'t> {
    pub(super) fn view(self) -> View<'t> {
        let tape = self.tape;
        match &tape.nodes[self.index] {
            Node::Null => View::Null,
            Node::Bool(value) => View::Bool(*value),
            Node::Number(token) => View::Number(token),
            Node::Text(text) => View::Text(text),
            Node::Array { len, .. } => View::Array(Items {
                tape,
                next: self.index + 1,
                left: *len,
            }),
            Node::Object { first, len, .. } => View::Object(Fields {
                tape,
                fields: tape.fields[*first..*first + *len].iter(),
            }),
        }
    }
    /// The tape index past this value's members.
    fn end(self) -> usize {
        match self.tape.nodes[self.index] {
            Node::Array { end, .. } | Node::Object { end, .. } => end,
            _ => self.index + 1,
        }
    }
}

/// A list's items, in order.
#[derive(Clone)]
pub(super) struct Items<'t> {
    tape: &'t Tape<'t>,
    next: usize,
    left: usize,
}
impl<'t> Iterator for Items<'t> {
    type Item = At<'t>;
    fn next(&mut self) -> Option<At<'t>> {
        self.left = self.left.checked_sub(1)?;
        let item = At {
            tape: self.tape,
            index: self.next,
        };
        self.next = item.end();
        Some(item)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}
impl ExactSizeIterator for Items<'_> {}

/// An object's distinct fields, in the order they first appear.
#[derive(Clone)]
pub(super) struct Fields<'t> {
    tape: &'t Tape<'t>,
    fields: std::slice::Iter<'t, (Cow<'t, str>, usize)>,
}
impl<'t> Iterator for Fields<'t> {
    type Item = (&'t str, At<'t>);
    fn next(&mut self) -> Option<Self::Item> {
        let (name, index) = self.fields.next()?;
        Some((
            name,
            At {
                tape: self.tape,
                index: *index,
            },
        ))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.fields.size_hint()
    }
}
impl ExactSizeIterator for Fields<'_> {}

struct Parser<'text, R> {
    /// The text, and its bytes: every slice the parser takes ends at an
    /// ASCII delimiter, so it is the text's own UTF-8.
    text: &'text str,
    input: &'text [u8],
    cursor: usize,
    reserve: R,
    depth_limit: usize,
    /// The room the first refused reservation asked for, and what the
    /// reservations after it would have added.
    refused: Option<(u64, u64)>,
    nodes: Vec<Node<'text>>,
    fields: Vec<(Cow<'text, str>, usize)>,
    /// The fields of the objects being read, innermost last.
    open: Vec<(Cow<'text, str>, usize)>,
    #[cfg(test)]
    held: crate::native_contract_laws::ScratchLease,
}
impl<'text, R: FnMut(u64, u64) -> Result<(), NativeError>> Parser<'text, R> {
    fn byte(&self) -> Option<u8> {
        self.input.get(self.cursor).copied()
    }
    /// Reserves a finished value of `bytes` payload, named by `name` bytes
    /// when it is an object field. A memory refusal stops the tape while
    /// syntax validation continues, counting the room the rest would take.
    /// Infrastructure causes end the read unchanged.
    fn keep(&mut self, bytes: usize, name: usize) -> Result<bool, NativeError> {
        let bytes = room(bytes.saturating_add(name));
        if let Some((_, owed)) = &mut self.refused {
            *owed = owed.saturating_add(16).saturating_add(bytes);
            return Ok(false);
        }
        match (self.reserve)(1, bytes) {
            Ok(()) => Ok(true),
            Err(NativeError::Retry(Retry::Room { wanted })) => {
                self.refused = Some((wanted.0, 0));
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    /// Writes `node` while no reservation has been refused, returning its
    /// index. After a refusal the tape stays as it was: the read can only
    /// end in that refusal or in a syntax error.
    fn push(&mut self, node: Node<'text>, bytes: usize) -> usize {
        let index = self.nodes.len();
        if self.refused.is_none() {
            #[cfg(test)]
            self.held.grow(16 + bytes as u64);
            #[cfg(not(test))]
            let _ = bytes;
            self.nodes.push(node);
        }
        index
    }
    fn slice(&self, range: std::ops::Range<usize>) -> Result<&'text str, NativeError> {
        self.text.get(range).ok_or_else(syntax)
    }
    fn whitespace(&mut self) {
        while self
            .byte()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.cursor += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.byte() == Some(byte) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }
    fn string(&mut self) -> Result<Cow<'text, str>, NativeError> {
        if !self.take(b'"') {
            return Err(syntax());
        }
        let start = self.cursor - 1;
        // A string without escapes or control bytes is the text between its
        // quotes. Any other string is unescaped whole once its end is found.
        let rest = &self.input[self.cursor..];
        match rest
            .iter()
            .position(|&byte| matches!(byte, b'"' | b'\\' | 0..0x20))
        {
            Some(end) if rest[end] == b'"' => {
                let body = self.cursor..self.cursor + end;
                self.cursor += end + 1;
                return self.slice(body).map(Cow::Borrowed);
            }
            Some(end) => self.cursor += end,
            None => return Err(syntax()),
        }
        let mut escaped = false;
        while let Some(byte) = self.byte() {
            self.cursor += 1;
            if byte == b'"' && !escaped {
                #[cfg(test)]
                let _unescape = crate::native_contract_laws::scratch((self.cursor - start) as u64);
                return serde_json::from_slice::<String>(&self.input[start..self.cursor])
                    .map(Cow::Owned)
                    .map_err(|_| syntax());
            }
            escaped = byte == b'\\' && !escaped;
        }
        Err(syntax())
    }
    /// Reads one value onto the tape, returning its index and the payload
    /// bytes its reservation counts. Its members are reserved as they finish;
    /// the value itself is reserved by its container.
    fn value(&mut self, depth: usize) -> Result<(usize, usize), NativeError> {
        self.whitespace();
        if depth > self.depth_limit {
            return Err(raise(
                "json_depth",
                format!(
                    "JSON exceeds the configured {}-level kernel nesting limit",
                    self.depth_limit
                ),
            ));
        }
        match self.byte() {
            Some(b'"') => {
                let text = self.string()?;
                let bytes = text.len();
                Ok((self.push(Node::Text(text), bytes), bytes))
            }
            Some(b'[') => {
                self.cursor += 1;
                let at = self.push(Node::Array { len: 0, end: 0 }, 0);
                let mut items = 0;
                if !self.take(b']') {
                    loop {
                        let (_, bytes) = self.value(depth + 1)?;
                        if self.keep(bytes, 0)? {
                            items += 1;
                        }
                        if self.take(b']') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(syntax());
                        }
                    }
                }
                let after = self.nodes.len();
                if let Some(Node::Array { len, end }) = self.nodes.get_mut(at) {
                    (*len, *end) = (items, after);
                }
                Ok((at, 0))
            }
            Some(b'{') => {
                self.cursor += 1;
                let at = self.push(
                    Node::Object {
                        first: 0,
                        len: 0,
                        end: 0,
                    },
                    0,
                );
                let mark = self.open.len();
                if !self.take(b'}') {
                    loop {
                        let name = self.string()?;
                        #[cfg(test)]
                        self.held.grow(name.len() as u64);
                        if !self.take(b':') {
                            return Err(syntax());
                        }
                        let (value, bytes) = self.value(depth + 1)?;
                        // A duplicate takes its last value at its first
                        // position, and is reserved once.
                        if let Some((_, old)) =
                            self.open[mark..].iter_mut().find(|(key, _)| *key == name)
                        {
                            *old = value;
                        } else if self.keep(bytes, name.len())? {
                            self.open.push((name, value));
                        }
                        if self.take(b'}') {
                            break;
                        }
                        if !self.take(b',') {
                            return Err(syntax());
                        }
                    }
                }
                let first = self.fields.len();
                let count = self.open.len() - mark;
                self.fields.extend(self.open.drain(mark..));
                let after = self.nodes.len();
                if let Some(Node::Object { first: f, len, end }) = self.nodes.get_mut(at) {
                    (*f, *len, *end) = (first, count, after);
                }
                Ok((at, 0))
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.cursor;
                while let Some(byte) = self.byte() {
                    if !matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                        break;
                    }
                    self.cursor += 1;
                }
                let token = self.slice(start..self.cursor)?;
                if !NumberToken::is_valid(token) {
                    return Err(syntax());
                }
                Ok((self.push(Node::Number(token), token.len()), token.len()))
            }
            Some(first) => {
                let (word, node): (&[u8], Node<'text>) = match first {
                    b'n' => (b"null", Node::Null),
                    b't' => (b"true", Node::Bool(true)),
                    b'f' => (b"false", Node::Bool(false)),
                    _ => return Err(syntax()),
                };
                if !self.input[self.cursor..].starts_with(word) {
                    return Err(syntax());
                }
                self.cursor += word.len();
                Ok((self.push(node, 0), 0))
            }
            None => Err(syntax()),
        }
    }
}
pub(super) fn syntax() -> NativeError {
    raise("json_syntax", "invalid JSON, including unpaired surrogates")
}
/// Reads strict JSON nesting at most `depth_limit` levels, reserving
/// each value it keeps in `new`.
pub(super) fn read<'text>(
    text: &'text str,
    depth_limit: usize,
    new: &mut Nursery<'_>,
) -> Result<Tape<'text>, NativeError> {
    read_with(text, depth_limit, |slots, bytes| new.reserve(slots, bytes))
}

/// Uses the canonical tokenizer and duplicate-field policy without allocating
/// guest identities. The caller supplies physical reservation, if needed.
///
/// The whole text is validated before the read answers: a syntax error
/// anywhere in it comes before a memory refusal. A refusal names the room
/// of the whole document: the first refused reservation's figure plus
/// every reservation after it.
pub(super) fn read_with(
    text: &str,
    depth_limit: usize,
    reserve: impl FnMut(u64, u64) -> Result<(), NativeError>,
) -> Result<Tape<'_>, NativeError> {
    let mut parser = Parser {
        text,
        input: text.as_bytes(),
        cursor: 0,
        reserve,
        depth_limit: depth_limit.min(64),
        refused: None,
        nodes: Vec::new(),
        fields: Vec::new(),
        open: Vec::new(),
        #[cfg(test)]
        held: crate::native_contract_laws::scratch(0),
    };
    let (_, bytes) = parser.value(0)?;
    parser.whitespace();
    if parser.cursor != parser.input.len() {
        return Err(syntax());
    }
    parser.keep(bytes, 0)?;
    if let Some((wanted, owed)) = parser.refused {
        return Err(Retry::Room {
            wanted: lash_kernel_doc::heap::LogicalBytes(wanted.saturating_add(owed)),
        }
        .into());
    }
    Ok(Tape {
        nodes: parser.nodes,
        fields: parser.fields,
        #[cfg(test)]
        _held: parser.held,
    })
}
