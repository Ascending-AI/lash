//! The one index of the durable heap (cutover contract §6 index rule):
//! every list of references an owner holds is this tree. That is a large
//! object's chunks, its edge pages and a run's segments.
//!
//! - A node names at most [`NODE_REFS`] references. A page at height one
//!   names the leaves, and a page at height `h` names nodes of height
//!   `h - 1`.
//! - The top node is embedded in its owner, which is the object's row or
//!   the run root. It is bounded like any node, and its height grows with
//!   the leaves, so nothing holds an unbounded list of parts.
//! - Each reference carries the first key under it, the leaves under it
//!   and, a tuple's, the content id of the node it names
//!   ([`identity::tuple_node`]). A reader checks every page it consumes
//!   against the reference that named it, so a tuple's path is checked to
//!   the root its head names.
//!
//! A point read through a tree of height `L` reads `L` index pages. A
//! point edit rewrites the index pages on its leaf's path, and only those.

use std::collections::BTreeSet;
use std::fmt::Debug;

use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter, decode_body};

use super::super::PageKind;
use super::super::identity;
use super::super::source::grant;

/// The most references one index node names, the top node included.
pub const NODE_REFS: usize = 96;

/// The references a node names when it is first written, when the tree
/// is not full: room for a quarter more.
const FILL: usize = NODE_REFS * 3 / 4;

/// What a tree's references are ordered by: a table chunk's placement,
/// an edge page's first id, or nothing.
pub trait Key: Copy + Ord + Debug {
    /// The fields the key takes in a reference.
    const FIELDS: u64;
    fn write(&self, out: &mut TokenWriter);
    /// # Errors
    ///
    /// Tokens that are not a key.
    fn read(input: &mut TokenReader<'_>) -> Result<Self, TokenError>;
}

impl Key for () {
    const FIELDS: u64 = 0;
    fn write(&self, _: &mut TokenWriter) {}
    fn read(_: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(())
    }
}

/// A leaf reference: what a page at height one names.
pub trait Leaf: TokenCodec + Clone + Debug {
    type Key: Key;
    fn key(&self) -> Self::Key;
    fn page(&self) -> PageRef;
    fn page_mut(&mut self) -> &mut PageRef;
    /// A tuple chunk's content leaf.
    fn id(&self) -> Option<[u8; 32]> {
        None
    }
}

/// A reference to an index page: the first key under it, the leaves
/// under it, its page and, a tuple's, the content id of the node it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part<K> {
    pub first: K,
    pub count: u64,
    pub page: PageRef,
    pub id: Option<[u8; 32]>,
}

impl<K: Key> TokenCodec for Part<K> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3 + K::FIELDS + u64::from(self.id.is_some()));
        self.first.write(out);
        out.unsigned(self.count);
        self.page.encode(out);
        encode_digest(&self.id, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let present = digest_arity(input.seq()?, 3 + K::FIELDS)?;
        Ok(Self {
            first: K::read(input)?,
            count: input.unsigned()?,
            page: PageRef::decode(input)?,
            id: decode_digest(input, present)?,
        })
    }
}

/// Writes an optional content id as a reference's last field: nothing when
/// it has none. Its presence is the reference's arity, one field more than
/// it has without one.
pub fn encode_digest(digest: &Option<[u8; 32]>, out: &mut TokenWriter) {
    if let Some(digest) = digest {
        out.bytes(digest);
    }
}

/// Whether a reference of `fields` fields besides its content id, whose
/// header gives `found` fields, has one.
///
/// # Errors
///
/// Any other field count.
pub fn digest_arity(found: u64, fields: u64) -> Result<bool, TokenError> {
    match found.checked_sub(fields) {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(lash_kernel_value::tokens::CanonicalFault::FieldCount {
            expected: fields,
            found,
        }
        .into()),
    }
}

/// Reads an optional content id, `present` as [`digest_arity`] gave it.
///
/// # Errors
///
/// Bytes that are not 32 long.
pub fn decode_digest(
    input: &mut TokenReader<'_>,
    present: bool,
) -> Result<Option<[u8; 32]>, TokenError> {
    present.then(|| digest(input)).transpose()
}

