//! The canonical page constructor (seam H). A sealed tree's pages are a
//! function of its rows, whatever history built them.
//!
//! Every level is cut into chunks by one rule, read from the chunk's own
//! start: a chunk ends after the item that reaches its count cap or
//! [`PAGE_TARGET_BYTES`] packing bytes, and a keyed chunk also ends after
//! an item whose boundary hash has its low bits zero. Sequences cut by
//! position only: leaf `k` holds cells `64k ..< 64(k+1)` unless a cell run
//! reaches the byte cap first. A boundary therefore depends only on the
//! items since the previous one, so a seal rebuilds a dirty region and
//! reuses a clean page as soon as the cut before it falls where it did.
use super::super::source::SEQUENCE_LEAF_CELLS;
use super::*;

/// Packing bytes an index entry counts beside its first key: its summary,
/// family and child reference.
const INDEX_ENTRY_BYTES: u64 = 96;

/// Rows a leaf holds at most: twice the keyed mean, or the sequence chunk.
pub(super) fn leaf_cap(sequence: bool) -> usize {
    if sequence {
        SEQUENCE_LEAF_CELLS as usize
    } else {
        2 * KEYED_LEAF_ROWS as usize
    }
}

/// Children an index node holds at most: twice the keyed mean fanout, or
/// the sequence fanout.
pub(super) fn index_cap(sequence: bool) -> usize {
    if sequence {
        INDEX_FANOUT as usize
    } else {
        2 * INDEX_FANOUT as usize
    }
}

