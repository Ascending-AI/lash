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

use super::run::objects::{Head, Heads, ObjectKind, RunValue};
use lash_kernel_doc::heap::{HeapFault, NormalKey, TextSummary};
use lash_kernel_doc::heap_values::{key_digest, sha256, text_summary};
use lash_kernel_value::tokens::{TokenError, TokenReader, TokenWriter};
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

// Tuple and error content identity (seal design, "Tuple identity and
// loads"): what a tuple's head and row commit to, built from its members
// as the arena holds them or as a chunk page writes them, so a load checks
// each page it reads against the head every referrer names the tuple by.
//
// - each member writes one token stream ([`Member::write`]): a keyable
//   member exactly the tokens it contributes to a tuple's flat normal
//   form, any other member under its own tag, by its head or its durable
//   id; an immutable member always by its whole head (kind, length or
//   summary, shape, root or flat form), so a parent authenticates every
//   field its member opens by, not its normal form alone;
// - leaves group [`TUPLE_LEAF`] members by position,
//   `leaf = SHA-256(lash.tuple.leaf ‖ start ‖ count ‖ tokens)`;
// - each level above groups [`TUPLE_FANOUT`] children left to right,
//   `index = SHA-256(lash.tuple.index ‖ start ‖ count ‖ n ‖ idⁿ)`, with
//   start and count u64 and n u32, little-endian, until one node remains;
//   levels one and two always exist, so the root is a node of level two
//   or more however few members the tuple has. A level-one node is one
//   index page of a chunked tuple, and each level above one index level
//   ([`super::run::index`]): the root is the node over its row's top;
// - a keyable tuple's normal form is its flat tokens,
//   `sum(9) seq(len) ‖ member tokens`, inline within
//   [`NormalKey::INLINE_MAX`], else the root's digest.
//
// Storage, chunking, concatenation and history never change it: only the
// members' positions and tokens do.

/// The members one tuple leaf groups: a chunk page's, by position.
pub const TUPLE_LEAF: usize = 64;

/// The children one tuple node groups: one index node's references.
pub const TUPLE_FANOUT: usize = super::run::index::NODE_REFS;

const TUPLE_LEAF_TAG: &[u8] = b"lash.tuple.leaf.v1\0";
const TUPLE_INDEX_TAG: &[u8] = b"lash.tuple.index.v1\0";
const ERROR_TAG: &[u8] = b"lash.error.v1\0";

/// Writes the key tokens of `float` alone: an integral one is the integer
/// it equals, whose tokens a big integer's flat normal form holds whole.
/// Inside a tuple a float past a word is named by that integer's head
/// ([`Member::write`]).
pub fn float_key(float: lash_kernel_doc::Float, out: &mut TokenWriter) {
    use lash_kernel_value::tokens::TokenCodec;
    match lash_kernel_doc::heap_values::integral_float(float.get()) {
        Some(int) => {
            out.sum(2, 1);
            int.encode(out);
        }
        None => {
            out.sum(3, 1);
            float.encode(out);
        }
    }
}