/// Reads a content id.
///
/// # Errors
///
/// Bytes that are not 32 long.
pub fn digest(input: &mut TokenReader<'_>) -> Result<[u8; 32], TokenError> {
    input
        .bytes()?
        .try_into()
        .map_err(|_| TokenError::Invalid("a content id is not 32 bytes".into()))
}

/// What one tree's pages are.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// The kind of its index pages.
    pub kind: PageKind,
    /// The fewest index levels below its top. A large object's chunk
    /// index has at least one, so its row names index pages, never chunks.
    pub min_height: u8,
    /// Every node but the last of its level is full, so a leaf's place is
    /// arithmetic and the tree is one shape for its count.
    pub full: bool,
    /// A tuple of this many members: every reference carries its node's
    /// content id.
    pub tuple: Option<u64>,
    /// Its pages are kept read once written, for a seal that walks them
    /// again.
    pub kept: bool,
}

impl Shape {
    /// The references one page groups when `refs` of one level are
    /// written together: a full tree's [`NODE_REFS`], another's all of
    /// them within the bound, else [`FILL`] to a page, spread evenly.
    pub fn width(&self, refs: u64) -> u64 {
        let bound = NODE_REFS as u64;
        if self.full {
            bound
        } else if refs <= bound {
            refs.max(1)
        } else {
            refs.div_ceil(refs.div_ceil(FILL as u64))
        }
    }

    /// The height of an index of `leaves`: the least, at least
    /// [`Self::min_height`], at which pages of [`Self::width`] bring the
    /// top within [`NODE_REFS`]. It is a function of the count alone and
    /// the one a build gives, and an edit's rebuild grows or shrinks to
    /// it.
    pub fn height(&self, leaves: u64) -> u8 {
        let mut height = 0;
        let mut count = leaves;
        while count > NODE_REFS as u64 || height < self.min_height {
            count = count.div_ceil(self.width(count));
            height += 1;
        }
        height
    }
}

/// The leaves a full node at `height` spans.
fn span(height: u8) -> u64 {
    (NODE_REFS as u64).saturating_pow(u32::from(height))
}

/// An index's top node, embedded in its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Index<L: Leaf> {
    /// The leaves themselves: no index page.
    Leaves(Vec<L>),
    /// References to the index pages of `height`.
    Nodes { height: u8, refs: Vec<Part<L::Key>> },
}

impl<L: Leaf> TokenCodec for Index<L> {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Leaves(leaves) => {
                out.sum(0, 1);
                leaves.encode(out);
            }
            Self::Nodes { height, refs } => {
                out.sum(1, 2);
                out.unsigned(u64::from(*height));
                refs.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let index = match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Leaves(Vec::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 2)?;
                let height = u8::try_from(input.unsigned()?)
                    .ok()
                    .filter(|height| *height > 0)
                    .ok_or_else(|| {
                        TokenError::Invalid("an index's height is out of range".into())
                    })?;
                Self::Nodes {
                    height,
                    refs: Vec::decode(input)?,
                }
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        };
        if index.width() > NODE_REFS {
            return Err(TokenError::Invalid(
                "an index's top names more than a node may".into(),
            ));
        }
        Ok(index)
    }
}

/// Where a read goes: to a leaf by its place, or to the last leaf whose
/// first key is at most a key, else the first.
#[derive(Clone, Copy, Debug)]
pub enum Seek<K> {
    At(u64),
    Key(K),
}

/// One node's references, as read or about to be written.
enum Level<L: Leaf> {
    Leaves(Vec<L>),
    Parts(Vec<Part<L::Key>>),
}

impl<L: Leaf> Level<L> {
    fn len(&self) -> usize {
        match self {
            Self::Leaves(leaves) => leaves.len(),
            Self::Parts(parts) => parts.len(),
        }
    }
}

fn corrupt(page: PageRef, reason: &str) -> HeapFault {
    HeapFault::Corrupt {
        page: page.id,
        reason: reason.into(),
    }
}

