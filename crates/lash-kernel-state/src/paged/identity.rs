//! Long text and bytes identity (contract §6 "Long text identity"): the one
//! grammar every key producer, payload builder and payload reader uses.
//!
//! A text of at most [`FLAT_MAX`] UTF-8 bytes, or bytes of at most
//! [`FLAT_MAX`] octets, keeps its flat normal key (its tokens inline up to
//! [`NormalKey::INLINE_MAX`], their digest above). A longer one is keyed by
//! the root id of its logical tree. Only the content's length chooses:
//! storage, the paging threshold and short-text representation never do.
//!
//! The logical tree is frozen and independent of how pages pack it:
//!
//! - text leaves are greedy [`LEAF`]-byte cuts, backed off to a scalar
//!   boundary; bytes leaves are exact [`LEAF`]-octet cuts;
//! - each level groups [`FANOUT`] children left to right, the tail group
//!   possibly unary, until one node remains, so depth follows the actual
//!   leaf count;
//! - counts are (UTF-8 bytes, scalars, UTF-16 units) for text and (octets,
//!   0, 0) for bytes, each a u64 little-endian; a child count is a u32
//!   little-endian;
//! - `leaf = SHA-256(tag ‖ counts ‖ content)`,
//!   `index = SHA-256(tag ‖ counts ‖ n ‖ (counts_i ‖ id_i)ⁿ)`, under the
//!   tags `lash.text.leaf`, `lash.text.index`, `lash.bytes.leaf` and
//!   `lash.bytes.index`.
//!
//! Page ids, grants and object ids are outside the identity.
//!
//! Residual trust: a reader that checks every edge it consumes proves the
//! consumed path belongs to the key. The summaries of subtrees it does not
//! read rest on canonical construction: the hash commits to them but does
//! not prove them true.

use lash_kernel_doc::heap::{HeapFault, NormalKey, TextSummary};
use lash_kernel_doc::heap_values::{key_digest, sha256, text_summary};
use lash_kernel_value::tokens::TokenWriter;
use std::ops::Range;

/// The longest content, in UTF-8 bytes or octets, keyed flat.
pub const FLAT_MAX: usize = 4096;
/// A leaf's greatest span, in UTF-8 bytes or octets.
pub const LEAF: usize = 4096;
/// The children an index node groups.
pub const FANOUT: usize = 32;

/// Whether content is text or bytes: the two never share an identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Bytes,
}

impl Kind {
    /// Text when `text`, else bytes.
    pub fn of(text: bool) -> Self {
        if text { Self::Text } else { Self::Bytes }
    }
    fn tags(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Self::Text => (b"lash.text.leaf", b"lash.text.index"),
            Self::Bytes => (b"lash.bytes.leaf", b"lash.bytes.index"),
        }
    }
}

/// A node of the logical tree: what it counts and the id it commits to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub counts: TextSummary,
    pub id: [u8; 32],
}

/// The counts of the `chunk` of `kind`; text must be UTF-8.
///
/// # Errors
///
/// A text chunk that is not UTF-8.
pub fn counts(chunk: &[u8], kind: Kind) -> Result<TextSummary, HeapFault> {
    Ok(match kind {
        Kind::Text => text_summary(
            std::str::from_utf8(chunk).map_err(|error| HeapFault::Malformed(error.to_string()))?,
        ),
        Kind::Bytes => TextSummary {
            utf8_bytes: chunk.len() as u64,
            chars: 0,
            utf16_units: 0,
        },
    })
}

/// The leaf spans of `content`, in order.
pub fn cuts(content: &[u8], kind: Kind) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= content.len() {
            return None;
        }
        let mut end = (start + LEAF).min(content.len());
        if kind == Kind::Text {
            while end < content.len() && (content[end] & 0xc0) == 0x80 {
                end -= 1;
            }
        }
        let span = start..end;
        start = end;
        Some(span)
    })
}

/// The leaf over `chunk`, whose counts are `counts`.
pub fn leaf(kind: Kind, counts: TextSummary, chunk: &[u8]) -> Node {
    Node {
        counts,
        id: sha256(&[kind.tags().0, &encode(counts), chunk]),
    }
}

