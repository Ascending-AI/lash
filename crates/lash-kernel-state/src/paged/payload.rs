//! Canonical greedy immutable payload trees (D-IMM). Byte, scalar and
//! UTF-16 lengths travel on links, so rank selection skips whole subtrees.
use super::PageKind;
use super::directory::Body;
use super::directory::tuple_tree;
use super::heap_impl::{PagedHeap, binary_summary, value_depth};
use super::tree::{Access, grant};
use super::{HeapWriteError, ObjectKind, SemanticHeap};
use lash_kernel_doc::Integer;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::text_summary;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
    lash_kernel_value::tokens::decode_body, lash_kernel_value::tokens::encode_body,
};

#[derive(Clone, Debug)]
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
#[cfg(test)]
pub(super) mod cursor_work {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    #[derive(Default, Debug)]
    pub struct Work {
        pub leaves: BTreeMap<PageId, u64>,
        pub copied: usize,
        pub scanned: usize,
    }
    thread_local! { pub static WORK: RefCell<Work> = RefCell::default(); }
}

/// A transient text walk with one validated leaf and a fanout-bounded frontier
/// per index level. Checkpoints retain only [`Self::text`] and [`Self::position`].
#[derive(Clone, Debug)]
pub struct TextCursor {
    text: TextRef,
    index: u64,
    offset: Option<usize>,
    walk: Option<TextWalk>,
}
impl TextCursor {
    /// Reconstruct lazily from the persisted scalar position, without reading pages.
    pub fn new(text: TextRef, index: u64) -> Self {
        Self {
            text,
            index,
            offset: None,
            walk: None,
        }
    }
    /// The descriptor retained by the loop's holdings and checkpoint.
    pub fn text(&self) -> &TextRef {
        &self.text
    }
    /// The next scalar's ordinal, independent of page layout and residency.
    pub fn position(&self) -> u64 {
        self.index
    }
    /// Return one scalar, preserving the boundary lookahead of text rank selection.
    pub fn next(&mut self, heap: &PagedHeap) -> Result<Option<char>, HeapFault> {
        if self.index >= self.text.summary.chars {
            return Ok(None);
        }
        let point = match &self.text.body {
            Payload::Resident(body) => {
                let offset = *self.offset.get_or_insert_with(|| {
                    body.char_indices()
                        .nth(self.index as usize)
                        .map_or(body.len(), |(at, _)| at)
                });
                let point = body
                    .get(offset..)
                    .and_then(|rest| rest.chars().next())
                    .ok_or_else(|| {
                        HeapFault::Malformed("a text cursor stands off a code point".into())
                    })?;
                self.offset = Some(offset + point.len_utf8());
                point
            }
            Payload::Paged(page) => {
                let walk = match &mut self.walk {
                    Some(walk) => walk,
                    empty => empty.insert(TextWalk::open(
                        &heap.access,
                        **page,
                        self.text.summary,
                        self.index,
                    )?),
                };
                let point = walk.point()?;
                walk.offset += point.len_utf8();
                // Previously offset(index + 1) validated the following leaf before
                // returning the current leaf's last scalar. Do not delay that fault.
                if walk.offset == walk.leaf.bytes().len()
                    && self.index + 1 < self.text.summary.chars
                {
                    walk.advance(&heap.access)?;
                }
                point
            }
        };
        self.index += 1;
        Ok(Some(point))
    }
}

