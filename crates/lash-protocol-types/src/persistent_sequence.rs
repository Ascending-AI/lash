//! Immutable indexed sequences. Only constructors can establish shared ancestry.

use std::sync::Arc;

const LEAF_ITEMS: usize = 32;

#[derive(Debug)]
enum Node<T> {
    Leaf {
        items: Vec<T>,
    },
    Branch {
        left: Arc<Self>,
        right: Arc<Self>,
        len: usize,
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
    pub fn from_vec(items: Vec<T>) -> Self {
        Self::default().append(&items)
    }
    pub fn append(&self, items: &[T]) -> Self {
        fn branch<T>(left: Arc<Node<T>>, right: Arc<Node<T>>) -> Arc<Node<T>> {
            let len = left.len() + right.len();
            Arc::new(Node::Branch { left, right, len })
        }
        fn push<T: Clone>(node: &Arc<Node<T>>, item: T) -> Arc<Node<T>> {
            match node.as_ref() {
                Node::Leaf { items } if items.len() < LEAF_ITEMS => {
                    let mut tail = Vec::with_capacity(LEAF_ITEMS);
                    tail.extend_from_slice(items);
                    tail.push(item);
                    Arc::new(Node::Leaf { items: tail })
                }
                _ if node.len().is_power_of_two() => {
                    branch(node.clone(), Arc::new(Node::Leaf { items: vec![item] }))
                }
                Node::Branch { left, right, .. } => branch(left.clone(), push(right, item)),
                Node::Leaf { .. } => unreachable!("a full leaf has power-of-two length"),
            }
        }
        let mut root = self.root.clone();
        for item in items {
            root = Some(match root {
                Some(node) => push(&node, item.clone()),
                None => Arc::new(Node::Leaf {
                    items: vec![item.clone()],
                }),
            });
        }
        Self { root }
    }
    pub fn prefix(&self, len: usize) -> Self {
        assert!(len <= self.len());
        fn take<T: Clone>(node: &Arc<Node<T>>, len: usize) -> Arc<Node<T>> {
            if len == node.len() {
                return node.clone();
            }
            match node.as_ref() {
                Node::Leaf { items } => Arc::new(Node::Leaf {
                    items: items[..len].to_vec(),
                }),
                Node::Branch { left, .. } if len <= left.len() => take(left, len),
                Node::Branch { left, right, .. } => Arc::new(Node::Branch {
                    left: left.clone(),
                    right: take(right, len - left.len()),
                    len,
                }),
            }
        }
        Self {
            root: self
                .root
                .as_ref()
                .filter(|_| len > 0)
                .map(|root| take(root, len)),
        }
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
