//! What changed between two versions of one tree, reading only the pages
//! the two do not share.

use std::collections::VecDeque;

use lash_kernel_doc::heap::HeapFault;

use super::{Access, Branch, Cell, Key, Link, Node, Tree};

/// One side of the merge: a subtree not yet opened, or one of its rows.
enum Item<T: Cell> {
    Sub(Branch<T>),
    Row(Key, T),
}

/// A row that differs between two versions: its key, with the old and the
/// new cell, each absent where that version holds none.
pub type Change<'a, T> = (&'a [u8], Option<&'a T>, Option<&'a T>);

impl<T: Cell> Tree<T> {
    /// Visits every key whose row differs between `self` and `newer`, in
    /// key order. Subtrees both versions share by page are skipped
    /// unread, so a version that changed few rows reads only the pages on
    /// their paths.
    ///
    /// # Errors
    ///
    /// The faults reading either version's pages raises.
    pub fn diff(
        &self,
        newer: &Self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(Change<'_, T>),
    ) -> Result<(), HeapFault> {
        let sequence = self.sequence;
        let side = |tree: &Self| -> VecDeque<Item<T>> {
            tree.root.iter().cloned().map(Item::Sub).collect()
        };
        let (mut old, mut new) = (side(self), side(newer));
        loop {
            match (old.front(), new.front()) {
                (None, None) => return Ok(()),
                (Some(Item::Sub(a)), Some(Item::Sub(b))) if shared(a, b) => {
                    old.pop_front();
                    new.pop_front();
                }
                (Some(Item::Sub(_)), _) => open(&mut old, access, sequence)?,
                (_, Some(Item::Sub(_))) => open(&mut new, access, sequence)?,
                (Some(Item::Row(a, va)), Some(Item::Row(b, vb))) => match a.cmp(b) {
                    std::cmp::Ordering::Less => {
                        visit((a.as_slice(), Some(va), None));
                        old.pop_front();
                    }
                    std::cmp::Ordering::Greater => {
                        visit((b.as_slice(), None, Some(vb)));
                        new.pop_front();
                    }
                    std::cmp::Ordering::Equal => {
                        if !va.same_version(vb) {
                            visit((a.as_slice(), Some(va), Some(vb)));
                        }
                        old.pop_front();
                        new.pop_front();
                    }
                },
                (Some(Item::Row(a, va)), None) => {
                    visit((a.as_slice(), Some(va), None));
                    old.pop_front();
                }
                (None, Some(Item::Row(b, vb))) => {
                    visit((b.as_slice(), None, Some(vb)));
                    new.pop_front();
                }
            }
        }
    }
}

/// Whether two subtrees are one sealed page over the same keys.
fn shared<T: Cell>(a: &Branch<T>, b: &Branch<T>) -> bool {
    match (&a.link, &b.link) {
        (Link::Sealed(x), Link::Sealed(y)) => {
            x.id == y.id && a.first == b.first && a.family == b.family
        }
        _ => false,
    }
}

/// Replaces the subtree at the front of `side` with what it holds, in key
/// order.
fn open<T: Cell>(
    side: &mut VecDeque<Item<T>>,
    access: &Access<'_>,
    sequence: bool,
) -> Result<(), HeapFault> {
    let Some(Item::Sub(branch)) = side.pop_front() else {
        return Ok(());
    };
    let items: Vec<Item<T>> = match Tree::load(&branch, access, sequence)? {
        Node::Staged(..) => {
            return Err(HeapFault::Malformed(
                "a staged node was loaded as a tree node".into(),
            ));
        }
        Node::Leaf(rows) => rows.into_iter().map(|(k, v)| Item::Row(k, v)).collect(),
        Node::Index(rows) => rows.into_iter().map(Item::Sub).collect(),
    };
    for item in items.into_iter().rev() {
        side.push_front(item);
    }
    Ok(())
}