#[derive(Clone, Debug)]
struct TextWalk {
    leaf: Leaf,
    offset: usize,
    path: Vec<std::vec::IntoIter<Part>>,
}
impl TextWalk {
    fn open(
        access: &Access<'_>,
        page: PageRef,
        summary: TextSummary,
        mut index: u64,
    ) -> Result<Self, HeapFault> {
        let mut part = Part { page, summary };
        let mut path = Vec::new();
        loop {
            match read(access, &part, true)? {
                Node::Leaf(leaf) => {
                    let text = std::str::from_utf8(leaf.bytes())
                        .map_err(|e| HeapFault::Malformed(e.to_string()))?;
                    let offset = text
                        .char_indices()
                        .inspect(|(_, c)| {
                            #[cfg(test)]
                            cursor_work::WORK.with_borrow_mut(|work| work.scanned += c.len_utf8());
                            #[cfg(not(test))]
                            let _ = c;
                        })
                        .nth(index as usize)
                        .map(|(at, _)| at)
                        .ok_or_else(|| HeapFault::Malformed("text rank mismatch".into()))?;
                    return Ok(Self { leaf, offset, path });
                }
                Node::Index(children) => {
                    let mut children = children.into_iter();
                    part = loop {
                        let child = children.next().ok_or_else(|| {
                            HeapFault::Malformed("text index summary mismatch".into())
                        })?;
                        if index < child.summary.chars {
                            break child;
                        }
                        index -= child.summary.chars;
                    };
                    path.push(children);
                }
            }
        }
    }
    fn point(&self) -> Result<char, HeapFault> {
        let bytes = self.leaf.bytes();
        let first = *bytes
            .get(self.offset)
            .ok_or_else(|| HeapFault::Malformed("text rank mismatch".into()))?;
        let width = if first < 0x80 {
            1
        } else if first < 0xe0 {
            2
        } else if first < 0xf0 {
            3
        } else {
            4
        };
        // The leaf was validated once. Decode only this scalar, never the suffix.
        let point = bytes
            .get(self.offset..self.offset + width)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| text.chars().next())
            .ok_or_else(|| HeapFault::Malformed("text rank mismatch".into()))?;
        #[cfg(test)]
        cursor_work::WORK.with_borrow_mut(|work| work.scanned += width);
        Ok(point)
    }
    fn advance(&mut self, access: &Access<'_>) -> Result<(), HeapFault> {
        let mut part = loop {
            let children = self
                .path
                .last_mut()
                .ok_or_else(|| HeapFault::Malformed("text index summary mismatch".into()))?;
            if let Some(child) = children.find(|child| child.summary.chars != 0) {
                break child;
            }
            self.path.pop();
        };
        loop {
            match read(access, &part, true)? {
                Node::Leaf(leaf) => {
                    self.leaf = leaf;
                    self.offset = 0;
                    return Ok(());
                }
                Node::Index(children) => {
                    let mut children = children.into_iter();
                    part = children
                        .find(|child| child.summary.chars != 0)
                        .ok_or_else(|| {
                            HeapFault::Malformed("text index summary mismatch".into())
                        })?;
                    self.path.push(children);
                }
            }
        }
    }
}

// The payload range borrows the immutable token image; no leaf-sized allocation.
#[derive(Clone, Debug)]
struct Leaf {
    tokens: Arc<[u8]>,
    range: Range<usize>,
}
impl Leaf {
    fn bytes(&self) -> &[u8] {
        &self.tokens[self.range.clone()]
    }
}

fn read(access: &Access<'_>, part: &Part, text: bool) -> Result<Node, HeapFault> {
    decode(part, text, access.read(part.page)?)
}
fn decode(part: &Part, text: bool, answer: super::PageAnswer) -> Result<Node, HeapFault> {
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
        };
        let end = input.position();
        let start = end - bytes.len();
        #[cfg(test)]
        cursor_work::WORK.with_borrow_mut(|work| {
            *work.leaves.entry(part.page.id).or_default() += 1;
            work.scanned += bytes.len();
        });
        input.finish().map_err(|e| bad(e.to_string()))?;
        if bytes.is_empty()
            || bytes.len() > PAYLOAD_LEAF_BYTES as usize
            || declared != part.summary
            || summary(bytes, text)? != declared
        {
            return Err(bad("payload leaf summary mismatch".into()));
        }
        #[cfg(test)]
        let backing = Arc::clone(&answer.tokens);
        let leaf = Leaf {
            tokens: answer.tokens,
            range: start..end,
        };
        #[cfg(test)]
        if !Arc::ptr_eq(&leaf.tokens, &backing) {
            cursor_work::WORK.with_borrow_mut(|work| work.copied += leaf.bytes().len());
        }
        Ok(Node::Leaf(leaf))
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
    Leaf(Leaf),
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
            Node::Leaf(leaf) => {
                let slice = leaf
                    .bytes()
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
    let _ = walk(access, &Part { summary, page }, text, range, visitor)?;
    Ok(())
}
/// One node of a payload tree: a leaf's bytes, or an index node's children
/// with their byte lengths.
#[derive(Clone, Debug)]
pub enum PayloadNode {
    Leaf(Vec<u8>),
    Index(Vec<(PageRef, u64)>),
}

/// The node of the payload tree at `page`, whose whole summary is
/// `summary`, that spans exactly bytes `range`, validated along its path
/// from the root: what a cold chunk of a long text or of bytes decodes
/// (seam D). It reads that path and the node, never a sibling.
///
/// # Errors
///
/// The page port's faults, and a range that is no node of the tree.
pub fn node_at(
    access: &Access<'_>,
    page: PageRef,
    summary: TextSummary,
    text: bool,
    range: Range<u64>,
) -> Result<PayloadNode, HeapFault> {
    let mut part = Part { summary, page };
    let mut offset = 0u64;
    loop {
        let node = read(access, &part, text)?;
        if offset == range.start && offset + part.summary.utf8_bytes == range.end {
            return Ok(match node {
                Node::Leaf(leaf) => PayloadNode::Leaf(leaf.bytes().to_vec()),
                Node::Index(children) => PayloadNode::Index(
                    children
                        .into_iter()
                        .map(|child| (child.page, child.summary.utf8_bytes))
                        .collect(),
                ),
            });
        }
        let Node::Index(children) = node else {
            return Err(HeapFault::Malformed(
                "a payload chunk is no node of its tree".into(),
            ));
        };
        let mut chosen = None;
        for child in children {
            let end = offset + child.summary.utf8_bytes;
            if range.start >= offset && range.end <= end && child.summary.utf8_bytes != 0 {
                chosen = Some(child);
                break;
            }
            offset = end;
        }
        part = chosen
            .ok_or_else(|| HeapFault::Malformed("a payload chunk is no node of its tree".into()))?;
    }
}