/// The index node over `children`, in order.
///
/// # Errors
///
/// No children, more than [`FANOUT`], or counts that overflow.
pub fn index(kind: Kind, children: &[Node]) -> Result<Node, HeapFault> {
    if children.is_empty() || children.len() > FANOUT {
        return Err(HeapFault::Malformed(format!(
            "an identity index groups {} children",
            children.len()
        )));
    }
    let mut counts = TextSummary {
        utf8_bytes: 0,
        chars: 0,
        utf16_units: 0,
    };
    let overflow = || HeapFault::Malformed("identity counts overflow".into());
    for child in children {
        counts.utf8_bytes = counts
            .utf8_bytes
            .checked_add(child.counts.utf8_bytes)
            .ok_or_else(overflow)?;
        counts.chars = counts
            .chars
            .checked_add(child.counts.chars)
            .ok_or_else(overflow)?;
        counts.utf16_units = counts
            .utf16_units
            .checked_add(child.counts.utf16_units)
            .ok_or_else(overflow)?;
    }
    let mut preimage = Vec::with_capacity(24 + 4 + children.len() * 56);
    preimage.extend_from_slice(&encode(counts));
    preimage.extend_from_slice(&(children.len() as u32).to_le_bytes());
    for child in children {
        preimage.extend_from_slice(&encode(child.counts));
        preimage.extend_from_slice(&child.id);
    }
    Ok(Node {
        counts,
        id: sha256(&[kind.tags().1, &preimage]),
    })
}

/// The root of `content`'s logical tree: the only producer of a long
/// value's identity.
///
/// # Errors
///
/// Empty content, which is never paged or long, or text that is not UTF-8.
pub fn of(content: &[u8], kind: Kind) -> Result<Node, HeapFault> {
    let mut level = cuts(content, kind)
        .map(|span| {
            let chunk = &content[span];
            counts(chunk, kind).map(|counts| leaf(kind, counts, chunk))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if level.is_empty() {
        return Err(HeapFault::Malformed(
            "empty content has no identity tree".into(),
        ));
    }
    while level.len() > 1 {
        level = level
            .chunks(FANOUT)
            .map(|children| index(kind, children))
            .collect::<Result<_, _>>()?;
    }
    Ok(level[0])
}

/// The normal key of `text`.
pub fn text_key(text: &str) -> NormalKey {
    if text.len() > FLAT_MAX {
        return NormalKey::Digest(long(text.as_bytes(), Kind::Text));
    }
    let mut out = TokenWriter::new();
    out.sum(4, 1);
    out.text(text);
    flat(out)
}

/// The normal key of `bytes`.
pub fn bytes_key(bytes: &[u8]) -> NormalKey {
    if bytes.len() > FLAT_MAX {
        return NormalKey::Digest(long(bytes, Kind::Bytes));
    }
    let mut out = TokenWriter::new();
    out.sum(5, 1);
    out.bytes(bytes);
    flat(out)
}

/// The root id of `content`, longer than [`FLAT_MAX`] and of its kind.
#[expect(
    clippy::expect_used,
    reason = "non-empty content of its kind (a &str is UTF-8) always has a tree, whose in-memory counts cannot overflow"
)]
fn long(content: &[u8], kind: Kind) -> [u8; 32] {
    of(content, kind)
        .expect("long content has an identity tree")
        .id
}

/// Writes the member `key` into a tuple's tokens: inline tokens as they
/// are, a digest under its own tag, so a tuple's key needs no member body.
pub fn embed(key: &NormalKey, out: &mut TokenWriter) {
    match key {
        NormalKey::Inline(tokens) => out.raw(tokens),
        NormalKey::Digest(digest) => {
            out.sum(11, 1);
            out.bytes(digest);
        }
    }
}

/// The flat key of finished `tokens`.
pub fn flat(out: TokenWriter) -> NormalKey {
    let bytes = out.into_bytes();
    if bytes.len() <= NormalKey::INLINE_MAX {
        NormalKey::Inline(bytes)
    } else {
        NormalKey::Digest(key_digest(&bytes))
    }
}

fn encode(counts: TextSummary) -> [u8; 24] {
    let mut out = [0; 24];
    out[..8].copy_from_slice(&counts.utf8_bytes.to_le_bytes());
    out[8..16].copy_from_slice(&counts.chars.to_le_bytes());
    out[16..].copy_from_slice(&counts.utf16_units.to_le_bytes());
    out
}
