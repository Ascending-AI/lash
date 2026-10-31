//! Source journals keep changed rows in a persistent radix index. Only sealing
//! builds canonical pages; a draft copies search paths, never a paged leaf.
use super::invalid;
use crate::paged::KeyedRoot;
use crate::paged::tree::{Access, Cell, SealWork, Tree};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};
use std::rc::Rc;

#[derive(Clone)]
enum Node<T: Cell> {
    Leaf {
        key: Rc<[u8]>,
        value: Option<T>,
    },
    Branch {
        bit: Bit,
        children: [Option<Rc<Self>>; 4],
    },
}

#[derive(Clone, Copy)]
struct Bit {
    at: usize,
    byte: usize,
    mask: u8,
}
impl Bit {
    fn new(at: usize) -> Self {
        Self {
            at,
            byte: at / 9,
            mask: if at.is_multiple_of(9) {
                0
            } else {
                1 << (8 - at % 9)
            },
        }
    }
    fn test(self, key: &[u8]) -> bool {
        if self.mask == 0 {
            key.len() > self.byte
        } else {
            key.get(self.byte).is_some_and(|byte| byte & self.mask != 0)
        }
    }
    fn digit(self, key: &[u8]) -> usize {
        let next = Self {
            at: self.at + 1,
            byte: self.byte + usize::from(self.mask == 1),
            mask: match self.mask {
                0 => 128,
                1 => 0,
                mask => mask >> 1,
            },
        };
        usize::from(self.test(key)) * 2 + usize::from(next.test(key))
    }
}

fn difference(a: &[u8], b: &[u8]) -> usize {
    for (i, (a, b)) in a.iter().zip(b).enumerate() {
        if a != b {
            return i * 9 + 1 + (a ^ b).leading_zeros() as usize;
        }
    }
    a.len().min(b.len()) * 9
}

fn leaf<'a, T: Cell>(mut node: &'a Node<T>, key: &[u8]) -> (&'a [u8], &'a Option<T>) {
    loop {
        match node {
            Node::Leaf { key, value } => return (key, value),
            Node::Branch { bit, children } => {
                node = children[bit.digit(key)]
                    .as_ref()
                    .or_else(|| children.iter().flatten().next())
                    .unwrap_or_else(|| unreachable!("radix branch has children"));
            }
        }
    }
}

fn put<T: Cell>(root: &mut Rc<Node<T>>, key: &[u8], value: Option<T>) {
    let (old, _) = leaf(root, key);
    let split = (old != key).then(|| difference(old, key) & !1);
    fn insert<T: Cell>(node: &mut Rc<Node<T>>, key: &[u8], value: Option<T>, split: Option<usize>) {
        if let Some(split) = split {
            let above = !matches!(node.as_ref(), Node::Branch { bit, .. } if bit.at <= split);
            if above {
                let bit = Bit::new(split);
                let old_digit = bit.digit(leaf(node, key).0);
                let new_digit = bit.digit(key);
                let mut children = std::array::from_fn(|_| None);
                children[old_digit] = Some(Rc::clone(node));
                children[new_digit] = Some(Rc::new(Node::Leaf {
                    key: key.into(),
                    value,
                }));
                *node = Rc::new(Node::Branch { bit, children });
                return;
            }
        }
        match Rc::make_mut(node) {
            Node::Leaf { value: old, .. } => *old = value,
            Node::Branch { bit, children } => {
                let child = &mut children[bit.digit(key)];
                if let Some(child) = child {
                    insert(child, key, value, split);
                } else {
                    *child = Some(Rc::new(Node::Leaf {
                        key: key.into(),
                        value,
                    }));
                }
            }
        }
    }
    insert(root, key, value, split);
}

fn erase<T: Cell>(root: &mut Rc<Node<T>>, key: &[u8]) -> bool {
    match Rc::make_mut(root) {
        Node::Leaf { key: old, .. } => old.as_ref() == key,
        Node::Branch { bit, children } => {
            let digit = bit.digit(key);
            if let Some(child) = &mut children[digit]
                && erase(child, key)
            {
                children[digit] = None;
            }
            let mut remaining = children.iter().flatten();
            let first = remaining
                .next()
                .unwrap_or_else(|| unreachable!("radix deletion keeps a child"));
            if remaining.next().is_none() {
                *root = Rc::clone(first);
            }
            false
        }
    }
}