/// A complete projection has committed to consuming every byte in its
/// range. Unlike `visit`, it can fetch authenticated siblings together.
/// Holding one fanout per level bounds staging; no descendants are named
/// until their own parent has decoded and granted them.
pub(super) fn copy(
    access: &Access<'_>,
    page: PageRef,
    summary: TextSummary,
    text: bool,
) -> Result<Vec<u8>, HeapFault> {
    fn walk(
        access: &Access<'_>,
        part: &Part,
        text: bool,
        answer: super::PageAnswer,
        out: &mut Vec<u8>,
    ) -> Result<(), HeapFault> {
        match decode(part, text, answer)? {
            Node::Leaf(leaf) => out.extend_from_slice(leaf.bytes()),
            Node::Index(children) => {
                let children: Vec<_> = children
                    .into_iter()
                    .filter(|child| child.summary.utf8_bytes != 0)
                    .collect();
                let pages: Vec<_> = children.iter().map(|child| child.page).collect();
                for (child, answer) in children.iter().zip(access.read_batch(&pages)) {
                    walk(access, child, text, answer?, out)?;
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if summary.utf8_bytes == 0 {
        return Ok(out);
    }
    walk(
        access,
        &Part { page, summary },
        text,
        access.read(page)?,
        &mut out,
    )?;
    Ok(out)
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
            Node::Leaf(leaf) => {
                let text = std::str::from_utf8(leaf.bytes())
                    .map_err(|e| HeapFault::Malformed(e.to_string()))?;
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

impl PageReader for PagedHeap {
    fn normal_key(&self, value: &PageValue) -> Result<Option<NormalKey>, HeapFault> {
        self.normalize_value(value)
    }
    fn equal(&self, a: &PageValue, b: &PageValue) -> Result<bool, HeapFault> {
        self.equal_values(a, b)
    }
    fn data_depth_limit(&self) -> usize {
        self.limits.document_depth()
    }
    fn value_depth_limit(&self) -> usize {
        self.limits.value_depth()
    }
    fn len(&self, object: EntityId) -> Result<u64, HeapFault> {
        self.read_head(object, |head| Ok(head.map_or(0, |h| h.len())))
    }
    fn list_get(&self, list: EntityId, index: u64) -> Result<Option<PageValue>, HeapFault> {
        self.read_head(list, |head| {
            Ok(match head.map(|h| &h.body) {
                Some(Body::List { rows, .. }) => {
                    rows.rank_get(index, &self.access)?.map(|(_, v)| v)
                }
                _ => None,
            })
        })
    }
    fn map_get(&self, map: EntityId, key: &PageValue) -> Result<Option<PageValue>, HeapFault> {
        if self.kind(map)? != Some(ObjectKind::Map) {
            return Ok(None);
        }
        self.table_get(map, key)
    }
    fn set_contains(&self, set: EntityId, member: &PageValue) -> Result<bool, HeapFault> {
        if self.kind(set)? != Some(ObjectKind::Set) {
            return Ok(false);
        }
        Ok(self.table_get(set, member)?.is_some())
    }
    fn record_get(
        &self,
        record: EntityId,
        field: FieldName<'_>,
    ) -> Result<Option<PageValue>, HeapFault> {
        if self.kind(record)? != Some(ObjectKind::Record) {
            return Ok(None);
        }
        let text = match field {
            FieldName::Static(field) => TextRef {
                normal: Default::default(),
                summary: text_summary(field),
                body: Payload::Resident(Arc::from(field)),
            },
            FieldName::Text(text) => text.clone(),
        };
        self.table_get(record, &PageValue::Text(text))
    }
    fn visit(
        &self,
        object: EntityId,
        visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        let head = self.current_head(object)?;
        match head.as_deref().map(|h| &h.body) {
            Some(Body::List { rows, .. }) => rows.visit(&self.access, &mut |_, v| {
                visitor(Element::Item(v)).is_continue()
            })?,
            Some(Body::Table { kind, rows, .. }) => rows.visit(&self.access, &mut |_, e| {
                visitor(match *kind {
                    ObjectKind::Set => Element::Item(&e.key),
                    ObjectKind::Record => match &e.key {
                        PageValue::Text(t) => Element::Field {
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
    fn error_data(&self, error: &ErrorRef) -> Result<PageValue, HeapFault> {
        super::stored::reads::error_data(&self.access, error)
    }
    fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<PageValue>, HeapFault> {
        if index >= tuple.summary.len {
            return Ok(None);
        }
        match &tuple.body {
            Payload::Resident(v) => Ok(v.get(index as usize).cloned()),
            Payload::Paged(p) => tuple_tree(tuple, **p)
                .rank_get(index, &self.access)
                .map(|v| v.map(|(_, v)| v)),
        }
    }
    fn tuple_visit(
        &self,
        tuple: &TupleRef,
        visitor: &mut dyn FnMut(&PageValue) -> ControlFlow<()>,
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
                tuple_tree(tuple, **p).visit(&self.access, &mut |_, v| visitor(v).is_continue())
            }
        }
    }
    fn payload_visit(
        &self,
        value: &PageValue,
        range: Range<u64>,
        visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>,
    ) -> Result<(), HeapFault> {
        match value {
            PageValue::Text(t) => match &t.body {
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
                    let _ = visitor(Chunk::Text(slice));
                    Ok(())
                }
                Payload::Paged(p) => {
                    super::payload::visit(&self.access, **p, t.summary, true, range, &mut |bytes| {
                        // The payload decoder proved UTF-8 and the range's boundaries.
                        match std::str::from_utf8(bytes) {
                            Ok(text) => visitor(Chunk::Text(text)),
                            Err(_) => ControlFlow::Break(()),
                        }
                    })
                }
            },
            PageValue::Bytes(b) => match &b.body {
                Payload::Resident(bytes) => {
                    if range.start > range.end {
                        return Err(HeapFault::Malformed("bytes range is reversed".into()));
                    }
                    let slice = bytes
                        .get(range.start as usize..range.end as usize)
                        .ok_or_else(|| HeapFault::Malformed("bytes range exceeds length".into()))?;
                    let _ = visitor(Chunk::Bytes(slice));
                    Ok(())
                }
                Payload::Paged(p) => super::payload::visit(
                    &self.access,
                    **p,
                    binary_summary(b.len),
                    false,
                    range,
                    &mut |bytes| visitor(Chunk::Bytes(bytes)),
                ),
            },
            PageValue::Int(IntRef::Paged {
                summary, digits, ..
            }) => {
                let len = summary.bits.div_ceil(8);
                let mut offset = range.start;
                let mut invalid = false;
                super::payload::visit(
                    &self.access,
                    **digits,
                    binary_summary(len),
                    false,
                    range,
                    &mut |bytes| {
                        offset += bytes.len() as u64;
                        if offset == len {
                            let expected = ((summary.bits - 1) % 8 + 1) as u32;
                            if bytes
                                .last()
                                .is_none_or(|last| 8 - last.leading_zeros() != expected)
                            {
                                invalid = true;
                                return ControlFlow::Break(());
                            }
                        }
                        visitor(Chunk::Bytes(bytes))
                    },
                )?;
                if invalid {
                    return Err(HeapFault::Corrupt {
                        page: digits.id,
                        reason: "integer summary mismatch".into(),
                    });
                }
                Ok(())
            }
            _ => Err(HeapFault::Malformed(
                "expected text, bytes or paged integer".into(),
            )),
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
            Payload::Paged(p) => super::payload::offset(&self.access, **p, text.summary, index),
        }
    }
}

impl PagedHeap {
    /// A text in its page form: inline when short, else payload pages.
    pub fn text(&self, text: &str) -> Result<TextRef, HeapFault> {
        super::stored::pages::text(&self.access, text)
    }
    pub fn bytes(&self, bytes: &[u8]) -> Result<BytesRef, HeapFault> {
        super::stored::pages::bytes(&self.access, bytes)
    }
    pub fn int(&self, int: Integer) -> Result<IntRef, HeapFault> {
        super::stored::pages::int(&self.access, int)
    }
    /// A tuple in its page form, its members canonical first.
    pub fn tuple(&self, members: Vec<PageValue>) -> Result<Arc<TupleRef>, HeapWriteError> {
        let values = members
            .into_iter()
            .map(|value| self.canonical(value))
            .collect::<Result<Vec<_>, _>>()?;
        if values
            .iter()
            .any(|value| value_depth(value) as usize >= self.limits.value_depth())
        {
            return Err(HeapWriteError::Raised {
                kind: "too_deep",
                message: "tuple nesting exceeds the value depth bound".into(),
            });
        }
        Ok(super::stored::pages::tuple(&self.access, values)?)
    }
}
