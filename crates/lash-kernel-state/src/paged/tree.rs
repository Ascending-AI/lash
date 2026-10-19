//! Bounded, transient B+ trees. A dirty link owns one mutable page; sealed
//! links retain their summaries and are not visited by sealing.
use super::source::{INDEX_FANOUT, KEYED_LEAF_ROWS, PAGE_TARGET_BYTES};
use super::{PageAnswer, PageBody, PageKind, PageSource, SealBudget};
use lash_kernel_doc::graph_tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};
use lash_kernel_doc::heap::{FamilyId, HeapFault, LogicalBytes, PageId, PageRef};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;

mod canonical;
#[cfg(test)]
pub(crate) use canonical::boundary_hash;
mod decoded;
mod diff;
pub use diff::Change;
mod inline;
mod residency;
mod retired;
pub use retired::Retired;
pub const INLINE_TREE_BYTES: usize = super::source::INLINE_BODY_MAX as usize;

/// Cells maintain their child links and accounting before publication.
pub trait Cell: TokenCodec + Clone + 'static {
    fn cost(&self) -> u64;
    /// Whether two record versions can be retained as one. Mutable nested
    /// trees override this: their packing placeholders are not identities.
    fn same_version(&self, other: &Self) -> bool {
        encode_body(self) == encode_body(other)
    }
    fn task_edges(&self) -> u64 {
        0
    }
    fn live_members(&self) -> u64 {
        0
    }
    fn decode_cell(
        input: &mut TokenReader<'_>,
        _family: Option<FamilyId>,
    ) -> Result<Self, TokenError> {
        Self::decode(input)
    }
    fn prepare(&mut self, _access: &Access<'_>, _budget: &mut SealWork) -> Result<bool, HeapFault> {
        Ok(true)
    }
    fn grants(&mut self, _children: &[PageRef]) -> Result<(), HeapFault> {
        Ok(())
    }
    fn children(&self, _out: &mut Vec<PageRef>) {}
}

/// Single-threaded page residency, bounded independently of guest cost.
#[derive(Clone)]
pub struct Access<'a> {
    source: Rc<RefCell<SourcePort<'a>>>,
    cache: Rc<RefCell<BTreeMap<PageId, PageAnswer>>>,
    bytes: Rc<RefCell<usize>>,
    dirty: Rc<RefCell<residency::DirtyResidency>>,
    decoded: Rc<RefCell<decoded::DecodedResidency>>,
    sizing: Rc<RefCell<TokenWriter>>,
}
impl<'a> Access<'a> {
    pub fn new(source: Box<dyn PageSource>) -> Self {
        Self {
            source: Rc::new(RefCell::new(SourcePort::Owned(source))),
            cache: Rc::new(RefCell::new(BTreeMap::new())),
            bytes: Rc::new(RefCell::new(0)),
            dirty: Default::default(),
            decoded: Default::default(),
            sizing: Default::default(),
        }
    }
    fn borrowed(source: &'a mut dyn PageSource) -> Self {
        Self {
            source: Rc::new(RefCell::new(SourcePort::Borrowed(source))),
            cache: Rc::new(RefCell::new(BTreeMap::new())),
            bytes: Rc::new(RefCell::new(0)),
            dirty: Default::default(),
            decoded: Default::default(),
            sizing: Default::default(),
        }
    }
    /// Packing and residency need the encoded length, not an owned body.
    /// Reuse one bounded buffer after each non-reentrant codec traversal.
    fn encoded_len<T: TokenCodec>(&self, value: &T) -> usize {
        let mut out = self.sizing.borrow_mut();
        out.clear();
        value.encode(&mut out);
        let len = out.len();
        if len > 65_536 {
            *out = TokenWriter::new();
        }
        len
    }
    pub fn read(&self, page: PageRef) -> Result<PageAnswer, HeapFault> {
        if let Some(answer) = self.cache.borrow().get(&page.id) {
            return Ok(answer.clone());
        }
        let mut answers = self.source.borrow_mut().read(&[page])?;
        if answers.len() != 1 {
            return Err(HeapFault::Malformed(
                "page port answered the wrong count".into(),
            ));
        }
        let answer = answers.remove(0)?;
        if answer.page.id != page.id {
            return Err(HeapFault::Corrupt {
                page: page.id,
                reason: "page port answered another page".into(),
            });
        }
        self.keep(&answer);
        Ok(answer)
    }
    fn keep(&self, answer: &PageAnswer) {
        let weight = answer.tokens.len()
            + answer.children.capacity() * std::mem::size_of::<PageRef>()
            + std::mem::size_of::<PageAnswer>()
            + 64;
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        *self.bytes.borrow_mut() += weight;
        self.cache
            .borrow_mut()
            .insert(answer.page.id, answer.clone());
    }
    /// Only explicitly demanded siblings enter this bounded read. Results
    /// stay in request order, including faults; decoding consumes them in
    /// traversal order rather than letting a later I/O fault skip a prefix.
    pub(super) fn read_batch(&self, pages: &[PageRef]) -> Vec<Result<PageAnswer, HeapFault>> {
        if pages.len() > super::PagePortLimits::STANDARD.pages {
            return vec![
                Err(HeapFault::Exhausted {
                    resource: lash_kernel_doc::heap::HostResource::ReadBatch,
                });
                pages.len()
            ];
        }
        let cached: Vec<_> = pages
            .iter()
            .map(|page| self.cache.borrow().get(&page.id).cloned())
            .collect();
        let mut missing: Vec<PageRef> = Vec::new();
        for (page, held) in pages.iter().zip(&cached) {
            if held.is_none() && !missing.iter().any(|wanted| wanted.id == page.id) {
                missing.push(*page);
            }
        }
        let mut fresh = BTreeMap::new();
        if !missing.is_empty() {
            let fetched = match self.source.borrow_mut().read(&missing) {
                Ok(fetched) if fetched.len() == missing.len() => fetched,
                Ok(_) => vec![
                    Err(HeapFault::Malformed(
                        "page port answered the wrong count".into()
                    ));
                    missing.len()
                ],
                Err(fault) => vec![Err(fault); missing.len()],
            };
            for (page, answer) in missing.into_iter().zip(fetched) {
                let held = answer.and_then(|held| {
                    if held.page.id != page.id {
                        return Err(HeapFault::Corrupt {
                            page: page.id,
                            reason: "page port answered another page".into(),
                        });
                    }
                    self.keep(&held);
                    Ok(held)
                });
                fresh.insert(page.id, held);
            }
        }
        pages
            .iter()
            .zip(cached)
            .map(|(page, cached)| {
                cached.map(Ok).unwrap_or_else(|| {
                    fresh.get(&page.id).cloned().unwrap_or_else(|| {
                        Err(HeapFault::Malformed(
                            "page port answered the wrong count".into(),
                        ))
                    })
                })
            })
            .collect()
    }
    pub fn seal(
        &self,
        kind: PageKind,
        tokens: Vec<u8>,
        mut children: Vec<PageRef>,
    ) -> Result<PageRef, HeapFault> {
        children.sort_by_key(|p| p.id);
        children.dedup_by_key(|p| p.id);
        // Logical references have 33 bytes while physical references use slots.
        // This conservative check also bounds the eventual envelope.
        if tokens.len() + children.len() * 32 + 32 > 65_536 {
            return Err(HeapFault::Malformed(
                "semantic page exceeds its node bound".into(),
            ));
        }
        self.source.borrow_mut().seal(PageBody {
            kind,
            tokens,
            children,
        })
    }
    pub fn with_source<T>(
        &self,
        f: impl FnOnce(&mut dyn PageSource) -> Result<T, HeapFault>,
    ) -> Result<T, HeapFault> {
        f(&mut *self.source.borrow_mut())
    }
    pub fn clear(&self) {
        self.cache.borrow_mut().clear();
        *self.bytes.borrow_mut() = 0;
        *self.decoded.borrow_mut() = Default::default();
    }
}