fn walk<T: Cell>(node: &Node<T>, visit: &mut dyn FnMut(&[u8], &Option<T>) -> bool) -> bool {
    match node {
        Node::Leaf { key, value } => visit(key, value),
        Node::Branch { children, .. } => children.iter().flatten().all(|child| walk(child, visit)),
    }
}

#[derive(Clone, Copy, Default)]
struct Totals {
    len: u64,
    cost: u64,
    tasks: u64,
    members: u64,
}
impl Totals {
    fn row<T: Cell>(value: &T) -> Self {
        Self {
            len: 1,
            cost: value.cost(),
            tasks: value.task_edges(),
            members: value.live_members(),
        }
    }
    fn replace(self, old: Self, new: Self) -> Result<Self, HeapFault> {
        let delta = |value: u64, old: u64, new: u64| {
            value
                .checked_sub(old)
                .and_then(|n| n.checked_add(new))
                .ok_or_else(|| invalid("source index aggregate overflow"))
        };
        Ok(Self {
            len: delta(self.len, old.len, new.len)?,
            cost: delta(self.cost, old.cost, new.cost)?,
            tasks: delta(self.tasks, old.tasks, new.tasks)?,
            members: delta(self.members, old.members, new.members)?,
        })
    }
}

/// A replacement selected while the index exclusively owns the observed row.
pub(super) enum RowEdit<T> {
    Keep,
    Put(T),
    Remove,
}

#[derive(Clone)]
pub(super) struct Index<T: Cell> {
    base: Tree<T>,
    hot: Option<Rc<Node<T>>>,
    totals: Totals,
    known: bool,
    revision: u64,
    nested_pages: bool,
}

