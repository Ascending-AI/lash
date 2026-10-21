//! Immutable indexed sequences. Only constructors can establish shared ancestry.

use crate::allocation::{AllocationBudget, AllocationError, AllocationHold};
use std::sync::Arc;

const LEAF_ITEMS: usize = 32;

#[derive(Debug)]
enum Node<T> {
    Leaf {
        items: Vec<T>,
        _hold: AllocationHold,
    },
    Branch {
        left: Arc<Self>,
        right: Arc<Self>,
        len: usize,
        _hold: AllocationHold,
    },
}

impl<T> Node<T> {
    fn len(&self) -> usize {
        match self {
            Self::Leaf { items, .. } => items.len(),
            Self::Branch { len, .. } => *len,
        }
    }
}

/// A balanced append tree with immutable, at most 32-item leaves. Cloning a
/// view never clones its elements. Traversal visits only the requested suffix.
#[derive(Debug)]
pub struct PersistentSequence<T> {
    root: Option<Arc<Node<T>>>,
}

impl<T> Clone for PersistentSequence<T> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
        }
    }
}

impl<T> Default for PersistentSequence<T> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<T: Clone> PersistentSequence<T> {
    pub fn from_vec(items: Vec<T>, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        Self::default().append(&items, budget)
    }

    pub fn append(&self, items: &[T], budget: &AllocationBudget) -> Result<Self, AllocationError> {
        fn branch<T>(
            left: Arc<Node<T>>,
            right: Arc<Node<T>>,
            budget: &AllocationBudget,
        ) -> Result<Arc<Node<T>>, AllocationError> {
            let hold = budget.reserve(
                (std::mem::size_of::<Node<T>>() + 2 * std::mem::size_of::<usize>()) as u64,
            )?;
            let len = left.len() + right.len();
            Ok(Arc::new(Node::Branch {
                left,
                right,
                len,
                _hold: hold,
            }))
        }
        fn push<T: Clone>(
            node: &Arc<Node<T>>,
            item: T,
            budget: &AllocationBudget,
        ) -> Result<Arc<Node<T>>, AllocationError> {
            match node.as_ref() {
                Node::Leaf { items, .. } if items.len() < LEAF_ITEMS => {
                    let hold = budget.reserve(
                        (std::mem::size_of::<Node<T>>()
                            + 2 * std::mem::size_of::<usize>()
                            + LEAF_ITEMS * std::mem::size_of::<T>()) as u64,
                    )?;
                    let mut tail = Vec::with_capacity(LEAF_ITEMS);
                    tail.extend_from_slice(items);
                    tail.push(item);
                    Ok(Arc::new(Node::Leaf {
                        items: tail,
                        _hold: hold,
                    }))
                }
                _ if node.len().is_power_of_two() => {
                    let hold = budget.reserve(
                        (std::mem::size_of::<Node<T>>()
                            + 2 * std::mem::size_of::<usize>()
                            + std::mem::size_of::<T>()) as u64,
                    )?;
                    branch(
                        node.clone(),
                        Arc::new(Node::Leaf {
                            items: vec![item],
                            _hold: hold,
                        }),
                        budget,
                    )
                }
                Node::Branch { left, right, .. } => {
                    branch(left.clone(), push(right, item, budget)?, budget)
                }
                Node::Leaf { .. } => unreachable!("a full leaf has power-of-two length"),
            }
        }
        let mut root = self.root.clone();
        for item in items {
            root = Some(match root {
                Some(node) => push(&node, item.clone(), budget)?,
                None => {
                    let hold = budget.reserve(
                        (std::mem::size_of::<Node<T>>()
                            + 2 * std::mem::size_of::<usize>()
                            + std::mem::size_of::<T>()) as u64,
                    )?;
                    Arc::new(Node::Leaf {
                        items: vec![item.clone()],
                        _hold: hold,
                    })
                }
            });
        }
        Ok(Self { root })
    }

    pub fn prefix(&self, len: usize, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        assert!(len <= self.len());
        fn take<T: Clone>(
            node: &Arc<Node<T>>,
            len: usize,
            budget: &AllocationBudget,
        ) -> Result<Arc<Node<T>>, AllocationError> {
            if len == node.len() {
                return Ok(node.clone());
            }
            match node.as_ref() {
                Node::Leaf { items, .. } => {
                    let hold = budget.reserve(
                        (std::mem::size_of::<Node<T>>()
                            + 2 * std::mem::size_of::<usize>()
                            + len * std::mem::size_of::<T>()) as u64,
                    )?;
                    Ok(Arc::new(Node::Leaf {
                        items: items[..len].to_vec(),
                        _hold: hold,
                    }))
                }
                Node::Branch { left, .. } if len <= left.len() => take(left, len, budget),
                Node::Branch { left, right, .. } => {
                    let hold = budget.reserve(
                        (std::mem::size_of::<Node<T>>() + 2 * std::mem::size_of::<usize>()) as u64,
                    )?;
                    Ok(Arc::new(Node::Branch {
                        left: left.clone(),
                        right: take(right, len - left.len(), budget)?,
                        len,
                        _hold: hold,
                    }))
                }
            }
        }
        Ok(Self {
            root: self
                .root
                .as_ref()
                .filter(|_| len > 0)
                .map(|root| take(root, len, budget))
                .transpose()?,
        })
    }
}