/// A member of a tuple or an error, as its content identity names it.
#[derive(Clone, Debug)]
pub enum Member<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(lash_kernel_doc::Float),
    Text(&'a str),
    Timestamp(i64),
    TaskRef(lash_kernel_doc::TaskId),
    /// A ref, by its referent's durable id.
    Ref(lash_kernel_doc::heap::EntityId),
    /// A ref whose referent has no durable id yet, by its process-local
    /// token: a tuple naming one is not certified before its seal.
    LocalRef(u64),
    /// An immutable object, by its head.
    Immutable(std::borrow::Cow<'a, Head>),
    /// A mutable object, by its durable id.
    Object(lash_kernel_doc::heap::EntityId),
    /// A mutable object with no durable id yet, by its process-local token.
    LocalObject(u64),
    Task(u64),
    Absent,
}

impl Member<'_> {
    /// Writes the member's tokens; gives whether it is a legal key, whose
    /// tokens are then the ones its key embeds in a tuple's normal form.
    pub fn write(&self, out: &mut TokenWriter) -> bool {
        use lash_kernel_doc::heap::Identity;
        use lash_kernel_value::tokens::TokenCodec;
        match self {
            Self::Null => out.sum(0, 0),
            Self::Bool(flag) => {
                out.sum(1, 1);
                flag.encode(out);
            }
            Self::Int(word) => {
                out.sum(2, 1);
                lash_kernel_doc::Integer::from(*word).encode(out);
            }
            Self::Float(float) => match lash_kernel_doc::heap_values::integral_float(float.get()) {
                // An integral float past a word is the big integer it
                // equals, which a tuple names by its head.
                Some(int) if int.word().is_none() => {
                    let mut normal = TokenWriter::new();
                    normal.sum(2, 1);
                    int.encode(&mut normal);
                    out.sum(25, 1);
                    Head::BigInt {
                        summary: lash_kernel_doc::heap::IntSummary {
                            bits: int.bits(),
                            negative: int.is_negative(),
                        },
                        normal: flat(normal),
                    }
                    .encode(out);
                }
                _ => float_key(*float, out),
            },
            Self::Text(text) => {
                out.sum(4, 1);
                out.text(text);
            }
            Self::Timestamp(word) => {
                out.sum(6, 1);
                lash_kernel_doc::Timestamp {
                    nanoseconds: lash_kernel_doc::Integer::from(*word),
                }
                .encode(out);
            }
            Self::TaskRef(task) => {
                out.sum(7, 1);
                Identity::Task(*task).encode(out);
            }
            Self::Ref(entity) => {
                out.sum(10, 1);
                Identity::Object(*entity).encode(out);
            }
            Self::LocalRef(token) => {
                out.sum(12, 1);
                out.unsigned(*token);
            }
            // A keyable immutable is named by its whole head, every field a
            // load opens it by, so the parent authenticates them all.
            Self::Immutable(head) => match (&**head, head.normal()) {
                (head, Some(_)) => {
                    out.sum(25, 1);
                    head.encode(out);
                }
                (Head::Function(callable), None) => {
                    out.sum(8, 1);
                    callable.encode(out);
                }
                (head, None) => {
                    out.sum(21, 1);
                    head.encode(out);
                    return false;
                }
            },
            Self::Object(entity) => {
                out.sum(20, 1);
                Identity::Object(*entity).encode(out);
                return false;
            }
            Self::LocalObject(token) => {
                out.sum(24, 1);
                out.unsigned(*token);
                return false;
            }
            Self::Task(task) => {
                out.sum(22, 1);
                out.unsigned(*task);
                return false;
            }
            Self::Absent => {
                out.sum(23, 0);
                return false;
            }
        }
        true
    }

    /// The member `value` names, which a page holding `heads` wrote;
    /// `None` for an immutable the page holds no head of.
    pub fn of_run<'h>(value: &'h RunValue, heads: &'h Heads) -> Option<Member<'h>> {
        use lash_kernel_doc::heap::{EntityId, FamilyId};
        let entity = |id: super::run::ids::ObjectId| EntityId {
            family: FamilyId::PRIMARY,
            ordinal: id.0,
        };
        Some(match value {
            RunValue::Null => Member::Null,
            RunValue::Absent => Member::Absent,
            RunValue::Bool(flag) => Member::Bool(*flag),
            RunValue::Int(word) => Member::Int(*word),
            RunValue::Float(float) => Member::Float(*float),
            RunValue::Text(text) => Member::Text(text),
            RunValue::Timestamp(word) => Member::Timestamp(*word),
            RunValue::Task(task) => Member::Task(*task),
            RunValue::TaskRef(task) => Member::TaskRef(lash_kernel_doc::TaskId(*task)),
            RunValue::Ref(id) => Member::Ref(entity(*id)),
            RunValue::Object(kind, id) if headed(*kind) => {
                Member::Immutable(std::borrow::Cow::Borrowed(heads.get(*id)?))
            }
            RunValue::Object(_, id) => Member::Object(entity(*id)),
        })
    }
}

/// Whether an object of `kind` is named by its head.
pub fn headed(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Tuple
            | ObjectKind::Error
            | ObjectKind::BigInt
            | ObjectKind::Timestamp
            | ObjectKind::LongText
            | ObjectKind::Bytes
            | ObjectKind::Function
            | ObjectKind::Handle
    )
}

/// The leaf over members `start .. start + count`, whose tokens are
/// `tokens`.
pub fn tuple_leaf(start: u64, count: u64, tokens: &[u8]) -> [u8; 32] {
    sha256(&[
        TUPLE_LEAF_TAG,
        &start.to_le_bytes(),
        &count.to_le_bytes(),
        tokens,
    ])
}

/// The members a tuple node of `level` spans; level zero is a leaf.
fn tuple_width(level: u32) -> u64 {
    (TUPLE_LEAF as u64).saturating_mul((TUPLE_FANOUT as u64).saturating_pow(level))
}

/// Node `at` of `level` of a tuple of `len` members, over its children's
/// `ids`.
pub fn tuple_node(len: u64, level: u32, at: u64, ids: &[[u8; 32]]) -> [u8; 32] {
    let width = tuple_width(level);
    let start = at.saturating_mul(width);
    let count = len.saturating_sub(start).min(width);
    let mut preimage = Vec::with_capacity(20 + ids.len() * 32);
    preimage.extend_from_slice(&start.to_le_bytes());
    preimage.extend_from_slice(&count.to_le_bytes());
    preimage.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in ids {
        preimage.extend_from_slice(id);
    }
    sha256(&[TUPLE_INDEX_TAG, &preimage])
}

/// The root of a tuple of `len` members whose leaves are `leaves`.
pub fn tuple_root(len: u64, leaves: &[[u8; 32]]) -> [u8; 32] {
    let mut nodes = leaves.to_vec();
    let mut level = 0;
    while nodes.len() > 1 || level < 2 {
        level += 1;
        nodes = nodes
            .chunks(TUPLE_FANOUT)
            .enumerate()
            .map(|(at, ids)| tuple_node(len, level, at as u64, ids))
            .collect();
        if nodes.is_empty() {
            return [0; 32];
        }
    }
    nodes[0]
}