pub use super::source::grant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Summary {
    len: u64,
    cost: u64,
    tasks: u64,
    members: u64,
}
impl TokenCodec for Summary {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        out.unsigned(self.len);
        out.unsigned(self.cost);
        out.unsigned(self.tasks);
        out.unsigned(self.members);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            len: input.unsigned()?,
            cost: input.unsigned()?,
            tasks: input.unsigned()?,
            members: input.unsigned()?,
        })
    }
}
#[derive(Clone)]
enum Link<T: Cell> {
    Sealed(PageRef),
    Inline {
        tokens: Arc<[u8]>,
        children: Vec<PageRef>,
    },
    Dirty(Rc<residency::DirtyNode<T>>),
}
#[derive(Clone)]
struct Branch<T: Cell> {
    family: Option<FamilyId>,
    encoded_bytes: Option<u64>,
    is_leaf: Option<bool>,
    first: Key,
    summary: Summary,
    tasks_known: bool,
    members_known: bool,
    link: Link<T>,
}
enum Node<T: Cell> {
    Leaf(Vec<(Key, T)>),
    Index(Vec<Branch<T>>),
    Staged(PageRef, u64, bool),
}

impl<T: Cell> Clone for Node<T> {
    #[inline]
    fn clone(&self) -> Self {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| match self {
            Self::Leaf(rows) => {
                work.tree_leaf_clones += 1;
                work.tree_rows_cloned += rows.len() as u64;
            }
            Self::Index(rows) => {
                work.tree_index_clones += 1;
                work.tree_rows_cloned += rows.len() as u64;
            }
            Self::Staged(..) => {}
        });
        match self {
            Self::Leaf(rows) => Self::Leaf(rows.clone()),
            Self::Index(rows) => Self::Index(rows.clone()),
            Self::Staged(page, bytes, leaf) => Self::Staged(*page, *bytes, *leaf),
        }
    }
}

