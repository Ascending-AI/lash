//! Bounded, transient B+ trees. A dirty link owns one mutable page; sealed
//! links retain their summaries and are not visited by sealing.
use super::source::{INDEX_FANOUT, KEYED_LEAF_ROWS, PAGE_TARGET_BYTES};
use super::{PageAnswer, PageBody, PageKind, PageSource, SealBudget};
use lash_kernel_doc::graph_tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};
use lash_kernel_doc::heap::{FamilyId, HeapFault, LogicalBytes, PageId, PageRef};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

mod inline;
pub const INLINE_TREE_BYTES: usize = super::source::INLINE_BODY_MAX as usize;

/// Cells maintain their child links and accounting before publication.
pub trait Cell: TokenCodec + Clone {
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
}
impl<'a> Access<'a> {
    pub fn new(source: Box<dyn PageSource>) -> Self {
        Self {
            source: Rc::new(RefCell::new(SourcePort::Owned(source))),
            cache: Rc::new(RefCell::new(BTreeMap::new())),
            bytes: Rc::new(RefCell::new(0)),
        }
    }
    fn borrowed(source: &'a mut dyn PageSource) -> Self {
        Self {
            source: Rc::new(RefCell::new(SourcePort::Borrowed(source))),
            cache: Rc::new(RefCell::new(BTreeMap::new())),
            bytes: Rc::new(RefCell::new(0)),
        }
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
        let answer = answers.remove(0);
        if answer.page.id != page.id {
            return Err(HeapFault::Corrupt {
                page: page.id,
                reason: "page port answered another page".into(),
            });
        }
        if *self.bytes.borrow() + answer.tokens.len() > 64 << 20 {
            self.clear();
        }
        *self.bytes.borrow_mut() += answer.tokens.len();
        self.cache.borrow_mut().insert(page.id, answer.clone());
        Ok(answer)
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
    }
}

/// Resolves only children authenticated by the page just read.
pub fn grant(page: &mut PageRef, children: &[PageRef]) -> Result<(), HeapFault> {
    *page = children
        .iter()
        .find(|p| p.id == page.id)
        .copied()
        .ok_or_else(|| HeapFault::Corrupt {
            page: page.id,
            reason: "a descriptor references an undeclared child".into(),
        })?;
    Ok(())
}

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
    Dirty(Rc<RefCell<Node<T>>>),
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
#[derive(Clone)]
enum Node<T: Cell> {
    Leaf(Vec<(Key, T)>),
    Index(Vec<Branch<T>>),
}