/// The root of the tuple of `len` members whose flat normal form is
/// `flat`: its member tokens grouped into leaves as [`TupleBuilder`]
/// groups them, so a head that holds a tuple's flat form derives its root
/// from it and never carries a second one.
///
/// # Errors
///
/// `flat` is no flat tuple form of `len` members.
pub fn flat_root(len: u64, flat: &[u8]) -> Result<[u8; 32], TokenError> {
    let mut input = TokenReader::new(flat);
    let (tag, fields) = input.sum()?;
    if (tag, fields) != (9, 1) || input.seq()? != len {
        return Err(TokenError::Invalid(format!(
            "a tuple's flat form is no tuple of {len} members"
        )));
    }
    let mut leaves = Vec::with_capacity(usize::try_from(tuple_leaves(len)).unwrap_or(0));
    let mut start = input.position();
    let mut at = 0_u64;
    while at < len {
        let count = (len - at).min(TUPLE_LEAF as u64);
        for _ in 0..count {
            input.skip_tree()?;
        }
        let end = input.position();
        leaves.push(tuple_leaf(at, count, &flat[start..end]));
        start = end;
        at += count;
    }
    input.finish()?;
    if leaves.is_empty() {
        leaves.push(tuple_leaf(0, 0, &[]));
    }
    Ok(tuple_root(len, &leaves))
}

/// The leaves of a tuple of `len` members: one per [`TUPLE_LEAF`], and
/// one empty leaf for the empty tuple.
pub fn tuple_leaves(len: u64) -> u64 {
    len.div_ceil(TUPLE_LEAF as u64).max(1)
}

/// A tuple's content identity: its normal form when it is a legal key,
/// and its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TupleContent {
    pub normal: Option<NormalKey>,
    pub root: [u8; 32],
}

/// Builds a tuple's [`TupleContent`] from its members, in order.
pub struct TupleBuilder {
    len: u64,
    at: u64,
    keyable: bool,
    member: TokenWriter,
    leaf: Vec<u8>,
    leaves: Vec<[u8; 32]>,
    /// The flat tokens, while they still fit inline.
    flat: Option<TokenWriter>,
}

impl TupleBuilder {
    /// A builder for a tuple of `len` members.
    pub fn new(len: u64) -> Self {
        let mut flat = TokenWriter::new();
        flat.sum(9, 1);
        flat.seq(len);
        Self {
            len,
            at: 0,
            keyable: true,
            member: TokenWriter::new(),
            leaf: Vec::new(),
            leaves: Vec::new(),
            flat: Some(flat),
        }
    }

    /// Adds the next member.
    pub fn push(&mut self, member: &Member<'_>) {
        self.member.clear();
        self.keyable &= member.write(&mut self.member);
        let tokens = self.member.as_bytes();
        self.leaf.extend_from_slice(tokens);
        if let Some(flat) = &mut self.flat {
            if self.keyable && flat.len() + tokens.len() <= NormalKey::INLINE_MAX {
                flat.raw(tokens);
            } else {
                self.flat = None;
            }
        }
        self.at += 1;
        if self.at.is_multiple_of(TUPLE_LEAF as u64) {
            self.close_leaf();
        }
    }

    fn close_leaf(&mut self) {
        let count = self.at - self.leaves.len() as u64 * TUPLE_LEAF as u64;
        let start = self.at - count;
        self.leaves.push(tuple_leaf(start, count, &self.leaf));
        self.leaf.clear();
    }

    /// The content of the tuple.
    ///
    /// # Errors
    ///
    /// Members other than the tuple's length in number.
    pub fn finish(mut self) -> Result<TupleContent, HeapFault> {
        if self.at != self.len {
            return Err(HeapFault::Malformed(format!(
                "a tuple of {} members was given {}",
                self.len, self.at
            )));
        }
        if self.leaves.len() as u64 != tuple_leaves(self.len) {
            self.close_leaf();
        }
        let root = tuple_root(self.len, &self.leaves);
        let normal = self.keyable.then(|| match self.flat {
            Some(flat) => NormalKey::Inline(flat.into_bytes()),
            None => NormalKey::Digest(root),
        });
        Ok(TupleContent { normal, root })
    }
}

/// The leaf over the chunk of a tuple that holds members `start ..`, as
/// a page holding `heads` wrote them: `None` for a member the page names
/// no head of.
pub fn chunk_leaf(start: u64, members: &[RunValue], heads: &Heads) -> Option<[u8; 32]> {
    let mut tokens = TokenWriter::new();
    for member in members {
        Member::of_run(member, heads)?.write(&mut tokens);
    }
    Some(tuple_leaf(start, members.len() as u64, tokens.as_bytes()))
}

/// The content digest of an error whose kind, message and data are
/// `members`.
pub fn error_content(members: &[Member<'_>; 3]) -> [u8; 32] {
    let mut out = TokenWriter::new();
    for member in members {
        member.write(&mut out);
    }
    sha256(&[ERROR_TAG, out.as_bytes()])
}