/// Ordered keys (directory/lookup) or ordinal keys (sequences). Sequence
/// rank uses subtree counts, so removal never renumbers later cells.
#[derive(Clone)]
pub struct Tree<T: Cell> {
    root: Option<Branch<T>>,
    sequence: bool,
}
/// A comparison with a published tree, without owning its mutable pages.
/// Dirty-page revisions distinguish same-size in-place edits; a weak link
/// prevents allocation reuse from making a retired version match again.
pub struct Version<T: Cell> {
    sequence: bool,
    root: Option<(Option<FamilyId>, Summary, VersionLink<T>)>,
}
enum VersionLink<T: Cell> {
    Sealed(PageId),
    Inline(Arc<[u8]>),
    Dirty(Weak<residency::DirtyNode<T>>, u64),
}
impl<T: Cell> Version<T> {
    pub fn matches(&self, tree: &Tree<T>) -> bool {
        if self.sequence != tree.sequence {
            return false;
        }
        match (&self.root, &tree.root) {
            (None, None) => true,
            (Some((family, summary, link)), Some(root))
                if *family == root.family && *summary == root.summary =>
            {
                match (link, &root.link) {
                    (VersionLink::Sealed(a), Link::Sealed(b)) => *a == b.id,
                    (VersionLink::Inline(a), Link::Inline { tokens: b, .. }) => a == b,
                    (VersionLink::Dirty(a, revision), Link::Dirty(b)) => {
                        std::ptr::eq(a.as_ptr(), Rc::as_ptr(b)) && *revision == b.revision.get()
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
}
impl<T: Cell> Default for Tree<T> {
    fn default() -> Self {
        Self::new(false)
    }
}
impl<T: Cell> Tree<T> {
    /// Captures only the identity and revision needed for a later comparison.
    /// Unlike cloning the tree, this never makes an edit copy its old rows.
    pub fn version(&self) -> Version<T> {
        Version {
            sequence: self.sequence,
            root: self.root.as_ref().map(|root| {
                let link = match &root.link {
                    Link::Sealed(page) => VersionLink::Sealed(page.id),
                    Link::Inline { tokens, .. } => VersionLink::Inline(Arc::clone(tokens)),
                    Link::Dirty(page) => {
                        VersionLink::Dirty(Rc::downgrade(page), page.revision.get())
                    }
                };
                (root.family, root.summary, link)
            }),
        }
    }
    pub fn same_version(&self, other: &Self) -> bool {
        if self.sequence != other.sequence {
            return false;
        }
        match (&self.root, &other.root) {
            (None, None) => true,
            (Some(a), Some(b)) if a.family == b.family && a.summary == b.summary => {
                match (&a.link, &b.link) {
                    (Link::Sealed(a), Link::Sealed(b)) => a.id == b.id,
                    (Link::Inline { tokens: a, .. }, Link::Inline { tokens: b, .. }) => a == b,
                    (Link::Dirty(a), Link::Dirty(b)) => Rc::ptr_eq(a, b),
                    _ => false,
                }
            }
            _ => false,
        }
    }
    /// Reads through a synchronous borrowed port with the same validation and
    /// bounded residency used by an activation's owned port.
    pub fn get_from_source(
        &self,
        key: &[u8],
        source: &mut dyn PageSource,
    ) -> Result<Option<T>, HeapFault> {
        self.get(key, &Access::borrowed(source))
    }

    pub fn new(sequence: bool) -> Self {
        Self {
            root: None,
            sequence,
        }
    }
    pub fn open(page: Option<PageRef>, len: u64, cost: LogicalBytes, sequence: bool) -> Self {
        Self {
            root: page.map(|p| Branch {
                family: None,
                encoded_bytes: None,
                is_leaf: None,
                tasks_known: false,
                members_known: false,
                first: Key(Vec::new()),
                summary: Summary {
                    len,
                    cost: cost.0,
                    tasks: 0,
                    members: 0,
                },
                link: Link::Sealed(p),
            }),
            sequence,
        }
    }
    /// The tree an external keyed root describes.
    #[must_use]
    pub fn of_keyed(root: &super::KeyedRoot) -> Self {
        Self::open(root.page(), root.len(), root.cost(), false)
    }
    /// The tree an external sequence root describes.
    #[must_use]
    pub fn of_sequence(root: &super::SequenceRoot) -> Self {
        Self::open(root.page(), root.len(), root.cost(), true)
    }
    /// The sealed tree's keyed root.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Malformed`] while rows are unsealed.
    pub fn keyed_root(&self) -> Result<super::KeyedRoot, HeapFault> {
        if self.is_empty() {
            return Ok(super::KeyedRoot::Empty);
        }
        super::KeyedRoot::of(self.page(), self.len(), LogicalBytes(self.cost()))
    }
    /// The sealed tree's sequence root.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Malformed`] while rows are unsealed.
    pub fn sequence_root(&self) -> Result<super::SequenceRoot, HeapFault> {
        if self.is_empty() {
            return Ok(super::SequenceRoot::Empty);
        }
        super::SequenceRoot::of(self.page(), self.len(), LogicalBytes(self.cost()))
    }
    pub fn in_family(mut self, family: FamilyId) -> Self {
        if let Some(root) = &mut self.root {
            root.family = Some(family);
        }
        self
    }
    pub fn len(&self) -> u64 {
        self.root.as_ref().map_or(0, |r| r.summary.len)
    }
    pub fn is_empty(&self) -> bool {
        self.root.as_ref().is_none_or(|root| root.summary.len == 0)
    }
    pub fn cost(&self) -> u64 {
        self.root.as_ref().map_or(0, |r| r.summary.cost)
    }
    pub(crate) fn cached_task_edges(&self) -> Option<u64> {
        self.root.as_ref().map_or(Some(0), |root| {
            root.tasks_known.then_some(root.summary.tasks)
        })
    }
    pub fn task_edges(&self) -> u64 {
        self.root.as_ref().map_or(0, |r| r.summary.tasks)
    }
    pub fn read_task_edges(&self, access: &Access<'_>) -> Result<u64, HeapFault> {
        let Some(root) = &self.root else {
            return Ok(0);
        };
        if root.tasks_known {
            return Ok(root.summary.tasks);
        }
        match &*Self::load_node(root, access, self.sequence)?.node() {
            Node::Leaf(rows) => checked_sum(rows.iter().map(|(_, c)| c.task_edges())),
            Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.tasks)),
            Node::Staged(..) => unreachable!("loaded staged node"),
        }
    }
    pub fn with_task_edges(mut self, tasks: u64) -> Self {
        if let Some(r) = &mut self.root {
            r.summary.tasks = tasks;
            r.tasks_known = true;
        }
        self
    }
    /// The membership aggregate lives in the tree. Bare keyed roots recover
    /// it from one bounded root page, never by walking the run leaves.
    pub fn live_members(&self, access: &Access<'_>) -> Result<u64, HeapFault> {
        let Some(root) = &self.root else {
            return Ok(0);
        };
        if root.members_known {
            return Ok(root.summary.members);
        }
        match &*Self::load_node(root, access, self.sequence)?.node() {
            Node::Leaf(rows) => checked_sum(rows.iter().map(|(_, c)| c.live_members())),
            Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.members)),
            Node::Staged(..) => unreachable!("loaded staged node"),
        }
    }
    pub fn page(&self) -> Option<PageRef> {
        self.root.as_ref().and_then(|r| match r.link {
            Link::Sealed(p) => Some(p),
            Link::Dirty(_) | Link::Inline { .. } => None,
        })
    }
    pub fn sealed(&self) -> bool {
        self.root
            .as_ref()
            .is_none_or(|r| !matches!(r.link, Link::Dirty(_)))
    }
    /// The node `branch` names, owned: a copy of the decoded node a read
    /// shares.
    fn load(branch: &Branch<T>, access: &Access<'_>, sequence: bool) -> Result<Node<T>, HeapFault> {
        Self::load_shared(branch, access, sequence).map(Rc::unwrap_or_clone)
    }

    /// The node `branch` names for a read: a working node borrowed in
    /// place, never copied row by row (FIG-6318).
    fn load_node(
        branch: &Branch<T>,
        access: &Access<'_>,
        sequence: bool,
    ) -> Result<Loaded<T>, HeapFault> {
        match &branch.link {
            Link::Dirty(n) if !matches!(&*n.borrow(), Node::Staged(..)) => {
                Ok(Loaded::Dirty(Rc::clone(n)))
            }
            _ => Self::load_shared(branch, access, sequence).map(Loaded::Shared),
        }
    }

    /// The node `branch` names, as the access's decoded nodes hold it: a
    /// read takes it without copying its rows (FIG-6321).
    fn load_shared(
        branch: &Branch<T>,
        access: &Access<'_>,
        sequence: bool,
    ) -> Result<Rc<Node<T>>, HeapFault> {
        match &branch.link {
            Link::Dirty(n) => {
                let node = n.borrow().clone();
                if let Node::Staged(page, bytes, leaf) = node {
                    let mut staged = branch.clone();
                    staged.link = Link::Sealed(page);
                    staged.encoded_bytes = Some(bytes);
                    staged.is_leaf = Some(leaf);
                    Self::load_shared(&staged, access, sequence)
                } else {
                    Ok(Rc::new(node))
                }
            }
            Link::Inline { tokens, children } => {
                access.inline_node(branch, tokens, children, sequence)
            }
            Link::Sealed(p) => {
                if let Some(node) = access.decoded_node::<T>(*p, branch, sequence)? {
                    return Ok(node);
                }
                let answer = access.read(*p)?;
                if branch
                    .encoded_bytes
                    .is_some_and(|n| n != answer.tokens.len() as u64)
                {
                    return Err(corrupt(*p, "tree root byte count mismatch"));
                }
                let leaf = if sequence {
                    PageKind::SEQUENCE_LEAF
                } else {
                    PageKind::KEYED_LEAF
                };
                let index = if sequence {
                    PageKind::SEQUENCE_INDEX
                } else {
                    PageKind::KEYED_INDEX
                };
                if branch
                    .is_leaf
                    .is_some_and(|hint| hint != (answer.kind == leaf))
                {
                    return Err(corrupt(*p, "tree root kind mismatch"));
                }
                let mut node = if answer.kind == leaf {
                    {
                        let mut input = TokenReader::new(&answer.tokens);
                        let count = input.seq().map_err(|e| corrupt(*p, e))?;
                        if count > canonical::leaf_cap(sequence) as u64 {
                            return Err(corrupt(*p, "too many leaf rows"));
                        }
                        let mut rows = Vec::with_capacity(count as usize);
                        for _ in 0..count {
                            input.expect_struct(2).map_err(|e| corrupt(*p, e))?;
                            let key = Key::decode(&mut input).map_err(|e| corrupt(*p, e))?;
                            let cell = T::decode_cell(&mut input, branch.family)
                                .map_err(|e| corrupt(*p, e))?;
                            rows.push((key, cell));
                        }
                        input.finish().map_err(|e| corrupt(*p, e))?;
                        Node::Leaf(rows)
                    }
                } else if answer.kind == index {
                    let rows = decode_body::<Vec<(Key, (Summary, (Option<FamilyId>, PageRef)))>>(
                        &answer.tokens,
                    )
                    .map_err(|e| corrupt(*p, e))?;
                    if rows.is_empty() || rows.len() > canonical::index_cap(sequence) {
                        return Err(corrupt(*p, "invalid index fanout"));
                    }
                    Node::Index(
                        rows.into_iter()
                            .map(|(first, (summary, (family, page)))| Branch {
                                encoded_bytes: None,
                                is_leaf: None,
                                tasks_known: true,
                                members_known: true,
                                family: family.or(branch.family),
                                first,
                                summary,
                                link: Link::Sealed(page),
                            })
                            .collect(),
                    )
                } else {
                    return Err(corrupt(*p, "wrong tree page kind"));
                };
                let (len, cost) = match &mut node {
                    Node::Staged(..) => unreachable!("loaded staged node"),
                    Node::Leaf(rows) => {
                        if rows.is_empty() || rows.len() > canonical::leaf_cap(sequence) {
                            return Err(corrupt(*p, "invalid leaf size"));
                        }
                        check_keys(rows.iter().map(|(key, _)| key.as_slice()))
                            .map_err(|e| corrupt(*p, e))?;
                        for (_, cell) in rows.iter_mut() {
                            cell.grants(&answer.children)?;
                        }
                        (
                            rows.len() as u64,
                            checked_sum(rows.iter().map(|(_, cell)| cell.cost()))?,
                        )
                    }
                    Node::Index(rows) => {
                        check_keys(rows.iter().map(|r| r.first.as_slice()))
                            .map_err(|e| corrupt(*p, e))?;
                        for row in rows.iter_mut() {
                            if let Link::Sealed(p) = &mut row.link {
                                grant(p, &answer.children)?;
                            }
                        }
                        (
                            checked_sum(rows.iter().map(|r| r.summary.len))?,
                            checked_sum(rows.iter().map(|r| r.summary.cost))?,
                        )
                    }
                };
                let tasks = match &node {
                    Node::Staged(..) => unreachable!("loaded staged node"),
                    Node::Leaf(rows) => {
                        checked_sum(rows.iter().map(|(_, cell)| cell.task_edges()))?
                    }
                    Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.tasks))?,
                };
                let members = match &node {
                    Node::Staged(..) => unreachable!("loaded staged node"),
                    Node::Leaf(rows) => {
                        checked_sum(rows.iter().map(|(_, cell)| cell.live_members()))?
                    }
                    Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.members))?,
                };
                if branch.members_known && members != branch.summary.members {
                    return Err(corrupt(*p, "membership aggregate mismatch"));
                }
                if branch.tasks_known && tasks != branch.summary.tasks {
                    return Err(corrupt(*p, "task-edge aggregate mismatch"));
                }
                if len != branch.summary.len || cost != branch.summary.cost {
                    return Err(corrupt(*p, "tree aggregate mismatch"));
                }
                let node = Rc::new(node);
                access.retain_decoded(
                    *p,
                    branch.family,
                    sequence,
                    &node,
                    Summary {
                        len,
                        cost,
                        tasks,
                        members,
                    },
                    answer.tokens.len(),
                );
                Ok(node)
            }
        }
    }
    pub fn get(&self, key: &[u8], access: &Access<'_>) -> Result<Option<T>, HeapFault> {
        self.get_with(key, access, |cell| cell.cloned())
    }
    /// Inspect the cell while its page is borrowed. A scalar read does not
    /// copy a nested tree descriptor or make its working pages shared.
    pub(crate) fn get_with<R>(
        &self,
        key: &[u8],
        access: &Access<'_>,
        read: impl FnOnce(Option<&T>) -> R,
    ) -> Result<R, HeapFault> {
        #[cfg(test)]
        note_lookup::<T>();
        fn walk<T: Cell, R>(
            branch: &Branch<T>,
            key: &[u8],
            access: &Access<'_>,
            sequence: bool,
            read: impl FnOnce(Option<&T>) -> R,
        ) -> Result<R, HeapFault> {
            match &*Tree::load_node(branch, access, sequence)?.node() {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => Ok(read(
                    rows.binary_search_by(|(k, _)| k.as_slice().cmp(key))
                        .ok()
                        .map(|i| &rows[i].1),
                )),
                Node::Index(rows) => walk(
                    &rows[rows
                        .partition_point(|r| r.first.as_slice() <= key)
                        .saturating_sub(1)],
                    key,
                    access,
                    sequence,
                    read,
                ),
            }
        }
        match self.root.as_ref() {
            Some(branch) => walk(branch, key, access, self.sequence, read),
            None => Ok(read(None)),
        }
    }
    pub fn rank_get(
        &self,
        rank: u64,
        access: &Access<'_>,
    ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
        #[cfg(test)]
        note_lookup::<T>();
        fn walk<T: Cell>(
            branch: &Branch<T>,
            mut rank: u64,
            access: &Access<'_>,
            sequence: bool,
        ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
            match &*Tree::load_node(branch, access, sequence)?.node() {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => Ok(rows
                    .get(rank as usize)
                    .map(|(k, v)| (k.0.clone(), v.clone()))),
                Node::Index(rows) => {
                    for row in rows {
                        if rank < row.summary.len {
                            return walk(row, rank, access, sequence);
                        }
                        rank -= row.summary.len;
                    }
                    Err(HeapFault::Malformed(
                        "tree rank summary is inconsistent".into(),
                    ))
                }
            }
        }
        if rank >= self.len() {
            return Ok(None);
        }
        self.root
            .as_ref()
            .map_or(Ok(None), |branch| walk(branch, rank, access, self.sequence))
    }
    pub fn after(
        &self,
        key: Option<&[u8]>,
        access: &Access<'_>,
    ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
        #[cfg(test)]
        note_lookup::<T>();
        fn walk<T: Cell>(
            branch: &Branch<T>,
            key: Option<&[u8]>,
            access: &Access<'_>,
            sequence: bool,
        ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
            match &*Tree::load_node(branch, access, sequence)?.node() {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => Ok(rows
                    .iter()
                    .find(|(k, _)| key.is_none_or(|a| k.as_slice() > a))
                    .map(|(k, v)| (k.0.clone(), v.clone()))),
                Node::Index(rows) => {
                    let start = key.map_or(0, |k| {
                        rows.partition_point(|r| r.first.as_slice() <= k)
                            .saturating_sub(1)
                    });
                    for row in &rows[start..] {
                        if let Some(v) = walk(row, key, access, sequence)? {
                            return Ok(Some(v));
                        }
                    }
                    Ok(None)
                }
            }
        }
        self.root
            .as_ref()
            .map_or(Ok(None), |r| walk(r, key, access, self.sequence))
    }
    pub fn passed(&self, key: &[u8], access: &Access<'_>) -> Result<u64, HeapFault> {
        #[cfg(test)]
        note_lookup::<T>();
        fn walk<T: Cell>(
            branch: &Branch<T>,
            key: &[u8],
            access: &Access<'_>,
            seq: bool,
        ) -> Result<u64, HeapFault> {
            match &*Tree::load_node(branch, access, seq)?.node() {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => Ok(rows.partition_point(|(k, _)| k.as_slice() <= key) as u64),
                Node::Index(rows) => {
                    let chosen = rows
                        .partition_point(|r| r.first.as_slice() <= key)
                        .saturating_sub(1);
                    let left = checked_sum(rows[..chosen].iter().map(|r| r.summary.len))?;
                    Ok(left + walk(&rows[chosen], key, access, seq)?)
                }
            }
        }
        self.root
            .as_ref()
            .map_or(Ok(0), |r| walk(r, key, access, self.sequence))
    }
    pub fn write(&mut self, key: Vec<u8>, value: T, access: &Access<'_>) -> Result<(), HeapFault> {
        self.change(Cow::Owned(key), access, false, |rows, position, key| {
            match position {
                Ok(i) => rows[i].1 = value,
                Err(i) => rows.insert(i, (Key(key.into_owned()), value)),
            }
            Ok(true)
        })
        .map(|_| ())
    }
    /// A fresh tree holding `rows`, as writing each into an empty tree in
    /// turn would: a later row replaces an earlier one with the same key.
    /// Its leaves are cut by the canonical rule from the first row, and each
    /// is tracked and may stage as it is made, so building it searches and
    /// copies no path per row.
    pub fn from_rows(
        mut rows: Vec<(Vec<u8>, T)>,
        sequence: bool,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        if !rows.is_sorted_by(|a, b| a.0 < b.0) {
            // Stable, so the last of equal keys is the one a write keeps.
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            let mut kept: Vec<(Vec<u8>, T)> = Vec::with_capacity(rows.len());
            for row in rows {
                match kept.last_mut() {
                    Some(last) if last.0 == row.0 => *last = row,
                    _ => kept.push(row),
                }
            }
            rows = kept;
        }
        let mut first: Vec<(Key, T)> = rows
            .into_iter()
            .map(|(key, value)| (Key(key), value))
            .collect();
        if first.is_empty() {
            return Ok(Self::new(sequence));
        }
        let rest = canonical::cut_leaf(&mut first, sequence, access);
        let mut leaves = Vec::with_capacity(rest.len() + 1);
        for rows in std::iter::once(first).chain(rest) {
            leaves.push(dirty(Node::Leaf(rows), access, sequence)?);
            access.stage_pressure()?;
        }
        let root = grow(leaves, access, sequence)?;
        access.stage_pressure()?;
        Ok(Self {
            root: Some(root),
            sequence,
        })
    }
    /// Edits an existing cell without retaining a cloned copy of its nested
    /// trees. `false` means the callback restored the identical cell version.
    /// Ancestor pages remain charged and protected while nested edits stage.
    pub fn update(
        &mut self,
        key: Vec<u8>,
        access: &Access<'_>,
        update: impl FnOnce(&mut T) -> Result<bool, HeapFault>,
    ) -> Result<bool, HeapFault> {
        self.change(Cow::Owned(key), access, true, |rows, position, _| {
            let i =
                position.map_err(|_| HeapFault::Malformed("updated tree row is missing".into()))?;
            update(&mut rows[i].1)
        })
    }
    /// Look up and replace a row in one traversal. Returning `None` leaves
    /// its values unchanged; only inserting a row copies the borrowed key.
    pub fn write_with<E: From<HeapFault>>(
        &mut self,
        key: &[u8],
        access: &Access<'_>,
        write: impl FnOnce(Option<&T>) -> Result<Option<T>, E>,
    ) -> Result<bool, E> {
        self.change(Cow::Borrowed(key), access, false, |rows, position, key| {
            let Some(value) = write(position.ok().map(|i| &rows[i].1))? else {
                return Ok(false);
            };
            match position {
                Ok(i) => rows[i].1 = value,
                Err(i) => rows.insert(i, (Key(key.into_owned()), value)),
            }
            Ok(true)
        })
    }
    fn change<E: From<HeapFault>, F>(
        &mut self,
        key: Cow<'_, [u8]>,
        access: &Access<'_>,
        preserve: bool,
        patch: F,
    ) -> Result<bool, E>
    where
        F: FnOnce(&mut Vec<(Key, T)>, Result<usize, usize>, Cow<'_, [u8]>) -> Result<bool, E>,
    {
        #[cfg(test)]
        note_lookup::<T>();
        fn edit<T: Cell, E: From<HeapFault>, F>(
            branch: &mut Branch<T>,
            key: Cow<'_, [u8]>,
            access: &Access<'_>,
            seq: bool,
            patch: &mut Option<F>,
            preserve: bool,
        ) -> Result<(bool, Vec<Branch<T>>), E>
        where
            F: FnOnce(&mut Vec<(Key, T)>, Result<usize, usize>, Cow<'_, [u8]>) -> Result<bool, E>,
        {
            let previous = (preserve && !matches!(&branch.link, Link::Dirty(page) if Rc::strong_count(page) == 1 && !matches!(&*page.borrow(), Node::Staged(..)))).then(|| branch.clone());
            let page = make_dirty(branch, access, seq)?;
            let revision = page.revision.get() - 1;
            let mut node = page.borrow_mut();
            match &mut *node {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => {
                    let position = rows.binary_search_by(|(k, _)| k.as_slice().cmp(key.as_ref()));
                    let (at, before, removed) = match position {
                        Ok(i) => (
                            i,
                            Some(decoded::RowSize::of(
                                &rows[i].0,
                                access.encoded_len(&rows[i].1),
                            )),
                            Some(Aggregate::of(&rows[i].1)),
                        ),
                        Err(i) => (i, None, None),
                    };
                    let changed = patch
                        .take()
                        .ok_or_else(|| HeapFault::Malformed("tree edit used twice".into()))?(
                        rows, position, key,
                    )?;
                    if !changed {
                        drop(node);
                        if let Some(previous) = previous {
                            *branch = previous;
                        } else {
                            page.revision.set(revision);
                        }
                        return Ok((false, Vec::new()));
                    }
                    let encoded = access.encoded_len(&rows[at].1);
                    let added = Aggregate::of(&rows[at].1);
                    let after = decoded::RowSize::of(&rows[at].0, encoded);
                    let bytes = access.track_row(&page, rows, seq, before, Some(after));
                    // A working leaf past its cap is cut where the canonical
                    // rule would cut it from its own start; the seal places
                    // every boundary canonically.
                    if rows.len() > canonical::leaf_cap(seq) || bytes > PAGE_TARGET_BYTES as usize {
                        let right = canonical::cut_leaf(rows, seq, access);
                        if !right.is_empty() {
                            refresh(branch, &node)?;
                            access.track(&page, &node, seq);
                            let right = right
                                .into_iter()
                                .map(|rows| dirty(Node::Leaf(rows), access, seq))
                                .collect::<Result<_, _>>()?;
                            return Ok((true, right));
                        }
                    }
                    // One row changed: adjust the leaf's aggregates by it
                    // instead of summing every row again (FIG-6465).
                    if branch.tasks_known && branch.members_known {
                        branch.encoded_bytes = None;
                        branch.is_leaf = None;
                        branch.summary = added.replace(branch.summary, removed)?;
                        if at == 0 && branch.first != rows[0].0 {
                            branch.first = rows[0].0.clone();
                        }
                    } else {
                        refresh(branch, &node)?;
                    }
                    return Ok((true, Vec::new()));
                }
                Node::Index(rows) => {
                    let i = rows
                        .partition_point(|r| r.first.as_slice() <= key.as_ref())
                        .saturating_sub(1);
                    let before = rows[i].summary;
                    let (changed, right) = edit(&mut rows[i], key, access, seq, patch, preserve)?;
                    if !changed {
                        drop(node);
                        if let Some(previous) = previous {
                            *branch = previous;
                        } else {
                            page.revision.set(revision);
                        }
                        return Ok((false, Vec::new()));
                    }
                    if !right.is_empty() {
                        rows.splice(i + 1..i + 1, right);
                    } else if branch.tasks_known && branch.members_known {
                        // One child changed: adjust by its delta (FIG-6465).
                        branch.encoded_bytes = None;
                        branch.is_leaf = None;
                        branch.summary = Aggregate::delta(branch.summary, before, rows[i].summary)?;
                        if i == 0 && branch.first != rows[0].first {
                            branch.first = rows[0].first.clone();
                        }
                        access.track(&page, &node, seq);
                        return Ok((true, Vec::new()));
                    }
                    if rows.len() > canonical::index_cap(seq) {
                        let right = canonical::cut_index(rows, seq);
                        refresh(branch, &node)?;
                        access.track(&page, &node, seq);
                        let right = right
                            .into_iter()
                            .map(|rows| dirty(Node::Index(rows), access, seq))
                            .collect::<Result<_, _>>()?;
                        return Ok((true, right));
                    }
                }
            }
            refresh(branch, &node)?;
            access.track(&page, &node, seq);
            Ok((true, Vec::new()))
        }
        let mut patch = Some(patch);
        let changed = match &mut self.root {
            None => {
                let mut rows = Vec::new();
                let changed = patch
                    .take()
                    .ok_or_else(|| HeapFault::Malformed("tree edit used twice".into()))?(
                    &mut rows,
                    Err(0),
                    key,
                )?;
                if changed {
                    self.root = Some(dirty(Node::Leaf(rows), access, self.sequence)?);
                }
                changed
            }
            Some(root) => {
                let (changed, right) =
                    edit(root, key, access, self.sequence, &mut patch, preserve)?;
                if !right.is_empty() {
                    let mut level = vec![root.clone()];
                    level.extend(right);
                    self.root = Some(grow(level, access, self.sequence)?);
                }
                changed
            }
        };
        access.stage_pressure()?;
        Ok(changed)
    }
    pub fn remove(&mut self, key: &[u8], access: &Access<'_>) -> Result<Option<T>, HeapFault> {
        #[cfg(test)]
        note_lookup::<T>();
        // Find the row without taking ownership of any search-path node.
        // In particular, an absent key must not dirty or measure a leaf.
        fn locate<T: Cell>(
            branch: &Branch<T>,
            key: &[u8],
            access: &Access<'_>,
            seq: bool,
            path: &mut Vec<usize>,
        ) -> Result<Option<usize>, HeapFault> {
            match &*Tree::load_node(branch, access, seq)?.node() {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => Ok(rows.binary_search_by(|(k, _)| k.as_slice().cmp(key)).ok()),
                Node::Index(rows) => {
                    let i = rows
                        .partition_point(|r| r.first.as_slice() <= key)
                        .saturating_sub(1);
                    path.push(i);
                    locate(&rows[i], key, access, seq, path)
                }
            }
        }
        fn edit<T: Cell>(
            branch: &mut Branch<T>,
            path: &[usize],
            at: usize,
            access: &Access<'_>,
            seq: bool,
        ) -> Result<T, HeapFault> {
            let page = make_dirty(branch, access, seq)?;
            let mut node = page.borrow_mut();
            let removed = match &mut *node {
                Node::Staged(..) => unreachable!("mutable staged node"),
                Node::Leaf(rows) => {
                    let (key, value) = rows.remove(at);
                    let before = decoded::RowSize::of(&key, access.encoded_len(&value));
                    access.track_row(&page, rows, seq, Some(before), None);
                    value
                }
                Node::Index(rows) => {
                    let i = path[0];
                    let removed = edit(&mut rows[i], &path[1..], at, access, seq)?;
                    if rows[i].summary.len == 0 {
                        rows.remove(i);
                    }
                    removed
                }
            };
            refresh(branch, &node)?;
            if matches!(&*node, Node::Index(_)) {
                access.track(&page, &node, seq);
            }
            Ok(removed)
        }
        let Some(root) = &mut self.root else {
            return Ok(None);
        };
        let mut path = Vec::new();
        let Some(at) = locate(root, key, access, self.sequence, &mut path)? else {
            return Ok(None);
        };
        let result = edit(root, &path, at, access, self.sequence)?;
        if self.is_empty() {
            self.root = None;
        }
        access.stage_pressure()?;
        Ok(Some(result))
    }
    pub fn visit(
        &self,
        access: &Access<'_>,
        visitor: &mut dyn FnMut(&[u8], &T) -> bool,
    ) -> Result<(), HeapFault> {
        fn walk<T: Cell>(
            branch: &Branch<T>,
            access: &Access<'_>,
            seq: bool,
            visitor: &mut dyn FnMut(&[u8], &T) -> bool,
        ) -> Result<bool, HeapFault> {
            match &*Tree::load_node(branch, access, seq)?.node() {
                Node::Staged(..) => unreachable!("loaded staged node"),
                Node::Leaf(rows) => {
                    for (k, v) in rows {
                        if !visitor(k.as_slice(), v) {
                            return Ok(false);
                        }
                    }
                }
                Node::Index(rows) => {
                    for row in rows {
                        if !walk(row, access, seq, visitor)? {
                            return Ok(false);
                        }
                    }
                }
            }
            Ok(true)
        }
        if let Some(root) = &self.root {
            walk(root, access, self.sequence, visitor)?;
        }
        Ok(())
    }
}
/// A node a read holds: a working node it borrows, or a decoded one it
/// shares.
enum Loaded<T: Cell> {
    Dirty(Rc<residency::DirtyNode<T>>),
    Shared(Rc<Node<T>>),
}
enum NodeGuard<'a, T: Cell> {
    Dirty(std::cell::Ref<'a, Node<T>>),
    Shared(&'a Node<T>),
}
impl<T: Cell> Loaded<T> {
    fn node(&self) -> NodeGuard<'_, T> {
        match self {
            Self::Dirty(node) => NodeGuard::Dirty(node.borrow()),
            Self::Shared(node) => NodeGuard::Shared(node),
        }
    }
}
impl<T: Cell> std::ops::Deref for NodeGuard<'_, T> {
    type Target = Node<T>;
    fn deref(&self) -> &Node<T> {
        match self {
            Self::Dirty(node) => node,
            Self::Shared(node) => node,
        }
    }
}
#[cfg(any(test, feature = "perf-witness"))]
thread_local! {
    static BINDING_COPIES: std::cell::Cell<Option<(u64, u64)>> = const { std::cell::Cell::new(None) };
}
/// Enables transient binding-leaf copy counts on this thread: leaves and rows.
#[cfg(any(test, feature = "perf-witness"))]
pub fn enable_binding_copies() {
    BINDING_COPIES.set(Some((0, 0)));
}
#[cfg(any(test, feature = "perf-witness"))]
pub fn take_binding_copies() -> (u64, u64) {
    BINDING_COPIES.replace(Some((0, 0))).unwrap_or_default()
}
fn make_dirty<T: Cell>(
    branch: &mut Branch<T>,
    access: &Access<'_>,
    seq: bool,
) -> Result<Rc<residency::DirtyNode<T>>, HeapFault> {
    let staged =
        matches!(&branch.link, Link::Dirty(page) if matches!(&*page.borrow(), Node::Staged(..)));
    if !matches!(branch.link, Link::Dirty(_)) || staged {
        let node = Tree::load(branch, access, seq)?;
        branch.family = None;
        branch.encoded_bytes = None;
        branch.is_leaf = None;
        branch.link = Link::Dirty(Rc::new(residency::DirtyNode::new(node)));
    }
    if let Link::Dirty(page) = &mut branch.link
        && Rc::strong_count(page) > 1
    {
        #[cfg(any(test, feature = "perf-witness"))]
        if std::any::TypeId::of::<T>() == std::any::TypeId::of::<super::BindingRecord>() {
            BINDING_COPIES.with(|count| {
                if let Some((leaves, rows)) = count.get()
                    && let Node::Leaf(values) = &*page.borrow()
                {
                    count.set(Some((leaves + 1, rows + values.len() as u64)));
                }
            });
        }
        let private = page.borrow().clone();
        let private = Rc::new(residency::DirtyNode::new(private));
        access.track_copy(page, &private, seq);
        *page = private;
    }
    match &branch.link {
        Link::Dirty(page) => {
            page.revision.set(
                page.revision
                    .get()
                    .checked_add(1)
                    .ok_or_else(|| HeapFault::Malformed("dirty page revision overflow".into()))?,
            );
            access.ensure_tracked(page, seq);
            Ok(Rc::clone(page))
        }
        Link::Sealed(_) | Link::Inline { .. } => {
            Err(HeapFault::Malformed("tree copy failed".into()))
        }
    }
}
fn dirty<T: Cell>(node: Node<T>, access: &Access<'_>, seq: bool) -> Result<Branch<T>, HeapFault> {
    let page = Rc::new(residency::DirtyNode::new(node));
    let mut branch = Branch {
        family: None,
        encoded_bytes: None,
        is_leaf: None,
        tasks_known: true,
        members_known: true,
        first: Key(Vec::new()),
        summary: Summary::default(),
        link: Link::Dirty(Rc::clone(&page)),
    };
    refresh(&mut branch, &page.borrow())?;
    access.track(&page, &page.borrow(), seq);
    Ok(branch)
}
/// A working root over `level`, adding index levels of at most the
/// fanout cap; the seal places their boundaries canonically.
fn grow<T: Cell>(
    mut level: Vec<Branch<T>>,
    access: &Access<'_>,
    seq: bool,
) -> Result<Branch<T>, HeapFault> {
    while level.len() > 1 {
        let mut next = Vec::new();
        while !level.is_empty() {
            let rest = level.split_off(level.len().min(canonical::index_cap(seq)));
            next.push(dirty(Node::Index(level), access, seq)?);
            level = rest;
        }
        level = next;
    }
    level
        .pop()
        .ok_or_else(|| HeapFault::Malformed("empty working root".into()))
}
fn refresh<T: Cell>(branch: &mut Branch<T>, node: &Node<T>) -> Result<(), HeapFault> {
    branch.encoded_bytes = None;
    branch.is_leaf = None;
    branch.tasks_known = true;
    branch.members_known = true;
    branch.summary = match node {
        Node::Staged(..) => unreachable!("refresh staged node"),
        Node::Leaf(rows) => {
            if let Some((first, _)) = rows.first() {
                if branch.first != *first {
                    branch.first = first.clone();
                }
            } else {
                branch.first = Key(Vec::new());
            }
            Summary {
                len: rows.len() as u64,
                cost: checked_sum(rows.iter().map(|r| r.1.cost()))?,
                tasks: checked_sum(rows.iter().map(|r| r.1.task_edges()))?,
                members: checked_sum(rows.iter().map(|r| r.1.live_members()))?,
            }
        }
        Node::Index(rows) => {
            if let Some(first) = rows.first() {
                if branch.first != first.first {
                    branch.first = first.first.clone();
                }
            } else {
                branch.first = Key(Vec::new());
            }
            Summary {
                len: checked_sum(rows.iter().map(|r| r.summary.len))?,
                cost: checked_sum(rows.iter().map(|r| r.summary.cost))?,
                tasks: checked_sum(rows.iter().map(|r| r.summary.tasks))?,
                members: checked_sum(rows.iter().map(|r| r.summary.members))?,
            }
        }
    };
    Ok(())
}
/// One cell's share of its leaf's aggregates.
struct Aggregate {
    cost: u64,
    tasks: u64,
    members: u64,
}
impl Aggregate {
    fn of<T: Cell>(cell: &T) -> Self {
        Self {
            cost: cell.cost(),
            tasks: cell.task_edges(),
            members: cell.live_members(),
        }
    }
    /// `summary` with one child's `before` summary replaced by `after`.
    fn delta(summary: Summary, before: Summary, after: Summary) -> Result<Summary, HeapFault> {
        let swap = |total: u64, out: u64, into: u64| {
            total
                .checked_sub(out)
                .and_then(|total| total.checked_add(into))
                .ok_or_else(|| HeapFault::Malformed("logical aggregate overflow".into()))
        };
        Ok(Summary {
            len: swap(summary.len, before.len, after.len)?,
            cost: swap(summary.cost, before.cost, after.cost)?,
            tasks: swap(summary.tasks, before.tasks, after.tasks)?,
            members: swap(summary.members, before.members, after.members)?,
        })
    }
    /// `summary` with this cell in place of `removed`, or added to it.
    fn replace(self, summary: Summary, removed: Option<Self>) -> Result<Summary, HeapFault> {
        let overflow = || HeapFault::Malformed("logical aggregate overflow".into());
        let swap = |total: u64, out: u64, into: u64| {
            total
                .checked_sub(out)
                .and_then(|total| total.checked_add(into))
                .ok_or_else(overflow)
        };
        let len = u64::from(removed.is_none());
        let removed = removed.unwrap_or(Self {
            cost: 0,
            tasks: 0,
            members: 0,
        });
        Ok(Summary {
            len: summary.len.checked_add(len).ok_or_else(overflow)?,
            cost: swap(summary.cost, removed.cost, self.cost)?,
            tasks: swap(summary.tasks, removed.tasks, self.tasks)?,
            members: swap(summary.members, removed.members, self.members)?,
        })
    }
}
pub fn checked_sum(mut values: impl Iterator<Item = u64>) -> Result<u64, HeapFault> {
    values.try_fold(0u64, |a, b| {
        a.checked_add(b)
            .ok_or_else(|| HeapFault::Malformed("logical aggregate overflow".into()))
    })
}
fn check_keys<'a>(keys: impl Iterator<Item = &'a [u8]>) -> Result<(), &'static str> {
    let mut last = None;
    for key in keys {
        if last.is_some_and(|l| l >= key) {
            return Err("unsorted or duplicate tree keys");
        }
        last = Some(key);
    }
    Ok(())
}
fn corrupt(page: PageRef, error: impl ToString) -> HeapFault {
    HeapFault::Corrupt {
        page: page.id,
        reason: error.to_string(),
    }
}