/// Ordered keys (directory/lookup) or ordinal keys (sequences). Sequence
/// rank uses subtree counts, so removal never renumbers later cells.
#[derive(Clone)]
pub struct Tree<T: Cell> {
    root: Option<Branch<T>>,
    sequence: bool,
}
impl<T: Cell> Default for Tree<T> {
    fn default() -> Self {
        Self::new(false)
    }
}
impl<T: Cell> Tree<T> {
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
    pub fn in_family(mut self, family: FamilyId) -> Self {
        if let Some(root) = &mut self.root {
            root.family = Some(family);
        }
        self
    }
    pub fn len(&self) -> u64 {
        self.root.as_ref().map_or(0, |r| r.summary.len)
    }
    pub fn cost(&self) -> u64 {
        self.root.as_ref().map_or(0, |r| r.summary.cost)
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
        match Self::load(root, access, self.sequence)? {
            Node::Leaf(rows) => checked_sum(rows.iter().map(|(_, c)| c.task_edges())),
            Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.tasks)),
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
        let node = Self::load(root, access, self.sequence)?;
        match node {
            Node::Leaf(rows) => checked_sum(rows.iter().map(|(_, c)| c.live_members())),
            Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.members)),
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
    fn load(branch: &Branch<T>, access: &Access<'_>, sequence: bool) -> Result<Node<T>, HeapFault> {
        match &branch.link {
            Link::Dirty(n) => Ok(n.borrow().clone()),
            Link::Inline { tokens, children } => Self::inline_node(branch, tokens, children, true),
            Link::Sealed(p) => {
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
                        if count > KEYED_LEAF_ROWS {
                            return Err(corrupt(*p, "too many leaf rows"));
                        }
                        let mut rows = Vec::new();
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
                    if rows.is_empty() || rows.len() > INDEX_FANOUT as usize {
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
                    Node::Leaf(rows) => {
                        if rows.is_empty() || rows.len() > KEYED_LEAF_ROWS as usize {
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
                    Node::Leaf(rows) => {
                        checked_sum(rows.iter().map(|(_, cell)| cell.task_edges()))?
                    }
                    Node::Index(rows) => checked_sum(rows.iter().map(|r| r.summary.tasks))?,
                };
                let members = match &node {
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
                Ok(node)
            }
        }
    }
    pub fn get(&self, key: &[u8], access: &Access<'_>) -> Result<Option<T>, HeapFault> {
        let Some(mut branch) = self.root.clone() else {
            return Ok(None);
        };
        loop {
            match Self::load(&branch, access, self.sequence)? {
                Node::Leaf(rows) => {
                    return Ok(rows
                        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                        .ok()
                        .map(|i| rows[i].1.clone()));
                }
                Node::Index(rows) => {
                    branch = rows[rows
                        .partition_point(|r| r.first.as_slice() <= key)
                        .saturating_sub(1)]
                    .clone()
                }
            }
        }
    }
    pub fn rank_get(
        &self,
        mut rank: u64,
        access: &Access<'_>,
    ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
        if rank >= self.len() {
            return Ok(None);
        }
        let Some(mut branch) = self.root.clone() else {
            return Ok(None);
        };
        loop {
            match Self::load(&branch, access, self.sequence)? {
                Node::Leaf(rows) => {
                    return Ok(rows
                        .get(rank as usize)
                        .map(|(k, v)| (k.0.clone(), v.clone())));
                }
                Node::Index(rows) => {
                    let mut chosen = None;
                    for row in rows {
                        if rank < row.summary.len {
                            chosen = Some(row);
                            break;
                        }
                        rank -= row.summary.len;
                    }
                    branch = chosen.ok_or_else(|| {
                        HeapFault::Malformed("tree rank summary is inconsistent".into())
                    })?;
                }
            }
        }
    }
    pub fn after(
        &self,
        key: Option<&[u8]>,
        access: &Access<'_>,
    ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
        fn walk<T: Cell>(
            branch: &Branch<T>,
            key: Option<&[u8]>,
            access: &Access<'_>,
            sequence: bool,
        ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
            match Tree::load(branch, access, sequence)? {
                Node::Leaf(rows) => Ok(rows
                    .into_iter()
                    .find(|(k, _)| key.is_none_or(|a| k.as_slice() > a))
                    .map(|(k, v)| (k.0, v))),
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
        fn walk<T: Cell>(
            branch: &Branch<T>,
            key: &[u8],
            access: &Access<'_>,
            seq: bool,
        ) -> Result<u64, HeapFault> {
            match Tree::load(branch, access, seq)? {
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
        fn edit<T: Cell>(
            branch: &mut Branch<T>,
            key: Key,
            value: T,
            access: &Access<'_>,
            seq: bool,
        ) -> Result<Option<Branch<T>>, HeapFault> {
            let page = make_dirty(branch, access, seq)?;
            let mut node = page.borrow_mut();
            match &mut *node {
                Node::Leaf(rows) => {
                    match rows.binary_search_by(|(k, _)| k.cmp(&key)) {
                        Ok(i) => rows[i].1 = value,
                        Err(i) => rows.insert(i, (key, value)),
                    }
                    let bytes = rows
                        .iter()
                        .map(|(k, v)| k.len() + encode_body(v).len() + 8)
                        .sum::<usize>();
                    if rows.len() > KEYED_LEAF_ROWS as usize || bytes > PAGE_TARGET_BYTES as usize {
                        if rows.len() == 1 {
                            return Err(HeapFault::Malformed("unbounded tree cell".into()));
                        }
                        let right = rows.split_off(rows.len() / 2);
                        refresh(branch, &node)?;
                        return Ok(Some(dirty(Node::Leaf(right))?));
                    }
                }
                Node::Index(rows) => {
                    let i = rows.partition_point(|r| r.first <= key).saturating_sub(1);
                    if let Some(right) = edit(&mut rows[i], key, value, access, seq)? {
                        rows.insert(i + 1, right);
                    }
                    if rows.len() > INDEX_FANOUT as usize {
                        let right = rows.split_off(rows.len() / 2);
                        refresh(branch, &node)?;
                        return Ok(Some(dirty(Node::Index(right))?));
                    }
                }
            }
            refresh(branch, &node)?;
            Ok(None)
        }
        let key = Key(key);
        match &mut self.root {
            None => self.root = Some(dirty(Node::Leaf(vec![(key, value)]))?),
            Some(root) => {
                if let Some(right) = edit(root, key, value, access, self.sequence)? {
                    self.root = Some(dirty(Node::Index(vec![root.clone(), right]))?);
                }
            }
        }
        Ok(())
    }
    pub fn remove(&mut self, key: &[u8], access: &Access<'_>) -> Result<Option<T>, HeapFault> {
        fn edit<T: Cell>(
            branch: &mut Branch<T>,
            key: &[u8],
            access: &Access<'_>,
            seq: bool,
        ) -> Result<Option<T>, HeapFault> {
            let page = make_dirty(branch, access, seq)?;
            let mut node = page.borrow_mut();
            let removed = match &mut *node {
                Node::Leaf(rows) => rows
                    .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                    .ok()
                    .map(|i| rows.remove(i).1),
                Node::Index(rows) => {
                    let i = rows
                        .partition_point(|r| r.first.as_slice() <= key)
                        .saturating_sub(1);
                    let removed = edit(&mut rows[i], key, access, seq)?;
                    if rows[i].summary.len == 0 {
                        rows.remove(i);
                    }
                    removed
                }
            };
            refresh(branch, &node)?;
            Ok(removed)
        }
        let result = self
            .root
            .as_mut()
            .map_or(Ok(None), |r| edit(r, key, access, self.sequence))?;
        if self.len() == 0 {
            self.root = None;
        }
        Ok(result)
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
            match Tree::load(branch, access, seq)? {
                Node::Leaf(rows) => {
                    for (k, v) in &rows {
                        if !visitor(k.as_slice(), v) {
                            return Ok(false);
                        }
                    }
                }
                Node::Index(rows) => {
                    for row in &rows {
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
    pub fn seal(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        fn walk<T: Cell>(
            branch: &mut Branch<T>,
            access: &Access<'_>,
            work: &mut SealWork,
            seq: bool,
        ) -> Result<bool, HeapFault> {
            if matches!(branch.link, Link::Inline { .. }) {
                make_dirty(branch, access, seq)?;
            }
            let Link::Dirty(page) = &branch.link else {
                return Ok(true);
            };
            let mut node = page.borrow_mut();
            let mut children = Vec::new();
            let (kind, tokens) = match &mut *node {
                Node::Leaf(rows) => {
                    for (_, cell) in rows.iter_mut() {
                        if !cell.prepare(access, work)? {
                            return Ok(false);
                        }
                        cell.children(&mut children);
                    }
                    (
                        if seq {
                            PageKind::SEQUENCE_LEAF
                        } else {
                            PageKind::KEYED_LEAF
                        },
                        encode_body(rows),
                    )
                }
                Node::Index(rows) => {
                    for child in rows.iter_mut() {
                        if !walk(child, access, work, seq)? {
                            return Ok(false);
                        }
                    }
                    let mut data = Vec::new();
                    for child in rows.iter() {
                        let Link::Sealed(p) = child.link else {
                            return Err(HeapFault::Malformed("unsealed index child".into()));
                        };
                        children.push(p);
                        data.push((child.first.clone(), (child.summary, (child.family, p))));
                    }
                    (
                        if seq {
                            PageKind::SEQUENCE_INDEX
                        } else {
                            PageKind::KEYED_INDEX
                        },
                        encode_body(&data),
                    )
                }
            };
            if !work.take(tokens.len() as u64 + children.len() as u64 * 32 + 32) {
                return Ok(false);
            }
            let is_leaf = matches!(&*node, Node::Leaf(_));
            let encoded_bytes = tokens.len() as u64;
            let sealed = access.seal(kind, tokens, children)?;
            drop(node);
            branch.link = Link::Sealed(sealed);
            branch.encoded_bytes = Some(encoded_bytes);
            branch.is_leaf = Some(is_leaf);
            Ok(true)
        }
        self.root
            .as_mut()
            .map_or(Ok(true), |r| walk(r, access, work, self.sequence))
    }
}
fn make_dirty<T: Cell>(
    branch: &mut Branch<T>,
    access: &Access<'_>,
    seq: bool,
) -> Result<Rc<RefCell<Node<T>>>, HeapFault> {
    if !matches!(branch.link, Link::Dirty(_)) {
        let node = Tree::load(branch, access, seq)?;
        branch.family = None;
        branch.encoded_bytes = None;
        branch.is_leaf = None;
        branch.link = Link::Dirty(Rc::new(RefCell::new(node)));
    }
    if let Link::Dirty(page) = &mut branch.link
        && Rc::strong_count(page) > 1
    {
        let private = page.borrow().clone();
        *page = Rc::new(RefCell::new(private));
    }
    match &branch.link {
        Link::Dirty(page) => Ok(Rc::clone(page)),
        Link::Sealed(_) | Link::Inline { .. } => {
            Err(HeapFault::Malformed("tree copy failed".into()))
        }
    }
}
fn dirty<T: Cell>(node: Node<T>) -> Result<Branch<T>, HeapFault> {
    let mut branch = Branch {
        family: None,
        encoded_bytes: None,
        is_leaf: None,
        tasks_known: true,
        members_known: true,
        first: Key(Vec::new()),
        summary: Summary::default(),
        link: Link::Dirty(Rc::new(RefCell::new(node.clone()))),
    };
    refresh(&mut branch, &node)?;
    Ok(branch)
}
fn refresh<T: Cell>(branch: &mut Branch<T>, node: &Node<T>) -> Result<(), HeapFault> {
    branch.encoded_bytes = None;
    branch.is_leaf = None;
    branch.tasks_known = true;
    branch.members_known = true;
    branch.summary = match node {
        Node::Leaf(rows) => {
            branch.first = rows
                .first()
                .map_or_else(|| Key(Vec::new()), |r| r.0.clone());
            Summary {
                len: rows.len() as u64,
                cost: checked_sum(rows.iter().map(|r| r.1.cost()))?,
                tasks: checked_sum(rows.iter().map(|r| r.1.task_edges()))?,
                members: checked_sum(rows.iter().map(|r| r.1.live_members()))?,
            }
        }
        Node::Index(rows) => {
            branch.first = rows
                .first()
                .map_or_else(|| Key(Vec::new()), |r| r.first.clone());
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key(Vec<u8>);
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
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
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