/// `H(level, key)`: FNV-1a over the level's little-endian bytes and the
/// key, then the SplitMix64 finalizer. Fixed and seedless; never a page
/// digest.
pub(crate) fn boundary_hash(level: u64, key: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in level.to_le_bytes().iter().chain(key) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

/// Whether a chunk at `level` (0 for leaves) holding `count` items of
/// `bytes` packing bytes ends after its last item, keyed `last`.
fn ends(sequence: bool, level: u64, count: usize, bytes: u64, last: &[u8]) -> bool {
    let (cap, mean) = if level == 0 {
        (leaf_cap(sequence), KEYED_LEAF_ROWS)
    } else {
        (index_cap(sequence), INDEX_FANOUT)
    };
    count >= cap
        || bytes >= PAGE_TARGET_BYTES
        || (!sequence && boundary_hash(level, last) & (mean - 1) == 0)
}

/// A leaf row's packing bytes: its encoded row tokens.
pub(super) fn row_bytes<T: Cell>(access: &Access<'_>, key: &Key, cell: &T) -> u64 {
    (2 + access.encoded_len(key) + access.encoded_len(cell)) as u64
}

fn entry_bytes(first: &Key) -> u64 {
    first.len() as u64 + INDEX_ENTRY_BYTES
}

/// Cuts `items` at the rule's boundaries from their own start, keeping the
/// first chunk in `items` and returning the rest. A working node that
/// passed its cap stays bounded this way between seals; the seal then
/// places every boundary canonically.
pub(super) fn cut<I>(
    items: &mut Vec<I>,
    sequence: bool,
    level: u64,
    measure: impl Fn(usize, &I) -> (u64, &[u8]),
) -> Vec<Vec<I>> {
    let mut cuts = Vec::new();
    let (mut count, mut bytes) = (0, 0);
    for (i, item) in items.iter().enumerate() {
        let (size, key) = measure(i, item);
        count += 1;
        bytes += size;
        if i + 1 < items.len() && ends(sequence, level, count, bytes, key) {
            cuts.push(i + 1);
            count = 0;
            bytes = 0;
        }
    }
    let mut pieces: Vec<Vec<I>> = cuts.iter().rev().map(|at| items.split_off(*at)).collect();
    pieces.reverse();
    pieces
}

/// Cuts an overflowing working leaf.
pub(super) fn cut_leaf<T: Cell>(
    rows: &mut Vec<(Key, T)>,
    sequence: bool,
    access: &Access<'_>,
) -> Vec<Vec<(Key, T)>> {
    let sizes: Vec<u64> = rows
        .iter()
        .map(|(key, cell)| row_bytes(access, key, cell))
        .collect();
    cut(rows, sequence, 0, |at, (key, _)| {
        (sizes[at], key.as_slice())
    })
}

/// Cuts an overflowing working index node. Its height is not known while
/// editing, so the cut uses level 1; the seal places it canonically.
pub(super) fn cut_index<T: Cell>(
    children: &mut Vec<Branch<T>>,
    sequence: bool,
) -> Vec<Vec<Branch<T>>> {
    cut(children, sequence, 1, |_, child| {
        (entry_bytes(&child.first), child.first.as_slice())
    })
}

/// The index height of the node `branch` names: 0 for a leaf. It descends
/// a working path where there is one, so it reads a page only below a
/// working node whose children are all sealed.
fn height<T: Cell>(
    branch: &Branch<T>,
    access: &Access<'_>,
    sequence: bool,
) -> Result<u64, HeapFault> {
    if let Link::Dirty(page) = &branch.link {
        match &*page.borrow() {
            Node::Leaf(_) => return Ok(0),
            Node::Staged(_, _, true) => return Ok(0),
            Node::Index(children) => {
                let next = children
                    .iter()
                    .find(|child| matches!(child.link, Link::Dirty(_)))
                    .or(children.first())
                    .ok_or_else(|| HeapFault::Malformed("empty working index".into()))?;
                return Ok(1 + height(next, access, sequence)?);
            }
            Node::Staged(..) => {}
        }
    }
    if branch.is_leaf == Some(true) {
        return Ok(0);
    }
    match &*Tree::load_shared(branch, access, sequence)? {
        Node::Leaf(_) => Ok(0),
        Node::Index(children) => Ok(1 + height(&children[0], access, sequence)?),
        Node::Staged(..) => unreachable!("loaded staged node"),
    }
}

/// Prepares every working leaf's cells for publication; `false` when the
/// budget ran out, keeping what was prepared.
fn prepare<T: Cell>(
    branch: &Branch<T>,
    access: &Access<'_>,
    work: &mut SealWork,
) -> Result<bool, HeapFault> {
    let Link::Dirty(page) = &branch.link else {
        return Ok(true);
    };
    let mut node = page.borrow_mut();
    match &mut *node {
        Node::Staged(..) => Ok(true),
        Node::Leaf(rows) => {
            access.forget_rows(page);
            for (_, cell) in rows.iter_mut() {
                if !cell.prepare(access, work)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Node::Index(children) => {
            for child in children.iter() {
                if !prepare(child, access, work)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

/// One level's open chunk. `ended` defers closing until the next item
/// arrives, so a level's only item becomes the root instead of the single
/// child of a node above it.
struct Open<I> {
    items: Vec<I>,
    bytes: u64,
    ended: bool,
}
impl<I> Default for Open<I> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            bytes: 0,
            ended: false,
        }
    }
}

/// Streams a tree's rows bottom-up into canonical chunks, taking a sealed
/// subtree whole when every level below it is at a boundary.
struct Builder<'s, 'a, T: Cell> {
    access: &'s Access<'a>,
    work: &'s mut SealWork,
    sequence: bool,
    /// False once the budget ran out: later chunks stay working nodes.
    sealing: bool,
    leaf: Open<(Key, T)>,
    /// The open leaf's encoded rows.
    tokens: TokenWriter,
    /// `index[k]` is level `k + 1`.
    index: Vec<Open<Branch<T>>>,
}

impl<'s, 'a, T: Cell> Builder<'s, 'a, T> {
    fn feed(&mut self, branch: &Branch<T>, height: u64) -> Result<(), HeapFault> {
        if matches!(branch.link, Link::Sealed(_)) && self.align(height)? {
            return self.push_branch(height + 1, branch.clone());
        }
        let loaded = Tree::load_node(branch, self.access, self.sequence)?;
        match (&*loaded.node(), height) {
            (Node::Leaf(rows), 0) => {
                for (key, cell) in rows {
                    self.push_row(key.clone(), cell.clone())?;
                }
            }
            (Node::Index(children), 1..) => {
                for child in children {
                    self.feed(child, height - 1)?;
                }
            }
            _ => return Err(HeapFault::Malformed("unbalanced tree".into())),
        }
        Ok(())
    }

    /// Closes the ended chunks of levels `0..=height`; true when all of
    /// them are then empty, so a sealed node of that height starts at a
    /// boundary here as it did where it was built.
    fn align(&mut self, height: u64) -> Result<bool, HeapFault> {
        if !self.leaf.items.is_empty() {
            if !self.leaf.ended {
                return Ok(false);
            }
            self.close_leaf()?;
        }
        for level in 1..=height {
            match self.index.get((level - 1) as usize) {
                Some(open) if !open.items.is_empty() => {
                    if !open.ended {
                        return Ok(false);
                    }
                    self.close_index(level)?;
                }
                _ => {}
            }
        }
        Ok(true)
    }

    fn push_row(&mut self, key: Key, cell: T) -> Result<(), HeapFault> {
        if self.leaf.ended {
            self.close_leaf()?;
        }
        let start = self.tokens.len();
        self.tokens.seq(2);
        key.encode(&mut self.tokens);
        cell.encode(&mut self.tokens);
        self.leaf.bytes += (self.tokens.len() - start) as u64;
        self.leaf.ended = ends(
            self.sequence,
            0,
            self.leaf.items.len() + 1,
            self.leaf.bytes,
            key.as_slice(),
        );
        self.leaf.items.push((key, cell));
        Ok(())
    }

    fn push_branch(&mut self, level: u64, branch: Branch<T>) -> Result<(), HeapFault> {
        let at = (level - 1) as usize;
        if self.index.len() <= at {
            self.index.resize_with(at + 1, Open::default);
        }
        if self.index[at].ended {
            self.close_index(level)?;
        }
        let sequence = self.sequence;
        let open = &mut self.index[at];
        open.bytes += entry_bytes(&branch.first);
        open.ended = ends(
            sequence,
            level,
            open.items.len() + 1,
            open.bytes,
            branch.first.as_slice(),
        );
        open.items.push(branch);
        Ok(())
    }

    fn close_leaf(&mut self) -> Result<(), HeapFault> {
        let branch = self.leaf_node()?;
        self.push_branch(1, branch)
    }

    fn close_index(&mut self, level: u64) -> Result<(), HeapFault> {
        let branch = self.index_node(level)?;
        self.push_branch(level + 1, branch)
    }

    /// The open leaf as a node: a sealed page while the budget lasts.
    fn leaf_node(&mut self) -> Result<Branch<T>, HeapFault> {
        let open = std::mem::take(&mut self.leaf);
        let body = std::mem::take(&mut self.tokens);
        let node = Node::Leaf(open.items);
        if self.sealing {
            let mut tokens = TokenWriter::with_capacity(body.len() + 10);
            let Node::Leaf(rows) = &node else {
                unreachable!()
            };
            tokens.seq(rows.len() as u64);
            tokens.raw(body.as_bytes());
            let mut children = Vec::new();
            for (_, cell) in rows {
                cell.children(&mut children);
            }
            let kind = if self.sequence {
                PageKind::SEQUENCE_LEAF
            } else {
                PageKind::KEYED_LEAF
            };
            if let Some(branch) = self.sealed(&node, kind, tokens.into_bytes(), children)? {
                return Ok(branch);
            }
        }
        dirty(node, self.access, self.sequence)
    }

    /// The open chunk of `level` as a node: a sealed page while the budget
    /// lasts and every child is sealed.
    fn index_node(&mut self, level: u64) -> Result<Branch<T>, HeapFault> {
        let open = std::mem::take(&mut self.index[(level - 1) as usize]);
        if self.sealing {
            let mut children = Vec::with_capacity(open.items.len());
            let mut data = Vec::with_capacity(open.items.len());
            for child in &open.items {
                let Link::Sealed(page) = child.link else {
                    break;
                };
                children.push(page);
                data.push((child.first.clone(), (child.summary, (child.family, page))));
            }
            if data.len() == open.items.len() {
                let kind = if self.sequence {
                    PageKind::SEQUENCE_INDEX
                } else {
                    PageKind::KEYED_INDEX
                };
                let node = Node::Index(open.items);
                if let Some(branch) = self.sealed(&node, kind, encode_body(&data), children)? {
                    return Ok(branch);
                }
                return dirty(node, self.access, self.sequence);
            }
        }
        dirty(Node::Index(open.items), self.access, self.sequence)
    }

    /// Seals `node`'s encoding, or `None` once the budget is spent.
    fn sealed(
        &mut self,
        node: &Node<T>,
        kind: PageKind,
        tokens: Vec<u8>,
        children: Vec<PageRef>,
    ) -> Result<Option<Branch<T>>, HeapFault> {
        if !self
            .work
            .take(tokens.len() as u64 + children.len() as u64 * 32 + 32)
        {
            self.sealing = false;
            return Ok(None);
        }
        let encoded_bytes = tokens.len() as u64;
        let page = self.access.seal(kind, tokens, children)?;
        let mut branch = Branch {
            family: None,
            encoded_bytes: None,
            is_leaf: None,
            tasks_known: true,
            members_known: true,
            first: Key(Vec::new()),
            summary: Summary::default(),
            link: Link::Sealed(page),
        };
        refresh(&mut branch, node)?;
        branch.encoded_bytes = Some(encoded_bytes);
        branch.is_leaf = Some(matches!(node, Node::Leaf(_)));
        Ok(Some(branch))
    }

    /// Closes every level; the root is the first level with one item.
    fn finish(mut self) -> Result<(Option<Branch<T>>, bool), HeapFault> {
        if self.index.is_empty() {
            if self.leaf.items.is_empty() {
                return Ok((None, self.sealing));
            }
            let root = self.leaf_node()?;
            return Ok((Some(root), self.sealing));
        }
        if !self.leaf.items.is_empty() {
            self.close_leaf()?;
        }
        let mut level = 1;
        loop {
            let at = (level - 1) as usize;
            let top = self.index.len() == level as usize;
            if top && self.index[at].items.len() == 1 {
                let root = self.index[at].items.pop();
                return Ok((root, self.sealing));
            }
            if !self.index[at].items.is_empty() {
                self.close_index(level)?;
            }
            level += 1;
        }
    }
}

impl<T: Cell> Tree<T> {
    /// Seals the tree's working pages into canonical pages. `false` when
    /// the budget ran out: what was sealed is kept, and the next seal
    /// continues from it.
    pub fn seal(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        let sequence = self.sequence;
        let Some(root) = &mut self.root else {
            return Ok(true);
        };
        if matches!(root.link, Link::Inline { .. }) {
            make_dirty(root, access, sequence)?;
        }
        if !matches!(root.link, Link::Dirty(_)) {
            return Ok(true);
        }
        if !prepare(root, access, work)? {
            return Ok(false);
        }
        let height = height(root, access, sequence)?;
        let family = root.family;
        let mut builder = Builder {
            access,
            work,
            sequence,
            sealing: true,
            leaf: Open::default(),
            tokens: TokenWriter::new(),
            index: Vec::new(),
        };
        builder.feed(root, height)?;
        let (built, sealed) = builder.finish()?;
        self.root = built.map(|mut root| {
            root.family = family;
            root
        });
        Ok(sealed)
    }
}
