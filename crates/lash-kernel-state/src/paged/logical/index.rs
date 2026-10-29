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
        key: Box<[u8]>,
        value: Option<T>,
    },
    Branch {
        bit: Bit,
        left: Rc<Self>,
        right: Rc<Self>,
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
            mask: if at % 9 == 0 { 0 } else { 1 << (8 - at % 9) },
        }
    }
    fn test(self, key: &[u8]) -> bool {
        if self.mask == 0 {
            key.len() > self.byte
        } else {
            key.get(self.byte).is_some_and(|byte| byte & self.mask != 0)
        }
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
            Node::Branch {
                bit: at,
                left,
                right,
            } => node = if at.test(key) { right } else { left },
        }
    }
}

fn put<T: Cell>(root: &mut Rc<Node<T>>, key: &[u8], value: Option<T>) {
    let (old, _) = leaf(root, key);
    let split = (old != key).then(|| difference(old, key));
    fn insert<T: Cell>(node: &mut Rc<Node<T>>, key: &[u8], value: Option<T>, split: Option<usize>) {
        if let Some(split) = split {
            let above = !matches!(node.as_ref(), Node::Branch { bit, .. } if bit.at < split);
            if above {
                let new = Rc::new(Node::Leaf {
                    key: key.into(),
                    value,
                });
                let old = Rc::clone(node);
                let (left, right) = if Bit::new(split).test(key) {
                    (old, new)
                } else {
                    (new, old)
                };
                *node = Rc::new(Node::Branch {
                    bit: Bit::new(split),
                    left,
                    right,
                });
                return;
            }
        }
        match Rc::make_mut(node) {
            Node::Leaf { value: old, .. } => *old = value,
            Node::Branch {
                bit: at,
                left,
                right,
            } => insert(if at.test(key) { right } else { left }, key, value, split),
        }
    }
    insert(root, key, value, split);
}

fn erase<T: Cell>(mut root: Rc<Node<T>>, key: &[u8]) -> Option<Rc<Node<T>>> {
    match Rc::make_mut(&mut root) {
        Node::Leaf { key: old, .. } => {
            if old.as_ref() == key {
                return None;
            }
        }
        Node::Branch {
            bit: at,
            left,
            right,
        } => {
            let (chosen, other) = if at.test(key) {
                (right, left)
            } else {
                (left, right)
            };
            match erase(Rc::clone(chosen), key) {
                Some(next) => *chosen = next,
                None => return Some(Rc::clone(other)),
            }
        }
    }
    Some(root)
}

fn walk<T: Cell>(node: &Node<T>, visit: &mut dyn FnMut(&[u8], &Option<T>) -> bool) -> bool {
    match node {
        Node::Leaf { key, value } => visit(key, value),
        Node::Branch { left, right, .. } => walk(left, visit) && walk(right, visit),
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

#[derive(Clone)]
pub(super) struct Index<T: Cell> {
    base: Tree<T>,
    hot: Option<Rc<Node<T>>>,
    totals: Totals,
    known: bool,
    revision: u64,
}

impl<T: Cell> Index<T> {
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
        Ok(())
    }
    fn hot(&self, key: &[u8]) -> Option<&Option<T>> {
        let (found, value) = leaf(self.hot.as_ref()?, key);
        (found == key).then_some(value)
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
    pub fn write(
        &mut self,
        key: impl AsRef<[u8]>,
        value: T,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        let key = key.as_ref();
        let old = self.get_with(key, access, |v| v.map_or(Totals::default(), Totals::row))?;
        self.changed(old, Totals::row(&value), access)?;
        self.insert(key, Some(value));
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
                Node::Branch {
                    bit: at,
                    left,
                    right,
                } => edit(if at.test(key) { right } else { left }, key, update),
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
        let Some(value) = self.get(key, access)? else {
            return Ok(None);
        };
        self.changed(Totals::row(&value), Totals::default(), access)?;
        if self.base.get_with(key, access, |v| v.is_some())? {
            self.insert(key, None);
        } else if let Some(root) = self.hot.take() {
            self.hot = erase(root, key);
        }
        Ok(Some(value))
    }
    pub fn len(&self) -> u64 {
        self.totals.len
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn cost(&self) -> u64 {
        self.totals.cost
    }
    pub fn task_edges(&self) -> u64 {
        self.totals.tasks
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
                Node::Branch { left, right, .. } => {
                    collect(left, out);
                    collect(right, out);
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
                Node::Branch { left, right, .. } => {
                    let mut last = left.as_ref();
                    while let Node::Branch { right, .. } = last {
                        last = right;
                    }
                    let Node::Leaf { key, .. } = last else {
                        unreachable!()
                    };
                    if cursor.is_none_or(|c| key.as_ref() > c) {
                        next(left, cursor).or_else(|| next(right, cursor))
                    } else {
                        next(right, cursor)
                    }
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
            while let Node::Branch { left, .. } = first {
                first = left;
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
            self.hot = self.hot.take().and_then(|root| erase(root, &key));
        }
        Ok(true)
    }
    pub fn seal(&mut self, access: &Access<'_>, work: &mut SealWork) -> Result<bool, HeapFault> {
        if !self.flush(access, work)? {
            return Ok(false);
        }
        self.base.seal(access, work)
    }
    pub fn seal_nested(
        &mut self,
        access: &Access<'_>,
        work: &mut SealWork,
    ) -> Result<bool, HeapFault> {
        if !self.flush(access, work)? {
            return Ok(false);
        }
        self.base.seal_nested(access, work)
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