impl<T: Cell> Index<T> {
    pub(super) fn from_rows(
        rows: Vec<(crate::paged::tree::Key, T)>,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let mut index = Self::of_tree(Tree::nested_from_rows(rows, false, access)?, false);
        index.ready(access)?;
        Ok(index)
    }
    pub(super) fn single(key: &[u8], value: T, access: &Access<'_>) -> Result<Self, HeapFault> {
        let base = Tree::nested_from_rows(vec![(key.to_vec(), value)], false, access)?;
        let mut index = Self::of_tree(base, false);
        index.ready(access)?;
        Ok(index)
    }
    pub fn new(sequence: bool) -> Self {
        Self::of_tree(Tree::new(sequence), true)
    }
    pub fn of_keyed(root: &KeyedRoot) -> Self {
        Self::of_tree(Tree::of_keyed(root), false)
    }
    fn of_tree(base: Tree<T>, known: bool) -> Self {
        let totals = Totals {
            len: base.len(),
            cost: base.cost(),
            tasks: base.task_edges(),
            members: 0,
        };
        Self {
            base,
            hot: None,
            totals,
            known,
            revision: 0,
            nested_pages: false,
        }
    }
    fn ready(&mut self, access: &Access<'_>) -> Result<(), HeapFault> {
        if !self.known {
            self.totals.tasks = self.base.read_task_edges(access)?;
            self.totals.members = self.base.live_members(access)?;
            self.known = true;
        }
        Ok(())
    }
    fn changed(&mut self, old: Totals, new: Totals, access: &Access<'_>) -> Result<(), HeapFault> {
        self.ready(access)?;
        let totals = self.totals.replace(old, new)?;
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("source index revision overflow"))?;
        self.totals = totals;
        self.revision = revision;
        self.nested_pages = false;
        Ok(())
    }
    fn hot(&self, key: &[u8]) -> Option<&Option<T>> {
        let mut node = self.hot.as_deref()?;
        loop {
            match node {
                Node::Leaf { key: found, value } => {
                    return (found.as_ref() == key).then_some(value);
                }
                Node::Branch { bit, children } => node = children[bit.digit(key)].as_deref()?,
            }
        }
    }
    fn insert(&mut self, key: &[u8], value: Option<T>) {
        match &mut self.hot {
            Some(root) => put(root, key, value),
            None => {
                self.hot = Some(Rc::new(Node::Leaf {
                    key: key.into(),
                    value,
                }))
            }
        }
    }
    pub fn get_with<R>(
        &self,
        key: &[u8],
        access: &Access<'_>,
        get: impl FnOnce(Option<&T>) -> R,
    ) -> Result<R, HeapFault> {
        match self.hot(key) {
            Some(value) => Ok(get(value.as_ref())),
            None => self.base.get_with(key, access, get),
        }
    }
    pub fn get(&self, key: &[u8], access: &Access<'_>) -> Result<Option<T>, HeapFault> {
        self.get_with(key, access, |v| v.cloned())
    }
    /// Read the current row once, then apply its checked replacement under
    /// this exclusive borrow. Aggregates use that same authenticated old row.
    pub(super) fn replace_with(
        &mut self,
        key: &[u8],
        access: &Access<'_>,
        replace: impl FnOnce(Option<&T>) -> Result<RowEdit<T>, HeapFault>,
    ) -> Result<bool, HeapFault> {
        let (old, replacement) = self.get_with(key, access, |previous| {
            replace(previous)
                .map(|replacement| (previous.map_or(Totals::default(), Totals::row), replacement))
        })??;
        match replacement {
            RowEdit::Keep => Ok(false),
            RowEdit::Put(value) => {
                self.changed(old, Totals::row(&value), access)?;
                self.insert(key, Some(value));
                Ok(true)
            }
            RowEdit::Remove => {
                if old.len == 0 {
                    return Err(invalid("removed source row is absent"));
                }
                self.changed(old, Totals::default(), access)?;
                if self.base.get_with(key, access, |v| v.is_some())? {
                    self.insert(key, None);
                } else if let Some(root) = &mut self.hot
                    && erase(root, key)
                {
                    self.hot = None;
                }
                Ok(true)
            }
        }
    }
    pub fn write(
        &mut self,
        key: impl AsRef<[u8]>,
        value: T,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        self.replace_with(key.as_ref(), access, |_| Ok(RowEdit::Put(value)))?;
        Ok(())
    }
    pub fn update(
        &mut self,
        key: impl AsRef<[u8]>,
        access: &Access<'_>,
        update: impl FnOnce(&mut T) -> Result<bool, HeapFault>,
    ) -> Result<bool, HeapFault> {
        let key = key.as_ref();
        self.ready(access)?;
        fn edit<T: Cell>(
            node: &mut Rc<Node<T>>,
            key: &[u8],
            update: impl FnOnce(&mut T) -> Result<bool, HeapFault>,
        ) -> Result<Option<(Totals, Totals)>, HeapFault> {
            match Rc::make_mut(node) {
                Node::Branch { bit, children } => edit(
                    children[bit.digit(key)]
                        .as_mut()
                        .ok_or_else(|| invalid("source index lost a row"))?,
                    key,
                    update,
                ),
                Node::Leaf { value, .. } => {
                    let value = value
                        .as_mut()
                        .ok_or_else(|| invalid("updated source row is absent"))?;
                    let before = Totals::row(value);
                    Ok(update(value)?.then(|| (before, Totals::row(value))))
                }
            }
        }
        if self.hot(key).is_some() {
            let root = self
                .hot
                .as_mut()
                .ok_or_else(|| invalid("source index lost its root"))?;
            let Some((old, new)) = edit(root, key, update)? else {
                return Ok(false);
            };
            self.changed(old, new, access)?;
        } else {
            let mut value = self
                .base
                .get(key, access)?
                .ok_or_else(|| invalid("updated source row is absent"))?;
            if !update(&mut value)? {
                return Ok(false);
            }
            self.write(key, value, access)?;
        }
        Ok(true)
    }
    pub fn remove(&mut self, key: &[u8], access: &Access<'_>) -> Result<Option<T>, HeapFault> {
        let mut removed = None;
        self.replace_with(key, access, |previous| {
            removed = previous.cloned();
            Ok(if removed.is_some() {
                RowEdit::Remove
            } else {
                RowEdit::Keep
            })
        })?;
        Ok(removed)
    }
    pub fn len(&self) -> u64 {
        self.totals.len
    }
    pub fn cost(&self) -> u64 {
        self.totals.cost
    }
    pub fn task_edges(&self) -> u64 {
        self.totals.tasks
    }
    pub(super) fn read_task_edges(&mut self, access: &Access<'_>) -> Result<u64, HeapFault> {
        self.ready(access)?;
        Ok(self.totals.tasks)
    }
    pub(super) fn write_batch(
        &mut self,
        mut rows: Vec<(Vec<u8>, T)>,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        if rows.is_empty() {
            return Ok(());
        }
        rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if rows.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(invalid("source batch replaces a row twice"));
        }
        self.ready(access)?;
        let mut totals = self.totals;
        for (key, _) in &rows {
            let old = self
                .get_with(key, access, |row| row.map(Totals::row))?
                .ok_or_else(|| invalid("source batch row is absent"))?;
            totals = totals.replace(old, Totals::default())?;
        }
        for (_, row) in &rows {
            totals = totals.replace(Totals::default(), Totals::row(row))?;
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("source index revision overflow"))?;
        for (key, row) in rows {
            self.insert(&key, Some(row));
        }
        self.totals = totals;
        self.revision = revision;
        self.nested_pages = false;
        Ok(())
    }
    pub fn live_members(&self, access: &Access<'_>) -> Result<u64, HeapFault> {
        if self.known {
            Ok(self.totals.members)
        } else {
            self.base.live_members(access)
        }
    }
    pub fn visit(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&[u8], &T) -> bool,
    ) -> Result<(), HeapFault> {
        let Some(hot) = &self.hot else {
            return self.base.visit(access, visit);
        };
        if self.base.is_empty() {
            walk(hot, &mut |key, value| {
                value.as_ref().is_none_or(|v| visit(key, v))
            });
            return Ok(());
        }
        let mut rows = Vec::new();
        fn collect<'a, T: Cell>(node: &'a Node<T>, out: &mut Vec<(&'a [u8], &'a Option<T>)>) {
            match node {
                Node::Leaf { key, value } => out.push((key, value)),
                Node::Branch { children, .. } => {
                    for child in children.iter().flatten() {
                        collect(child, out);
                    }
                }
            }
        }
        collect(hot, &mut rows);
        let mut at = 0;
        let mut running = true;
        self.base.visit(access, &mut |key, value| {
            while at < rows.len() && rows[at].0 < key {
                if let Some(value) = rows[at].1 {
                    running = visit(rows[at].0, value);
                }
                at += 1;
                if !running {
                    return false;
                }
            }
            if at < rows.len() && rows[at].0 == key {
                if let Some(value) = rows[at].1 {
                    running = visit(key, value);
                }
                at += 1;
            } else {
                running = visit(key, value);
            }
            running
        })?;
        while running && at < rows.len() {
            if let Some(value) = rows[at].1 {
                running = visit(rows[at].0, value);
            }
            at += 1;
        }
        Ok(())
    }
    pub fn after(
        &self,
        cursor: Option<&[u8]>,
        access: &Access<'_>,
    ) -> Result<Option<(Vec<u8>, T)>, HeapFault> {
        fn next<T: Cell>(node: &Node<T>, cursor: Option<&[u8]>) -> Option<(Vec<u8>, T)> {
            match node {
                Node::Leaf { key, value } => {
                    if cursor.is_none_or(|c| key.as_ref() > c) {
                        value.as_ref().map(|v| (key.to_vec(), v.clone()))
                    } else {
                        None
                    }
                }
                Node::Branch { children, .. } => {
                    let mut last = node;
                    while let Node::Branch { children, .. } = last {
                        last = children
                            .iter()
                            .flatten()
                            .next_back()
                            .unwrap_or_else(|| unreachable!("radix child"));
                    }
                    let Node::Leaf { key, .. } = last else {
                        unreachable!()
                    };
                    if cursor.is_some_and(|c| key.as_ref() <= c) {
                        return None;
                    }
                    children
                        .iter()
                        .flatten()
                        .find_map(|child| next(child, cursor))
                }
            }
        }
        let hot = self.hot.as_ref().and_then(|root| next(root, cursor));
        let mut position = cursor.map(<[u8]>::to_vec);
        loop {
            let base = self.base.after(position.as_deref(), access)?;
            let Some((key, value)) = base else {
                return Ok(hot);
            };
            if self.hot(&key).is_some() {
                position = Some(key);
                continue;
            }
            return Ok(match hot {
                Some(row) if row.0 < key => Some(row),
                _ => Some((key, value)),
            });
        }
    }
    fn flush(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        while let Some(root) = &self.hot {
            let mut first = root.as_ref();
            while let Node::Branch { children, .. } = first {
                first = children
                    .iter()
                    .flatten()
                    .next()
                    .unwrap_or_else(|| unreachable!("radix child"));
            }
            let Node::Leaf { key, value } = first else {
                unreachable!()
            };
            let key = key.to_vec();
            let mut value = value.clone();
            if let Some(value) = &mut value {
                if !value.prepare(access, work)? {
                    self.insert(&key, Some(value.clone()));
                    return Ok(false);
                }
                self.base.write(key.clone(), value.clone(), access)?;
            } else {
                self.base.remove(&key, access)?;
            }
            if let Some(root) = &mut self.hot
                && erase(root, &key)
            {
                self.hot = None;
            }
        }
        Ok(true)
    }
    pub fn seal(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        if !self.flush(access, work)? {
            return Ok(false);
        }
        self.base.seal(access, work)
    }
    pub fn keyed_root(&self) -> Result<KeyedRoot, HeapFault> {
        if self.hot.is_some() {
            return Err(invalid("source index is unsealed"));
        }
        self.base.keyed_root()
    }
    pub fn same_version(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.base.same_version(&other.base)
            && match (&self.hot, &other.hot) {
                (None, None) => true,
                (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                _ => false,
            }
    }
    pub fn children(&self, out: &mut Vec<lash_kernel_doc::heap::PageRef>) {
        self.base.children(out);
    }
    pub fn grants(&mut self, children: &[lash_kernel_doc::heap::PageRef]) -> Result<(), HeapFault> {
        self.base.grants(children)
    }
}

