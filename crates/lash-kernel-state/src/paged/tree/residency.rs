//! Dirty pages share a bounded staging registry across all activation trees.
use super::decoded::{RowSize, RowSums};
use super::*;
use std::collections::VecDeque;
use std::rc::Weak;

pub(super) const DIRTY_LIMIT: usize = 32 << 20;
const DIRTY_TARGET: usize = 24 << 20;
trait Stage {
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault>;
}
impl<T: Cell> Stage for RefCell<Node<T>> {
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault> {
        let node = self.borrow().clone();
        if matches!(node, Node::Staged(..)) {
            return Ok(());
        }
        // Preparing and sealing may change storage links, never guest content.
        // All working versions sharing this node see the same immutable stage.
        let mut branch = dirty(node, access, sequence)?;
        let mut tree = Tree {
            root: Some(branch.clone()),
            sequence,
        };
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        tree.seal(access, &mut work)?;
        branch = tree
            .root
            .ok_or_else(|| HeapFault::Malformed("nonempty staged tree lost its root".into()))?;
        let Link::Sealed(page) = branch.link else {
            unreachable!()
        };
        *self.borrow_mut() = Node::Staged(
            page,
            branch
                .encoded_bytes
                .ok_or_else(|| HeapFault::Malformed("staged tree has no byte summary".into()))?,
            branch
                .is_leaf
                .ok_or_else(|| HeapFault::Malformed("staged tree has no node kind".into()))?,
        );
        Ok(())
    }
}
struct Entry {
    node: Weak<dyn Stage>,
    bytes: usize,
    sequence: bool,
    /// A leaf's row sums as of its last tracking; None once its rows may
    /// have changed untracked.
    rows: Option<RowSums>,
}
impl Entry {
    fn holds<T: Cell>(&self, page: &Rc<RefCell<Node<T>>>) -> bool {
        self.node.strong_count() > 0 && std::ptr::addr_eq(self.node.as_ptr(), Rc::as_ptr(page))
    }
}
#[derive(Default)]
pub(super) struct DirtyResidency {
    entries: BTreeMap<usize, Entry>,
    order: VecDeque<usize>,
    bytes: usize,
    staging: bool,
}
impl Access<'_> {
    pub(super) fn track<T: Cell>(
        &self,
        page: &Rc<RefCell<Node<T>>>,
        node: &Node<T>,
        sequence: bool,
    ) {
        if matches!(node, Node::Staged(..)) {
            return;
        }
        let rows = match node {
            Node::Leaf(rows) => Some(super::decoded::row_sums(rows)),
            _ => None,
        };
        let weight = match (node, rows) {
            (Node::Leaf(leaf), Some(sums)) => super::decoded::leaf_weight(leaf, sums),
            _ => super::decoded::node_weight(node),
        };
        self.record(page, weight, sequence, rows);
    }
    /// Tracks `page` unless it is tracked already with what it holds now.
    pub(super) fn ensure_tracked<T: Cell>(&self, page: &Rc<RefCell<Node<T>>>, sequence: bool) {
        let known = self
            .dirty
            .borrow()
            .entries
            .get(&(Rc::as_ptr(page) as usize))
            .is_some_and(|entry| {
                entry.holds(page)
                    && (entry.rows.is_some() || !matches!(&*page.borrow(), Node::Leaf(_)))
            });
        if !known {
            self.track(page, &page.borrow(), sequence);
        }
    }
    /// Tracks `copy`, a private copy-on-write duplicate of `page`, as
    /// `page` is tracked: the same rows weigh the same, so a write after
    /// a publication does not measure the leaf again (FIG-6318).
    pub(super) fn track_copy<T: Cell>(
        &self,
        page: &Rc<RefCell<Node<T>>>,
        copy: &Rc<RefCell<Node<T>>>,
        sequence: bool,
    ) {
        let known = self
            .dirty
            .borrow()
            .entries
            .get(&(Rc::as_ptr(page) as usize))
            .filter(|entry| entry.holds(page))
            .map(|entry| (entry.bytes, entry.rows));
        match known {
            Some((bytes, rows)) => self.record(
                copy,
                bytes - std::mem::size_of::<Entry>() - 64,
                sequence,
                rows,
            ),
            None => self.track(copy, &copy.borrow(), sequence),
        }
    }
    /// Re-tracks a leaf after one of its rows changed from `before` (None
    /// for an inserted row) to `after`, and gives the leaf's encoded body
    /// size. The other rows are not encoded again.
    pub(super) fn track_row<T: Cell>(
        &self,
        page: &Rc<RefCell<Node<T>>>,
        rows: &Vec<(Key, T)>,
        sequence: bool,
        before: Option<RowSize>,
        after: RowSize,
    ) -> usize {
        let known = self
            .dirty
            .borrow()
            .entries
            .get(&(Rc::as_ptr(page) as usize))
            .filter(|entry| entry.holds(page))
            .and_then(|entry| entry.rows);
        let sums = known
            .and_then(|sums| match before {
                Some(row) => sums.without(row),
                None => Some(sums),
            })
            .map(|sums| sums.add(after))
            .unwrap_or_else(|| super::decoded::row_sums(rows));
        let weight = super::decoded::leaf_weight(rows, sums);
        self.record(page, weight, sequence, Some(sums));
        sums.encoded
    }
    /// Seal preparation may relink a leaf's cells in place: its row sums
    /// are measured again when it is next tracked.
    pub(super) fn forget_rows<T: Cell>(&self, page: &Rc<RefCell<Node<T>>>) {
        if let Some(entry) = self
            .dirty
            .borrow_mut()
            .entries
            .get_mut(&(Rc::as_ptr(page) as usize))
            .filter(|entry| entry.holds(page))
        {
            entry.rows = None;
        }
    }
    fn record<T: Cell>(
        &self,
        page: &Rc<RefCell<Node<T>>>,
        weight: usize,
        sequence: bool,
        rows: Option<RowSums>,
    ) {
        let bytes = weight + std::mem::size_of::<Entry>() + 64;
        let id = Rc::as_ptr(page) as usize;
        let mut dirty = self.dirty.borrow_mut();
        // Weak references do not change copy-on-write's ownership test.
        let erased: Rc<dyn Stage> = page.clone();
        let entry = Entry {
            node: Rc::downgrade(&erased),
            bytes,
            sequence,
            rows,
        };
        if let Some(old) = dirty.entries.insert(id, entry) {
            dirty.bytes -= old.bytes;
        } else {
            dirty.order.push_back(id);
        }
        dirty.bytes += bytes;
    }
    pub(super) fn stage_pressure(&self) -> Result<(), HeapFault> {
        {
            let mut dirty = self.dirty.borrow_mut();
            if dirty.staging || dirty.bytes <= DIRTY_LIMIT {
                return Ok(());
            }
            dirty.staging = true;
        }
        let result = (|| {
            loop {
                let entry = {
                    let mut dirty = self.dirty.borrow_mut();
                    if dirty.bytes <= DIRTY_TARGET {
                        break;
                    }
                    let Some(id) = dirty.order.pop_front() else {
                        break;
                    };
                    let Some(entry) = dirty.entries.remove(&id) else {
                        continue;
                    };
                    dirty.bytes -= entry.bytes;
                    (id, entry)
                };
                if let Some(node) = entry.1.node.upgrade()
                    && let Err(fault) = node.spill(self, entry.1.sequence)
                {
                    let mut dirty = self.dirty.borrow_mut();
                    dirty.bytes += entry.1.bytes;
                    dirty.order.push_front(entry.0);
                    dirty.entries.insert(entry.0, entry.1);
                    return Err(fault);
                }
            }
            Ok(())
        })();
        self.dirty.borrow_mut().staging = false;
        result
    }
    #[cfg(test)]
    pub(crate) fn dirty_bytes(&self) -> usize {
        self.dirty.borrow().bytes
    }
}