pub struct SealWork {
    pub remaining: SealBudget,
    pub pages: u64,
}
impl SealWork {
    pub fn new(budget: SealBudget) -> Self {
        Self {
            remaining: budget,
            pages: 0,
        }
    }
    pub fn take(&mut self, bytes: u64) -> bool {
        if self.remaining.pages == 0 || self.remaining.bytes < bytes {
            return false;
        }
        self.remaining.pages -= 1;
        self.remaining.bytes -= bytes;
        self.pages += 1;
        true
    }
}
// Keys are bytes tokens, not a Vec<u8> sequence, keeping cells packed.

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key(Vec<u8>);
#[cfg(test)]
thread_local! {
    static KEY_COPIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static HEAD_LOOKUPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_lookup<T: 'static>() {
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<super::directory::ObjectHead>() {
        HEAD_LOOKUPS.set(HEAD_LOOKUPS.get() + 1);
    }
}
impl Clone for Key {
    fn clone(&self) -> Self {
        #[cfg(test)]
        KEY_COPIES.set(KEY_COPIES.get() + 1);
        Self(self.0.clone())
    }
}
impl Key {
    fn as_slice(&self) -> &[u8] {
        &self.0
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}
impl TokenCodec for Key {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(&self.0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.bytes().map(|b| Self(b.to_vec()))
    }
}

enum SourcePort<'a> {
    Owned(Box<dyn PageSource>),
    Borrowed(&'a mut dyn PageSource),
}
impl PageSource for SourcePort<'_> {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        match self {
            Self::Owned(source) => source.read(pages),
            Self::Borrowed(source) => source.read(pages),
        }
    }
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        match self {
            Self::Owned(source) => source.seal(page),
            Self::Borrowed(source) => source.seal(page),
        }
    }
}