impl Index<super::Edge> {
    pub fn seal_nested(
        &mut self,
        access: &Access<'_>,
        work: &mut SealWork,
    ) -> Result<bool, HeapFault> {
        if !self.flush(access, work)? {
            return Ok(false);
        }
        // Edge cells need no preparation. A yielded nested seal has therefore
        // already chosen pages. Resume those pages without flattening a small
        // partial index and discarding its completed chunks on every retry.
        let sealed = if self.nested_pages {
            self.base.seal(access, work)?
        } else {
            self.base.seal_nested(access, work)?
        };
        self.nested_pages = !sealed;
        Ok(sealed)
    }
}

impl<T: Cell> TokenCodec for Index<T> {
    fn encode(&self, out: &mut TokenWriter) {
        if self.hot.is_none() {
            self.base.encode(out);
        } else {
            self.base.encode_pending(
                self.len(),
                self.cost(),
                self.task_edges(),
                self.totals.members,
                out,
            );
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self::of_tree(Tree::decode(input)?, false))
    }
}

pub(super) trait Rows<T: Cell> {
    fn rows(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&[u8], &T) -> bool,
    ) -> Result<(), HeapFault>;
}
impl<T: Cell> Rows<T> for Tree<T> {
    fn rows(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&[u8], &T) -> bool,
    ) -> Result<(), HeapFault> {
        self.visit(access, visit)
    }
}
impl<T: Cell> Rows<T> for Index<T> {
    fn rows(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&[u8], &T) -> bool,
    ) -> Result<(), HeapFault> {
        self.visit(access, visit)
    }
}
pub(super) fn visit_tree<T: Cell>(
    tree: &impl Rows<T>,
    access: &Access<'_>,
    visit: &mut dyn FnMut(&T) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    let mut error = None;
    tree.rows(access, &mut |_, value| match visit(value) {
        Ok(()) => true,
        Err(fault) => {
            error = Some(fault);
            false
        }
    })?;
    error.map_or(Ok(()), Err)
}