/// Of keys in order, the one a key falls in: the last at most it, else
/// the first.
fn placed<K: Key>(keys: impl Iterator<Item = K>, key: &K) -> usize {
    keys.take_while(|first| first <= key)
        .count()
        .saturating_sub(1)
}

impl<L: Leaf> Index<L> {
    /// The index of no leaves.
    pub fn empty(shape: &Shape) -> Self {
        match shape.min_height {
            0 => Self::Leaves(Vec::new()),
            height => Self::Nodes {
                height,
                refs: Vec::new(),
            },
        }
    }

    /// The index levels below the top.
    pub fn height(&self) -> u8 {
        match self {
            Self::Leaves(_) => 0,
            Self::Nodes { height, .. } => *height,
        }
    }

    /// The references the top names.
    pub fn width(&self) -> usize {
        match self {
            Self::Leaves(leaves) => leaves.len(),
            Self::Nodes { refs, .. } => refs.len(),
        }
    }

    /// How many leaves the index holds.
    pub fn leaves(&self) -> u64 {
        match self {
            Self::Leaves(leaves) => leaves.len() as u64,
            Self::Nodes { refs, .. } => refs
                .iter()
                .fold(0u64, |sum, part| sum.saturating_add(part.count)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width() == 0
    }

    /// The pages the top names, which its owner's page references.
    pub fn pages(&self) -> Vec<PageRef> {
        match self {
            Self::Leaves(leaves) => leaves.iter().map(Leaf::page).collect(),
            Self::Nodes { refs, .. } => refs.iter().map(|part| part.page).collect(),
        }
    }

    /// Grants the pages the top names from its owner's `children`.
    ///
    /// # Errors
    ///
    /// A page the owner does not reference.
    pub fn grant(&mut self, children: &[PageRef]) -> Result<(), HeapFault> {
        match self {
            Self::Leaves(leaves) => {
                for leaf in leaves {
                    grant(leaf.page_mut(), children)?;
                }
            }
            Self::Nodes { refs, .. } => {
                for part in refs {
                    grant(&mut part.page, children)?;
                }
            }
        }
        Ok(())
    }

    /// A tuple's content root: the node above the top's references.
    pub fn tuple_root(&self, len: u64) -> Option<[u8; 32]> {
        let Self::Nodes { height, refs } = self else {
            return None;
        };
        let ids = refs
            .iter()
            .map(|part| part.id)
            .collect::<Option<Vec<_>>>()?;
        Some(identity::tuple_node(len, u32::from(*height) + 1, 0, &ids))
    }

    /// Checks what decoding cannot: the top is of `shape`, its references
    /// each name leaves, in key order, a full tree's at its one shape, and
    /// a tuple's each with a content id.
    ///
    /// # Errors
    ///
    /// What is wrong.
    pub fn check(&self, shape: &Shape) -> Result<(), String> {
        let keys: Vec<L::Key> = match self {
            Self::Leaves(leaves) => leaves.iter().map(Leaf::key).collect(),
            Self::Nodes { refs, .. } => refs.iter().map(|part| part.first).collect(),
        };
        if !keys.windows(2).all(|pair| pair[0] <= pair[1]) {
            return Err("an index's top is out of key order".into());
        }
        if self.height() < shape.min_height {
            return Err("an index's top is below its least height".into());
        }
        let ids = match self {
            Self::Leaves(leaves) => leaves.iter().all(|leaf| leaf.id().is_some()),
            Self::Nodes { refs, .. } => {
                if refs.iter().any(|part| part.count == 0) {
                    return Err("an index names an empty part".into());
                }
                refs.iter().all(|part| part.id.is_some())
            }
        };
        let tupled = match self {
            Self::Leaves(leaves) => leaves.iter().any(|leaf| leaf.id().is_some()),
            Self::Nodes { refs, .. } => refs.iter().any(|part| part.id.is_some()),
        };
        if (shape.tuple.is_some() && !ids) || (shape.tuple.is_none() && tupled) {
            return Err("an index's content ids are another kind's".into());
        }
        if self.height() != shape.height(self.leaves()) {
            return Err("an index is not at the height its leaves give".into());
        }
        if shape.full {
            if let Self::Nodes { height, refs } = self {
                let last = refs.len().saturating_sub(1);
                if refs.iter().enumerate().any(|(at, part)| {
                    (at < last && part.count != span(*height)) || part.count > span(*height)
                }) {
                    return Err("a full index's top is not full".into());
                }
            }
        }
        Ok(())
    }

    /// The index page `part` names, at `height`, whose first leaf is
    /// `start`: its references granted, and checked against `part`.
    fn node(
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        part: &Part<L::Key>,
        height: u8,
        start: u64,
    ) -> Result<Level<L>, HeapFault> {
        let answer = access.read(part.page)?;
        if answer.kind != shape.kind {
            return Err(corrupt(part.page, "an index part is not an index page"));
        }
        let decoded = |error: TokenError| corrupt(part.page, &error.to_string());
        let (level, count, keys, ids) = if height == 1 {
            let mut leaves: Vec<L> = decode_body(&answer.tokens).map_err(decoded)?;
            for leaf in &mut leaves {
                grant(leaf.page_mut(), &answer.children)?;
            }
            let keys: Vec<L::Key> = leaves.iter().map(Leaf::key).collect();
            let ids: Option<Vec<_>> = leaves.iter().map(Leaf::id).collect();
            let leaves_len = leaves.len() as u64;
            (Level::Leaves(leaves), leaves_len, keys, ids)
        } else {
            let mut parts: Vec<Part<L::Key>> = decode_body(&answer.tokens).map_err(decoded)?;
            for child in &mut parts {
                grant(&mut child.page, &answer.children)?;
            }
            let under = span(height - 1);
            let last = parts.len().saturating_sub(1);
            if parts.iter().enumerate().any(|(at, child)| {
                child.count == 0
                    || (shape.full && ((at < last && child.count != under) || child.count > under))
            }) {
                return Err(corrupt(
                    part.page,
                    "an index page's parts are not its shape",
                ));
            }
            let count = parts
                .iter()
                .try_fold(0u64, |sum, child| sum.checked_add(child.count))
                .ok_or_else(|| corrupt(part.page, "an index page's count overflows"))?;
            let keys: Vec<L::Key> = parts.iter().map(|child| child.first).collect();
            let ids: Option<Vec<_>> = parts.iter().map(|child| child.id).collect();
            (Level::Parts(parts), count, keys, ids)
        };
        if level.len() == 0 || level.len() > NODE_REFS {
            return Err(corrupt(
                part.page,
                "an index page names no reference, or more than it may",
            ));
        }
        if count != part.count
            || keys.first() != Some(&part.first)
            || !keys.windows(2).all(|pair| pair[0] <= pair[1])
        {
            return Err(corrupt(
                part.page,
                "an index page is not the one its parent names",
            ));
        }
        if let Some(len) = shape.tuple {
            let ids =
                ids.ok_or_else(|| corrupt(part.page, "a tuple's index page has no content ids"))?;
            let id = identity::tuple_node(len, u32::from(height), start / span(height), &ids);
            if part.id != Some(id) {
                return Err(corrupt(
                    part.page,
                    "a tuple's index page is not the one its parent commits to",
                ));
            }
        }
        Ok(level)
    }

    /// The leaf `seek` finds and its place, each index page on the way
    /// read once and checked against the reference that named it; `None`
    /// past the last leaf or in an empty index.
    ///
    /// # Errors
    ///
    /// The page port's faults; [`HeapFault::Corrupt`] for an index page
    /// that is not the one its parent names.
    pub fn get(
        &self,
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        seek: Seek<L::Key>,
    ) -> Result<Option<(u64, L)>, HeapFault> {
        let pick = |leaves: &[L], start: u64| -> Option<(u64, L)> {
            let at = match seek {
                Seek::At(at) => usize::try_from(at.checked_sub(start)?).ok()?,
                Seek::Key(key) => placed(leaves.iter().map(Leaf::key), &key),
            };
            leaves.get(at).map(|leaf| (start + at as u64, leaf.clone()))
        };
        let (mut height, mut refs) = match self {
            Self::Leaves(leaves) => return Ok(pick(leaves, 0)),
            Self::Nodes { height, refs } => (*height, refs.clone()),
        };
        let mut start = 0u64;
        loop {
            let at = match seek {
                Seek::At(at) => {
                    let mut found = None;
                    let mut first = start;
                    for (slot, part) in refs.iter().enumerate() {
                        if at < first.saturating_add(part.count) {
                            found = Some(slot);
                            break;
                        }
                        first = first.saturating_add(part.count);
                    }
                    match found {
                        Some(slot) => slot,
                        None => return Ok(None),
                    }
                }
                Seek::Key(key) => placed(refs.iter().map(|part| part.first), &key),
            };
            let Some(part) = refs.get(at).copied() else {
                return Ok(None);
            };
            start += refs[..at].iter().map(|part| part.count).sum::<u64>();
            match Self::node(access, shape, &part, height, start)? {
                Level::Leaves(leaves) => return Ok(pick(&leaves, start)),
                Level::Parts(parts) => {
                    refs = parts;
                    height -= 1;
                }
            }
        }
    }

    /// Calls `each` with every leaf and its place, in order, until it
    /// answers `true`; gives whether one did. Each index page is read
    /// once and checked against the reference that named it.
    ///
    /// # Errors
    ///
    /// As [`Self::get`], and `each`'s.
    pub fn each(
        &self,
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        each: &mut dyn FnMut(u64, &L) -> Result<bool, HeapFault>,
    ) -> Result<bool, HeapFault> {
        fn walk<L: Leaf>(
            access: &super::super::tree::Access<'_>,
            shape: &Shape,
            level: &Level<L>,
            height: u8,
            start: u64,
            each: &mut dyn FnMut(u64, &L) -> Result<bool, HeapFault>,
        ) -> Result<bool, HeapFault> {
            match level {
                Level::Leaves(leaves) => {
                    for (at, leaf) in leaves.iter().enumerate() {
                        if each(start + at as u64, leaf)? {
                            return Ok(true);
                        }
                    }
                }
                Level::Parts(parts) => {
                    let mut first = start;
                    for part in parts {
                        let below = Index::<L>::node(access, shape, part, height, first)?;
                        if walk(access, shape, &below, height - 1, first, each)? {
                            return Ok(true);
                        }
                        first += part.count;
                    }
                }
            }
            Ok(false)
        }
        match self {
            Self::Leaves(leaves) => walk(access, shape, &Level::Leaves(leaves.clone()), 0, 0, each),
            Self::Nodes { height, refs } => {
                walk(access, shape, &Level::Parts(refs.clone()), *height, 0, each)
            }
        }
    }

    /// The index over `leaves`, in order, its index pages sealed.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn build(
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        leaves: Vec<L>,
    ) -> Result<Self, HeapFault> {
        Self::empty(shape).rebuild(
            access,
            shape,
            &BTreeSet::new(),
            &mut |_, leaf| Ok(vec![leaf]),
            leaves,
        )
    }

    /// The index over `leaves`, in order, as [`Self::rebuild`] gives it
    /// from this one: a place both hold is changed only where its leaf
    /// differs, the places past `leaves`' end are dropped and the leaves
    /// past this index's end appended, so an index page is sealed again
    /// only where its content changes.
    ///
    /// # Errors
    ///
    /// As [`Self::rebuild`].
    pub fn revise(
        &self,
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        leaves: Vec<L>,
    ) -> Result<Self, HeapFault> {
        let mut old = Vec::new();
        self.each(access, shape, &mut |_, leaf| {
            old.push(lash_kernel_value::tokens::encode_body(leaf));
            Ok(false)
        })?;
        let mut leaves = leaves.into_iter();
        let mut fresh = std::collections::BTreeMap::new();
        let mut held = 0u64;
        for (at, (was, now)) in old.iter().zip(leaves.by_ref()).enumerate() {
            held += 1;
            if *was != lash_kernel_value::tokens::encode_body(&now) {
                fresh.insert(at as u64, now);
            }
        }
        let mut changed: BTreeSet<u64> = fresh.keys().copied().collect();
        changed.extend(held..old.len() as u64);
        let tail: Vec<L> = leaves.collect();
        if changed.is_empty() && tail.is_empty() {
            return Ok(self.clone());
        }
        self.rebuild(
            access,
            shape,
            &changed,
            &mut |at, _| Ok(fresh.remove(&at).into_iter().collect()),
            tail,
        )
    }

    /// The index with each leaf whose place is in `changed` replaced by
    /// what `replace` gives for it (none drops it) and `tail` appended:
    /// only the index pages on a changed leaf's path, or the last path
    /// when `tail` is not empty, are read and written again; a node past
    /// its bound splits, and the top moves to the height its leaves give
    /// ([`Shape::height`]): it grows a level, or gives way to the nodes
    /// below it while they fit one node. Only when they do not is the
    /// index built again from its leaves.
    ///
    /// # Errors
    ///
    /// As [`Self::get`], and `replace`'s.
    pub fn rebuild(
        &self,
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        changed: &BTreeSet<u64>,
        replace: &mut dyn FnMut(u64, L) -> Result<Vec<L>, HeapFault>,
        tail: Vec<L>,
    ) -> Result<Self, HeapFault> {
        let tail = (!tail.is_empty()).then_some(tail);
        let (mut height, mut level) = match self {
            Self::Leaves(leaves) => (
                0,
                Self::redo_level(
                    access,
                    shape,
                    Level::Leaves(leaves.clone()),
                    0,
                    0,
                    changed,
                    replace,
                    tail,
                )?,
            ),
            Self::Nodes { refs, .. } if refs.is_empty() => {
                (0, Level::Leaves(tail.unwrap_or_default()))
            }
            Self::Nodes { height, refs } => (
                *height,
                Self::redo_level(
                    access,
                    shape,
                    Level::Parts(refs.clone()),
                    *height,
                    0,
                    changed,
                    replace,
                    tail,
                )?,
            ),
        };
        let leaves = match &level {
            Level::Leaves(leaves) => leaves.len() as u64,
            Level::Parts(parts) => parts.iter().map(|part| part.count).sum(),
        };
        let target = shape.height(leaves);
        while level.len() > NODE_REFS || height < target {
            level = Level::Parts(Self::group(access, shape, level, height, 0)?);
            height += 1;
        }
        while height > target {
            let Level::Parts(parts) = &level else {
                return Err(HeapFault::Malformed(
                    "an index above its height holds leaves".into(),
                ));
            };
            let mut below = if height == 1 {
                Level::Leaves(Vec::new())
            } else {
                Level::Parts(Vec::new())
            };
            let mut first = 0u64;
            for part in parts {
                match (&mut below, Self::node(access, shape, part, height, first)?) {
                    (Level::Leaves(out), Level::Leaves(leaves)) => out.extend(leaves),
                    (Level::Parts(out), Level::Parts(parts)) => out.extend(parts),
                    _ => {
                        return Err(HeapFault::Malformed(
                            "an index page is of another level".into(),
                        ));
                    }
                }
                first += part.count;
            }
            if below.len() > NODE_REFS {
                let top = Self::Nodes {
                    height,
                    refs: parts.clone(),
                };
                let mut leaves = Vec::new();
                top.each(access, shape, &mut |_, leaf| {
                    leaves.push(leaf.clone());
                    Ok(false)
                })?;
                return Self::build(access, shape, leaves);
            }
            level = below;
            height -= 1;
        }
        Ok(match level {
            Level::Leaves(leaves) => Self::Leaves(leaves),
            Level::Parts(refs) => Self::Nodes { height, refs },
        })
    }

    /// The references of `level`, a node at `height` whose first leaf is
    /// `start`, with its changed leaves replaced and `tail` appended at its
    /// end: the parts below it rewritten where they change.
    #[expect(clippy::too_many_arguments, reason = "one node of one rebuild")]
    fn redo_level(
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        level: Level<L>,
        height: u8,
        start: u64,
        changed: &BTreeSet<u64>,
        replace: &mut dyn FnMut(u64, L) -> Result<Vec<L>, HeapFault>,
        mut tail: Option<Vec<L>>,
    ) -> Result<Level<L>, HeapFault> {
        Ok(match level {
            Level::Leaves(leaves) => {
                let mut out = Vec::with_capacity(leaves.len());
                for (at, leaf) in leaves.into_iter().enumerate() {
                    let at = start + at as u64;
                    if changed.contains(&at) {
                        out.extend(replace(at, leaf)?);
                    } else {
                        out.push(leaf);
                    }
                }
                out.extend(tail.unwrap_or_default());
                Level::Leaves(out)
            }
            Level::Parts(parts) => {
                let last = parts.len().saturating_sub(1);
                let mut out = Vec::with_capacity(parts.len());
                let mut first = start;
                for (at, part) in parts.into_iter().enumerate() {
                    let ends = if at == last { tail.take() } else { None };
                    let end = first.saturating_add(part.count);
                    if ends.is_none() && changed.range(first..end).next().is_none() {
                        out.push(part);
                    } else {
                        let below = Self::node(access, shape, &part, height, first)?;
                        let below = Self::redo_level(
                            access,
                            shape,
                            below,
                            height - 1,
                            first,
                            changed,
                            replace,
                            ends,
                        )?;
                        out.extend(Self::group(access, shape, below, height - 1, first)?);
                    }
                    first = end;
                }
                Level::Parts(out)
            }
        })
    }

    /// The pages of height `height + 1` over `level`, the references of
    /// height `height` whose first leaf is `start`, sealed: a full tree's
    /// [`NODE_REFS`] to a page, another's in one page within the bound,
    /// else [`FILL`] to a page.
    fn group(
        access: &super::super::tree::Access<'_>,
        shape: &Shape,
        level: Level<L>,
        height: u8,
        start: u64,
    ) -> Result<Vec<Part<L::Key>>, HeapFault> {
        let len = level.len();
        if len == 0 {
            return Ok(Vec::new());
        }
        let width = usize::try_from(shape.width(len as u64)).unwrap_or(NODE_REFS);
        let above = height + 1;
        let first_node = start / span(above);
        let seal = |tokens: Vec<u8>, children: Vec<PageRef>| {
            if shape.kept {
                access.seal_kept(shape.kind, tokens, children)
            } else {
                access.seal(shape.kind, tokens, children)
            }
        };
        let id = |at: usize, ids: Option<Vec<[u8; 32]>>| -> Result<Option<[u8; 32]>, HeapFault> {
            let Some(len) = shape.tuple else {
                return Ok(None);
            };
            let ids = ids.ok_or_else(|| {
                HeapFault::Malformed("a tuple's index names a reference with no content id".into())
            })?;
            Ok(Some(identity::tuple_node(
                len,
                u32::from(above),
                first_node + at as u64,
                &ids,
            )))
        };
        let mut out = Vec::with_capacity(len.div_ceil(width));
        match level {
            Level::Leaves(leaves) => {
                for (at, run) in leaves.chunks(width).enumerate() {
                    let run = run.to_vec();
                    let children = run.iter().map(Leaf::page).collect();
                    let ids = run.iter().map(Leaf::id).collect();
                    let tokens = lash_kernel_value::tokens::encode_body(&run);
                    out.push(Part {
                        first: run[0].key(),
                        count: run.len() as u64,
                        page: seal(tokens, children)?,
                        id: id(at, ids)?,
                    });
                }
            }
            Level::Parts(parts) => {
                for (at, run) in parts.chunks(width).enumerate() {
                    let run = run.to_vec();
                    let children = run.iter().map(|part| part.page).collect();
                    let ids = run.iter().map(|part| part.id).collect();
                    let count = run
                        .iter()
                        .try_fold(0u64, |sum, part| sum.checked_add(part.count))
                        .ok_or_else(|| HeapFault::Malformed("an index's count overflows".into()))?;
                    let tokens = lash_kernel_value::tokens::encode_body(&run);
                    out.push(Part {
                        first: run[0].first,
                        count,
                        page: seal(tokens, children)?,
                        id: id(at, ids)?,
                    });
                }
            }
        }
        Ok(out)
    }
}