impl<T> PersistentSequence<T> {
    pub fn len(&self) -> usize {
        self.root.as_ref().map_or(0, |root| root.len())
    }
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
    pub fn get(&self, mut index: usize) -> Option<&T> {
        let mut node = self.root.as_deref()?;
        loop {
            match node {
                Node::Leaf { items, .. } => return items.get(index),
                Node::Branch { left, right, .. } => {
                    node = if index < left.len() {
                        left
                    } else {
                        index -= left.len();
                        right
                    };
                }
            }
        }
    }
    pub fn iter(&self) -> SequenceIter<'_, T> {
        self.iter_from(0)
    }
    pub fn iter_from(&self, start: usize) -> SequenceIter<'_, T> {
        let mut iter = SequenceIter {
            pending: [None; usize::BITS as usize],
            height: 0,
            leaf: [].iter(),
        };
        if let Some(root) = &self.root {
            iter.descend(root, start);
        }
        iter
    }
    /// Allocation metadata for a new draft. Shared subtrees keep their
    /// original identity; only detached paths are reported again.
    pub fn changed_allocations(&self, previous: &Self, mut visit: impl FnMut(usize, u64)) {
        fn walk<T>(
            node: &Arc<Node<T>>,
            previous: Option<&Arc<Node<T>>>,
            visit: &mut impl FnMut(usize, u64),
        ) {
            if previous.is_some_and(|prior| Arc::ptr_eq(prior, node)) {
                return;
            }
            let extra = match node.as_ref() {
                Node::Leaf { items, .. } => items.capacity() * std::mem::size_of::<T>(),
                Node::Branch { .. } => 0,
            };
            visit(
                Arc::as_ptr(node) as usize,
                (std::mem::size_of::<Node<T>>() + 2 * std::mem::size_of::<usize>() + extra) as u64,
            );
            if let Node::Branch { left, right, .. } = node.as_ref() {
                let (a, b) = match previous.map(Arc::as_ref) {
                    Some(Node::Branch {
                        left: a, right: b, ..
                    }) if a.len() == left.len() => (Some(a), Some(b)),
                    _ => (previous, None),
                };
                walk(left, a, visit);
                walk(right, b, visit);
            }
        }
        if let Some(root) = &self.root {
            walk(root, previous.root.as_ref(), &mut visit);
        }
    }
    /// Skip structurally shared subtrees, validating only detached leaves.
    pub fn shared_prefix(&self, other: &Self, same: impl Fn(&T, &T) -> bool) -> usize {
        fn compare<T>(
            left: &Arc<Node<T>>,
            right: &Arc<Node<T>>,
            same: &impl Fn(&T, &T) -> bool,
        ) -> usize {
            if Arc::ptr_eq(left, right) {
                return left.len();
            }
            match (left.as_ref(), right.as_ref()) {
                (
                    Node::Branch {
                        left: a, right: b, ..
                    },
                    Node::Branch {
                        left: c, right: d, ..
                    },
                ) if a.len() == c.len() => {
                    let shared = compare(a, c, same);
                    if shared == a.len() {
                        shared + compare(b, d, same)
                    } else {
                        shared
                    }
                }
                (Node::Branch { left: a, .. }, _) if a.len() >= right.len() => {
                    compare(a, right, same)
                }
                (_, Node::Branch { left: b, .. }) if b.len() >= left.len() => {
                    compare(left, b, same)
                }
                (Node::Leaf { items: a, .. }, Node::Leaf { items: b, .. }) => {
                    a.iter().zip(b).take_while(|(a, b)| same(a, b)).count()
                }
                _ => 0,
            }
        }
        match (&self.root, &other.root) {
            (Some(a), Some(b)) => compare(a, b, &same),
            _ => 0,
        }
    }
}

/// Ordered traversal, descending to an appended suffix without visiting its prefix.
pub struct SequenceIter<'a, T> {
    pending: [Option<&'a Node<T>>; usize::BITS as usize],
    height: usize,
    leaf: std::slice::Iter<'a, T>,
}
impl<'a, T> SequenceIter<'a, T> {
    fn descend(&mut self, mut node: &'a Node<T>, mut start: usize) {
        if start >= node.len() {
            return;
        }
        loop {
            match node {
                Node::Leaf { items, .. } => {
                    self.leaf = items[start..].iter();
                    return;
                }
                Node::Branch { left, right, .. } => {
                    node = if start < left.len() {
                        self.pending[self.height] = Some(right);
                        self.height += 1;
                        left
                    } else {
                        start -= left.len();
                        right
                    };
                }
            }
        }
    }
}
impl<'a, T> Iterator for SequenceIter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.leaf.next() {
                return Some(item);
            }
            self.height = self.height.checked_sub(1)?;
            let node = self.pending[self.height].take()?;
            self.descend(node, 0);
        }
    }
}
