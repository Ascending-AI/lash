//! Canonical greedy immutable payload trees (D-IMM). Byte, scalar and
//! UTF-16 lengths travel on links, so rank selection skips whole subtrees.
use super::PageKind;
use super::identity;
use super::tree::{Access, grant};

use lash_kernel_doc::heap::*;
use std::ops::{ControlFlow, Range};
use std::sync::Arc;
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
    lash_kernel_value::tokens::decode_body, lash_kernel_value::tokens::encode_body,
};

/// One edge of a payload tree: the child's counts, the identity it commits
/// to (`paged::identity`) and the page that holds it.
#[derive(Clone, Debug)]
struct Part {
    summary: TextSummary,
    id: [u8; 32],
    page: PageRef,
}
impl Part {
    fn root(root: PayloadRoot, summary: TextSummary) -> Self {
        Self {
            summary,
            id: root.id,
            page: root.page,
        }
    }
    fn node(&self) -> identity::Node {
        identity::Node {
            counts: self.summary,
            id: self.id,
        }
    }
}
impl TokenCodec for Part {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.summary.encode(out);
        out.bytes(&self.id);
        self.page.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            summary: TextSummary::decode(input)?,
            id: input
                .bytes()?
                .try_into()
                .map_err(|_| TokenError::Invalid("a payload identity is 32 bytes".into()))?,
            page: PageRef::decode(input)?,
        })
    }
}
/// Writes `bytes`, a text's when `text`, as the payload tree of its
/// logical identity tree, and returns its root page with the root id the
/// identity grammar gives (the long value's key above `identity::FLAT_MAX`).
///
/// # Errors
///
/// The page port's faults; empty content; a text that is no UTF-8.
pub fn build(access: &Access<'_>, bytes: &[u8], text: bool) -> Result<PayloadRoot, HeapFault> {
    let kind = identity::Kind::of(text);
    let mut parts = Vec::new();
    for span in identity::cuts(bytes, kind) {
        let chunk = &bytes[span];
        let summary = identity::counts(chunk, kind)?;
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
        let id = identity::leaf(kind, summary, chunk).id;
        parts.push(Part { summary, id, page });
    }
    if parts.is_empty() {
        return Err(HeapFault::Malformed("empty payload must be inline".into()));
    }
    while parts.len() > 1 {
        let mut next = Vec::new();
        for children in parts.chunks(identity::FANOUT) {
            let node = identity::index(kind, &children.iter().map(Part::node).collect::<Vec<_>>())?;
            let page = access.seal(
                PageKind::PAYLOAD_INDEX,
                encode_body(&children.to_vec()),
                children.iter().map(|p| p.page).collect(),
            )?;
            next.push(Part {
                summary: node.counts,
                id: node.id,
                page,
            });
        }
        parts = next;
    }
    Ok(PayloadRoot {
        page: parts[0].page,
        id: parts[0].id,
    })
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

#[derive(Clone, Debug)]
struct TextWalk {
    leaf: Leaf,
    offset: usize,
    path: Vec<std::vec::IntoIter<Part>>,
}
impl TextWalk {
    fn open(
        access: &Access<'_>,
        root: PayloadRoot,
        summary: TextSummary,
        mut index: u64,
    ) -> Result<Self, HeapFault> {
        let mut part = Part::root(root, summary);
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
        let kind = identity::Kind::of(text);
        if bytes.is_empty()
            || bytes.len() > PAYLOAD_LEAF_BYTES as usize
            || declared != part.summary
            || identity::counts(bytes, kind)? != declared
        {
            return Err(bad("payload leaf summary mismatch".into()));
        }
        if identity::leaf(kind, declared, bytes).id != part.id {
            return Err(bad("payload leaf identity mismatch".into()));
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
        let node = identity::index(
            identity::Kind::of(text),
            &children.iter().map(Part::node).collect::<Vec<_>>(),
        )
        .map_err(|_| bad("payload index aggregate mismatch".into()))?;
        if node.counts != part.summary {
            return Err(bad("payload index aggregate mismatch".into()));
        }
        if node.id != part.id {
            return Err(bad("payload index identity mismatch".into()));
        }
        if !text && !canonical_spans(&children) {
            return Err(bad("payload index spans are not canonical".into()));
        }
        for child in &mut children {
            grant(&mut child.page, &answer.children)?;
        }
        Ok(Node::Index(children))
    } else {
        Err(bad("wrong payload page kind".into()))
    }
}
/// Bytes children span what the identity grammar's position gives them:
/// every child but the last spans one full subtree of `LEAF · FANOUTᵏ`
/// octets, and the last at most that. Rank selection then trusts no unread
/// subtree's length.
fn canonical_spans(children: &[Part]) -> bool {
    let Some((last, full)) = children.split_last() else {
        return false;
    };
    let span = full.first().unwrap_or(last).summary.utf8_bytes;
    let mut whole = identity::LEAF as u64;
    while whole < span {
        whole = whole.saturating_mul(identity::FANOUT as u64);
    }
    (full.is_empty() || whole == span)
        && full.iter().all(|child| child.summary.utf8_bytes == whole)
        && last.summary.utf8_bytes != 0
        && last.summary.utf8_bytes <= whole
}
enum Node {
    Leaf(Leaf),
    Index(Vec<Part>),
}
pub fn visit(
    access: &Access<'_>,
    root: PayloadRoot,
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
    let _ = walk(access, &Part::root(root, summary), text, range, visitor)?;
    Ok(())
}
/// One node of a payload tree: a leaf's bytes, or an index node's
/// children, each the root and counts its edge commits it to.
#[derive(Clone, Debug)]
pub(crate) enum PayloadNode {
    Leaf(Vec<u8>),
    Index(Vec<(PayloadRoot, TextSummary)>),
}

/// The node at `root`, read alone and checked against the counts
/// `summary` and the identity `root.id` the edge naming it commits to:
/// what a cold chunk of a long text or of bytes decodes (seam D). A
/// reader that descends from the row's root through the edges each read
/// gives checks every edge on its path, and reads no sibling.
///
/// # Errors
///
/// The page port's faults, and a node its edge does not certify.
pub(crate) fn node(
    access: &Access<'_>,
    root: PayloadRoot,
    summary: TextSummary,
    text: bool,
) -> Result<PayloadNode, HeapFault> {
    Ok(match read(access, &Part::root(root, summary), text)? {
        Node::Leaf(leaf) => PayloadNode::Leaf(leaf.bytes().to_vec()),
        Node::Index(children) => PayloadNode::Index(
            children
                .into_iter()
                .map(|child| {
                    (
                        PayloadRoot {
                            page: child.page,
                            id: child.id,
                        },
                        child.summary,
                    )
                })
                .collect(),
        ),
    })
}

/// A complete projection has committed to consuming every byte in its
/// range. Unlike `visit`, it can fetch authenticated siblings together.
/// Holding one fanout per level bounds staging; no descendants are named
/// until their own parent has decoded and granted them.
pub(super) fn copy(
    access: &Access<'_>,
    root: PayloadRoot,
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
        &Part::root(root, summary),
        text,
        access.read(root.page)?,
        &mut out,
    )?;
    Ok(out)
}

pub fn offset(
    access: &Access<'_>,
    root: PayloadRoot,
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
    let mut part = Part::root(root, summary);
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
