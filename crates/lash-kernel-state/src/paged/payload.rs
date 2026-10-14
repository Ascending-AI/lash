//! Canonical greedy immutable payload trees (D-IMM). Byte, scalar and
//! UTF-16 lengths travel on links, so rank selection skips whole subtrees.
use super::PageKind;
use super::directory::tuple_tree;
use super::directory::{Body, ordinal_key, value_cost};
use super::heap_impl::{
    PagedHeap, binary_summary, census_fault, native_raised, native_write, value_depth,
};
use super::tree::{Access, SealWork, Tree, checked_sum, grant};
use super::{ObjectInit, ObjectKind, SealBudget, SemanticHeap};
use lash_kernel_doc::Integer;
use lash_kernel_doc::graph_tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::{integer_digits, text_summary};
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

#[derive(Clone)]
struct Part {
    summary: TextSummary,
    page: PageRef,
}
impl TokenCodec for Part {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.summary.encode(out);
        self.page.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            summary: TextSummary::decode(input)?,
            page: PageRef::decode(input)?,
        })
    }
}
pub fn build(access: &Access<'_>, bytes: &[u8], text: bool) -> Result<PageRef, HeapFault> {
    let mut parts = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let mut end = (start + PAYLOAD_LEAF_BYTES as usize).min(bytes.len());
        if text {
            while end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
                end -= 1;
            }
        }
        let chunk = &bytes[start..end];
        let summary = summary(chunk, text)?;
        let mut out = TokenWriter::new();
        out.seq(3);
        text.encode(&mut out);
        summary.encode(&mut out);
        if text {
            out.text(std::str::from_utf8(chunk).map_err(|e| HeapFault::Malformed(e.to_string()))?);
        } else {
            out.bytes(chunk);
        }
        let page = access.seal(PageKind::PAYLOAD_LEAF, out.into_bytes(), Vec::new())?;
        parts.push(Part { summary, page });
        start = end;
    }
    if parts.is_empty() {
        return Err(HeapFault::Malformed("empty payload must be inline".into()));
    }
    while parts.len() > 1 {
        let mut next = Vec::new();
        for children in parts.chunks(32) {
            let summary = add(children.iter().map(|p| p.summary))?;
            let page = access.seal(
                PageKind::PAYLOAD_INDEX,
                encode_body(&children.to_vec()),
                children.iter().map(|p| p.page).collect(),
            )?;
            next.push(Part { summary, page });
        }
        parts = next;
    }
    Ok(parts[0].page)
}
fn summary(bytes: &[u8], text: bool) -> Result<TextSummary, HeapFault> {
    Ok(if text {
        text_summary(std::str::from_utf8(bytes).map_err(|e| HeapFault::Malformed(e.to_string()))?)
    } else {
        TextSummary {
            utf8_bytes: bytes.len() as u64,
            chars: 0,
            utf16_units: 0,
        }
    })
}
fn add(parts: impl Iterator<Item = TextSummary>) -> Result<TextSummary, HeapFault> {
    let mut sum = TextSummary {
        utf8_bytes: 0,
        chars: 0,
        utf16_units: 0,
    };
    for p in parts {
        sum.utf8_bytes = sum
            .utf8_bytes
            .checked_add(p.utf8_bytes)
            .ok_or_else(|| HeapFault::Malformed("payload length overflow".into()))?;
        sum.chars = sum
            .chars
            .checked_add(p.chars)
            .ok_or_else(|| HeapFault::Malformed("payload character overflow".into()))?;
        sum.utf16_units = sum
            .utf16_units
            .checked_add(p.utf16_units)
            .ok_or_else(|| HeapFault::Malformed("payload UTF-16 overflow".into()))?;
    }
    Ok(sum)
}
fn read(access: &Access<'_>, part: &Part, text: bool) -> Result<Node, HeapFault> {
    let answer = access.read(part.page)?;
    let bad = |reason: String| HeapFault::Corrupt {
        page: part.page.id,
        reason,
    };
    if answer.kind == PageKind::PAYLOAD_LEAF {
        let mut input = TokenReader::new(&answer.tokens);
        input.expect_struct(3).map_err(|e| bad(e.to_string()))?;
        if input.bool().map_err(|e| bad(e.to_string()))? != text {
            return Err(bad("payload kind mismatch".into()));
        }
        let declared = TextSummary::decode(&mut input).map_err(|e| bad(e.to_string()))?;
        let bytes = if text {
            input.text().map_err(|e| bad(e.to_string()))?.as_bytes()
        } else {
            input.bytes().map_err(|e| bad(e.to_string()))?
        }
        .to_vec();
        input.finish().map_err(|e| bad(e.to_string()))?;
        if bytes.is_empty()
            || bytes.len() > PAYLOAD_LEAF_BYTES as usize
            || declared != part.summary
            || summary(&bytes, text)? != declared
        {
            return Err(bad("payload leaf summary mismatch".into()));
        }
        Ok(Node::Leaf(bytes))
    } else if answer.kind == PageKind::PAYLOAD_INDEX {
        let mut children: Vec<Part> =
            decode_body(&answer.tokens).map_err(|e| bad(e.to_string()))?;
        if children.is_empty()
            || children.len() > 32
            || add(children.iter().map(|p| p.summary))? != part.summary
        {
            return Err(bad("payload index aggregate mismatch".into()));
        }
        for child in &mut children {
            grant(&mut child.page, &answer.children)?;
        }
        Ok(Node::Index(children))
    } else {
        Err(bad("wrong payload page kind".into()))
    }
}
enum Node {
    Leaf(Vec<u8>),
    Index(Vec<Part>),
}
pub fn visit(
    access: &Access<'_>,
    page: PageRef,
    summary: TextSummary,
    text: bool,
    range: Range<u64>,
    visitor: &mut dyn FnMut(&[u8]) -> ControlFlow<()>,
) -> Result<(), HeapFault> {
    fn walk(
        access: &Access<'_>,
        part: &Part,
        text: bool,
        range: Range<u64>,
        visitor: &mut dyn FnMut(&[u8]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, HeapFault> {
        if range.start >= range.end {
            return Ok(ControlFlow::Continue(()));
        }
        match read(access, part, text)? {
            Node::Leaf(bytes) => {
                let slice = bytes
                    .get(range.start as usize..range.end as usize)
                    .ok_or_else(|| HeapFault::Malformed("payload range outside leaf".into()))?;
                if text {
                    std::str::from_utf8(slice)
                        .map_err(|_| HeapFault::Malformed("text range cuts a scalar".into()))?;
                }
                Ok(visitor(slice))
            }
            Node::Index(children) => {
                let mut offset = 0;
                for child in children {
                    let end = offset + child.summary.utf8_bytes;
                    if end > range.start
                        && offset < range.end
                        && walk(
                            access,
                            &child,
                            text,
                            range.start.saturating_sub(offset)..(range.end.min(end) - offset),
                            visitor,
                        )?
                        .is_break()
                    {
                        return Ok(ControlFlow::Break(()));
                    }
                    offset = end;
                }
                Ok(ControlFlow::Continue(()))
            }
        }
    }
    if range.start > range.end || range.end > summary.utf8_bytes {
        return Err(HeapFault::Malformed(
            "payload range is out of bounds".into(),
        ));
    }
    walk(access, &Part { summary, page }, text, range, visitor)?;
    Ok(())
}
pub fn offset(
    access: &Access<'_>,
    page: PageRef,
    summary: TextSummary,
    mut index: u64,
) -> Result<u64, HeapFault> {
    if index > summary.chars {
        return Err(HeapFault::Malformed(
            "character index is out of bounds".into(),
        ));
    }
    if index == summary.chars {
        return Ok(summary.utf8_bytes);
    }
    let mut part = Part { summary, page };
    let mut offset = 0;
    loop {
        match read(access, &part, true)? {
            Node::Leaf(bytes) => {
                let text =
                    std::str::from_utf8(&bytes).map_err(|e| HeapFault::Malformed(e.to_string()))?;
                return text
                    .char_indices()
                    .nth(index as usize)
                    .map(|(i, _)| offset + i as u64)
                    .ok_or_else(|| HeapFault::Malformed("text rank mismatch".into()));
            }
            Node::Index(children) => {
                let mut chosen = None;
                for child in children {
                    if index < child.summary.chars {
                        chosen = Some(child);
                        break;
                    }
                    index -= child.summary.chars;
                    offset += child.summary.utf8_bytes;
                }
                part = chosen
                    .ok_or_else(|| HeapFault::Malformed("text index summary mismatch".into()))?;
            }
        }
    }
}

impl NativeHeap for PagedHeap {
    fn equal(&self, a: &Value, b: &Value) -> Result<bool, HeapFault> {
        self.equal_values(a, b)
    }
    fn data_depth_limit(&self) -> usize {
        128
    }
    fn value_depth_limit(&self) -> usize {
        128
    }
    fn len(&self, object: EntityId) -> Result<u64, HeapFault> {
        Ok(self.head(object)?.map_or(0, |h| h.len()))
    }
    fn list_get(&self, list: EntityId, index: u64) -> Result<Option<Value>, HeapFault> {
        Ok(match self.head(list)?.map(|h| h.body) {
            Some(Body::List {
                inline: Some(v), ..
            }) => v.get(index as usize).cloned(),
            Some(Body::List { rows, .. }) => rows.rank_get(index, &self.access)?.map(|(_, v)| v),
            _ => None,
        })
    }
    fn map_get(&self, map: EntityId, key: &Value) -> Result<Option<Value>, HeapFault> {
        if self.kind(map)? != Some(ObjectKind::Map) {
            return Ok(None);
        }
        self.table_get(map, key)
    }
    fn set_contains(&self, set: EntityId, member: &Value) -> Result<bool, HeapFault> {
        if self.kind(set)? != Some(ObjectKind::Set) {
            return Ok(false);
        }
        Ok(self.table_get(set, member)?.is_some())
    }
    fn record_get(&self, record: EntityId, field: &str) -> Result<Option<Value>, HeapFault> {
        if self.kind(record)? != Some(ObjectKind::Record) {
            return Ok(None);
        }
        self.table_get(
            record,
            &Value::Text(TextRef {
                summary: text_summary(field),
                body: Payload::Resident(Arc::from(field)),
            }),
        )
    }
    fn visit(
        &self,
        object: EntityId,
        visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        match self.head(object)?.map(|h| h.body) {
            Some(Body::List {
                inline: Some(v), ..
            }) => {
                for v in &v {
                    if visitor(Element::Item(v)).is_break() {
                        break;
                    }
                }
            }
            Some(Body::List { rows, .. }) => rows.visit(&self.access, &mut |_, v| {
                visitor(Element::Item(v)).is_continue()
            })?,
            Some(Body::Table { kind, rows, .. }) => rows.visit(&self.access, &mut |_, e| {
                visitor(match kind {
                    ObjectKind::Set => Element::Item(&e.key),
                    ObjectKind::Record => match &e.key {
                        Value::Text(t) => Element::Field {
                            name: t,
                            value: &e.value,
                        },
                        _ => return false,
                    },
                    _ => Element::Entry {
                        key: &e.key,
                        value: &e.value,
                    },
                })
                .is_continue()
            })?,
            _ => (),
        }
        Ok(())
    }
    fn error_data(&self, error: &ErrorRef) -> Result<Value, HeapFault> {
        match &error.data {
            Payload::Resident(value) => Ok((**value).clone()),
            Payload::Paged(page) => {
                let cost = error
                    .cost
                    .0
                    .checked_sub(error.kind.summary.utf8_bytes)
                    .and_then(|v| v.checked_sub(error.message.summary.utf8_bytes))
                    .ok_or_else(|| HeapFault::Malformed("error cost summary mismatch".into()))?;
                let tree = Tree::<Value>::open(Some(*page), 1, LogicalBytes(cost), true)
                    .with_task_edges(error.task_edges);
                let tree = match error.family {
                    Some(family) => tree.in_family(family),
                    None => tree,
                };
                tree.rank_get(0, &self.access)?
                    .map(|(_, value)| value)
                    .ok_or_else(|| HeapFault::Malformed("error data page is empty".into()))
            }
        }
    }
    fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<Value>, HeapFault> {
        if index >= tuple.summary.len {
            return Ok(None);
        }
        match &tuple.body {
            Payload::Resident(v) => Ok(v.get(index as usize).cloned()),
            Payload::Paged(p) => tuple_tree(tuple, *p)
                .rank_get(index, &self.access)
                .map(|v| v.map(|(_, v)| v)),
        }
    }
    fn tuple_visit(
        &self,
        tuple: &TupleRef,
        visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        match &tuple.body {
            Payload::Resident(v) => {
                for v in v.iter() {
                    if visitor(v).is_break() {
                        break;
                    }
                }
                Ok(())
            }
            Payload::Paged(p) => {
                tuple_tree(tuple, *p).visit(&self.access, &mut |_, v| visitor(v).is_continue())
            }
        }
    }
    fn payload_visit(
        &self,
        value: &Value,
        range: Range<u64>,
        visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        match value {
            Value::Text(t) => match &t.body {
                Payload::Resident(text) => {
                    if range.start > range.end {
                        return Err(HeapFault::Malformed("text range is reversed".into()));
                    }
                    let slice = text
                        .get(range.start as usize..range.end as usize)
                        .ok_or_else(|| {
                            HeapFault::Malformed(
                                "text range cuts a scalar or exceeds length".into(),
                            )
                        })?;
                    visitor(Chunk::Text(slice));
                    Ok(())
                }
                Payload::Paged(p) => {
                    super::payload::visit(&self.access, *p, t.summary, true, range, &mut |bytes| {
                        // The payload decoder proved UTF-8 and the range's boundaries.
                        match std::str::from_utf8(bytes) {
                            Ok(text) => visitor(Chunk::Text(text)),
                            Err(_) => ControlFlow::Break(()),
                        }
                    })
                }
            },
            Value::Bytes(b) => match &b.body {
                Payload::Resident(bytes) => {
                    if range.start > range.end {
                        return Err(HeapFault::Malformed("bytes range is reversed".into()));
                    }
                    let slice = bytes
                        .get(range.start as usize..range.end as usize)
                        .ok_or_else(|| HeapFault::Malformed("bytes range exceeds length".into()))?;
                    visitor(Chunk::Bytes(slice));
                    Ok(())
                }
                Payload::Paged(p) => super::payload::visit(
                    &self.access,
                    *p,
                    binary_summary(b.len),
                    false,
                    range,
                    &mut |bytes| visitor(Chunk::Bytes(bytes)),
                ),
            },
            _ => Err(HeapFault::Malformed("expected text or bytes".into())),
        }
    }
    fn text_offset(&self, text: &TextRef, index: u64) -> Result<u64, HeapFault> {
        if index > text.summary.chars {
            return Err(HeapFault::Malformed(
                "text character index exceeds length".into(),
            ));
        }
        match &text.body {
            Payload::Resident(t) => Ok(t
                .char_indices()
                .nth(index as usize)
                .map_or(t.len() as u64, |(i, _)| i as u64)),
            Payload::Paged(p) => super::payload::offset(&self.access, *p, text.summary, index),
        }
    }
    fn materialize_text(&mut self, text: &TextRef) -> Result<Arc<str>, NativeError> {
        self.reserve(0, text.summary.utf8_bytes)?;
        Ok(self.copy_text(text)?)
    }
    fn materialize_bytes(&mut self, bytes: &BytesRef) -> Result<Arc<[u8]>, NativeError> {
        self.reserve(0, bytes.len)?;
        Ok(self.copy_bytes(bytes)?)
    }
    fn materialize_int(&mut self, int: &IntRef) -> Result<Integer, NativeError> {
        let bytes = match int {
            IntRef::Inline(i) => i.bits().div_ceil(8),
            IntRef::Paged { summary, .. } => summary.bits.div_ceil(8),
        };
        self.reserve(0, bytes)?;
        Ok(self.copy_int(int)?)
    }
    fn text(&mut self, text: &str) -> Result<TextRef, NativeError> {
        Ok(TextRef {
            summary: text_summary(text),
            body: if text.len() as u64 <= IMMUTABLE_INLINE_MAX {
                Payload::Resident(Arc::from(text))
            } else {
                Payload::Paged(super::payload::build(&self.access, text.as_bytes(), true)?)
            },
        })
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<BytesRef, NativeError> {
        Ok(BytesRef {
            len: bytes.len() as u64,
            body: if bytes.len() as u64 <= IMMUTABLE_INLINE_MAX {
                Payload::Resident(Arc::from(bytes))
            } else {
                Payload::Paged(super::payload::build(&self.access, bytes, false)?)
            },
        })
    }
    fn int(&mut self, int: Integer) -> Result<IntRef, NativeError> {
        if int.bits().div_ceil(8) <= IMMUTABLE_INLINE_MAX {
            return Ok(IntRef::Inline(int));
        }
        let summary = IntSummary {
            bits: int.bits(),
            negative: int.is_negative(),
        };
        Ok(IntRef::Paged {
            summary,
            digits: super::payload::build(&self.access, &integer_digits(&int), false)?,
        })
    }
    fn tuple(&mut self, members: Vec<Value>) -> Result<TupleRef, NativeError> {
        let mut values = Vec::with_capacity(members.len());
        let mut depth = 1;
        for v in members {
            let v = self.canonical(v).map_err(native_write)?;
            depth = depth.max(value_depth(&v) + 1);
            values.push(v);
        }
        if depth > 128 {
            return Err(native_raised(
                "depth",
                "tuple nesting exceeds the value depth bound",
            ));
        }
        let has_edges = values.iter().any(super::directory::value_has_edges);
        let task_edges = checked_sum(values.iter().map(lash_kernel_doc::heap_values::task_count))?;
        let summary = TupleSummary {
            task_edges,
            len: values.len() as u64,
            cost: LogicalBytes(checked_sum(values.iter().map(value_cost))?),
            has_edges,
            depth,
        };
        let body = if encode_body(&values).len() <= IMMUTABLE_INLINE_MAX as usize {
            Payload::Resident(Arc::from(values))
        } else {
            let mut tree = Tree::new(true);
            for (i, value) in values.into_iter().enumerate() {
                tree.write(ordinal_key(i as u64), value, &self.access)?;
            }
            tree.seal(
                &self.access,
                &mut SealWork::new(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                }),
            )?;
            Payload::Paged(
                tree.page()
                    .ok_or_else(|| HeapFault::Malformed("nonempty tuple has no page".into()))?,
            )
        };
        Ok(TupleRef {
            summary,
            body,
            family: None,
        })
    }
    fn concat(&mut self, parts: &[Value]) -> Result<Value, NativeError> {
        match parts.first() {
            Some(Value::Text(_)) => {
                let mut text = String::new();
                let len = checked_sum(parts.iter().map(|v| match v {
                    Value::Text(t) => t.summary.utf8_bytes,
                    _ => 0,
                }))?;
                self.reserve(0, len)?;
                for p in parts {
                    let Value::Text(t) = p else {
                        return Err(native_raised("type_error", "concat requires texts"));
                    };
                    text.push_str(&self.copy_text(t)?);
                }
                self.text(&text).map(Value::Text)
            }
            Some(Value::Bytes(_)) => {
                let mut bytes = Vec::new();
                let len = checked_sum(parts.iter().map(|v| match v {
                    Value::Bytes(b) => b.len,
                    _ => 0,
                }))?;
                self.reserve(0, len)?;
                for p in parts {
                    let Value::Bytes(b) = p else {
                        return Err(native_raised("type_error", "concat requires bytes"));
                    };
                    bytes.extend_from_slice(&self.copy_bytes(b)?);
                }
                self.bytes(&bytes).map(Value::Bytes)
            }
            Some(Value::Tuple(_)) => {
                let mut values = Vec::new();
                for p in parts {
                    let Value::Tuple(t) = p else {
                        return Err(native_raised("type_error", "concat requires tuples"));
                    };
                    self.tuple_visit(t, &mut |v| {
                        values.push(v.clone());
                        ControlFlow::Continue(())
                    })?;
                }
                self.reserve(
                    values.len() as u64,
                    checked_sum(values.iter().map(value_cost))?,
                )?;
                self.tuple(values).map(Value::Tuple)
            }
            _ => Err(native_raised(
                "type_error",
                "concat requires immutable sequences",
            )),
        }
    }
    fn slice(&mut self, value: &Value, range: Range<u64>) -> Result<Value, NativeError> {
        let len = match value {
            Value::Text(t) => t.summary.utf8_bytes,
            Value::Bytes(b) => b.len,
            Value::Tuple(t) => {
                return self
                    .tuple_slice(t, range)
                    .map(Value::Tuple)
                    .map_err(native_write);
            }
            _ => {
                return Err(native_raised(
                    "type_error",
                    "slice requires an immutable sequence",
                ));
            }
        };
        if range.start > range.end || range.end > len {
            return Err(native_raised(
                "index_out_of_range",
                "slice range exceeds length",
            ));
        }
        self.reserve(0, range.end - range.start)?;
        let mut bytes = Vec::new();
        self.payload_visit(value, range, &mut |chunk| {
            bytes.extend_from_slice(match chunk {
                Chunk::Text(t) => t.as_bytes(),
                Chunk::Bytes(b) => b,
            });
            ControlFlow::Continue(())
        })?;
        match value {
            Value::Text(_) => self
                .text(
                    std::str::from_utf8(&bytes)
                        .map_err(|_| native_raised("index_out_of_range", "slice cuts a scalar"))?,
                )
                .map(Value::Text),
            _ => self.bytes(&bytes).map(Value::Bytes),
        }
    }
    fn allocate(&mut self, object: Object) -> Result<EntityId, NativeError> {
        self.allocate_object(match object {
            Object::List(v) => ObjectInit::List(v),
            Object::Map(v) => ObjectInit::Map(v),
            Object::Set(v) => ObjectInit::Set(v),
            Object::Record(v) => ObjectInit::Record(v),
        })
        .map_err(native_write)
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        let bytes = values
            .checked_mul(16)
            .and_then(|v| v.checked_add(bytes))
            .ok_or_else(|| HeapFault::Malformed("native reservation overflow".into()))?;
        self.admit(bytes, &[]).map_err(native_write)?;
        if let Some(census) = &mut self.census {
            census
                .changed(LogicalBytes(0), LogicalBytes(bytes))
                .map_err(census_fault)?;
            self.accounting = census.certificate();
        }
        self.reserved = self
            .reserved
            .checked_add(bytes)
            .ok_or_else(|| HeapFault::Malformed("outstanding reservation overflow".into()))?;
        Ok(())
    }
}
